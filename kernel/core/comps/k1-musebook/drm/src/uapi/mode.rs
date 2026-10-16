// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Mode-setting user-space structures.
//!
//! Based on Linux `include/uapi/drm/drm_mode.h`; links below identify the source.
//! Field names and order match Linux exactly; see `../../CONVENTIONS.md` for why
//! that is not negotiable.
//!
//! Several of these structures carry implicit padding in C. Those are marked
//! with `#[padding_struct]`, which inserts the padding explicitly so the type can
//! be treated as POD. Getting this wrong means leaking uninitialized kernel bytes
//! to user space, so the padding is called out per structure below.

/// Length of the mode name buffer, `DRM_DISPLAY_MODE_LEN`.
pub const DISPLAY_MODE_LEN: usize = 32;

/// Length of the property name buffer, `DRM_PROP_NAME_LEN`.
pub const PROP_NAME_LEN: usize = 32;

/// `struct drm_mode_modeinfo`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L226>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct ModeInfo {
    /// Pixel clock in kHz.
    pub clock: u32,
    /// Horizontal active pixels.
    pub hdisplay: u16,
    /// Horizontal sync start.
    pub hsync_start: u16,
    /// Horizontal sync end.
    pub hsync_end: u16,
    /// Horizontal total.
    pub htotal: u16,
    /// Horizontal skew.
    pub hskew: u16,
    /// Vertical active lines.
    pub vdisplay: u16,
    /// Vertical sync start.
    pub vsync_start: u16,
    /// Vertical sync end.
    pub vsync_end: u16,
    /// Vertical total.
    pub vtotal: u16,
    /// Vertical scan.
    pub vscan: u16,
    /// Approximate refresh rate in Hz.
    pub vrefresh: u32,
    /// `DRM_MODE_FLAG_*`.
    pub flags: u32,
    /// `DRM_MODE_TYPE_*`.
    pub type_: u32,
    /// NUL-terminated mode name.
    pub name: [u8; DISPLAY_MODE_LEN],
}

/// `struct drm_mode_card_res`, the argument of `MODE_GETRESOURCES`.
///
/// Follows the two-pass convention: with `count_*` zero the kernel only reports
/// the actual counts; otherwise it fills up to the supplied capacity and still
/// reports the actual total. See the crate ABI documentation.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L128>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct CardRes {
    /// User pointer to a `u32` array of framebuffer IDs.
    pub fb_id_ptr: u64,
    /// User pointer to a `u32` array of CRTC IDs.
    pub crtc_id_ptr: u64,
    /// User pointer to a `u32` array of connector IDs.
    pub connector_id_ptr: u64,
    /// User pointer to a `u32` array of encoder IDs.
    pub encoder_id_ptr: u64,
    /// Number of framebuffers.
    pub count_fbs: u32,
    /// Number of CRTCs.
    pub count_crtcs: u32,
    /// Number of connectors.
    pub count_connectors: u32,
    /// Number of encoders.
    pub count_encoders: u32,
    /// Minimum framebuffer width.
    pub min_width: u32,
    /// Maximum framebuffer width.
    pub max_width: u32,
    /// Minimum framebuffer height.
    pub min_height: u32,
    /// Maximum framebuffer height.
    pub max_height: u32,
}

/// `struct drm_mode_crtc`, shared by `MODE_GETCRTC` and `MODE_SETCRTC`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L145>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct Crtc {
    /// User pointer to a `u32` array of connector IDs to drive.
    pub set_connectors_ptr: u64,
    /// Number of entries in `set_connectors_ptr`.
    pub count_connectors: u32,
    /// Object ID of the CRTC.
    pub crtc_id: u32,
    /// Object ID of the framebuffer to scan out.
    pub fb_id: u32,
    /// Horizontal offset into the framebuffer.
    pub x: u32,
    /// Vertical offset into the framebuffer.
    pub y: u32,
    /// Size of the gamma ramp.
    pub gamma_size: u32,
    /// Non-zero if `mode` is meaningful.
    pub mode_valid: u32,
    /// The mode to program.
    pub mode: ModeInfo,
}

