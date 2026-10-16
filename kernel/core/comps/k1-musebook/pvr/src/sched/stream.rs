// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: strict PowerVR firmware command-stream validation.

//! Command stream validation.
//!
//! Ports `pvr_stream.c`, `pvr_stream.h`, `pvr_stream_defs.c`,
//! `pvr_stream_defs.h` (723 lines).
//!
//! # Why this is a security boundary
//!
//! Job commands arrive as an opaque byte stream that Mesa builds and the firmware
//! consumes. The driver's job is to check it before the firmware sees it: a
//! stream is a sequence of typed, versioned fields, and upstream validates each
//! against a table in `pvr_stream_defs.c` that says what may appear, in what
//! order, and how wide it is.
//!
//! Skipping or loosening this hands unprivileged user space a channel to the
//! firmware. Any field this module does not understand must be rejected, never
//! passed through.
//!
//! Some fields are mandatory only when a particular quirk is present — upstream's
//! `stream_musthave_quirks`, derived from [`crate::device_info::Quirks`]. Since
//! BXE-2-32's quirk set is untested, a stream rejected for a missing must-have
//! field should say which quirk demanded it.
//!
//! # What a stream looks like
//!
//! Transcribed from upstream's `DOC: Streams` in `pvr_rogue_fwif_stream.h`:
//!
//! ```text
//! u32 main_stream_len   inclusive of this 8-byte header
//! u32 padding           must be zero
//! ...                   main stream fields, in table order, naturally aligned
//! u32 ext_header        optional; type, data bits, continuation bit
//! ...                   extension fields for the quirks the header names
//! ```
//!
//! The main stream is fixed by the GPU's *features*; the extension stream carries
//! the parameters that only exist because of a *quirk*. Both are positional:
//! there is no tag on a field, so a stream that omits one field and supplies the
//! next is indistinguishable from a valid stream one field shorter. That is why
//! the declared length is checked against the table's consumption and why a
//! trailing byte the table does not account for is refused.
//!
//! # The one thing this module does not decide
//!
//! Whether a field is *present* is a question about the device, not about the
//! stream, and the answer comes from the firmware's device-info block via
//! [`crate::device_info::DeviceInfo`]. A wrong answer there does not make a
//! stream be accepted that should not be — it makes every stream be rejected,
//! because Mesa and the driver would disagree about the layout from the first
//! optional field onwards.

use alloc::vec::Vec;

use crate::{
    device_info::DeviceInfo,
    error::{PvrError, PvrResult},
    rogue::fwif::{
        RogueFwifCdmRegistersCswitch, RogueFwifCmdCompute, RogueFwifCmdFrag, RogueFwifCmdGeom,
        RogueFwifCmdTransfer, RogueFwifGeomRegistersCaswitch,
    },
};

/// Which stream a definition applies to.
///
/// Mirrors upstream `enum pvr_stream_type`. There is deliberately no static
/// transfer context: upstream has no `PVR_STREAM_TYPE_STATIC_TRANSFER_CONTEXT`,
/// and `init_transfer_fw_objs()` requires `static_context_state_len == 0`, so a
/// transfer context that supplies static state is refused rather than parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamType {
    /// Geometry job command.
    Geometry,
    /// Fragment job command.
    Fragment,
    /// Compute job command.
    Compute,
    /// Transfer job command.
    Transfer,
    /// A render context's static state.
    StaticRenderContext,
    /// A compute context's static state.
    StaticComputeContext,
}

impl StreamType {
    /// Position of this type in upstream's `enum pvr_stream_type`.
    ///
    /// Used to index the must-have quirk table, which upstream keys the same way.
    const fn index(self) -> usize {
        match self {
            Self::Geometry => 0,
            Self::Fragment => 1,
            Self::Compute => 2,
            Self::Transfer => 3,
            Self::StaticRenderContext => 4,
            Self::StaticComputeContext => 5,
        }
    }

    /// The definition table for this type.
    const fn defs(self) -> &'static CmdDefs {
        match self {
            Self::Geometry => &CMD_GEOM_STREAM,
            Self::Fragment => &CMD_FRAG_STREAM,
            Self::Compute => &CMD_COMPUTE_STREAM,
            Self::Transfer => &CMD_TRANSFER_STREAM,
            Self::StaticRenderContext => &STATIC_RENDER_CONTEXT_STATE_STREAM,
            Self::StaticComputeContext => &STATIC_COMPUTE_CONTEXT_STATE_STREAM,
        }
    }
}

/// Number of stream types, upstream `PVR_STREAM_TYPE_MAX`.
const STREAM_TYPE_MAX: usize = 6;

/// Number of extension header types, upstream `PVR_STREAM_EXTHDR_TYPE_MAX`.
///
/// The header's type field is three bits wide, so this is a hard ceiling rather
/// than a chosen bound.
const EXTHDR_TYPE_MAX: usize = 8;

/// Shift of the type field in an extension header. Upstream
/// `PVR_STREAM_EXTHDR_TYPE_SHIFT`.
const EXTHDR_TYPE_SHIFT: u32 = 29;
/// Mask of the type field in an extension header. Upstream
/// `PVR_STREAM_EXTHDR_TYPE_MASK`.
const EXTHDR_TYPE_MASK: u32 = 7 << EXTHDR_TYPE_SHIFT;
/// Set when another extension header follows. Upstream
/// `PVR_STREAM_EXTHDR_CONTINUATION`.
const EXTHDR_CONTINUATION: u32 = 1 << 28;
/// Mask of the quirk bits in an extension header. Upstream
/// `PVR_STREAM_EXTHDR_DATA_MASK`.
const EXTHDR_DATA_MASK: u32 = !(EXTHDR_TYPE_MASK | EXTHDR_CONTINUATION);

/// Geometry extension header, bit 0. Upstream `PVR_STREAM_EXTHDR_GEOM0_BRN49927`.
const EXTHDR_GEOM0_BRN49927: u32 = 1 << 0;
/// Valid bits of the geometry extension header. Upstream
/// `PVR_STREAM_EXTHDR_GEOM0_VALID`.
const EXTHDR_GEOM0_VALID: u32 = EXTHDR_GEOM0_BRN49927;

/// Fragment extension header, bit 0. Upstream `PVR_STREAM_EXTHDR_FRAG0_BRN47217`.
const EXTHDR_FRAG0_BRN47217: u32 = 1 << 0;
/// Fragment extension header, bit 1. Upstream `PVR_STREAM_EXTHDR_FRAG0_BRN49927`.
const EXTHDR_FRAG0_BRN49927: u32 = 1 << 1;
/// Valid bits of the fragment extension header. Upstream
/// `PVR_STREAM_EXTHDR_FRAG0_VALID`.
///
/// Transcribed exactly: upstream defines this as `PVR_STREAM_EXTHDR_FRAG0_BRN49927`
/// alone and omits [`EXTHDR_FRAG0_BRN47217`]. See the `Divergence` note on
/// [`musthave_masks`] for the consequence, which this port reproduces rather
/// than quietly repairs.
const EXTHDR_FRAG0_VALID: u32 = EXTHDR_FRAG0_BRN49927;

/// Compute extension header, bit 0. Upstream
/// `PVR_STREAM_EXTHDR_COMPUTE0_BRN49927`.
const EXTHDR_COMPUTE0_BRN49927: u32 = 1 << 0;
/// Valid bits of the compute extension header. Upstream
/// `PVR_STREAM_EXTHDR_COMPUTE0_VALID`.
const EXTHDR_COMPUTE0_VALID: u32 = EXTHDR_COMPUTE0_BRN49927;

/// A device feature a stream field's presence depends on.
///
/// Mirrors the anonymous `enum { PVR_FEATURE_* }` in upstream's
/// `pvr_device_info.h`, which is a *different* numbering from the device-info
/// block's bit positions: it exists only to name the thirteen features
/// `pvr_device_has_feature()` can answer for, two of which are derived rather
/// than read from a bit.
///
/// An enumeration rather than upstream's `u32` with a `PVR_FEATURE_NOT` bit
/// smuggled into bit 31, because the negation belongs to the *gate* and not to
/// the feature; see [`FeatureGate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    /// Upstream `PVR_FEATURE_CDM_USER_MODE_QUEUE`. Derived, see
    /// [`Self::present`].
    CdmUserModeQueue,
    /// Upstream `PVR_FEATURE_CLUSTER_GROUPING`.
    ClusterGrouping,
    /// Upstream `PVR_FEATURE_COMPUTE_MORTON_CAPABLE`.
    ComputeMortonCapable,
    /// Upstream `PVR_FEATURE_GPU_MULTICORE_SUPPORT`.
    GpuMulticoreSupport,
    /// Upstream `PVR_FEATURE_ISP_ZLS_D24_S8_PACKING_OGL_MODE`.
    IspZlsD24S8PackingOglMode,
    /// Upstream `PVR_FEATURE_REQUIRES_FB_CDC_ZLS_SETUP`. Derived, see
    /// [`Self::present`].
    RequiresFbCdcZlsSetup,
    /// Upstream `PVR_FEATURE_S7_TOP_INFRASTRUCTURE`.
    S7TopInfrastructure,
    /// Upstream `PVR_FEATURE_TESSELLATION`.
    Tessellation,
    /// Upstream `PVR_FEATURE_TPU_DM_GLOBAL_REGISTERS`.
    TpuDmGlobalRegisters,
    /// Upstream `PVR_FEATURE_VDM_DRAWINDIRECT`.
    VdmDrawindirect,
    /// Upstream `PVR_FEATURE_VDM_OBJECT_LEVEL_LLS`.
    VdmObjectLevelLls,
    /// Upstream `PVR_FEATURE_ZLS_SUBTILE`.
    ZlsSubtile,
}

