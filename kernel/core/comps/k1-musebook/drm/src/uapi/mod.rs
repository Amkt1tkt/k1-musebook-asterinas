// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The DRM user-space ABI: ioctl numbers and argument structures.
//!
//! Layouts follow Linux's DRM UAPI. The original port used the Linux 6.6
//! BSP headers; fields subsequently added by the shipped userspace headers
//! are identified below. References name upstream headers, not a required
//! local checkout layout.
//!
//! Rules for this module, from `../../CONVENTIONS.md`:
//!
//! - Field names match Linux, with a trailing `_` only where a Rust keyword
//!   forces it. Do not rename or reorder fields for readability; these are
//!   memory layouts shared with prebuilt binaries.
//! - Every item carries a link to its Linux definition.
//! - Structures are `#[repr(C)]` and `Pod`.

pub mod cap;
pub mod fourcc;
pub mod ioctl_nr;
pub mod mode;
pub mod sync;

/// `struct drm_version`.
///
/// The three `int` fields are followed by four bytes of padding in C, so that
/// `name_len` (a `__kernel_size_t`) lands on an eight-byte boundary.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L118>.
#[repr(C)]
#[padding_struct]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmVersion {
    /// Driver major version.
    pub version_major: i32,
    /// Driver minor version.
    pub version_minor: i32,
    /// Driver patch level.
    pub version_patchlevel: i32,
    /// Capacity of the `name` buffer on input; bytes written on output.
    pub name_len: usize,
    /// User pointer to the driver name buffer.
    pub name: u64,
    /// Capacity of the `date` buffer on input; bytes written on output.
    pub date_len: usize,
    /// User pointer to the driver date buffer.
    pub date: u64,
    /// Capacity of the `desc` buffer on input; bytes written on output.
    pub desc_len: usize,
    /// User pointer to the driver description buffer.
    pub desc: u64,
}

/// `struct drm_unique`, used by both `GET_UNIQUE` and the legacy `SET_UNIQUE`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L138>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmUnique {
    /// Capacity of the `unique` buffer on input; bytes written on output.
    pub unique_len: usize,
    /// User pointer to the bus-ID buffer.
    pub unique: u64,
}

/// `struct drm_set_version`.
///
/// All four fields are signed, and `-1` means "do not change" for the two
/// request halves. Every call is answered with the versions actually in force,
/// which is what lets a client negotiate rather than guess.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1030>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmSetVersion {
    /// Requested DRM interface major, or `-1` to leave it alone.
    pub drm_di_major: i32,
    /// Requested DRM interface minor, or `-1`.
    pub drm_di_minor: i32,
    /// Requested driver-specific major, or `-1`.
    pub drm_dd_major: i32,
    /// Requested driver-specific minor, or `-1`.
    pub drm_dd_minor: i32,
}

/// `struct drm_get_cap`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L666>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmGetCap {
    /// One of the `DRM_CAP_*` values in [`cap`].
    pub capability: u64,
    /// The driver's answer.
    pub value: u64,
}

/// `struct drm_set_client_cap`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L750>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmSetClientCap {
    /// One of the `DRM_CLIENT_CAP_*` values in [`cap`].
    pub capability: u64,
    /// Non-zero to enable.
    pub value: u64,
}

/// `struct drm_auth`, the argument of `GET_MAGIC` and `AUTH_MAGIC`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L182>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmAuth {
    /// The authentication token.
    pub magic: u32,
}