/// `struct drm_mode_set_plane`, the argument of `MODE_SETPLANE`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L188>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SetPlane {
    /// Object ID of the plane.
    pub plane_id: u32,
    /// Object ID of the CRTC.
    pub crtc_id: u32,
    /// Object ID of the framebuffer, which carries the pixel format.
    pub fb_id: u32,
    /// `DRM_MODE_PRESENT_*` flags.
    pub flags: u32,
    /// Destination X, signed so the plane may hang off screen.
    pub crtc_x: i32,
    /// Destination Y, signed so the plane may hang off screen.
    pub crtc_y: i32,
    /// Destination width.
    pub crtc_w: u32,
    /// Destination height.
    pub crtc_h: u32,
    /// Source X in 16.16 fixed point.
    pub src_x: u32,
    /// Source Y in 16.16 fixed point.
    pub src_y: u32,
    /// Source height in 16.16 fixed point.
    pub src_h: u32,
    /// Source width in 16.16 fixed point.
    pub src_w: u32,
}

/// `struct drm_mode_get_plane`, the argument of `MODE_GETPLANE`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L253>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct GetPlane {
    /// Object ID of the plane being queried. Set by the caller.
    pub plane_id: u32,
    /// Object ID of the plane's current CRTC.
    pub crtc_id: u32,
    /// Object ID of the plane's current framebuffer.
    pub fb_id: u32,
    /// Bitmask of compatible CRTCs, indexed by CRTC index.
    pub possible_crtcs: u32,
    /// Never used by Linux; reported as zero.
    pub gamma_size: u32,
    /// Number of entries in `format_type_ptr`.
    pub count_format_types: u32,
    /// User pointer to a `u32` array of supported FourCC formats.
    pub format_type_ptr: u64,
}

/// `struct drm_mode_get_plane_res`, the argument of `MODE_GETPLANERESOURCES`.
///
/// Carries four bytes of tail padding in C, made explicit here.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L296>.
#[repr(C)]
#[padding_struct]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct GetPlaneRes {
    /// User pointer to a `u32` array of plane IDs.
    pub plane_id_ptr: u64,
    /// Number of planes.
    pub count_planes: u32,
}

/// `struct drm_mode_get_encoder`, the argument of `MODE_GETENCODER`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L319>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct GetEncoder {
    /// Object ID of the encoder. Set by the caller.
    pub encoder_id: u32,
    /// `DRM_MODE_ENCODER_*`.
    pub encoder_type: u32,
    /// Object ID of the encoder's current CRTC.
    pub crtc_id: u32,
    /// Bitmask of compatible CRTCs.
    pub possible_crtcs: u32,
    /// Bitmask of encoders that may be cloned with this one.
    pub possible_clones: u32,
}