impl Feature {
    /// Whether the device has this feature.
    ///
    /// Ports upstream `pvr_device_has_feature()`, including its two derived
    /// cases, which are *not* single bits in the device-info block:
    ///
    /// - `CDM_USER_MODE_QUEUE` is a range test on the valued feature
    ///   `cdm_control_stream_format`;
    /// - `REQUIRES_FB_CDC_ZLS_SETUP` is a range test on `fbcdc_algorithm`, and
    ///   only asked when that feature is present at all.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
    /// (`pvr_device_has_feature`).
    pub fn present(self, info: &DeviceInfo) -> bool {
        let features = &info.features;
        match self {
            Self::ClusterGrouping => features.cluster_grouping,
            Self::ComputeMortonCapable => features.compute_morton_capable,
            Self::GpuMulticoreSupport => features.gpu_multicore_support,
            Self::IspZlsD24S8PackingOglMode => features.isp_zls_d24_s8_packing_ogl_mode,
            Self::S7TopInfrastructure => features.s7_top_infrastructure,
            Self::Tessellation => features.tessellation,
            Self::TpuDmGlobalRegisters => features.tpu_dm_global_registers,
            Self::VdmDrawindirect => features.vdm_drawindirect,
            Self::VdmObjectLevelLls => features.vdm_object_level_lls,
            Self::ZlsSubtile => features.zls_subtile,

            // Derived. Upstream reads the value with `PVR_FEATURE_VALUE`, which
            // leaves its zero-initialised local alone when the feature is
            // absent, so an absent `cdm_control_stream_format` fails the range
            // test — which our `FeatureValues` default of zero reproduces.
            Self::CdmUserModeQueue => {
                let format = info.values.cdm_control_stream_format;
                (2..=4).contains(&format)
            }
            Self::RequiresFbCdcZlsSetup => {
                features.fbcdc_algorithm && (info.values.fbcdc_algorithm < 3 || features.fb_cdc_v4)
            }
        }
    }
}

/// When a stream field is present.
///
/// Upstream encodes this in one `u32`: `PVR_FEATURE_NONE` (`U32_MAX`) for
/// unconditional, a feature index for "present if", and the index or-ed with
/// `PVR_FEATURE_NOT` (`BIT(31)`) for "present if not".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureGate {
    /// The field is always present. Upstream `PVR_FEATURE_NONE`.
    Always,
    /// The field is present when the feature is. Upstream a bare feature index.
    Has(Feature),
    /// The field is present when the feature is *not*. Upstream `PVR_FEATURE_NOT`.
    Not(Feature),
}

impl FeatureGate {
    /// Whether a field with this gate appears in a stream for `info`.
    ///
    /// Ports upstream `stream_def_is_supported()`.
    fn is_supported(self, info: &DeviceInfo) -> bool {
        match self {
            Self::Always => true,
            Self::Has(feature) => feature.present(info),
            Self::Not(feature) => !feature.present(info),
        }
    }
}

/// How wide a stream field is, and therefore how it is aligned.
///
/// Mirrors upstream `enum pvr_stream_size`. The scalar widths align to
/// themselves; an array aligns to eight regardless of its element width, which
/// is upstream's `PVR_STREAM_SIZE_ARRAY` passing `sizeof(u64)` as the alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldWidth {
    /// One byte. Upstream `PVR_STREAM_SIZE_8`.
    U8,
    /// Two bytes. Upstream `PVR_STREAM_SIZE_16`.
    U16,
    /// Four bytes. Upstream `PVR_STREAM_SIZE_32`.
    U32,
    /// Eight bytes. Upstream `PVR_STREAM_SIZE_64`.
    U64,
    /// A byte array of this length, aligned to eight. Upstream
    /// `PVR_STREAM_SIZE_ARRAY` with `array_size`.
    Array(usize),
}

impl FieldWidth {
    /// Bytes the field occupies in the stream and in the destination.
    const fn len(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 => 8,
            Self::Array(len) => len,
        }
    }

    /// Alignment the stream offset is rounded up to before the field is read.
    const fn align(self) -> usize {
        match self {
            Self::Array(_) => 8,
            other => other.len(),
        }
    }
}

/// One field of a stream. Upstream `struct pvr_stream_def`.
#[derive(Clone, Copy, Debug)]
pub struct StreamDef {
    /// Byte offset of the field within the destination structure.
    pub offset: usize,
    /// Width of the field.
    pub width: FieldWidth,
    /// When the field is present.
    pub feature: FeatureGate,
}

/// The fields one quirk contributes to an extension stream.
///
/// Upstream `struct pvr_stream_ext_def`.
#[derive(Clone, Copy, Debug)]
pub struct ExtDef {
    /// The fields, in stream order.
    pub stream: &'static [StreamDef],
    /// Bit in the extension header that announces them.
    pub header_mask: u32,
    /// The upstream BRN number the client is claiming.
    pub quirk: u32,
}

/// One extension header's worth of definitions. Upstream
/// `struct pvr_stream_ext_header`.
#[derive(Clone, Copy, Debug)]
pub struct ExtHeader {
    /// Definitions selectable through this header, in bit order.
    pub ext_streams: &'static [ExtDef],
    /// Bits a client may set in this header. Anything else is refused.
    pub valid_mask: u32,
}

/// A complete stream definition. Upstream `struct pvr_stream_cmd_defs`.
#[derive(Clone, Copy, Debug)]
pub struct CmdDefs {
    /// Which stream this describes.
    pub r#type: StreamType,
    /// The main stream's fields, in stream order.
    pub main_stream: &'static [StreamDef],
    /// The extension headers, indexed by the header's type field.
    pub ext_headers: &'static [ExtHeader],
    /// Size of the firmware structure the stream fills.
    pub dest_size: usize,
}

/// Builds a [`StreamDef`], mirroring upstream's `PVR_STREAM_DEF*` macros.
macro_rules! stream_def {
    ($owner:ty, $field:expr, $width:expr) => {
        StreamDef {
            offset: $field,
            width: $width,
            feature: FeatureGate::Always,
        }
    };
    ($owner:ty, $field:expr, $width:expr, has $feature:ident) => {
        StreamDef {
            offset: $field,
            width: $width,
            feature: FeatureGate::Has(Feature::$feature),
        }
    };
    ($owner:ty, $field:expr, $width:expr, not $feature:ident) => {
        StreamDef {
            offset: $field,
            width: $width,
            feature: FeatureGate::Not(Feature::$feature),
        }
    };
}

/// Byte offset of a nested field, for the tables below.
macro_rules! at {
    ($($path:tt)*) => { core::mem::offset_of!($($path)*) };
}

