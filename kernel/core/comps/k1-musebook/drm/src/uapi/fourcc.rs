// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Pixel format codes and format modifiers.
//!
//! Transcribed from `official/bsp-src/linux-6.6/include/uapi/drm/drm_fourcc.h`.
//! Only the formats a driver in this tree can actually scan out are listed: an
//! unknown format code must be rejected, so a longer list would be a liability
//! rather than an improvement.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_fourcc.h>.

/// Builds a format code from four characters, as Linux's `fourcc_code` does.
const fn fourcc(code: [u8; 4]) -> u32 {
    (code[0] as u32) | ((code[1] as u32) << 8) | ((code[2] as u32) << 16) | ((code[3] as u32) << 24)
}

/// `DRM_FORMAT_C8`: 8-bit palette index.
pub const C8: u32 = fourcc(*b"C8  ");
/// `DRM_FORMAT_RGB332`: 8 bits per pixel, `[7:0] R:G:B 3:3:2`.
pub const RGB332: u32 = fourcc(*b"RGB8");
/// `DRM_FORMAT_XRGB1555`: 16 bits per pixel, `[15:0] x:R:G:B 1:5:5:5`.
pub const XRGB1555: u32 = fourcc(*b"XR15");
/// `DRM_FORMAT_ARGB1555`: 16 bits per pixel, `[15:0] A:R:G:B 1:5:5:5`.
pub const ARGB1555: u32 = fourcc(*b"AR15");
/// `DRM_FORMAT_RGB565`: 16 bits per pixel, `[15:0] R:G:B 5:6:5`.
pub const RGB565: u32 = fourcc(*b"RG16");
/// `DRM_FORMAT_RGB888`: 24 bits per pixel, `[23:0] R:G:B`.
pub const RGB888: u32 = fourcc(*b"RG24");
/// `DRM_FORMAT_BGR888`: 24 bits per pixel, `[23:0] B:G:R`.
pub const BGR888: u32 = fourcc(*b"BG24");
/// `DRM_FORMAT_XRGB8888`: 32 bits per pixel, `[31:0] x:R:G:B 8:8:8:8`.
///
/// The format every KMS client can be assumed to handle, and the one
/// `modetest` picks by default.
pub const XRGB8888: u32 = fourcc(*b"XR24");
/// `DRM_FORMAT_XBGR8888`: 32 bits per pixel, `[31:0] x:B:G:R 8:8:8:8`.
pub const XBGR8888: u32 = fourcc(*b"XB24");
/// `DRM_FORMAT_ARGB8888`: 32 bits per pixel, `[31:0] A:R:G:B 8:8:8:8`.
pub const ARGB8888: u32 = fourcc(*b"AR24");
/// `DRM_FORMAT_ABGR8888`: 32 bits per pixel, `[31:0] A:B:G:R 8:8:8:8`.
pub const ABGR8888: u32 = fourcc(*b"AB24");
/// `DRM_FORMAT_XRGB2101010`: 32 bits per pixel, `[31:0] x:R:G:B 2:10:10:10`.
pub const XRGB2101010: u32 = fourcc(*b"XR30");

/// `DRM_FORMAT_MOD_LINEAR`: no tiling, no compression.
///
/// A plane that only handles linear buffers must still report this modifier
/// rather than an empty list: an empty list means "modifiers unsupported" and
/// sends Mesa down a different allocation path.
pub const MOD_LINEAR: u64 = 0;

/// `DRM_FORMAT_MOD_INVALID`, the "no modifier chosen yet" sentinel.
///
/// `fourcc_mod_code(NONE, (1 << 56) - 1)`, with the vendor field zero.
pub const MOD_INVALID: u64 = (1 << 56) - 1;

/// Returns the number of bytes one pixel of `format` occupies, for the single
/// plane formats above.
///
/// `None` means the format is not one we accept, which is how `ADDFB2` rejects
/// a framebuffer no plane could ever scan out.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_fourcc.c#L142>
/// (`drm_format_info`, whose `cpp[0]` this reproduces).
pub const fn bytes_per_pixel(format: u32) -> Option<u32> {
    match format {
        C8 | RGB332 => Some(1),
        XRGB1555 | ARGB1555 | RGB565 => Some(2),
        RGB888 | BGR888 => Some(3),
        XRGB8888 | XBGR8888 | ARGB8888 | ABGR8888 | XRGB2101010 => Some(4),
        _ => None,
    }
}

/// Translates the legacy `ADDFB` `(bpp, depth)` pair into a format code.
///
/// The legacy interface predates format codes and describes a buffer by depth
/// and bits per pixel, which is ambiguous for anything but the handful of
/// combinations Linux recognizes. Everything else is rejected rather than
/// guessed at, because guessing wrong means scanning out garbage.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_fourcc.c#L44>
/// (`drm_mode_legacy_fb_format`).
pub const fn legacy_fb_format(bpp: u32, depth: u32) -> Option<u32> {
    match (bpp, depth) {
        (8, 8) => Some(C8),
        (16, 15) => Some(XRGB1555),
        (16, 16) => Some(RGB565),
        (24, 24) => Some(RGB888),
        (32, 24) => Some(XRGB8888),
        (32, 30) => Some(XRGB2101010),
        (32, 32) => Some(ARGB8888),
        _ => None,
    }
}