/// `struct drm_mode_get_connector`, the argument of `MODE_GETCONNECTOR`.
///
/// Also follows the two-pass convention. The trailing `pad` field is part of the
/// Linux definition, not something we added.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L397>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct GetConnector {
    /// User pointer to a `u32` array of encoder IDs.
    pub encoders_ptr: u64,
    /// User pointer to a [`ModeInfo`] array.
    pub modes_ptr: u64,
    /// User pointer to a `u32` array of property IDs.
    pub props_ptr: u64,
    /// User pointer to a `u64` array of property values.
    pub prop_values_ptr: u64,
    /// Number of modes.
    pub count_modes: u32,
    /// Number of properties.
    pub count_props: u32,
    /// Number of encoders.
    pub count_encoders: u32,
    /// Object ID of the current encoder.
    pub encoder_id: u32,
    /// Object ID of the connector.
    pub connector_id: u32,
    /// `DRM_MODE_CONNECTOR_*`.
    pub connector_type: u32,
    /// Per-type connector number; not an object ID and not a stable identifier.
    pub connector_type_id: u32,
    /// `enum drm_connector_status`.
    pub connection: u32,
    /// Physical width of the sink in millimetres.
    pub mm_width: u32,
    /// Physical height of the sink in millimetres.
    pub mm_height: u32,
    /// `enum subpixel_order`.
    pub subpixel: u32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_mode_get_property`, the argument of `MODE_GETPROPERTY`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L509>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct GetProperty {
    /// User pointer to a `u64` array of legal values.
    pub values_ptr: u64,
    /// User pointer to a `struct drm_mode_property_enum` array.
    pub enum_blob_ptr: u64,
    /// Object ID of the property. Set by the caller.
    pub prop_id: u32,
    /// `DRM_MODE_PROP_*`.
    pub flags: u32,
    /// Symbolic property name; this is what user space matches on.
    pub name: [u8; PROP_NAME_LEN],
    /// Number of entries in `values_ptr`.
    pub count_values: u32,
    /// Number of entries in `enum_blob_ptr`.
    pub count_enum_blobs: u32,
}

/// `struct drm_mode_property_enum`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L468>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct PropertyEnum {
    /// The enum value or bitmask bit position.
    pub value: u64,
    /// Symbolic name of this value.
    pub name: [u8; PROP_NAME_LEN],
}

/// `struct drm_mode_obj_get_properties`, the argument of `MODE_OBJ_GETPROPERTIES`.
///
/// Carries four bytes of tail padding in C, made explicit here.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L575>.
#[repr(C)]
#[padding_struct]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct ObjGetProperties {
    /// User pointer to a `u32` array of property IDs.
    pub props_ptr: u64,
    /// User pointer to a `u64` array of property values.
    pub prop_values_ptr: u64,
    /// Number of properties.
    pub count_props: u32,
    /// Object ID being queried.
    pub obj_id: u32,
    /// `DRM_MODE_OBJECT_*`.
    pub obj_type: u32,
}

/// `struct drm_mode_obj_set_property`, the argument of `MODE_OBJ_SETPROPERTY`.
///
/// Carries four bytes of tail padding in C, made explicit here.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L583>.
#[repr(C)]
#[padding_struct]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct ObjSetProperty {
    /// The new value.
    pub value: u64,
    /// Object ID of the property.
    pub prop_id: u32,
    /// Object ID being modified.
    pub obj_id: u32,
    /// `DRM_MODE_OBJECT_*`.
    pub obj_type: u32,
}

/// `struct drm_mode_get_blob`, the argument of `MODE_GETPROPBLOB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L563>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct GetBlob {
    /// Object ID of the blob.
    pub blob_id: u32,
    /// Capacity of `data` on input; blob length on output.
    pub length: u32,
    /// User pointer to the destination buffer.
    pub data: u64,
}

/// `struct drm_mode_create_blob`, the argument of `MODE_CREATEPROPBLOB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L1027>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct CreateBlob {
    /// User pointer to the data to copy in.
    pub data: u64,
    /// Length of the data.
    pub length: u32,
    /// Object ID of the new blob.
    pub blob_id: u32,
}

/// `struct drm_mode_destroy_blob`, the argument of `MODE_DESTROYPROPBLOB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L1038>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DestroyBlob {
    /// Object ID of the blob to destroy.
    pub blob_id: u32,
}

/// `struct drm_mode_fb_cmd`, the argument of the legacy `MODE_ADDFB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L596>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct FbCmd {
    /// Object ID of the framebuffer; an output.
    pub fb_id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Stride in bytes.
    pub pitch: u32,
    /// Bits per pixel.
    pub bpp: u32,
    /// Colour depth.
    pub depth: u32,
    /// GEM handle of the backing buffer.
    pub handle: u32,
}