/// Upstream `rogue_fwif_cmd_geom_stream`.
static CMD_GEOM_STREAM_MAIN: &[StreamDef] = &[
    stream_def!(
        RogueFwifCmdGeom,
        at!(RogueFwifCmdGeom, regs.vdm_ctrl_stream_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdGeom,
        at!(RogueFwifCmdGeom, regs.tpu_border_colour_table),
        FieldWidth::U64
    ),
    stream_def!(RogueFwifCmdGeom, at!(RogueFwifCmdGeom, regs.vdm_draw_indirect0), FieldWidth::U64, has VdmDrawindirect),
    stream_def!(RogueFwifCmdGeom, at!(RogueFwifCmdGeom, regs.vdm_draw_indirect1), FieldWidth::U32, has VdmDrawindirect),
    stream_def!(
        RogueFwifCmdGeom,
        at!(RogueFwifCmdGeom, regs.ppp_ctrl),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdGeom,
        at!(RogueFwifCmdGeom, regs.te_psg),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdGeom,
        at!(RogueFwifCmdGeom, regs.vdm_context_resume_task0_size),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdGeom, at!(RogueFwifCmdGeom, regs.vdm_context_resume_task3_size), FieldWidth::U32, has VdmObjectLevelLls),
    stream_def!(
        RogueFwifCmdGeom,
        at!(RogueFwifCmdGeom, regs.view_idx),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdGeom, at!(RogueFwifCmdGeom, regs.pds_coeff_free_prog), FieldWidth::U32, has Tessellation),
];

/// Upstream `rogue_fwif_cmd_geom_stream_brn49927`.
static CMD_GEOM_STREAM_BRN49927: &[StreamDef] = &[stream_def!(
    RogueFwifCmdGeom,
    at!(RogueFwifCmdGeom, regs.tpu),
    FieldWidth::U32
)];

/// Upstream `cmd_geom_ext_streams_0`.
static CMD_GEOM_EXT_STREAMS_0: &[ExtDef] = &[ExtDef {
    stream: CMD_GEOM_STREAM_BRN49927,
    header_mask: EXTHDR_GEOM0_BRN49927,
    quirk: 49927,
}];

/// Upstream `cmd_geom_ext_headers`.
static CMD_GEOM_EXT_HEADERS: &[ExtHeader] = &[ExtHeader {
    ext_streams: CMD_GEOM_EXT_STREAMS_0,
    valid_mask: EXTHDR_GEOM0_VALID,
}];

/// Upstream `pvr_cmd_geom_stream`.
static CMD_GEOM_STREAM: CmdDefs = CmdDefs {
    r#type: StreamType::Geometry,
    main_stream: CMD_GEOM_STREAM_MAIN,
    ext_headers: CMD_GEOM_EXT_HEADERS,
    dest_size: size_of::<RogueFwifCmdGeom>(),
};

/// Upstream `rogue_fwif_cmd_frag_stream`.
static CMD_FRAG_STREAM_MAIN: &[StreamDef] = &[
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_scissor_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_dbias_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_oclqry_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_zlsctl),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_zload_store_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_stencil_load_store_base),
        FieldWidth::U64
    ),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, regs.fb_cdc_zls), FieldWidth::U64, has RequiresFbCdcZlsSetup),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.pbe_word),
        FieldWidth::Array(8 * 24)
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.tpu_border_colour_table),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.pds_bgnd),
        FieldWidth::Array(8 * 3)
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.pds_pr_bgnd),
        FieldWidth::Array(8 * 3)
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.usc_clear_register),
        FieldWidth::Array(4 * 8)
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.usc_pixel_output_ctrl),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_bgobjdepth),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_bgobjvals),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_aa),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, regs.isp_xtp_pipe_enable), FieldWidth::U32, has S7TopInfrastructure),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.isp_ctl),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.event_pixel_pds_info),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, regs.pixel_phantom), FieldWidth::U32, has ClusterGrouping),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.view_idx),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, regs.event_pixel_pds_data),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, regs.isp_oclqry_stride), FieldWidth::U32, has GpuMulticoreSupport),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, regs.isp_zls_pixels), FieldWidth::U32, has ZlsSubtile),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, regs.rgx_cr_blackpearl_fix), FieldWidth::U32, has IspZlsD24S8PackingOglMode),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, zls_stride),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdFrag,
        at!(RogueFwifCmdFrag, sls_stride),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdFrag, at!(RogueFwifCmdFrag, execute_count), FieldWidth::U32, has GpuMulticoreSupport),
];

/// Upstream `rogue_fwif_cmd_frag_stream_brn47217`.
static CMD_FRAG_STREAM_BRN47217: &[StreamDef] = &[stream_def!(
    RogueFwifCmdFrag,
    at!(RogueFwifCmdFrag, regs.isp_oclqry_stride),
    FieldWidth::U32
)];

/// Upstream `rogue_fwif_cmd_frag_stream_brn49927`.
static CMD_FRAG_STREAM_BRN49927: &[StreamDef] = &[stream_def!(
    RogueFwifCmdFrag,
    at!(RogueFwifCmdFrag, regs.tpu),
    FieldWidth::U32
)];

/// Upstream `cmd_frag_ext_streams_0`.
static CMD_FRAG_EXT_STREAMS_0: &[ExtDef] = &[
    ExtDef {
        stream: CMD_FRAG_STREAM_BRN47217,
        header_mask: EXTHDR_FRAG0_BRN47217,
        quirk: 47217,
    },
    ExtDef {
        stream: CMD_FRAG_STREAM_BRN49927,
        header_mask: EXTHDR_FRAG0_BRN49927,
        quirk: 49927,
    },
];

/// Upstream `cmd_frag_ext_headers`.
static CMD_FRAG_EXT_HEADERS: &[ExtHeader] = &[ExtHeader {
    ext_streams: CMD_FRAG_EXT_STREAMS_0,
    valid_mask: EXTHDR_FRAG0_VALID,
}];

/// Upstream `pvr_cmd_frag_stream`.
static CMD_FRAG_STREAM: CmdDefs = CmdDefs {
    r#type: StreamType::Fragment,
    main_stream: CMD_FRAG_STREAM_MAIN,
    ext_headers: CMD_FRAG_EXT_HEADERS,
    dest_size: size_of::<RogueFwifCmdFrag>(),
};

/// Upstream `rogue_fwif_cmd_compute_stream`.
static CMD_COMPUTE_STREAM_MAIN: &[StreamDef] = &[
    stream_def!(
        RogueFwifCmdCompute,
        at!(RogueFwifCmdCompute, regs.tpu_border_colour_table),
        FieldWidth::U64
    ),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.cdm_cb_queue), FieldWidth::U64, has CdmUserModeQueue),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.cdm_cb_base), FieldWidth::U64, has CdmUserModeQueue),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.cdm_cb), FieldWidth::U64, has CdmUserModeQueue),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.cdm_ctrl_stream_base), FieldWidth::U64, not CdmUserModeQueue),
    stream_def!(
        RogueFwifCmdCompute,
        at!(RogueFwifCmdCompute, regs.cdm_context_state_base_addr),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdCompute,
        at!(RogueFwifCmdCompute, regs.cdm_resume_pds1),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.cdm_item), FieldWidth::U32, has ComputeMortonCapable),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.compute_cluster), FieldWidth::U32, has ClusterGrouping),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, regs.tpu_tag_cdm_ctrl), FieldWidth::U32, has TpuDmGlobalRegisters),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, stream_start_offset), FieldWidth::U32, has CdmUserModeQueue),
    stream_def!(RogueFwifCmdCompute, at!(RogueFwifCmdCompute, execute_count), FieldWidth::U32, has GpuMulticoreSupport),
];

/// Upstream `rogue_fwif_cmd_compute_stream_brn49927`.
static CMD_COMPUTE_STREAM_BRN49927: &[StreamDef] = &[stream_def!(
    RogueFwifCmdCompute,
    at!(RogueFwifCmdCompute, regs.tpu),
    FieldWidth::U32
)];

/// Upstream `cmd_compute_ext_streams_0`.
static CMD_COMPUTE_EXT_STREAMS_0: &[ExtDef] = &[ExtDef {
    stream: CMD_COMPUTE_STREAM_BRN49927,
    header_mask: EXTHDR_COMPUTE0_BRN49927,
    quirk: 49927,
}];

/// Upstream `cmd_compute_ext_headers`.
static CMD_COMPUTE_EXT_HEADERS: &[ExtHeader] = &[ExtHeader {
    ext_streams: CMD_COMPUTE_EXT_STREAMS_0,
    valid_mask: EXTHDR_COMPUTE0_VALID,
}];

/// Upstream `pvr_cmd_compute_stream`.
static CMD_COMPUTE_STREAM: CmdDefs = CmdDefs {
    r#type: StreamType::Compute,
    main_stream: CMD_COMPUTE_STREAM_MAIN,
    ext_headers: CMD_COMPUTE_EXT_HEADERS,
    dest_size: size_of::<RogueFwifCmdCompute>(),
};

/// Upstream `rogue_fwif_cmd_transfer_stream`.
static CMD_TRANSFER_STREAM_MAIN: &[StreamDef] = &[
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.pds_bgnd0_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.pds_bgnd1_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.pds_bgnd3_sizeinfo),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_mtile_base),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.pbe_wordx_mrty),
        FieldWidth::Array(8 * 9)
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_bgobjvals),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.usc_pixel_output_ctrl),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.usc_clear_register0),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.usc_clear_register1),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.usc_clear_register2),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.usc_clear_register3),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_mtile_size),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_render_origin),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_ctl),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_aa),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.event_pixel_pds_info),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.event_pixel_pds_code),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.event_pixel_pds_data),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_render),
        FieldWidth::U32
    ),
    stream_def!(
        RogueFwifCmdTransfer,
        at!(RogueFwifCmdTransfer, regs.isp_rgn),
        FieldWidth::U32
    ),
    stream_def!(RogueFwifCmdTransfer, at!(RogueFwifCmdTransfer, regs.isp_xtp_pipe_enable), FieldWidth::U32, has S7TopInfrastructure),
    stream_def!(RogueFwifCmdTransfer, at!(RogueFwifCmdTransfer, regs.frag_screen), FieldWidth::U32, has GpuMulticoreSupport),
];

