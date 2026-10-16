// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Device and client capability identifiers.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L638>.

/// `DRM_CAP_*`, queried through `DRM_IOCTL_GET_CAP`.
pub mod device {
    /// Dumb buffers are supported.
    pub const DUMB_BUFFER: u64 = 0x1;
    /// `SETCRTC`/`PAGE_FLIP` accept a CRTC index in the high bits of the vblank
    /// sequence type.
    pub const VBLANK_HIGH_CRTC: u64 = 0x2;
    /// Preferred bit depth for dumb buffers.
    pub const DUMB_PREFERRED_DEPTH: u64 = 0x3;
    /// Non-zero if user space should render into a shadow buffer rather than
    /// directly into the dumb buffer.
    pub const DUMB_PREFER_SHADOW: u64 = 0x4;
    /// Bitmask of supported PRIME directions; see [`PRIME_IMPORT`] and
    /// [`PRIME_EXPORT`].
    ///
    /// [`PRIME_IMPORT`]: self::PRIME_IMPORT
    /// [`PRIME_EXPORT`]: self::PRIME_EXPORT
    pub const PRIME: u64 = 0x5;
    /// `DRM_PRIME_CAP_IMPORT`: `PRIME_FD_TO_HANDLE` works.
    pub const PRIME_IMPORT: u64 = 0x1;
    /// `DRM_PRIME_CAP_EXPORT`: `PRIME_HANDLE_TO_FD` works.
    pub const PRIME_EXPORT: u64 = 0x2;
    /// Vblank timestamps use a monotonic clock.
    pub const TIMESTAMP_MONOTONIC: u64 = 0x6;
    /// Asynchronous (tear-capable) page flips are supported.
    pub const ASYNC_PAGE_FLIP: u64 = 0x7;
    /// Preferred cursor width.
    pub const CURSOR_WIDTH: u64 = 0x8;
    /// Preferred cursor height.
    pub const CURSOR_HEIGHT: u64 = 0x9;
    /// `ADDFB2` accepts framebuffer format modifiers.
    ///
    /// GBM needs this; without it Mesa falls back to linear-only paths.
    pub const ADDFB2_MODIFIERS: u64 = 0x10;
    /// `PAGE_FLIP` accepts a target vblank sequence.
    pub const PAGE_FLIP_TARGET: u64 = 0x11;
    /// The vblank event reports whether the CRTC was inside vblank.
    pub const CRTC_IN_VBLANK_EVENT: u64 = 0x12;
    /// Sync objects are supported.
    pub const SYNCOBJ: u64 = 0x13;
    /// Timeline sync objects are supported.
    pub const SYNCOBJ_TIMELINE: u64 = 0x14;
    /// Asynchronous flips are supported through the atomic interface.
    ///
    /// Added after linux-6.6, so `GET_CAP` reports `EINVAL` for it. Listed here
    /// because a newer `libdrm` may ask.
    pub const ATOMIC_ASYNC_PAGE_FLIP: u64 = 0x15;
}

/// `DRM_CLIENT_CAP_*`, set through `DRM_IOCTL_SET_CLIENT_CAP`.
///
/// Refusing [`UNIVERSAL_PLANES`] or [`ATOMIC`] makes `cosmic-comp` give up
/// during backend initialization, so both must be accepted.
///
/// [`UNIVERSAL_PLANES`]: self::UNIVERSAL_PLANES
/// [`ATOMIC`]: self::ATOMIC
pub mod client {
    /// Expose stereo 3D modes.
    pub const STEREO_3D: u64 = 1;
    /// Expose all planes, not just overlays.
    pub const UNIVERSAL_PLANES: u64 = 2;
    /// Expose atomic properties and accept `MODE_ATOMIC`.
    pub const ATOMIC: u64 = 3;
    /// Expose the picture aspect-ratio mode flags.
    pub const ASPECT_RATIO: u64 = 4;
    /// Expose writeback connectors.
    pub const WRITEBACK_CONNECTORS: u64 = 5;
    /// Expose the cursor plane hotspot properties.
    ///
    /// Added after linux-6.6, so `SET_CLIENT_CAP` rejects it with `EINVAL`.
    pub const CURSOR_PLANE_HOTSPOT: u64 = 6;
}