/// `struct drm_mode_fb_cmd2`, the argument of `MODE_ADDFB2` and `MODE_GETFB2`.
///
/// In C, four bytes of padding sit between `offsets` and `modifier` so that the
/// latter is eight-byte aligned; `#[padding_struct]` reproduces that.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L644>.
#[repr(C)]
#[padding_struct]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct FbCmd2 {
    /// Object ID of the framebuffer.
    pub fb_id: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// FourCC format code from `drm_fourcc.h`.
    pub pixel_format: u32,
    /// `DRM_MODE_FB_INTERLACED` and `DRM_MODE_FB_MODIFIERS`.
    pub flags: u32,
    /// GEM handle per plane; zero for unused planes.
    pub handles: [u32; 4],
    /// Stride in bytes per plane.
    pub pitches: [u32; 4],
    /// Byte offset into the buffer per plane.
    pub offsets: [u32; 4],
    /// Format modifier per plane; ignored unless `DRM_MODE_FB_MODIFIERS` is set.
    pub modifier: [u64; 4],
}

/// `struct drm_mode_crtc_page_flip`, the argument of `MODE_PAGE_FLIP`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L916>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct CrtcPageFlip {
    /// Object ID of the CRTC.
    pub crtc_id: u32,
    /// Object ID of the framebuffer to flip to.
    pub fb_id: u32,
    /// `DRM_MODE_PAGE_FLIP_*`.
    pub flags: u32,
    /// Reserved; must be zero.
    pub reserved: u32,
    /// Opaque cookie echoed back in the completion event.
    pub user_data: u64,
}

/// `struct drm_mode_create_dumb`, the argument of `MODE_CREATE_DUMB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L943>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct CreateDumb {
    /// Height in pixels.
    pub height: u32,
    /// Width in pixels.
    pub width: u32,
    /// Bits per pixel.
    pub bpp: u32,
    /// Flags; must be zero.
    pub flags: u32,
    /// GEM handle of the new buffer; an output.
    pub handle: u32,
    /// Stride in bytes; an output.
    pub pitch: u32,
    /// Buffer size in bytes; an output.
    pub size: u64,
}

/// `struct drm_mode_map_dumb`, the argument of `MODE_MAP_DUMB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L953>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct MapDumb {
    /// GEM handle of the buffer to map.
    pub handle: u32,
    /// Padding; must be zero.
    pub pad: u32,
    /// Fake offset to pass to a subsequent `mmap` on the DRM node.
    ///
    /// Fixed-width for 32/64-bit compatibility. We assign our own offsets rather
    /// than reproducing Linux's `drm_vma_offset_manager` values, but guarantee
    /// that repeated calls for one handle return the same offset.
    pub offset: u64,
}

/// `struct drm_mode_destroy_dumb`, the argument of `MODE_DESTROY_DUMB`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L967>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DestroyDumb {
    /// GEM handle of the buffer to destroy.
    pub handle: u32,
}

/// `struct drm_mode_atomic`, the argument of `MODE_ATOMIC`.
///
/// The four parallel arrays are read together: for each of `count_objs` objects,
/// `count_props_ptr[i]` says how many `(prop, value)` pairs belong to it, taken
/// in order from `props_ptr` and `prop_values_ptr`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L1016>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct Atomic {
    /// `DRM_MODE_ATOMIC_*`.
    pub flags: u32,
    /// Number of objects being modified.
    pub count_objs: u32,
    /// User pointer to a `u32` array of object IDs.
    pub objs_ptr: u64,
    /// User pointer to a `u32` array of per-object property counts.
    pub count_props_ptr: u64,
    /// User pointer to a `u32` array of property IDs.
    pub props_ptr: u64,
    /// User pointer to a `u64` array of property values.
    pub prop_values_ptr: u64,
    /// Reserved; must be zero.
    pub reserved: u64,
    /// Opaque cookie echoed back in the completion event.
    pub user_data: u64,
}