/// Upstream `pvr_cmd_transfer_stream`.
static CMD_TRANSFER_STREAM: CmdDefs = CmdDefs {
    r#type: StreamType::Transfer,
    main_stream: CMD_TRANSFER_STREAM_MAIN,
    ext_headers: &[],
    dest_size: size_of::<RogueFwifCmdTransfer>(),
};

/// Upstream `rogue_fwif_static_render_context_state_stream`.
///
/// The order is upstream's, which is *not* the declaration order of
/// `rogue_fwif_geom_registers_caswitch_geom_state`: the stream lists tasks 0, 1,
/// 2, 3, 4 for store and then resume, while the structure interleaves them
/// 0, 1, 2 / 0, 1, 2 / 3, 4 / 3, 4. Keeping the stream order and letting the
/// offsets jump around is what makes the two agree.
static STATIC_RENDER_CONTEXT_STATE_MAIN: &[StreamDef] = &[
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        at!(
            RogueFwifGeomRegistersCaswitch,
            geom_reg_vdm_context_state_base_addr
        ),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        at!(
            RogueFwifGeomRegistersCaswitch,
            geom_reg_vdm_context_state_resume_addr
        ),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        at!(
            RogueFwifGeomRegistersCaswitch,
            geom_reg_ta_context_state_base_addr
        ),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::StoreTask0),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::StoreTask1),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::StoreTask2),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::StoreTask3),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::StoreTask4),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::ResumeTask0),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::ResumeTask1),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::ResumeTask2),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::ResumeTask3),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(0, GeomStateField::ResumeTask4),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::StoreTask0),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::StoreTask1),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::StoreTask2),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::StoreTask3),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::StoreTask4),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::ResumeTask0),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::ResumeTask1),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::ResumeTask2),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::ResumeTask3),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifGeomRegistersCaswitch,
        geom_state_at(1, GeomStateField::ResumeTask4),
        FieldWidth::U64
    ),
];

/// One of the ten registers a `geom_state[]` element holds.
///
/// A named index rather than ten `offset_of!` spellings, because
/// `offset_of!(_, geom_state[0].x)` is not an expression `offset_of!` accepts.
#[derive(Clone, Copy)]
enum GeomStateField {
    /// `geom_reg_vdm_context_store_task0`.
    StoreTask0,
    /// `geom_reg_vdm_context_store_task1`.
    StoreTask1,
    /// `geom_reg_vdm_context_store_task2`.
    StoreTask2,
    /// `geom_reg_vdm_context_store_task3`.
    StoreTask3,
    /// `geom_reg_vdm_context_store_task4`.
    StoreTask4,
    /// `geom_reg_vdm_context_resume_task0`.
    ResumeTask0,
    /// `geom_reg_vdm_context_resume_task1`.
    ResumeTask1,
    /// `geom_reg_vdm_context_resume_task2`.
    ResumeTask2,
    /// `geom_reg_vdm_context_resume_task3`.
    ResumeTask3,
    /// `geom_reg_vdm_context_resume_task4`.
    ResumeTask4,
}

/// Offset of `geom_state[index].field` within [`RogueFwifGeomRegistersCaswitch`].
const fn geom_state_at(index: usize, field: GeomStateField) -> usize {
    use crate::rogue::fwif::RogueFwifGeomRegistersCaswitchGeomState as GeomState;

    let base = core::mem::offset_of!(RogueFwifGeomRegistersCaswitch, geom_state)
        + index * size_of::<GeomState>();
    let within = match field {
        GeomStateField::StoreTask0 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_store_task0)
        }
        GeomStateField::StoreTask1 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_store_task1)
        }
        GeomStateField::StoreTask2 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_store_task2)
        }
        GeomStateField::StoreTask3 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_store_task3)
        }
        GeomStateField::StoreTask4 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_store_task4)
        }
        GeomStateField::ResumeTask0 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_resume_task0)
        }
        GeomStateField::ResumeTask1 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_resume_task1)
        }
        GeomStateField::ResumeTask2 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_resume_task2)
        }
        GeomStateField::ResumeTask3 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_resume_task3)
        }
        GeomStateField::ResumeTask4 => {
            core::mem::offset_of!(GeomState, geom_reg_vdm_context_resume_task4)
        }
    };
    base + within
}

/// Upstream `pvr_static_render_context_state_stream`.
static STATIC_RENDER_CONTEXT_STATE_STREAM: CmdDefs = CmdDefs {
    r#type: StreamType::StaticRenderContext,
    main_stream: STATIC_RENDER_CONTEXT_STATE_MAIN,
    ext_headers: &[],
    dest_size: size_of::<RogueFwifGeomRegistersCaswitch>(),
};

/// Upstream `rogue_fwif_static_compute_context_state_stream`.
static STATIC_COMPUTE_CONTEXT_STATE_MAIN: &[StreamDef] = &[
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_context_pds0),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_context_pds1),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_terminate_pds),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_terminate_pds1),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_resume_pds0),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_context_pds0_b),
        FieldWidth::U64
    ),
    stream_def!(
        RogueFwifCdmRegistersCswitch,
        at!(RogueFwifCdmRegistersCswitch, cdmreg_cdm_resume_pds0_b),
        FieldWidth::U64
    ),
];

/// Upstream `pvr_static_compute_context_state_stream`.
static STATIC_COMPUTE_CONTEXT_STATE_STREAM: CmdDefs = CmdDefs {
    r#type: StreamType::StaticComputeContext,
    main_stream: STATIC_COMPUTE_CONTEXT_STATE_MAIN,
    ext_headers: &[],
    dest_size: size_of::<RogueFwifCdmRegistersCswitch>(),
};

/// The extension header bits a device's quirks make mandatory.
///
/// Ports upstream `pvr_stream_create_musthave_masks()`. Indexed by
/// [`StreamType::index`] then by extension header type.
///
/// Divergence: upstream computes this once in `pvr_device_gpu_init()` and caches
/// it in `pvr_device::stream_musthave_quirks`; this recomputes it per stream.
/// It is three branches over two booleans, the result is identical by
/// construction, without retaining another per-device cache.
/// Recorded in `ABI.md` compatibility limits.
///
/// # Divergence in upstream itself, reproduced here
///
/// `PVR_STREAM_EXTHDR_FRAG0_VALID` is defined as `PVR_STREAM_EXTHDR_FRAG0_BRN49927`
/// alone and omits `PVR_STREAM_EXTHDR_FRAG0_BRN47217`, even though this function
/// makes that same bit mandatory when BRN47217 is present. On such a device every
/// fragment stream is therefore refused: setting bit 0 fails the `valid_mask`
/// check in [`process_ext_stream`], and not setting it leaves the must-have mask
/// non-zero. The arithmetic on both sides is self-consistent — it is the mask
/// that is missing a term.
///
/// This port transcribes both masks unchanged, because Mesa is built against
/// upstream's behaviour and a driver that accepted a stream upstream rejects is
/// the more dangerous of the two errors. [`validate`] logs a warning naming the
/// quirk when the situation arises, so the failure is attributable rather than a
/// bare `EINVAL`. Recorded in `ABI.md` compatibility limits.
fn musthave_masks(info: &DeviceInfo) -> [[u32; EXTHDR_TYPE_MAX]; STREAM_TYPE_MAX] {
    let mut masks = [[0u32; EXTHDR_TYPE_MAX]; STREAM_TYPE_MAX];

    if crate::driver::has_uapi_quirk(info, 47217) {
        masks[StreamType::Fragment.index()][0] |= EXTHDR_FRAG0_BRN47217;
    }
    if crate::driver::has_uapi_quirk(info, 49927) {
        masks[StreamType::Geometry.index()][0] |= EXTHDR_GEOM0_BRN49927;
        masks[StreamType::Fragment.index()][0] |= EXTHDR_FRAG0_BRN49927;
        masks[StreamType::Compute.index()][0] |= EXTHDR_COMPUTE0_BRN49927;
    }

    masks
}

