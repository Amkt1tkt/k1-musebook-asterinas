// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: validated firmware feature and hardware-quirk descriptions.

//! Hardware features, quirks and enhancements.
//!
//! Ports `pvr_device_info.c` and `pvr_device_info.h` (441 lines).
//!
//! BVNC comes from the device registers; feature, quirk (BRN) and enhancement
//! (ERN) masks come from the matching firmware's device-info block.
//! Unknown bits are retained and reported. A matching firmware image alone does
//! not establish that every hardware feature or workaround is implemented.

use alloc::vec::Vec;

use crate::rogue::fwif::dev_info_bits;

/// Hardware features.
///
/// A subset of upstream's 60-odd booleans: those the port actually branches on.
/// Add fields as the port grows, keeping the bit positions in
/// [`Self::from_bitmask`] aligned with `pvr_rogue_fwif_dev_info.h`.
///
/// [`Self::from_bitmask`]: DeviceInfo::from_bitmask
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// Compute (CDM) support.
    pub compute: bool,
    /// Compute work may overlap other data masters. Feeds `max_coeffs`.
    pub compute_overlap: bool,
    /// MIPS firmware processor. Expected true on BXE-2-32.
    pub mips: bool,
    /// META firmware processor. Expected false on BXE-2-32.
    pub meta: bool,
    /// RISC-V firmware processor. Expected false on BXE-2-32.
    pub riscv_fw_processor: bool,
    /// Number of clusters is reported by the firmware.
    pub num_clusters: bool,
    /// Multicore GPU.
    pub gpu_multicore_support: bool,
    /// `RGX_CR_CORE_ID` uses the packed BVNC layout.
    pub pbvnc_coreid_reg: bool,
    /// The RogueXE variant, which has a smaller minimum free list.
    pub roguexe: bool,
    /// Tile region headers are addressed through a separate heap, requiring
    /// [`Quirks::brn63142`].
    pub tile_region_protection: bool,
    /// System cache (SLC) size is configurable.
    pub slc_size_configurable: bool,
    /// The firmware reports a physical bus width. The width itself is
    /// [`FeatureValues::phys_bus_width`]; the MIPS firmware path requires more
    /// than 32 bits and refuses to run otherwise.
    pub phys_bus_width: bool,
    /// The firmware core lives in the MARS power domain rather than SIDEKICK,
    /// which changes which idle bits [`crate::fw::startstop::stop`] waits on.
    /// The reported value is [`FeatureValues::layout_mars`].
    pub layout_mars: bool,
    /// PBE2 is inside the XE block, which widens `ROGUE_CR_SOFT_RESET`'s
    /// full-write mask. Selects between the two `MASKFULL` variants in
    /// [`crate::fw::startstop`].
    pub pbe2_in_xe: bool,
    /// A second TPU generation, whose reset needs `ROGUE_CR_SOFT_RESET2`.
    pub xe_tpu2: bool,
    /// `ROGUE_CR_SYS_BUS_SECURE` defaults to protected and must be cleared
    /// before the minimal boot-time register setup.
    pub sys_bus_secure_reset: bool,
    /// XT-generation top-level infrastructure, on which
    /// `ROGUE_CR_BIF_READS_EXT_STATUS` does not exist and must not be polled.
    pub xt_top_infrastructure: bool,
    /// The firmware reports the SLC cache line width. The width itself is
    /// [`FeatureValues::slc_cache_line_size_bits`].
    pub slc_cache_line_size_bits: bool,
    /// The firmware reports the SLC size. The size itself is
    /// [`FeatureValues::slc_size_in_kilobytes`].
    pub slc_size_in_kilobytes: bool,

    // These features determine command-stream layout.
    // Every one of these decides whether a field *appears* in a job command
    // stream, so a missing one is not a lost optimisation: Mesa and the driver
    // would disagree about the stream's layout from that field onwards and every
    // submission would be refused. See `sched::stream::Feature`.
    /// Clusters are grouped. Upstream `cluster_grouping`.
    pub cluster_grouping: bool,
    /// The compute data master can walk in Morton order. Upstream
    /// `compute_morton_capable`.
    pub compute_morton_capable: bool,
    /// Version 4 of the frame buffer compression descriptor cache. Upstream
    /// `fb_cdc_v4`.
    pub fb_cdc_v4: bool,
    /// The firmware reports a frame buffer compression algorithm. The algorithm
    /// itself is [`FeatureValues::fbcdc_algorithm`].
    pub fbcdc_algorithm: bool,
    /// The ISP packs D24S8 in OpenGL order. Upstream
    /// `isp_zls_d24_s8_packing_ogl_mode`.
    pub isp_zls_d24_s8_packing_ogl_mode: bool,
    /// S7-generation top-level infrastructure. Upstream `s7_top_infrastructure`.
    pub s7_top_infrastructure: bool,
    /// Tessellation. Upstream `tessellation`.
    pub tessellation: bool,
    /// The TPU has per-data-master global registers. Upstream
    /// `tpu_dm_global_registers`.
    pub tpu_dm_global_registers: bool,
    /// The VDM can issue indirect draws. Upstream `vdm_drawindirect`.
    pub vdm_drawindirect: bool,
    /// The VDM's local link list is per object. Upstream `vdm_object_level_lls`.
    pub vdm_object_level_lls: bool,
    /// Sub-tile Z/stencil load and store. Upstream `zls_subtile`.
    pub zls_subtile: bool,
    /// The XE memory hierarchy, which changes how the fragment context state is
    /// sized. Upstream `xe_memory_hierarchy`.
    pub xe_memory_hierarchy: bool,
    /// The firmware reports the raster pipe count. The count itself is
    /// [`FeatureValues::num_raster_pipes`].
    pub num_raster_pipes: bool,
    /// The firmware reports the ISP IPP pipe count. The count itself is
    /// [`FeatureValues::num_isp_ipp_pipes`].
    pub num_isp_ipp_pipes: bool,
    /// The firmware reports the maximum number of XPU slaves. The count itself is
    /// [`FeatureValues::xpu_max_slaves`].
    pub xpu_max_slaves: bool,

    // The two below decide how a hardware render target is described to the
    // firmware; see `crate::render::hwrt`. Upstream reads both with
    // `PVR_FEATURE_VALUE()` and treats a missing one as fatal for
    // `CREATE_HWRT_DATASET`, so they are flags-plus-values like the rest.
    /// The firmware reports how many samples the ISP processes per pixel. The
    /// count itself is [`FeatureValues::isp_samples_per_pixel`].
    pub isp_samples_per_pixel: bool,
    /// The firmware reports a simple internal parameter format version. The
    /// version itself is [`FeatureValues::simple_parameter_format_version`], and
    /// its presence selects the 2x2 macrotile layout.
    pub simple_parameter_format_version: bool,
}