/// `struct drm_format_modifier_blob`, the payload of the `IN_FORMATS` property.
///
/// The blob is a header followed by a `u32` format array and a
/// [`FormatModifier`] array, both at byte offsets the header gives. GBM reads it
/// to learn which modifiers a plane accepts for a given format; a plane that
/// advertises modifiers without this blob makes Mesa fall back to linear.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L873>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct FormatModifierBlob {
    /// Blob layout version; `FORMAT_BLOB_CURRENT`, i.e. 1.
    pub version: u32,
    /// Reserved; must be zero.
    pub flags: u32,
    /// Number of entries in the format array.
    pub count_formats: u32,
    /// Byte offset of the format array from the start of the blob.
    pub formats_offset: u32,
    /// Number of entries in the modifier array.
    pub count_modifiers: u32,
    /// Byte offset of the modifier array from the start of the blob.
    pub modifiers_offset: u32,
}

/// The version [`FormatModifierBlob::version`] must carry, `FORMAT_BLOB_CURRENT`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L871>.
pub const FORMAT_BLOB_CURRENT: u32 = 1;

/// `struct drm_format_modifier`: one modifier plus the formats it applies to.
///
/// `formats` is a bitmask over the blob's format array, biased by `offset`, so a
/// blob with more than 64 formats needs several entries per modifier.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L845>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct FormatModifier {
    /// Bitmask of formats, bit `i` meaning format `offset + i`.
    pub formats: u64,
    /// Index of the format the least significant bit of `formats` refers to.
    pub offset: u32,
    /// Padding; must be zero.
    pub pad: u32,
    /// The modifier itself.
    pub modifier: u64,
}

/// `DRM_MODE_OBJECT_*` type codes.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L461>.
pub mod object_type {
    /// A CRTC.
    pub const CRTC: u32 = 0xcccccccc;
    /// A connector.
    pub const CONNECTOR: u32 = 0xc0c0c0c0;
    /// An encoder.
    pub const ENCODER: u32 = 0xe0e0e0e0;
    /// A mode.
    pub const MODE: u32 = 0xdededede;
    /// A property.
    pub const PROPERTY: u32 = 0xb0b0b0b0;
    /// A framebuffer.
    pub const FB: u32 = 0xfbfbfbfb;
    /// A property blob.
    pub const BLOB: u32 = 0xbbbbbbbb;
    /// A plane.
    pub const PLANE: u32 = 0xeeeeeeee;
    /// Any object type; used only as a wildcard in lookups.
    pub const ANY: u32 = 0;
}

/// `DRM_MODE_ATOMIC_*` flags.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L1000>.
pub mod atomic_flags {
    /// Do not block waiting for the commit to land.
    pub const NONBLOCK: u32 = 0x0200;
    /// Allow the commit to change the active mode.
    pub const ALLOW_MODESET: u32 = 0x0400;
    /// Validate only; do not apply.
    pub const TEST_ONLY: u32 = 0x0100;
    /// Request a completion event.
    pub const PAGE_FLIP_EVENT: u32 = 0x01;
    /// Request an asynchronous (tear-capable) flip.
    pub const PAGE_FLIP_ASYNC: u32 = 0x02;

    /// Every flag `MODE_ATOMIC` accepts, i.e. `DRM_MODE_ATOMIC_FLAGS`.
    ///
    /// A flag outside this set is `EINVAL`: user space uses the rejection to
    /// discover which extensions a kernel has.
    pub const ALL: u32 = PAGE_FLIP_EVENT | PAGE_FLIP_ASYNC | TEST_ONLY | NONBLOCK | ALLOW_MODESET;
}

/// `DRM_MODE_PAGE_FLIP_*` flags, the `flags` field of [`CrtcPageFlip`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L903>.
pub mod page_flip_flags {
    /// Queue a completion event, readable from the DRM node.
    pub const EVENT: u32 = 0x01;
    /// Flip as soon as possible rather than at the next vblank, tearing if need
    /// be. Refused here; `DRM_CAP_ASYNC_PAGE_FLIP` reports 0.
    pub const ASYNC: u32 = 0x02;
    /// `target_vblank` is absolute.
    pub const TARGET_ABSOLUTE: u32 = 0x4;
    /// `target_vblank` is relative to the current sequence.
    pub const TARGET_RELATIVE: u32 = 0x8;