/// `struct drm_gem_close`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L780>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmGemClose {
    /// The handle to release.
    pub handle: u32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_gem_flink`, which publishes a handle under a global name.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L789>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmGemFlink {
    /// The handle to publish.
    pub handle: u32,
    /// The resulting global name.
    pub name: u32,
}

/// `struct drm_gem_open`, which resolves a global name back to a handle.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L801>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmGemOpen {
    /// The global name to look up.
    pub name: u32,
    /// The resulting handle.
    pub handle: u32,
    /// The object's size in bytes.
    pub size: u64,
}

/// `struct drm_event`, the header every event read from a DRM node starts with.
///
/// `length` covers the header itself, which is how a reader skips an event type
/// it does not know.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1058>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmEventHeader {
    /// One of the [`event_type`] values.
    pub type_: u32,
    /// Total length of this event in bytes, header included.
    pub length: u32,
}

/// `struct drm_event_vblank`, used by both vblank and page-flip completions.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1063>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmEventVblank {
    /// The common header.
    pub base: DrmEventHeader,
    /// The cookie user space supplied with the request.
    pub user_data: u64,
    /// Timestamp seconds.
    pub tv_sec: u32,
    /// Timestamp microseconds.
    pub tv_usec: u32,
    /// Vblank sequence number.
    pub sequence: u32,
    /// Object ID of the CRTC the event belongs to.
    ///
    /// Zero on kernels older than the `DRM_CAP_CRTC_IN_VBLANK_EVENT` capability;
    /// we always fill it in, and report that capability as 1.
    pub crtc_id: u32,
}

/// `DRM_EVENT_*` codes, the `type` field of [`DrmEventHeader`].
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1053>.
pub mod event_type {
    /// `DRM_EVENT_VBLANK`: a vblank the client asked to be told about.
    pub const VBLANK: u32 = 0x01;
    /// `DRM_EVENT_FLIP_COMPLETE`: the framebuffer of a page flip is now live.
    pub const FLIP_COMPLETE: u32 = 0x02;
    /// `DRM_EVENT_CRTC_SEQUENCE`: the `CRTC_QUEUE_SEQUENCE` reply, which we do
    /// not implement.
    pub const CRTC_SEQUENCE: u32 = 0x03;
}

/// `struct drm_prime_handle`, shared by both PRIME directions.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1027>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct DrmPrimeHandle {
    /// The GEM handle; an output for `FD_TO_HANDLE`, an input otherwise.
    pub handle: u32,
    /// `O_*` flags applied to the exported file descriptor.
    pub flags: u32,
    /// The dma-buf file descriptor.
    pub fd: i32,
}

/// Every argument structure's size, asserted at compile time.
///
/// Layouts must match the headers used by the shipped userspace, including
/// post-6.6 fields such as the sync-object deadline hint. The dispatcher uses
/// the command number and handles argument copying separately, so the encoded
/// ioctl size alone is not the current dispatch key.
///
/// These independent expected sizes catch accidental changes in Rust layout;
/// they do not replace comparison with userspace headers when updating Mesa
/// or libdrm. Mode structures are declared in Linux `drm_mode.h`, sync and
/// core structures in `drm.h` (Mesa also carries these under `include/drm-uapi`).
mod sizes {
    use super::{mode::*, sync::*, *};

    macro_rules! assert_size {
        ($($type:ty => $size:expr,)*) => {
            $(const _: () = assert!(core::mem::size_of::<$type>() == $size);)*
        };
    }

    assert_size! {
        DrmVersion => 64,
        DrmUnique => 16,
        DrmSetVersion => 16,
        DrmGetCap => 16,
        DrmSetClientCap => 16,
        DrmAuth => 4,
        DrmGemClose => 8,
        DrmGemFlink => 8,
        DrmGemOpen => 16,
        DrmEventHeader => 8,
        DrmEventVblank => 32,
        DrmPrimeHandle => 12,

        SyncobjCreate => 8,
        SyncobjDestroy => 8,
        SyncobjHandle => 16,
        SyncobjTransfer => 32,
        SyncobjWait => 40,
        SyncobjTimelineWait => 48,
        SyncobjArray => 16,
        SyncobjTimelineArray => 24,
        SyncobjEventfd => 24,

        ModeInfo => 68,
        CardRes => 64,
        Crtc => 104,
        SetPlane => 48,
        GetPlane => 32,
        GetPlaneRes => 16,
        GetEncoder => 20,
        GetConnector => 80,
        GetProperty => 64,
        PropertyEnum => 40,
        ObjGetProperties => 32,
        ObjSetProperty => 24,
        GetBlob => 16,
        CreateBlob => 16,
        DestroyBlob => 4,
        FbCmd => 28,
        FbCmd2 => 104,
        CrtcPageFlip => 24,
        CreateDumb => 32,
        MapDumb => 16,
        DestroyDumb => 4,
        Atomic => 56,
        FormatModifierBlob => 24,
        FormatModifier => 24,
    }
}