/// Values carried by the features that have one.
///
/// The firmware appends a `u64` per *valued* feature that is present, in bit
/// order, after the feature bitmask. Only the ones this port reads are surfaced;
/// the rest are consumed and dropped, which is why
/// [`FEATURE_HAS_VALUE`] has to stay complete even for features with no field
/// here.
///
/// A value is only meaningful when the matching flag in [`Features`] is set;
/// upstream's `PVR_FEATURE_VALUE()` leaves the caller's variable untouched
/// otherwise, so each field's documented fallback matches the initializer at
/// upstream's call site in `pvr_drv.c`.
#[derive(Clone, Copy, Debug)]
pub struct FeatureValues {
    /// Common Store size, in dwords. Upstream default `512 * 4 * 4`.
    pub common_store_size_in_dwords: u32,
    /// Maximum ISP tiles in flight. Upstream default 0.
    pub isp_max_tiles_in_flight: u32,
    /// Maximum USC partitions. Upstream default 0.
    pub max_partitions: u32,
    /// Number of clusters. Upstream default 1.
    pub num_clusters: u32,
    /// Physical bus width in bits. No upstream default: `pvr_mips_wrapper_init()`
    /// treats a missing value as fatal, so [`Self::default`] leaves it 0 and
    /// callers must consult [`Features::phys_bus_width`] first.
    pub phys_bus_width: u32,
    /// MARS layout revision. Upstream default 0, meaning "no MARS domain".
    pub layout_mars: u32,
    /// SLC cache line width in bits. No upstream default: `rogue_slc_init()`
    /// propagates the lookup error.
    pub slc_cache_line_size_bits: u32,
    /// SLC size in kilobytes. Upstream default 0, which selects
    /// `ROGUE_FWIF_INICFG_DISABLE_DM_OVERLAP`.
    pub slc_size_in_kilobytes: u32,
    /// Tile width in pixels. Upstream default 0.
    pub tile_size_x: u32,
    /// Tile height in pixels. Upstream default 0.
    pub tile_size_y: u32,
    /// Minimum USC output registers per pixel. Upstream default 0.
    pub usc_min_output_registers_per_pix: u32,
    /// Compute control stream format. Upstream default 0.
    ///
    /// Read as a range test rather than a number: `PVR_FEATURE_CDM_USER_MODE_QUEUE`
    /// is "this is between two and four", which a zero fails — the same answer
    /// upstream's zero-initialised local gives when the feature is absent.
    pub cdm_control_stream_format: u32,
    /// Frame buffer compression descriptor cache algorithm. Upstream default 0.
    pub fbcdc_algorithm: u32,
    /// Number of raster pipes. Upstream default 0.
    pub num_raster_pipes: u32,
    /// Number of ISP IPP pipes. Upstream default 0.
    pub num_isp_ipp_pipes: u32,
    /// Maximum number of XPU slave cores. Upstream default 0.
    pub xpu_max_slaves: u32,
    /// ISP samples processed per pixel. No upstream default:
    /// `get_cr_te_aa_val()` and `get_cr_isp_mtile_size_val()` propagate the
    /// lookup error rather than assuming one.
    pub isp_samples_per_pixel: u32,
    /// Simple internal parameter format version. No upstream default: the value
    /// is only read when [`Features::simple_parameter_format_version`] is set,
    /// and upstream `WARN_ON`s anything but 2.
    pub simple_parameter_format_version: u32,
}

impl Default for FeatureValues {
    fn default() -> Self {
        Self {
            common_store_size_in_dwords: 512 * 4 * 4,
            isp_max_tiles_in_flight: 0,
            max_partitions: 0,
            num_clusters: 1,
            phys_bus_width: 0,
            layout_mars: 0,
            slc_cache_line_size_bits: 0,
            slc_size_in_kilobytes: 0,
            tile_size_x: 0,
            tile_size_y: 0,
            usc_min_output_registers_per_pix: 0,
            cdm_control_stream_format: 0,
            fbcdc_algorithm: 0,
            num_raster_pipes: 0,
            num_isp_ipp_pipes: 0,
            xpu_max_slaves: 0,
            isp_samples_per_pixel: 0,
            simple_parameter_format_version: 0,
        }
    }
}