/// Validates a stream and converts it into the firmware's representation.
///
/// Ports upstream `pvr_stream_process()`. Returns the firmware structure as
/// bytes: the caller copies it into a CCCB or a firmware context object, and
/// reaches individual fields with `offset_of!`. A `Vec<u8>` rather than a typed
/// structure because the same function fills five different destinations and
/// because the bytes are only ever memcpy'd onward.
///
/// Returns [`crate::PvrError::InvalidArgument`] for anything unrecognised. That
/// is the whole point of the function: be strict. The rejections, in the order
/// they are checked:
///
/// 1. an empty stream;
/// 2. a stream too short to hold its own 8-byte header;
/// 3. a non-zero header padding word — reserved for a future extension, so a
///    client that sets it is asking for something this driver does not implement;
/// 4. a declared `main_stream_len` below the header or beyond the stream;
/// 5. a main stream that runs past `main_stream_len`, field by field;
/// 6. an extension header with an unknown type;
/// 7. an extension header with a data bit outside `valid_mask`;
/// 8. an extension stream for a quirk the device does not have;
/// 9. an extension stream that runs past the end;
/// 10. a missing must-have quirk, whether or not an extension stream was
///     supplied at all.
///
/// Trailing bytes that no definition accounts for are *not* an error, matching
/// upstream: the main stream is bounded by its declared length and the extension
/// stream terminates on its continuation bit. A client that appends junk after
/// the last header has it ignored, exactly as upstream ignores it.
pub fn validate(r#type: StreamType, raw: &[u8], info: &DeviceInfo) -> PvrResult<Vec<u8>> {
    let defs = r#type.defs();
    let mut dest = alloc::vec![0u8; defs.dest_size];

    // Upstream `if (!stream || !stream_size)`.
    if raw.is_empty() {
        return Err(PvrError::InvalidArgument);
    }

    let mut offset = 0usize;
    let main_stream_len = read_u32(raw, &mut offset, raw.len())?;
    let padding = read_u32(raw, &mut offset, raw.len())?;

    let main_stream_len = main_stream_len as usize;
    if main_stream_len < offset || main_stream_len > raw.len() || padding != 0 {
        return Err(PvrError::InvalidArgument);
    }

    process_1(
        defs.main_stream,
        raw,
        &mut offset,
        main_stream_len,
        &mut dest,
        info,
    )?;

    let masks = musthave_masks(info);
    if offset < raw.len() {
        process_ext_stream(defs, raw, offset, &mut dest, info, masks[r#type.index()])?;
    } else {
        // No extension stream, so there must be no must-have quirks.
        for (header_type, mask) in masks[r#type.index()]
            .iter()
            .enumerate()
            .take(defs.ext_headers.len())
        {
            if *mask != 0 {
                warn_musthave(r#type, header_type, *mask);
                return Err(PvrError::InvalidArgument);
            }
        }
    }

    Ok(dest)
}

/// Reads one field out of the stream and copies it into the destination.
///
/// Ports upstream `pvr_stream_process_1()`, including the order of its two
/// checks: a definition whose offset is outside the destination is an error even
/// when the feature gate would have skipped the field, because that is a bug in
/// the table rather than in the stream.
fn process_1(
    defs: &[StreamDef],
    stream: &[u8],
    offset: &mut usize,
    stream_size: usize,
    dest: &mut [u8],
    info: &DeviceInfo,
) -> PvrResult<()> {
    for def in defs {
        if def.offset >= dest.len() {
            return Err(PvrError::InvalidArgument);
        }
        if !def.feature.is_supported(info) {
            continue;
        }

        let len = def.width.len();
        // Upstream writes `data_size` bytes at `dest + offset` having checked
        // only the start. The tables are static and every entry fits — asserted
        // by `every_definition_fits_its_destination` — so this is the same
        // behaviour with the bound the C cannot express.
        let end = def
            .offset
            .checked_add(len)
            .ok_or(PvrError::InvalidArgument)?;
        if end > dest.len() {
            return Err(PvrError::InvalidArgument);
        }

        let start = align_up(*offset, def.width.align())?;
        let stream_end = start.checked_add(len).ok_or(PvrError::InvalidArgument)?;
        if stream_end > stream_size {
            return Err(PvrError::InvalidArgument);
        }
        dest[def.offset..end].copy_from_slice(&stream[start..stream_end]);
        *offset = stream_end;
    }

    Ok(())
}

/// Walks the extension headers and the streams they select.
///
/// Ports upstream `pvr_stream_process_ext_stream()`. The loop is `do { } while
/// (continuation)`, so a stream whose main part ends short of the total length
/// must carry at least one header — which is exactly what `offset < raw.len()`
/// told the caller.
fn process_ext_stream(
    defs: &CmdDefs,
    stream: &[u8],
    mut offset: usize,
    dest: &mut [u8],
    info: &DeviceInfo,
    mut masks: [u32; EXTHDR_TYPE_MAX],
) -> PvrResult<()> {
    loop {
        let ext_header = read_u32(stream, &mut offset, stream.len())?;
        let header_type = ((ext_header & EXTHDR_TYPE_MASK) >> EXTHDR_TYPE_SHIFT) as usize;
        let data = ext_header & EXTHDR_DATA_MASK;

        let Some(header) = defs.ext_headers.get(header_type) else {
            return Err(PvrError::InvalidArgument);
        };
        if data & !header.valid_mask != 0 {
            return Err(PvrError::InvalidArgument);
        }

        masks[header_type] &= !data;

        for ext_def in header.ext_streams {
            if ext_header & ext_def.header_mask == 0 {
                continue;
            }
            if !crate::driver::has_uapi_quirk(info, ext_def.quirk) {
                ostd::warn!(
                    "command stream claims quirk BRN{} the device does not have",
                    ext_def.quirk
                );
                return Err(PvrError::InvalidArgument);
            }
            process_1(
                ext_def.stream,
                stream,
                &mut offset,
                stream.len(),
                dest,
                info,
            )?;
        }

        if ext_header & EXTHDR_CONTINUATION == 0 {
            break;
        }
    }

    for (header_type, mask) in masks.iter().enumerate().take(defs.ext_headers.len()) {
        if *mask != 0 {
            warn_musthave(defs.r#type, header_type, *mask);
            return Err(PvrError::InvalidArgument);
        }
    }

    Ok(())
}

/// Names the quirk whose extension fields a stream failed to supply.
///
/// A bare `EINVAL` on a device nobody has run this driver on is close to
/// undiagnosable: the client is not wrong about the uapi, it is wrong about which
/// hardware bugs this GPU has.
fn warn_musthave(r#type: StreamType, header_type: usize, mask: u32) {
    let quirk = r#type
        .defs()
        .ext_headers
        .get(header_type)
        .and_then(|header| {
            header
                .ext_streams
                .iter()
                .find(|def| def.header_mask & mask != 0)
        })
        .map(|def| def.quirk);

    match quirk {
        Some(quirk) => ostd::warn!(
            "{:?} stream omits the fields BRN{} makes mandatory (header {}, mask {:#x})",
            r#type,
            quirk,
            header_type,
            mask
        ),
        None => ostd::warn!(
            "{:?} stream omits mandatory extension bits {:#x} in header {}, \
             which no definition claims — the must-have mask and the valid mask disagree",
            r#type,
            mask,
            header_type
        ),
    }
}

/// Reads a `u32`, advancing `offset`. Upstream `pvr_stream_get_data` for a `u32`.
fn read_u32(stream: &[u8], offset: &mut usize, stream_size: usize) -> PvrResult<u32> {
    let start = align_up(*offset, 4)?;
    let end = start.checked_add(4).ok_or(PvrError::InvalidArgument)?;
    if end > stream_size || end > stream.len() {
        return Err(PvrError::InvalidArgument);
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&stream[start..end]);
    *offset = end;
    Ok(u32::from_le_bytes(bytes))
}

/// Rounds up to a power-of-two alignment. Upstream's `ALIGN()`.
fn align_up(value: usize, align: usize) -> PvrResult<usize> {
    debug_assert!(align.is_power_of_two());
    value
        .checked_add(align - 1)
        .map(|sum| sum & !(align - 1))
        .ok_or(PvrError::InvalidArgument)
}

/// Builds a stream a well-formed client would produce.
///
/// Shared by the tests and available to a caller that wants to check its own
/// understanding of the layout. `values` supplies one entry per *present* main
/// stream field, in table order; a field is written at the width and alignment
/// the table gives it, so a caller cannot accidentally produce a stream the
/// validator would reject for a reason other than the one under test.
///
/// Returns `None` if `values` does not have exactly one entry per present field,
/// which is the mistake this helper exists to make impossible to make silently.
#[cfg(any(test, ktest))]
pub(crate) fn build_stream(
    r#type: StreamType,
    info: &DeviceInfo,
    values: &[u64],
) -> Option<Vec<u8>> {
    let present: Vec<&StreamDef> = r#type
        .defs()
        .main_stream
        .iter()
        .filter(|def| def.feature.is_supported(info))
        .collect();
    if present.len() != values.len() {
        return None;
    }

    let mut out = alloc::vec![0u8; 8];
    for (def, value) in present.iter().zip(values) {
        let align = def.width.align();
        while !out.len().is_multiple_of(align) {
            out.push(0);
        }
        match def.width {
            FieldWidth::U8 => out.push(*value as u8),
            FieldWidth::U16 => out.extend_from_slice(&(*value as u16).to_le_bytes()),
            FieldWidth::U32 => out.extend_from_slice(&(*value as u32).to_le_bytes()),
            FieldWidth::U64 => out.extend_from_slice(&value.to_le_bytes()),
            FieldWidth::Array(len) => {
                // Every eight bytes of the array carry the same value, so a test
                // can recognise the whole block from one number.
                for _ in 0..len / 8 {
                    out.extend_from_slice(&value.to_le_bytes());
                }
                for _ in 0..len % 8 {
                    out.push(*value as u8);
                }
            }
        }
    }

    let len = out.len() as u32;
    out[..4].copy_from_slice(&len.to_le_bytes());
    Some(out)
}

/// How many main-stream fields a device described by `info` expects.
///
/// The companion of [`main_stream_len`], for a caller that wants to produce a
/// value per field rather than a byte count.
pub fn main_stream_field_count(r#type: StreamType, info: &DeviceInfo) -> usize {
    r#type
        .defs()
        .main_stream
        .iter()
        .filter(|def| def.feature.is_supported(info))
        .count()
}

/// How long a stream for `type` is on a device described by `info`.
///
/// The size a well-formed client produces: the 8-byte header plus every present
/// main-stream field at its natural alignment. Exists so that the tests can
/// build accepted streams without hand-computing offsets, and so that a caller
/// can sanity-check a `cmd_stream_len` before copying from user space.
pub fn main_stream_len(r#type: StreamType, info: &DeviceInfo) -> usize {
    let mut offset = 8;
    for def in r#type.defs().main_stream {
        if !def.feature.is_supported(info) {
            continue;
        }
        offset = (offset + def.width.align() - 1) & !(def.width.align() - 1);
        offset += def.width.len();
    }
    offset
}

/// Acceptance and rejection tests for the definition tables and the walker.
///
/// The stream is the one thing in this driver that unprivileged user space
/// controls the contents of byte for byte, so the rejection cases matter more
/// than the acceptance ones and there is one per class of malformed input.
///
/// No device is needed: [`validate`] takes a [`DeviceInfo`] by reference and a
/// test can describe whatever GPU it wants to talk about.
#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::device_info::{Features, Quirks};

    /// A device with none of the optional features and none of the quirks.
    ///
    /// The layout Mesa would produce for the plainest possible Rogue, and the
    /// baseline the feature-gating tests vary from.
    fn plain() -> DeviceInfo {
        DeviceInfo::default()
    }

    /// The same, with `brn49927` present so the extension stream is reachable.
    fn with_brn49927() -> DeviceInfo {
        let mut info = plain();
        info.quirks = Quirks {
            brn49927: true,
            ..Quirks::default()
        };
        info
    }

    /// Reads a `u64` out of a validated command.
    fn read_u64(command: &[u8], offset: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&command[offset..offset + 8]);
        u64::from_le_bytes(bytes)
    }

    /// Reads a `u32` out of a validated command.
    fn read_u32_at(command: &[u8], offset: usize) -> u32 {
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(&command[offset..offset + 4]);
        u32::from_le_bytes(bytes)
    }

    /// Appends an extension header.
    fn push_ext_header(stream: &mut Vec<u8>, header: u32) {
        while !stream.len().is_multiple_of(4) {
            stream.push(0);
        }
        stream.extend_from_slice(&header.to_le_bytes());
    }

    #[ktest]
    fn a_well_formed_geometry_stream_lands_in_the_right_fields() {
        let info = plain();
        // Six fields are present on a plain device: the two 64-bit addresses,
        // then ppp_ctrl, te_psg, vdm_context_resume_task0_size and view_idx.
        let values = [0x1111_2222_3333_4444, 0x5555_6666_7777_8888, 9, 10, 11, 12];
        let stream = build_stream(StreamType::Geometry, &info, &values).expect("field count");

        let command = validate(StreamType::Geometry, &stream, &info).expect("accepted");
        assert_eq!(command.len(), size_of::<RogueFwifCmdGeom>());

        assert_eq!(
            read_u64(&command, at!(RogueFwifCmdGeom, regs.vdm_ctrl_stream_base)),
            values[0]
        );
        assert_eq!(
            read_u64(
                &command,
                at!(RogueFwifCmdGeom, regs.tpu_border_colour_table)
            ),
            values[1]
        );
        assert_eq!(
            read_u32_at(&command, at!(RogueFwifCmdGeom, regs.ppp_ctrl)),
            9
        );
        assert_eq!(
            read_u32_at(&command, at!(RogueFwifCmdGeom, regs.te_psg)),
            10
        );
        assert_eq!(
            read_u32_at(&command, at!(RogueFwifCmdGeom, regs.view_idx)),
            12
        );

        // The fields the plain device has no features for are untouched, which is
        // the whole point of the gate: they are not in the stream at all.
        assert_eq!(
            read_u64(&command, at!(RogueFwifCmdGeom, regs.vdm_draw_indirect0)),
            0
        );
        assert_eq!(
            read_u32_at(&command, at!(RogueFwifCmdGeom, regs.pds_coeff_free_prog)),
            0
        );
    }

    #[ktest]
    fn a_feature_adds_fields_and_moves_everything_after_them() {
        let mut info = plain();
        info.features = Features {
            vdm_drawindirect: true,
            ..Features::default()
        };

        // Two more fields now, so a stream built for the plain device is short.
        let plain_stream =
            build_stream(StreamType::Geometry, &plain(), &[1, 2, 3, 4, 5, 6]).expect("count");
        assert_eq!(
            validate(StreamType::Geometry, &plain_stream, &info),
            Err(PvrError::InvalidArgument)
        );

        let stream = build_stream(
            StreamType::Geometry,
            &info,
            &[1, 2, 0xabc, 0xdef, 3, 4, 5, 6],
        )
        .expect("count");
        let command = validate(StreamType::Geometry, &stream, &info).expect("accepted");
        assert_eq!(
            read_u64(&command, at!(RogueFwifCmdGeom, regs.vdm_draw_indirect0)),
            0xabc
        );
        assert_eq!(
            read_u32_at(&command, at!(RogueFwifCmdGeom, regs.vdm_draw_indirect1)),
            0xdef
        );
    }

    #[ktest]
    fn a_not_feature_gate_selects_the_other_field() {
        // `cdm_ctrl_stream_base` appears only when CDM_USER_MODE_QUEUE is absent,
        // and the three `cdm_cb*` registers only when it is present. That feature
        // is derived from a valued one, so it is set by writing the value.
        let without = plain();
        assert!(!Feature::CdmUserModeQueue.present(&without));

        let mut with = plain();
        with.values.cdm_control_stream_format = 3;
        assert!(Feature::CdmUserModeQueue.present(&with));

        // Absent: tpu_border_colour_table, cdm_ctrl_stream_base,
        // cdm_context_state_base_addr, cdm_resume_pds1.
        let stream = build_stream(StreamType::Compute, &without, &[1, 2, 3, 4]).expect("count");
        let command = validate(StreamType::Compute, &stream, &without).expect("accepted");
        assert_eq!(
            read_u64(
                &command,
                at!(RogueFwifCmdCompute, regs.cdm_ctrl_stream_base)
            ),
            2
        );
        assert_eq!(read_u64(&command, at!(RogueFwifCmdCompute, regs.cdm_cb)), 0);

        // Present: the three queue registers replace it, plus stream_start_offset.
        let stream =
            build_stream(StreamType::Compute, &with, &[1, 2, 3, 4, 5, 6, 7]).expect("count");
        let command = validate(StreamType::Compute, &stream, &with).expect("accepted");
        assert_eq!(
            read_u64(&command, at!(RogueFwifCmdCompute, regs.cdm_cb_queue)),
            2
        );
        assert_eq!(
            read_u64(
                &command,
                at!(RogueFwifCmdCompute, regs.cdm_ctrl_stream_base)
            ),
            0
        );
    }

    #[ktest]
    fn an_empty_stream_is_refused() {
        assert_eq!(
            validate(StreamType::Geometry, &[], &plain()),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn a_stream_too_short_for_its_own_header_is_refused() {
        for len in 1..8 {
            let stream = alloc::vec![0u8; len];
            assert_eq!(
                validate(StreamType::Geometry, &stream, &plain()),
                Err(PvrError::InvalidArgument),
                "a {len}-byte stream cannot hold an 8-byte header"
            );
        }
    }

    #[ktest]
    fn a_non_zero_header_padding_word_is_refused() {
        let info = plain();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        // The word after the length is reserved for a future extension, so a
        // client that sets it is asking for something this driver has not
        // implemented — not something to ignore.
        stream[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn a_declared_length_that_excludes_the_header_is_refused() {
        let info = plain();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        for len in [0u32, 1, 7] {
            stream[..4].copy_from_slice(&len.to_le_bytes());
            assert_eq!(
                validate(StreamType::Geometry, &stream, &info),
                Err(PvrError::InvalidArgument),
                "main_stream_len {len} is below the header"
            );
        }
    }

    #[ktest]
    fn a_declared_length_beyond_the_stream_is_refused() {
        let info = plain();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        let too_long = stream.len() as u32 + 1;
        stream[..4].copy_from_slice(&too_long.to_le_bytes());
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn a_main_stream_shorter_than_the_table_is_refused() {
        let info = plain();
        let full = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();

        // Truncate one field at a time. Every one of these is a stream that is
        // internally consistent — the declared length matches the bytes — and
        // still wrong, because the table says more fields must follow.
        for drop in 1..=4 {
            let mut stream = full[..full.len() - drop].to_vec();
            let len = stream.len() as u32;
            stream[..4].copy_from_slice(&len.to_le_bytes());
            assert_eq!(
                validate(StreamType::Geometry, &stream, &info),
                Err(PvrError::InvalidArgument),
                "a stream {drop} bytes short must be refused"
            );
        }
    }

    #[ktest]
    fn an_extension_header_of_an_unknown_type_is_refused() {
        let info = with_brn49927();
        // brn49927 makes the extension header mandatory, so build the accepted
        // stream first and then corrupt the header's type field.
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        push_ext_header(&mut stream, EXTHDR_GEOM0_BRN49927);
        stream.extend_from_slice(&7u32.to_le_bytes());
        assert!(validate(StreamType::Geometry, &stream, &info).is_ok());

        for r#type in 1..8u32 {
            let mut stream =
                build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
            push_ext_header(
                &mut stream,
                (r#type << EXTHDR_TYPE_SHIFT) | EXTHDR_GEOM0_BRN49927,
            );
            stream.extend_from_slice(&7u32.to_le_bytes());
            assert_eq!(
                validate(StreamType::Geometry, &stream, &info),
                Err(PvrError::InvalidArgument),
                "geometry has one extension header, so type {type} does not exist"
            );
        }
    }

    #[ktest]
    fn an_extension_header_bit_outside_the_valid_mask_is_refused() {
        let info = with_brn49927();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        // Bit 1 is not defined for the geometry header. Accepting it would mean a
        // future quirk's bit silently doing nothing.
        push_ext_header(&mut stream, EXTHDR_GEOM0_BRN49927 | (1 << 1));
        stream.extend_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn an_extension_stream_for_an_absent_quirk_is_refused() {
        // The device does *not* have brn49927, so a client claiming the quirk's
        // extra field is either confused or probing.
        let info = plain();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        push_ext_header(&mut stream, EXTHDR_GEOM0_BRN49927);
        stream.extend_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn a_truncated_extension_stream_is_refused() {
        let info = with_brn49927();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        // The header promises brn49927's `tpu` register and then the stream ends.
        push_ext_header(&mut stream, EXTHDR_GEOM0_BRN49927);
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn a_missing_must_have_quirk_is_refused_with_and_without_an_extension_stream() {
        let info = with_brn49927();

        // No extension stream at all: upstream's "there must not be any must-have
        // quirks" branch.
        let stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );

        // An extension header that does not claim the quirk: the must-have mask
        // survives the `&= !data` and is caught after the loop.
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        push_ext_header(&mut stream, 0);
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn an_extension_stream_field_reaches_its_destination() {
        let info = with_brn49927();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        push_ext_header(&mut stream, EXTHDR_GEOM0_BRN49927);
        stream.extend_from_slice(&0xfeedu32.to_le_bytes());

        let command = validate(StreamType::Geometry, &stream, &info).expect("accepted");
        assert_eq!(
            read_u32_at(&command, at!(RogueFwifCmdGeom, regs.tpu)),
            0xfeed
        );
    }

    #[ktest]
    fn a_continuation_bit_makes_a_second_header_mandatory() {
        let info = with_brn49927();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        push_ext_header(&mut stream, EXTHDR_GEOM0_BRN49927 | EXTHDR_CONTINUATION);
        stream.extend_from_slice(&7u32.to_le_bytes());
        // The continuation bit says another header follows, and none does.
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn the_fragment_must_have_mask_and_valid_mask_contradict_each_other() {
        // Upstream defines `PVR_STREAM_EXTHDR_FRAG0_VALID` as BRN49927 alone while
        // `pvr_stream_create_musthave_masks()` makes BRN47217's bit mandatory when
        // that quirk is present. This test pins the consequence rather than
        // asserting it is desirable: on such a device no fragment stream can be
        // built, because bit 0 both must be set and may not be.
        let mut info = plain();
        info.quirks = Quirks {
            brn47217: true,
            ..Quirks::default()
        };

        let masks = musthave_masks(&info);
        assert_eq!(
            masks[StreamType::Fragment.index()][0],
            EXTHDR_FRAG0_BRN47217
        );
        assert_eq!(EXTHDR_FRAG0_VALID & EXTHDR_FRAG0_BRN47217, 0);

        let base_values: Vec<u64> = (0..CMD_FRAG_STREAM_MAIN
            .iter()
            .filter(|def| def.feature.is_supported(&info))
            .count())
            .map(|i| i as u64 + 1)
            .collect();
        let base = build_stream(StreamType::Fragment, &info, &base_values).unwrap();

        // Not claiming it: the must-have mask is non-zero at the end.
        assert_eq!(
            validate(StreamType::Fragment, &base, &info),
            Err(PvrError::InvalidArgument)
        );

        // Claiming it: refused by the valid mask instead.
        let mut claimed = base.clone();
        push_ext_header(&mut claimed, EXTHDR_FRAG0_BRN47217);
        claimed.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            validate(StreamType::Fragment, &claimed, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn a_transfer_stream_has_no_extension_headers() {
        let info = plain();
        assert!(CMD_TRANSFER_STREAM.ext_headers.is_empty());

        let count = CMD_TRANSFER_STREAM_MAIN
            .iter()
            .filter(|def| def.feature.is_supported(&info))
            .count();
        let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
        let stream = build_stream(StreamType::Transfer, &info, &values).unwrap();
        let command = validate(StreamType::Transfer, &stream, &info).expect("accepted");
        assert_eq!(command.len(), size_of::<RogueFwifCmdTransfer>());

        // A stream that appends an extension header to a type that has none is
        // refused, because header type 0 does not exist for it.
        let mut with_header = stream.clone();
        push_ext_header(&mut with_header, 0);
        assert_eq!(
            validate(StreamType::Transfer, &with_header, &info),
            Err(PvrError::InvalidArgument)
        );
    }

    #[ktest]
    fn an_array_field_is_copied_whole_and_aligned_to_eight() {
        let info = plain();
        // `pbe_word` is 24 u64s and the first array in the fragment stream.
        let count = CMD_FRAG_STREAM_MAIN
            .iter()
            .filter(|def| def.feature.is_supported(&info))
            .count();
        let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
        let stream = build_stream(StreamType::Fragment, &info, &values).unwrap();
        let command = validate(StreamType::Fragment, &stream, &info).expect("accepted");

        // The seventh present field on a plain device is `pbe_word` (the six
        // 64-bit ISP addresses come first; `fb_cdc_zls` is gated off).
        let expected = 7u64;
        let base = at!(RogueFwifCmdFrag, regs.pbe_word);
        for i in 0..24 {
            assert_eq!(read_u64(&command, base + i * 8), expected, "pbe_word[{i}]");
        }
    }

    #[ktest]
    fn every_definition_fits_its_destination() {
        // Upstream checks only that a definition's offset is inside the
        // destination, not that the whole field is. The tables are static, so this
        // is a property of the transcription rather than of any stream — and if it
        // ever failed, `process_1`'s bound check would turn every submission into
        // `EINVAL` rather than a memory error.
        for r#type in [
            StreamType::Geometry,
            StreamType::Fragment,
            StreamType::Compute,
            StreamType::Transfer,
            StreamType::StaticRenderContext,
            StreamType::StaticComputeContext,
        ] {
            let defs = r#type.defs();
            let all = defs.main_stream.iter().chain(
                defs.ext_headers
                    .iter()
                    .flat_map(|header| header.ext_streams)
                    .flat_map(|ext| ext.stream),
            );
            for def in all {
                let end = def.offset + def.width.len();
                assert!(
                    end <= defs.dest_size,
                    "{:?}: a field at {} of {} bytes overruns a {}-byte destination",
                    r#type,
                    def.offset,
                    def.width.len(),
                    defs.dest_size
                );
                // Scalars only. `align()` is the alignment the *stream* cursor is
                // rounded up to before a field is read, not anything the
                // destination has to satisfy: upstream copies arrays with
                // `dest + offset` and an alignment argument that applies to the
                // source, so `u32[8]` sitting at a destination offset of 20 is
                // correct. Conflating the two is what this assertion did.
                if !matches!(def.width, FieldWidth::Array(_)) {
                    assert!(
                        def.offset.is_multiple_of(def.width.align().min(8)),
                        "{:?}: a scalar field at {} is not naturally aligned",
                        r#type,
                        def.offset
                    );
                }
            }
        }
    }

    /// Cross-checks the tables against upstream `pvr_stream_defs.c`.
    ///
    /// Transcribed as `(width, gate)` per entry, in stream order, from the commit
    /// `pvr/UPSTREAM` records. This is the check that catches a field silently
    /// dropped or reordered during the port: the offsets come from `offset_of!`
    /// and cannot be wrong about the *structure*, but nothing else would notice a
    /// missing row or a `PVR_STREAM_DEF` transcribed as
    /// `PVR_STREAM_DEF_FEATURE`.
    #[ktest]
    fn the_tables_match_upstream_pvr_stream_defs() {
        use FeatureGate::{Always, Has, Not};
        use FieldWidth::{Array, U32, U64};

        let geom: &[(FieldWidth, FeatureGate)] = &[
            (U64, Always),
            (U64, Always),
            (U64, Has(Feature::VdmDrawindirect)),
            (U32, Has(Feature::VdmDrawindirect)),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Has(Feature::VdmObjectLevelLls)),
            (U32, Always),
            (U32, Has(Feature::Tessellation)),
        ];
        let frag: &[(FieldWidth, FeatureGate)] = &[
            (U64, Always),
            (U64, Always),
            (U64, Always),
            (U64, Always),
            (U64, Always),
            (U64, Always),
            (U64, Has(Feature::RequiresFbCdcZlsSetup)),
            (Array(8 * 24), Always),
            (U64, Always),
            (Array(8 * 3), Always),
            (Array(8 * 3), Always),
            (Array(4 * 8), Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Has(Feature::S7TopInfrastructure)),
            (U32, Always),
            (U32, Always),
            (U32, Has(Feature::ClusterGrouping)),
            (U32, Always),
            (U32, Always),
            (U32, Has(Feature::GpuMulticoreSupport)),
            (U32, Has(Feature::ZlsSubtile)),
            (U32, Has(Feature::IspZlsD24S8PackingOglMode)),
            (U32, Always),
            (U32, Always),
            (U32, Has(Feature::GpuMulticoreSupport)),
        ];
        let compute: &[(FieldWidth, FeatureGate)] = &[
            (U64, Always),
            (U64, Has(Feature::CdmUserModeQueue)),
            (U64, Has(Feature::CdmUserModeQueue)),
            (U64, Has(Feature::CdmUserModeQueue)),
            (U64, Not(Feature::CdmUserModeQueue)),
            (U64, Always),
            (U32, Always),
            (U32, Has(Feature::ComputeMortonCapable)),
            (U32, Has(Feature::ClusterGrouping)),
            (U32, Has(Feature::TpuDmGlobalRegisters)),
            (U32, Has(Feature::CdmUserModeQueue)),
            (U32, Has(Feature::GpuMulticoreSupport)),
        ];
        let transfer: &[(FieldWidth, FeatureGate)] = &[
            (U64, Always),
            (U64, Always),
            (U64, Always),
            (U64, Always),
            (Array(8 * 9), Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Always),
            (U32, Has(Feature::S7TopInfrastructure)),
            (U32, Has(Feature::GpuMulticoreSupport)),
        ];

        let expected: &[(StreamType, &[(FieldWidth, FeatureGate)])] = &[
            (StreamType::Geometry, geom),
            (StreamType::Fragment, frag),
            (StreamType::Compute, compute),
            (StreamType::Transfer, transfer),
        ];

        for (r#type, rows) in expected {
            let defs = r#type.defs();
            assert_eq!(
                defs.main_stream.len(),
                rows.len(),
                "{type:?} has the wrong number of main stream fields"
            );
            for (index, (def, (width, gate))) in defs.main_stream.iter().zip(*rows).enumerate() {
                assert_eq!(def.width, *width, "{type:?} field {index} width");
                assert_eq!(def.feature, *gate, "{type:?} field {index} gate");
            }
        }

        // The two static-state streams are all unconditional 64-bit registers:
        // 23 for the render one and 7 for the compute one.
        for (r#type, count) in [
            (StreamType::StaticRenderContext, 23),
            (StreamType::StaticComputeContext, 7),
        ] {
            let defs = r#type.defs();
            assert_eq!(defs.main_stream.len(), count);
            for def in defs.main_stream {
                assert_eq!(def.width, U64);
                assert_eq!(def.feature, Always);
            }
        }

        // The extension tables: one header each for geometry and compute with one
        // quirk in it, two quirks for fragment, none for transfer or the static
        // streams.
        assert_eq!(CMD_GEOM_EXT_HEADERS.len(), 1);
        assert_eq!(CMD_GEOM_EXT_HEADERS[0].ext_streams.len(), 1);
        assert_eq!(CMD_GEOM_EXT_HEADERS[0].ext_streams[0].quirk, 49927);
        assert_eq!(CMD_FRAG_EXT_HEADERS.len(), 1);
        assert_eq!(CMD_FRAG_EXT_HEADERS[0].ext_streams.len(), 2);
        assert_eq!(CMD_FRAG_EXT_HEADERS[0].ext_streams[0].quirk, 47217);
        assert_eq!(CMD_FRAG_EXT_HEADERS[0].ext_streams[1].quirk, 49927);
        assert_eq!(CMD_COMPUTE_EXT_HEADERS.len(), 1);
        assert_eq!(CMD_COMPUTE_EXT_HEADERS[0].ext_streams.len(), 1);
        assert_eq!(CMD_COMPUTE_EXT_HEADERS[0].ext_streams[0].quirk, 49927);
        assert!(CMD_TRANSFER_STREAM.ext_headers.is_empty());
        assert!(STATIC_RENDER_CONTEXT_STATE_STREAM.ext_headers.is_empty());
        assert!(STATIC_COMPUTE_CONTEXT_STATE_STREAM.ext_headers.is_empty());
    }

    #[ktest]
    fn the_static_render_context_stream_visits_both_geometry_cores_in_upstreams_order() {
        // Upstream lists store 0..4 then resume 0..4 for core 0, then the same for
        // core 1, while the structure interleaves them. Getting this wrong would
        // put a resume address in a store slot, which the firmware would use as a
        // valid GPU address.
        let stream = STATIC_RENDER_CONTEXT_STATE_MAIN;
        assert_eq!(stream.len(), 23);

        let core_size = size_of::<crate::rogue::fwif::RogueFwifGeomRegistersCaswitchGeomState>();
        let base = at!(RogueFwifGeomRegistersCaswitch, geom_state);
        for (index, def) in stream[3..13].iter().enumerate() {
            assert!(
                def.offset >= base && def.offset < base + core_size,
                "field {index} of core 0 landed outside core 0"
            );
        }
        for (index, def) in stream[13..23].iter().enumerate() {
            assert!(
                def.offset >= base + core_size && def.offset < base + 2 * core_size,
                "field {index} of core 1 landed outside core 1"
            );
        }

        // Every one of the twenty is a different register.
        let mut offsets: Vec<usize> = stream.iter().map(|def| def.offset).collect();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(offsets.len(), 23);
    }

    #[ktest]
    fn the_musthave_masks_follow_the_devices_quirks() {
        // No quirks: nothing is mandatory, and a stream with no extension part is
        // accepted — which the acceptance test above already relies on.
        let masks = musthave_masks(&plain());
        assert!(masks.iter().all(|row| row.iter().all(|mask| *mask == 0)));

        // brn49927 makes it mandatory on three of the six stream types and on
        // none of the others.
        let masks = musthave_masks(&with_brn49927());
        assert_eq!(
            masks[StreamType::Geometry.index()][0],
            EXTHDR_GEOM0_BRN49927
        );
        assert_eq!(
            masks[StreamType::Fragment.index()][0],
            EXTHDR_FRAG0_BRN49927
        );
        assert_eq!(
            masks[StreamType::Compute.index()][0],
            EXTHDR_COMPUTE0_BRN49927
        );
        assert_eq!(masks[StreamType::Transfer.index()][0], 0);
        assert_eq!(masks[StreamType::StaticRenderContext.index()][0], 0);
        assert_eq!(masks[StreamType::StaticComputeContext.index()][0], 0);
    }

    #[ktest]
    fn the_declared_length_bounds_the_main_stream_even_when_more_bytes_follow() {
        // A client that declares a short main stream and then supplies the rest of
        // the fields must not have them read as main stream fields: past the
        // declared length they are extension stream, and the walker has to refuse
        // rather than read on.
        let info = plain();
        let mut stream = build_stream(StreamType::Geometry, &info, &[1, 2, 3, 4, 5, 6]).unwrap();
        let short = (stream.len() - 8) as u32;
        stream[..4].copy_from_slice(&short.to_le_bytes());
        assert_eq!(
            validate(StreamType::Geometry, &stream, &info),
            Err(PvrError::InvalidArgument)
        );
    }
}