    /// Every flag `PAGE_FLIP` accepts, i.e. `DRM_MODE_PAGE_FLIP_FLAGS`.
    pub const ALL: u32 = EVENT | ASYNC | TARGET_ABSOLUTE | TARGET_RELATIVE;
}

/// `DRM_MODE_FB_*` flags, the `flags` field of [`FbCmd2`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L629>.
pub mod fb_flags {
    /// The framebuffer holds interlaced field data.
    pub const INTERLACED: u32 = 1 << 0;
    /// The `modifier` array is meaningful.
    pub const MODIFIERS: u32 = 1 << 1;

    /// Every flag `ADDFB2` accepts.
    pub const ALL: u32 = INTERLACED | MODIFIERS;
}

/// `DRM_MODE_PRESENT_*` flags, the `flags` field of [`SetPlane`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L181>.
pub mod present_flags {
    /// Present the top field of an interlaced framebuffer.
    pub const TOP_FIELD: u32 = 1 << 0;
    /// Present the bottom field of an interlaced framebuffer.
    pub const BOTTOM_FIELD: u32 = 1 << 1;

    /// Every flag `SETPLANE` accepts.
    pub const ALL: u32 = TOP_FIELD | BOTTOM_FIELD;
}

/// `DRM_MODE_PROP_*` flags, the `flags` field of [`GetProperty`].
///
/// The type of a property is encoded in two places for historical reasons: the
/// three legacy bits below 6, and the `EXTENDED_TYPE` field above them. User
/// space checks both, so both must be set consistently.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L474>.
pub mod prop_flags {
    /// Legacy and unused; Linux never sets it.
    pub const PENDING: u32 = 1 << 0;
    /// The property carries a numeric range; `values` holds `[min, max]`.
    pub const RANGE: u32 = 1 << 1;
    /// The property cannot be written.
    pub const IMMUTABLE: u32 = 1 << 2;
    /// The property carries one of a set of named values.
    pub const ENUM: u32 = 1 << 3;
    /// The property carries a blob object ID.
    pub const BLOB: u32 = 1 << 4;
    /// The property carries a set of named bits.
    pub const BITMASK: u32 = 1 << 5;

    /// The mask covering the extended type field.
    pub const EXTENDED_TYPE: u32 = 0x0000ffc0;
    /// The property carries an object ID; `values[0]` is the object type.
    pub const OBJECT: u32 = 1 << 6;
    /// The property carries a signed range; `values` holds `[min, max]` as `i64`.
    pub const SIGNED_RANGE: u32 = 2 << 6;
    /// The property is part of the atomic interface and is hidden from clients
    /// that did not set `DRM_CLIENT_CAP_ATOMIC`.
    pub const ATOMIC: u32 = 0x80000000;
}

/// `DRM_MODE_CONNECTOR_*` types, the `connector_type` field of [`GetConnector`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L370>.
pub mod connector_type {
    /// `DRM_MODE_CONNECTOR_Unknown`.
    pub const UNKNOWN: u32 = 0;
    /// `DRM_MODE_CONNECTOR_VGA`.
    pub const VGA: u32 = 1;
    /// `DRM_MODE_CONNECTOR_DisplayPort`.
    pub const DISPLAY_PORT: u32 = 10;
    /// `DRM_MODE_CONNECTOR_HDMIA`.
    pub const HDMI_A: u32 = 11;
    /// `DRM_MODE_CONNECTOR_eDP`, which is what the K1's internal panel is.
    pub const EDP: u32 = 14;
    /// `DRM_MODE_CONNECTOR_VIRTUAL`, used by the software driver.
    pub const VIRTUAL: u32 = 15;
    /// `DRM_MODE_CONNECTOR_DSI`.
    pub const DSI: u32 = 16;
}

/// `DRM_MODE_ENCODER_*` types, the `encoder_type` field of [`GetEncoder`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L305>.
pub mod encoder_type {
    /// `DRM_MODE_ENCODER_NONE`.
    pub const NONE: u32 = 0;
    /// `DRM_MODE_ENCODER_TMDS`.
    pub const TMDS: u32 = 2;
    /// `DRM_MODE_ENCODER_VIRTUAL`.
    pub const VIRTUAL: u32 = 5;
    /// `DRM_MODE_ENCODER_DSI`, the K1's DPU-to-panel path.
    pub const DSI: u32 = 6;
}