/// Hardware bugs to work around, upstream's "BRN"s.
///
/// All twelve that `pvr_device_info.c` maps are present, so a set bit this
/// structure has no field for really does mean "the firmware knows a quirk this
/// driver has never heard of".
#[derive(Clone, Copy, Debug, Default)]
pub struct Quirks {
    /// Splits the Common Store, capping the allocation region.
    pub brn44079: bool,
    /// Reported to user space as a must-have quirk.
    pub brn47217: bool,
    /// Compute without overlap needs two extra lines of coefficients.
    pub brn48492: bool,
    /// Reported to user space as an optional quirk.
    pub brn48545: bool,
    /// Reported to user space as a must-have quirk.
    pub brn49927: bool,
    /// Not yet consumed by this port.
    pub brn50767: bool,
    /// Reported to user space as an optional quirk.
    pub brn51764: bool,
    /// Reported to user space as a must-have quirk.
    pub brn62269: bool,
    /// Region headers need their own heap, so [`crate::uapi::HeapId::RgnHdr`]
    /// becomes present.
    pub brn63142: bool,
    /// Not yet consumed by this port.
    pub brn63553: bool,
    /// Raises the minimum free list size on RogueXE.
    pub brn66011: bool,
    /// Not yet consumed by this port.
    pub brn71242: bool,
}

/// Hardware improvements to opt into, upstream's "ERN"s.
///
/// All eight that `pvr_device_info.c` maps are present; see [`Quirks`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Enhancements {
    /// Improved MMU cache flush handling. Also reported to user space.
    pub ern35421: bool,
    /// Compute shared registers count towards `max_coeffs`.
    pub ern38020: bool,
    /// No shared registers are held back for pending allocation.
    pub ern38748: bool,
    /// Reported to user space.
    pub ern42064: bool,
    /// Not yet consumed by this port.
    pub ern42290: bool,
    /// Not yet consumed by this port.
    pub ern42606: bool,
    /// Not yet consumed by this port.
    pub ern47025: bool,
    /// Not yet consumed by this port.
    pub ern57596: bool,
}

/// The complete description of one GPU, as the firmware reports it.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceInfo {
    /// Features present.
    pub features: Features,
    /// Values carried by the features that have one.
    pub values: FeatureValues,
    /// Bugs to work around.
    pub quirks: Quirks,
    /// Improvements available.
    pub enhancements: Enhancements,
}

/// Result of decoding firmware bitmasks: known fields plus every set bit this
/// port does not yet map into [`Features`] / [`Quirks`] / [`Enhancements`].
///
/// Unknown bits remain available for reporting; they are not treated as supported
/// capabilities.
#[derive(Clone, Debug, Default)]
pub struct BitmaskDecode {
    /// Fields this port understands.
    pub info: DeviceInfo,
    /// Feature bit indices that were set but have no field in [`Features`].
    pub unknown_features: Vec<u32>,
    /// Quirk bit indices that were set but have no field in [`Quirks`].
    pub unknown_quirks: Vec<u32>,
    /// Enhancement bit indices that were set but have no field in [`Enhancements`].
    pub unknown_enhancements: Vec<u32>,
}

/// Errors from [`DeviceInfo::from_device_info_block`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceInfoError {
    /// The buffer is shorter than the device-info header.
    TruncatedHeader,
    /// A mask length in the header overruns the remaining buffer.
    TruncatedMasks,
    /// A valued feature is set but its parameter is past the end of the
    /// parameter array.
    ///
    /// Upstream returns `-EINVAL` from `pvr_device_info_set_features()` here and
    /// calls the stream malformed, which it is: the driver and the firmware
    /// disagree about how many parameters follow the mask, so every value after
    /// this point would be read from the wrong slot.
    MissingFeatureParam,
}

/// Feature bits that carry a `u64` parameter, in ascending bit order.
///
/// **This list must be complete, not just the features this port reads.** The
/// firmware packs one parameter per *valued* feature that is present, in bit
/// order and with nothing to mark where one ends; misclassifying a single bit
/// shifts every later value by one slot, and the symptom would be a plausible
/// but wrong `DEV_QUERY` answer rather than a failure.
///
/// Transcribed from the `FEATURE_MAPPING_VALUE()` rows of upstream's
/// `features_mapping[]`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device_info.c>.
pub const VALUED_FEATURES: &[u32] = {
    use dev_info_bits::features as F;
    &[
        F::FEATURE_CDM_CONTROL_STREAM_FORMAT,
        F::FEATURE_COMMON_STORE_SIZE_IN_DWORDS,
        F::FEATURE_ECC_RAMS,
        F::FEATURE_FBCDC,
        F::FEATURE_FBCDC_ALGORITHM,
        F::FEATURE_FBCDC_ARCHITECTURE,
        F::FEATURE_FBC_MAX_DEFAULT_DESCRIPTORS,
        F::FEATURE_FBC_MAX_LARGE_DESCRIPTORS,
        F::FEATURE_ISP_MAX_TILES_IN_FLIGHT,
        F::FEATURE_ISP_SAMPLES_PER_PIXEL,
        F::FEATURE_LAYOUT_MARS,
        F::FEATURE_MAX_PARTITIONS,
        F::FEATURE_META,
        F::FEATURE_META_COREMEM_SIZE,
        F::FEATURE_NUM_CLUSTERS,
        F::FEATURE_NUM_ISP_IPP_PIPES,
        F::FEATURE_NUM_OSIDS,
        F::FEATURE_NUM_RASTER_PIPES,
        F::FEATURE_PHYS_BUS_WIDTH,
        F::FEATURE_SIMPLE_PARAMETER_FORMAT_VERSION,
        F::FEATURE_SLC_BANKS,
        F::FEATURE_SLC_CACHE_LINE_SIZE_BITS,
        F::FEATURE_SLC_SIZE_IN_KILOBYTES,
        F::FEATURE_TILE_SIZE_X,
        F::FEATURE_TILE_SIZE_Y,
        F::FEATURE_USC_MIN_OUTPUT_REGISTERS_PER_PIX,
        F::FEATURE_VIRTUAL_ADDRESS_SPACE_BITS,
        F::FEATURE_XE_ARCHITECTURE,
        F::FEATURE_XPU_MAX_REGBANKS_ADDR_WIDTH,
        F::FEATURE_XPU_MAX_SLAVES,
        F::FEATURE_XPU_REGISTER_BROADCAST,
    ]
};