/// `enum subpixel_order`, the `subpixel` field of [`GetConnector`].
///
/// Note the values start at 1: a zero would be read as an out-of-range value
/// rather than as "unknown".
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L107>.
pub mod subpixel {
    /// `DRM_MODE_SUBPIXEL_UNKNOWN`.
    pub const UNKNOWN: u32 = 1;
    /// `DRM_MODE_SUBPIXEL_HORIZONTAL_RGB`.
    pub const HORIZONTAL_RGB: u32 = 2;
    /// `DRM_MODE_SUBPIXEL_NONE`.
    pub const NONE: u32 = 6;
}

/// `DRM_MODE_TYPE_*` flags, the `type_` field of [`ModeInfo`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L46>.
pub mod mode_type {
    /// The mode the sink itself asks for. User space picks it by default, so a
    /// fixed panel must mark its native mode with this.
    pub const PREFERRED: u32 = 1 << 3;
    /// The mode came from the driver rather than from a user-supplied timing.
    pub const DRIVER: u32 = 1 << 6;

    /// Mode-type bits this port advertises (a subset of Linux `DRM_MODE_TYPE_ALL`).
    pub const ALL: u32 = PREFERRED | DRIVER;
}

/// `DRM_MODE_FLAG_*` flags, the `flags` field of [`ModeInfo`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L57>.
pub mod mode_flag {
    /// Positive horizontal sync polarity.
    pub const PHSYNC: u32 = 1 << 0;
    /// Negative horizontal sync polarity.
    pub const NHSYNC: u32 = 1 << 1;
    /// Positive vertical sync polarity.
    pub const PVSYNC: u32 = 1 << 2;
    /// Negative vertical sync polarity.
    pub const NVSYNC: u32 = 1 << 3;
    /// Interlaced scan.
    pub const INTERLACE: u32 = 1 << 4;
    /// Double scan.
    pub const DBLSCAN: u32 = 1 << 5;
    /// Composite sync.
    pub const CSYNC: u32 = 1 << 6;
    /// Positive composite sync polarity.
    pub const PCSYNC: u32 = 1 << 7;
    /// Negative composite sync polarity.
    pub const NCSYNC: u32 = 1 << 8;
    /// Horizontal sync serration.
    pub const HSKEW: u32 = 1 << 9;
    /// Broadcast video.
    pub const BCAST: u32 = 1 << 10;
    /// Pixel multiplexing.
    pub const PIXMUX: u32 = 1 << 11;
    /// Double clock.
    pub const DBLCLK: u32 = 1 << 12;
    /// Clock divided by two.
    pub const CLKDIV2: u32 = 1 << 13;
    /// The mask covering the stereo 3D layout field.
    pub const STEREO_3D_MASK: u32 = 0x1f << 14;
    /// The mask covering the picture aspect ratio field.
    pub const PICTURE_ASPECT_MASK: u32 = 0xf << 19;

    /// Every flag a mode may carry.
    pub const ALL: u32 = PHSYNC
        | NHSYNC
        | PVSYNC
        | NVSYNC
        | INTERLACE
        | DBLSCAN
        | CSYNC
        | PCSYNC
        | NCSYNC
        | HSKEW
        | BCAST
        | PIXMUX
        | DBLCLK
        | CLKDIV2
        | STEREO_3D_MASK
        | PICTURE_ASPECT_MASK;
}

/// `DRM_MODE_DPMS_*` values of the connector `DPMS` property.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L96>.
pub mod dpms {
    /// The output is on.
    pub const ON: u64 = 0;
    /// The output is in standby.
    pub const STANDBY: u64 = 1;
    /// The output is suspended.
    pub const SUSPEND: u64 = 2;
    /// The output is off.
    pub const OFF: u64 = 3;
}