/// Whether `bits` is strictly ascending, i.e. sorted with no duplicates.
///
/// A `const fn` with a `while` loop rather than an iterator chain because it has
/// to run inside the `const` assertion below, where iterators are unavailable.
const fn is_strictly_ascending(bits: &[u32]) -> bool {
    let mut i = 1;
    while i < bits.len() {
        if bits[i - 1] >= bits[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// [`feature_has_value`] binary-searches [`VALUED_FEATURES`], which only answers
/// correctly while that list is sorted by *bit value*.
///
/// The list is transcribed in upstream's order, which is alphabetical by feature
/// name; that it also comes out in ascending bit order is a coincidence nothing
/// in the source protects. Adding a valued feature whose name sorts differently
/// from its bit — alphabetically, as the surrounding rows invite — would make
/// `binary_search` miss it, and a missed valued feature shifts every later
/// firmware parameter by one slot with no error anywhere. Refuse to build
/// instead of answering wrongly, the same trade the layout assertions in
/// `rogue::fwif` make.
const _: () = assert!(is_strictly_ascending(VALUED_FEATURES));

/// Whether a feature bit is followed by a parameter word.
///
/// Binary-searches [`VALUED_FEATURES`], so this is only correct while that list
/// is sorted by bit value with no duplicates. The `const` assertion above holds
/// that invariant at compile time.
fn feature_has_value(bit: u32) -> bool {
    VALUED_FEATURES.binary_search(&bit).is_ok()
}

/// Header at the start of the firmware's device-info block.
///
/// Layout mirrors upstream `struct pvr_fw_device_info_header`, not the trailing
/// `FwInfoHeader`. All four fields are `u64`; each count measures `u64` words.
/// The masks follow the full 32-byte header. A round-trip fixture alone cannot
/// validate these widths because its writer may share the parser's assumptions.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_fw_info.h>
/// (`struct pvr_fw_device_info_header`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DeviceInfoBlockHeader {
    /// Quirk (BRN) mask length in `u64` words.
    pub brn_mask_size: u64,
    /// Enhancement (ERN) mask length in `u64` words.
    pub ern_mask_size: u64,
    /// Feature mask length in `u64` words.
    pub feature_mask_size: u64,
    /// Feature parameter array length in `u64` words (values for valued features).
    pub feature_param_size: u64,
}

impl DeviceInfoBlockHeader {
    /// Bytes occupied by this header.
    pub const LEN: usize = 32;

    /// Decode from the first 32 bytes of a device-info block (little-endian).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DeviceInfoError> {
        if bytes.len() < Self::LEN {
            return Err(DeviceInfoError::TruncatedHeader);
        }
        Ok(Self {
            brn_mask_size: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            ern_mask_size: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            feature_mask_size: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            feature_param_size: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        })
    }
}

const _: () = assert!(DeviceInfoBlockHeader::LEN == size_of::<DeviceInfoBlockHeader>());

impl DeviceInfo {
    /// Decodes the three bitmasks from a firmware image's device-info block.
    ///
    /// The masks may be longer than what this driver knows about — the firmware
    /// is allowed to be newer than us. Set bits beyond our mapped fields are
    /// collected into [`BitmaskDecode`]'s `unknown_*` lists, mirroring upstream's
    /// "Unsupported quirks in firmware image" warning.
    ///
    /// `feature_params` is the array that follows the feature mask: one `u64`
    /// per *present* feature that has a value, in bit order. Running off its end
    /// is [`DeviceInfoError::MissingFeatureParam`], exactly as upstream's
    /// `pvr_device_info_set_features()` returns `-EINVAL` there.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device_info.c>.
    pub fn from_bitmask(
        features: &[u64],
        feature_params: &[u64],
        quirks: &[u64],
        enhancements: &[u64],
    ) -> Result<BitmaskDecode, DeviceInfoError> {
        let mut info = DeviceInfo::default();
        let mut unknown_features = Vec::new();
        let mut unknown_quirks = Vec::new();
        let mut unknown_enhancements = Vec::new();

        let mut param_idx = 0usize;
        let mut param_error = None;
        collect_bits(features, |bit| {
            use dev_info_bits::features as F;

            // The parameter is consumed for every valued feature that is set,
            // whether or not this port has a field to put it in. Skipping one
            // would misalign every parameter after it.
            let value = if feature_has_value(bit) {
                let value = feature_params.get(param_idx).copied();
                param_idx += 1;
                if value.is_none() {
                    param_error = Some(DeviceInfoError::MissingFeatureParam);
                }
                value.unwrap_or(0)
            } else {
                0
            };
            let value = value as u32;

            match bit {
                b if b == F::FEATURE_COMPUTE => info.features.compute = true,
                b if b == F::FEATURE_COMPUTE_OVERLAP => info.features.compute_overlap = true,
                b if b == F::FEATURE_MIPS => info.features.mips = true,
                b if b == F::FEATURE_META => info.features.meta = true,
                b if b == F::FEATURE_RISCV_FW_PROCESSOR => info.features.riscv_fw_processor = true,
                b if b == F::FEATURE_NUM_CLUSTERS => {
                    info.features.num_clusters = true;
                    info.values.num_clusters = value;
                }
                b if b == F::FEATURE_GPU_MULTICORE_SUPPORT => {
                    info.features.gpu_multicore_support = true
                }
                b if b == F::FEATURE_PBVNC_COREID_REG => info.features.pbvnc_coreid_reg = true,
                b if b == F::FEATURE_ROGUEXE => info.features.roguexe = true,
                b if b == F::FEATURE_TILE_REGION_PROTECTION => {
                    info.features.tile_region_protection = true
                }
                b if b == F::FEATURE_SLC_SIZE_CONFIGURABLE => {
                    info.features.slc_size_configurable = true
                }
                b if b == F::FEATURE_PHYS_BUS_WIDTH => {
                    info.features.phys_bus_width = true;
                    info.values.phys_bus_width = value;
                }
                b if b == F::FEATURE_LAYOUT_MARS => {
                    info.features.layout_mars = true;
                    info.values.layout_mars = value;
                }
                b if b == F::FEATURE_PBE2_IN_XE => info.features.pbe2_in_xe = true,
                b if b == F::FEATURE_XE_TPU2 => info.features.xe_tpu2 = true,
                b if b == F::FEATURE_SYS_BUS_SECURE_RESET => {
                    info.features.sys_bus_secure_reset = true
                }
                b if b == F::FEATURE_XT_TOP_INFRASTRUCTURE => {
                    info.features.xt_top_infrastructure = true
                }
                b if b == F::FEATURE_SLC_CACHE_LINE_SIZE_BITS => {
                    info.features.slc_cache_line_size_bits = true;
                    info.values.slc_cache_line_size_bits = value;
                }
                b if b == F::FEATURE_SLC_SIZE_IN_KILOBYTES => {
                    info.features.slc_size_in_kilobytes = true;
                    info.values.slc_size_in_kilobytes = value;
                }
                b if b == F::FEATURE_COMMON_STORE_SIZE_IN_DWORDS => {
                    info.values.common_store_size_in_dwords = value
                }
                b if b == F::FEATURE_ISP_MAX_TILES_IN_FLIGHT => {
                    info.values.isp_max_tiles_in_flight = value
                }
                b if b == F::FEATURE_MAX_PARTITIONS => info.values.max_partitions = value,
                b if b == F::FEATURE_TILE_SIZE_X => info.values.tile_size_x = value,
                b if b == F::FEATURE_TILE_SIZE_Y => info.values.tile_size_y = value,
                b if b == F::FEATURE_USC_MIN_OUTPUT_REGISTERS_PER_PIX => {
                    info.values.usc_min_output_registers_per_pix = value
                }

                // Features that determine command-stream layout.
                b if b == F::FEATURE_CLUSTER_GROUPING => info.features.cluster_grouping = true,
                b if b == F::FEATURE_COMPUTE_MORTON_CAPABLE => {
                    info.features.compute_morton_capable = true
                }
                b if b == F::FEATURE_FB_CDC_V4 => info.features.fb_cdc_v4 = true,
                b if b == F::FEATURE_FBCDC_ALGORITHM => {
                    info.features.fbcdc_algorithm = true;
                    info.values.fbcdc_algorithm = value;
                }
                b if b == F::FEATURE_ISP_ZLS_D24_S8_PACKING_OGL_MODE => {
                    info.features.isp_zls_d24_s8_packing_ogl_mode = true
                }
                b if b == F::FEATURE_S7_TOP_INFRASTRUCTURE => {
                    info.features.s7_top_infrastructure = true
                }
                b if b == F::FEATURE_TESSELLATION => info.features.tessellation = true,
                b if b == F::FEATURE_TPU_DM_GLOBAL_REGISTERS => {
                    info.features.tpu_dm_global_registers = true
                }
                b if b == F::FEATURE_VDM_DRAWINDIRECT => info.features.vdm_drawindirect = true,
                b if b == F::FEATURE_VDM_OBJECT_LEVEL_LLS => {
                    info.features.vdm_object_level_lls = true
                }
                b if b == F::FEATURE_ZLS_SUBTILE => info.features.zls_subtile = true,
                b if b == F::FEATURE_XE_MEMORY_HIERARCHY => {
                    info.features.xe_memory_hierarchy = true
                }
                b if b == F::FEATURE_CDM_CONTROL_STREAM_FORMAT => {
                    info.values.cdm_control_stream_format = value
                }
                b if b == F::FEATURE_NUM_RASTER_PIPES => {
                    info.features.num_raster_pipes = true;
                    info.values.num_raster_pipes = value;
                }
                b if b == F::FEATURE_NUM_ISP_IPP_PIPES => {
                    info.features.num_isp_ipp_pipes = true;
                    info.values.num_isp_ipp_pipes = value;
                }
                b if b == F::FEATURE_XPU_MAX_SLAVES => {
                    info.features.xpu_max_slaves = true;
                    info.values.xpu_max_slaves = value;
                }

                // Parameters used by HWRT datasets.
                b if b == F::FEATURE_ISP_SAMPLES_PER_PIXEL => {
                    info.features.isp_samples_per_pixel = true;
                    info.values.isp_samples_per_pixel = value;
                }
                b if b == F::FEATURE_SIMPLE_PARAMETER_FORMAT_VERSION => {
                    info.features.simple_parameter_format_version = true;
                    info.values.simple_parameter_format_version = value;
                }

                other => unknown_features.push(other),
            }
        });

        if let Some(err) = param_error {
            return Err(err);
        }

        collect_bits(quirks, |bit| {
            use dev_info_bits::quirks as Q;
            match bit {
                b if b == Q::BRN_44079 => info.quirks.brn44079 = true,
                b if b == Q::BRN_47217 => info.quirks.brn47217 = true,
                b if b == Q::BRN_48492 => info.quirks.brn48492 = true,
                b if b == Q::BRN_48545 => info.quirks.brn48545 = true,
                b if b == Q::BRN_49927 => info.quirks.brn49927 = true,
                b if b == Q::BRN_50767 => info.quirks.brn50767 = true,
                b if b == Q::BRN_51764 => info.quirks.brn51764 = true,
                b if b == Q::BRN_62269 => info.quirks.brn62269 = true,
                b if b == Q::BRN_63142 => info.quirks.brn63142 = true,
                b if b == Q::BRN_63553 => info.quirks.brn63553 = true,
                b if b == Q::BRN_66011 => info.quirks.brn66011 = true,
                b if b == Q::BRN_71242 => info.quirks.brn71242 = true,
                other => unknown_quirks.push(other),
            }
        });

        collect_bits(enhancements, |bit| {
            use dev_info_bits::enhancements as E;
            match bit {
                b if b == E::ERN_35421 => info.enhancements.ern35421 = true,
                b if b == E::ERN_38020 => info.enhancements.ern38020 = true,
                b if b == E::ERN_38748 => info.enhancements.ern38748 = true,
                b if b == E::ERN_42064 => info.enhancements.ern42064 = true,
                b if b == E::ERN_42290 => info.enhancements.ern42290 = true,
                b if b == E::ERN_42606 => info.enhancements.ern42606 = true,
                b if b == E::ERN_47025 => info.enhancements.ern47025 = true,
                b if b == E::ERN_57596 => info.enhancements.ern57596 = true,
                other => unknown_enhancements.push(other),
            }
        });

        Ok(BitmaskDecode {
            info,
            unknown_features,
            unknown_quirks,
            unknown_enhancements,
        })
    }

    /// Parse a raw device-info block (header + BRN + ERN + feature masks).
    pub fn from_device_info_block(block: &[u8]) -> Result<BitmaskDecode, DeviceInfoError> {
        let header = DeviceInfoBlockHeader::from_bytes(block)?;
        let mut off = DeviceInfoBlockHeader::LEN;
        let quirks = take_u64_slice(block, &mut off, header.brn_mask_size as usize)?;
        let enhancements = take_u64_slice(block, &mut off, header.ern_mask_size as usize)?;
        let features = take_u64_slice(block, &mut off, header.feature_mask_size as usize)?;
        let params = take_u64_slice(block, &mut off, header.feature_param_size as usize)?;
        Self::from_bitmask(&features, &params, &quirks, &enhancements)
    }
}

fn take_u64_slice(
    block: &[u8],
    off: &mut usize,
    words: usize,
) -> Result<Vec<u64>, DeviceInfoError> {
    let bytes = words
        .checked_mul(8)
        .ok_or(DeviceInfoError::TruncatedMasks)?;
    let end = off
        .checked_add(bytes)
        .ok_or(DeviceInfoError::TruncatedMasks)?;
    if end > block.len() {
        return Err(DeviceInfoError::TruncatedMasks);
    }
    let mut out = Vec::with_capacity(words);
    for chunk in block[*off..end].as_chunks::<8>().0 {
        out.push(u64::from_le_bytes(*chunk));
    }
    *off = end;
    Ok(out)
}

fn collect_bits(words: &[u64], mut on_bit: impl FnMut(u32)) {
    for (word_idx, word) in words.iter().enumerate() {
        let mut w = *word;
        let base = (word_idx as u32).saturating_mul(64);
        let mut bit = 0u32;
        while w != 0 {
            if (w & 1) != 0 {
                on_bit(base + bit);
            }
            w >>= 1;
            bit += 1;
        }
    }
}

/// Synthetic device-info fixtures.
///
/// Builds a deterministic device-info block independently of the licensed
/// firmware installed by the initramfs. The structure follows `UPSTREAM`;
/// synthetic feature values model BXS-4-64 (36.53.104.796), so this fixture
/// validates parsing and driver paths without claiming the K1's full feature set.
#[cfg(any(test, ktest))]
pub(crate) mod fixture {
    use alloc::vec::Vec;

    use super::{DeviceInfoBlockHeader, dev_info_bits};

    /// Physical bus width the fixture reports, in bits.
    pub(crate) const PHYS_BUS_WIDTH: u64 = 40;
    /// Common Store size the fixture reports, in dwords.
    pub(crate) const COMMON_STORE_SIZE_IN_DWORDS: u64 = 1344 * 4 * 4;
    /// Maximum ISP tiles in flight the fixture reports.
    pub(crate) const ISP_MAX_TILES_IN_FLIGHT: u64 = 6;
    /// Maximum USC partitions the fixture reports.
    pub(crate) const MAX_PARTITIONS: u64 = 16;
    /// Cluster count the fixture reports.
    pub(crate) const NUM_CLUSTERS: u64 = 1;
    /// Tile dimensions the fixture reports, in pixels.
    pub(crate) const TILE_SIZE: u64 = 16;
    /// Minimum USC output registers per pixel the fixture reports.
    pub(crate) const USC_MIN_OUTPUT_REGISTERS_PER_PIX: u64 = 2;
    /// SLC cache line width the fixture reports, in bits.
    ///
    /// Below 1,024, which is the threshold at which `rogue_slc_init()` bypasses
    /// the burst combiner — the branch worth exercising on a small part.
    pub(crate) const SLC_CACHE_LINE_SIZE_BITS: u64 = 512;
    /// SLC size the fixture reports, in kilobytes.
    pub(crate) const SLC_SIZE_IN_KILOBYTES: u64 = 128;

    fn set(words: &mut [u64], bit: u32) {
        words[(bit / 64) as usize] |= 1u64 << (bit % 64);
    }

    fn zeroed(max_bit: u32) -> Vec<u64> {
        alloc::vec![0u64; max_bit.div_ceil(64) as usize]
    }

    /// The feature, parameter, quirk and enhancement masks, in that order.
    ///
    /// The parameters are ordered by feature bit index, which is the only order
    /// the firmware defines and the reason [`super::VALUED_FEATURES`] has to be
    /// complete.
    pub(crate) fn bxe_masks() -> (Vec<u64>, Vec<u64>, Vec<u64>, Vec<u64>) {
        use dev_info_bits::{enhancements as E, features as F, quirks as Q};

        let mut features = zeroed(F::MAX);
        for bit in [
            F::FEATURE_COMMON_STORE_SIZE_IN_DWORDS,
            F::FEATURE_COMPUTE,
            F::FEATURE_COMPUTE_OVERLAP,
            F::FEATURE_ISP_MAX_TILES_IN_FLIGHT,
            F::FEATURE_MAX_PARTITIONS,
            F::FEATURE_MIPS,
            F::FEATURE_NUM_CLUSTERS,
            F::FEATURE_PBE2_IN_XE,
            F::FEATURE_PBVNC_COREID_REG,
            F::FEATURE_PHYS_BUS_WIDTH,
            F::FEATURE_ROGUEXE,
            F::FEATURE_SLC_CACHE_LINE_SIZE_BITS,
            F::FEATURE_SLC_SIZE_IN_KILOBYTES,
            F::FEATURE_SYS_BUS_SECURE_RESET,
            F::FEATURE_TILE_SIZE_X,
            F::FEATURE_TILE_SIZE_Y,
            F::FEATURE_USC_MIN_OUTPUT_REGISTERS_PER_PIX,
        ] {
            set(&mut features, bit);
        }

        // One parameter per *present* valued feature, in ascending bit order —
        // the only order the firmware defines. Adding a valued feature above
        // means inserting its parameter at the matching position here.
        let params = alloc::vec![
            COMMON_STORE_SIZE_IN_DWORDS,
            ISP_MAX_TILES_IN_FLIGHT,
            MAX_PARTITIONS,
            NUM_CLUSTERS,
            PHYS_BUS_WIDTH,
            SLC_CACHE_LINE_SIZE_BITS,
            SLC_SIZE_IN_KILOBYTES,
            TILE_SIZE,
            TILE_SIZE,
            USC_MIN_OUTPUT_REGISTERS_PER_PIX,
        ];

        let mut quirks = zeroed(Q::MAX);
        set(&mut quirks, Q::BRN_44079);

        let mut enhancements = zeroed(E::MAX);
        set(&mut enhancements, E::ERN_35421);
        set(&mut enhancements, E::ERN_38748);

        (features, params, quirks, enhancements)
    }

    /// Serialises masks into the on-image device-info block layout.
    pub(crate) fn block(
        features: &[u64],
        params: &[u64],
        quirks: &[u64],
        enhancements: &[u64],
    ) -> Vec<u8> {
        // Counts are `u64`, matching upstream's header. Writing them narrower here
        // would keep the round-trip green while making the real firmware
        // unparseable, which is exactly how the width error went unnoticed.
        let mut out = Vec::with_capacity(DeviceInfoBlockHeader::LEN);
        out.extend_from_slice(&(quirks.len() as u64).to_le_bytes());
        out.extend_from_slice(&(enhancements.len() as u64).to_le_bytes());
        out.extend_from_slice(&(features.len() as u64).to_le_bytes());
        out.extend_from_slice(&(params.len() as u64).to_le_bytes());
        for word in quirks
            .iter()
            .chain(enhancements.iter())
            .chain(features.iter())
            .chain(params.iter())
        {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out
    }
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::{fixture::*, *};

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn from_bitmask_happy_path() {
        let (f, p, q, e) = bxe_masks();
        let d = DeviceInfo::from_bitmask(&f, &p, &q, &e).expect("well-formed masks");
        assert!(d.info.features.compute);
        assert!(d.info.features.compute_overlap);
        assert!(d.info.features.mips);
        assert!(!d.info.features.meta);
        assert!(!d.info.features.riscv_fw_processor);
        assert!(d.info.features.num_clusters);
        assert!(d.info.features.pbvnc_coreid_reg);
        assert!(d.info.features.phys_bus_width);
        assert!(d.info.features.roguexe);
        assert!(d.info.features.pbe2_in_xe);
        assert!(d.info.features.sys_bus_secure_reset);
        assert!(d.info.features.slc_cache_line_size_bits);
        // Not an XT part and the firmware core is not in the MARS domain, so
        // `pvr_fw_stop()` takes the SIDEKICK/SLC idle path.
        assert!(!d.info.features.xt_top_infrastructure);
        assert!(!d.info.features.layout_mars);
        assert!(!d.info.features.xe_tpu2);
        assert!(d.info.quirks.brn44079);
        assert!(!d.info.quirks.brn63142);
        assert!(d.info.enhancements.ern35421);
        assert!(d.info.enhancements.ern38748);
        assert!(d.unknown_features.is_empty());
        assert!(d.unknown_quirks.is_empty());
        assert!(d.unknown_enhancements.is_empty());
    }

    /// Every valued feature must land in its own slot. A misclassified bit in
    /// [`VALUED_FEATURES`] shows up here as a plausible-looking wrong number,
    /// which is exactly the failure this test exists to catch.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn valued_features_land_in_the_right_slots() {
        let (f, p, q, e) = bxe_masks();
        let d = DeviceInfo::from_bitmask(&f, &p, &q, &e).expect("well-formed masks");
        assert_eq!(
            d.info.values.common_store_size_in_dwords,
            COMMON_STORE_SIZE_IN_DWORDS as u32
        );
        assert_eq!(
            d.info.values.isp_max_tiles_in_flight,
            ISP_MAX_TILES_IN_FLIGHT as u32
        );
        assert_eq!(d.info.values.max_partitions, MAX_PARTITIONS as u32);
        assert_eq!(d.info.values.num_clusters, NUM_CLUSTERS as u32);
        assert_eq!(d.info.values.phys_bus_width, PHYS_BUS_WIDTH as u32);
        assert_eq!(
            d.info.values.slc_cache_line_size_bits,
            SLC_CACHE_LINE_SIZE_BITS as u32
        );
        assert_eq!(
            d.info.values.slc_size_in_kilobytes,
            SLC_SIZE_IN_KILOBYTES as u32
        );
        assert_eq!(d.info.values.tile_size_x, TILE_SIZE as u32);
        assert_eq!(d.info.values.tile_size_y, TILE_SIZE as u32);
        assert_eq!(
            d.info.values.usc_min_output_registers_per_pix,
            USC_MIN_OUTPUT_REGISTERS_PER_PIX as u32
        );
    }

    /// The parameter list is indexed by *presence*, so dropping a valued feature
    /// must shift the parameters that follow it rather than leaving a hole.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn valued_features_are_consumed_in_bit_order() {
        use dev_info_bits::features as F;

        let (mut f, mut p, q, e) = bxe_masks();
        // Clear MAX_PARTITIONS (bit 24) and its parameter, the third one.
        f[(F::FEATURE_MAX_PARTITIONS / 64) as usize] &= !(1u64 << (F::FEATURE_MAX_PARTITIONS % 64));
        p.remove(2);

        let d = DeviceInfo::from_bitmask(&f, &p, &q, &e).expect("well-formed masks");
        assert_eq!(d.info.values.max_partitions, 0);
        assert_eq!(d.info.values.num_clusters, NUM_CLUSTERS as u32);
        assert_eq!(d.info.values.tile_size_x, TILE_SIZE as u32);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn from_bitmask_rejects_a_short_parameter_array() {
        let (f, mut p, q, e) = bxe_masks();
        p.pop();
        assert_eq!(
            DeviceInfo::from_bitmask(&f, &p, &q, &e).err(),
            Some(DeviceInfoError::MissingFeatureParam)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn from_bitmask_reports_unknown_bits() {
        use dev_info_bits::{enhancements as E, features as F, quirks as Q};

        // Bit 64 is past both tables, which is what makes it "unknown" rather
        // than merely unmapped.
        const _: () = assert!(Q::MAX < 64 && E::MAX < 64);

        let (mut f, p, mut q, mut e) = bxe_masks();
        // A feature bit this port has no field for (AXI_ACELITE = 0) and which
        // carries no parameter, so the parameter array stays valid.
        f[0] |= 1u64 << 0;
        // Every quirk and enhancement below MAX is mapped, so "unknown" here can
        // only mean a firmware newer than this driver: append a word and set a
        // bit past the end of the tables.
        q.push(1);
        e.push(1);

        let d = DeviceInfo::from_bitmask(&f, &p, &q, &e).expect("well-formed masks");
        assert!(d.info.features.compute);
        assert_eq!(d.unknown_features, alloc::vec![F::FEATURE_AXI_ACELITE]);
        assert_eq!(d.unknown_quirks, alloc::vec![64]);
        assert_eq!(d.unknown_enhancements, alloc::vec![64]);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn from_device_info_block_truncated() {
        assert_eq!(
            DeviceInfo::from_device_info_block(&[0u8; 16]).err(),
            Some(DeviceInfoError::TruncatedHeader)
        );
        // Header claims a huge quirk mask that does not fit.
        let mut buf = alloc::vec![0u8; DeviceInfoBlockHeader::LEN];
        buf[0..8].copy_from_slice(&100u64.to_le_bytes()); // brn_mask_size
        assert_eq!(
            DeviceInfo::from_device_info_block(&buf).err(),
            Some(DeviceInfoError::TruncatedMasks)
        );
    }

    /// Checks the header shape of `rogue_36.29.52.182_v1.fw`.
    ///
    /// These counts come from the image via `tools/fwinfo.py`, independently of the
    /// [`block`] fixture writer, so a shared writer/parser width error is detectable.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn the_real_firmware_header_is_four_u64s() {
        // brn 1 word, ern 1 word, features 2 words, parameters 29 words.
        let mut buf = alloc::vec![0u8; DeviceInfoBlockHeader::LEN];
        buf[0..8].copy_from_slice(&1u64.to_le_bytes());
        buf[8..16].copy_from_slice(&1u64.to_le_bytes());
        buf[16..24].copy_from_slice(&2u64.to_le_bytes());
        buf[24..32].copy_from_slice(&29u64.to_le_bytes());

        let header = DeviceInfoBlockHeader::from_bytes(&buf).expect("header parses");
        assert_eq!(header.brn_mask_size, 1);
        assert_eq!(header.ern_mask_size, 1);
        assert_eq!(header.feature_mask_size, 2);
        assert_eq!(header.feature_param_size, 29);

        // A 16-byte header is not merely wrong, it is short: the parser must say so
        // rather than reading counts out of the first mask word.
        assert_eq!(
            DeviceInfoBlockHeader::from_bytes(&buf[..16]).err(),
            Some(DeviceInfoError::TruncatedHeader)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn from_device_info_block_round_trip() {
        let (f, p, q, e) = bxe_masks();
        let d = DeviceInfo::from_device_info_block(&block(&f, &p, &q, &e)).expect("block");
        assert!(d.info.features.mips);
        assert!(d.info.quirks.brn44079);
        assert_eq!(d.info.values.num_clusters, NUM_CLUSTERS as u32);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn valued_features_is_sorted_and_in_range() {
        // Ordering is enforced at compile time by the `const` assertion next to
        // `VALUED_FEATURES`; what is left to check is that every entry names a bit
        // the generated table knows about.
        assert!(
            VALUED_FEATURES
                .iter()
                .all(|bit| *bit < dev_info_bits::features::MAX)
        );
    }
}
