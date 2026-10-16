// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! DRM ioctl numbers.
//!
//! The command numbers are given as the raw `nr` (the low byte), together with
//! [`iowr`]/[`iow`]/[`ior`]/[`io`] helpers that reproduce Linux's `_IOC`
//! encoding. Matching on the raw `nr` is preferable in the dispatcher: the
//! encoded size field varies with the pointer width of the caller's headers, and
//! some clients get it subtly wrong.
//!
//! References:
//! - <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/asm-generic/ioctl.h>
//! - <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L983>

/// The ioctl type byte for DRM: `'d'`.
pub const DRM_IOCTL_BASE: u32 = 0x64;

/// The first command number reserved for driver-private ioctls.
///
/// PowerVR's fourteen ioctls live at `DRM_COMMAND_BASE + 0..=0x0d`; see
/// `../../ABI.md`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L980>.
pub const DRM_COMMAND_BASE: u32 = 0x40;

/// One past the last driver-private command number.
pub const DRM_COMMAND_END: u32 = 0xA0;

/// Returns whether a command number belongs to a driver rather than the core.
///
/// The dispatcher checks this first, so that a driver cannot be shadowed by a core
/// ioctl that happens to share a number.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L837>.
pub const fn is_driver_private(nr: u32) -> bool {
    DRM_COMMAND_BASE <= nr && nr < DRM_COMMAND_END
}

const NR_SHIFT: u32 = 0;
const TYPE_SHIFT: u32 = 8;
const SIZE_SHIFT: u32 = 16;
const DIR_SHIFT: u32 = 30;

const DIR_NONE: u32 = 0;
const DIR_WRITE: u32 = 1;
const DIR_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << DIR_SHIFT)
        | (DRM_IOCTL_BASE << TYPE_SHIFT)
        | (nr << NR_SHIFT)
        | ((size as u32) << SIZE_SHIFT)
}

/// Encodes a `DRM_IO(nr)` command, which carries no argument.
pub const fn io(nr: u32) -> u32 {
    ioc(DIR_NONE, nr, 0)
}

/// Encodes a `DRM_IOR(nr, T)` command, which only writes to user space.
pub const fn ior(nr: u32, size: usize) -> u32 {
    ioc(DIR_READ, nr, size)
}

/// Encodes a `DRM_IOW(nr, T)` command, which only reads from user space.
pub const fn iow(nr: u32, size: usize) -> u32 {
    ioc(DIR_WRITE, nr, size)
}

/// Encodes a `DRM_IOWR(nr, T)` command, which reads and writes user space.
pub const fn iowr(nr: u32, size: usize) -> u32 {
    ioc(DIR_READ | DIR_WRITE, nr, size)
}

/// Extracts the command number from an encoded ioctl request.
pub const fn nr_of(request: u32) -> u32 {
    (request >> NR_SHIFT) & 0xff
}

/// Extracts the type byte from an encoded ioctl request.
pub const fn type_of(request: u32) -> u32 {
    (request >> TYPE_SHIFT) & 0xff
}

/// Command numbers, verified against `include/uapi/drm/drm.h` in the BSP tree.
///
/// Gaps are the legacy DRI1 interfaces we deliberately do not implement; see
/// the divergence list in `../../ABI.md`.
pub mod nr {
    /// `DRM_IOCTL_VERSION`, `DRM_IOWR(0x00, struct drm_version)`.
    pub const VERSION: u32 = 0x00;
    /// `DRM_IOCTL_GET_UNIQUE`, `DRM_IOWR(0x01, struct drm_unique)`.
    pub const GET_UNIQUE: u32 = 0x01;
    /// `DRM_IOCTL_GET_MAGIC`, `DRM_IOR(0x02, struct drm_auth)`.
    pub const GET_MAGIC: u32 = 0x02;
    /// `DRM_IOCTL_GET_CAP`, `DRM_IOWR(0x0c, struct drm_get_cap)`.
    pub const GET_CAP: u32 = 0x0c;
    /// `DRM_IOCTL_SET_CLIENT_CAP`, `DRM_IOW(0x0d, struct drm_set_client_cap)`.
    pub const SET_CLIENT_CAP: u32 = 0x0d;
    /// `DRM_IOCTL_SET_VERSION`, `DRM_IOWR(0x07, struct drm_set_version)`.
    pub const SET_VERSION: u32 = 0x07;
    /// `DRM_IOCTL_GEM_CLOSE`, `DRM_IOW(0x09, struct drm_gem_close)`.
    pub const GEM_CLOSE: u32 = 0x09;
    /// `DRM_IOCTL_GEM_FLINK`, `DRM_IOWR(0x0a, struct drm_gem_flink)`.
    pub const GEM_FLINK: u32 = 0x0a;
    /// `DRM_IOCTL_GEM_OPEN`, `DRM_IOWR(0x0b, struct drm_gem_open)`.
    pub const GEM_OPEN: u32 = 0x0b;
    /// `DRM_IOCTL_AUTH_MAGIC`, `DRM_IOW(0x11, struct drm_auth)`.
    pub const AUTH_MAGIC: u32 = 0x11;
    /// `DRM_IOCTL_SET_MASTER`, `DRM_IO(0x1e)`.
    pub const SET_MASTER: u32 = 0x1e;
    /// `DRM_IOCTL_DROP_MASTER`, `DRM_IO(0x1f)`.
    pub const DROP_MASTER: u32 = 0x1f;
    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD`, `DRM_IOWR(0x2d, struct drm_prime_handle)`.
    pub const PRIME_HANDLE_TO_FD: u32 = 0x2d;
    /// `DRM_IOCTL_PRIME_FD_TO_HANDLE`, `DRM_IOWR(0x2e, struct drm_prime_handle)`.
    pub const PRIME_FD_TO_HANDLE: u32 = 0x2e;
    /// `DRM_IOCTL_WAIT_VBLANK`, `DRM_IOWR(0x3a, union drm_wait_vblank)`.
    pub const WAIT_VBLANK: u32 = 0x3a;

    /// `DRM_IOCTL_MODE_GETRESOURCES`, `DRM_IOWR(0xA0, struct drm_mode_card_res)`.
    pub const MODE_GETRESOURCES: u32 = 0xA0;
    /// `DRM_IOCTL_MODE_GETCRTC`, `DRM_IOWR(0xA1, struct drm_mode_crtc)`.
    pub const MODE_GETCRTC: u32 = 0xA1;
    /// `DRM_IOCTL_MODE_SETCRTC`, `DRM_IOWR(0xA2, struct drm_mode_crtc)`.
    pub const MODE_SETCRTC: u32 = 0xA2;
    /// `DRM_IOCTL_MODE_CURSOR`, `DRM_IOWR(0xA3, struct drm_mode_cursor)`.
    pub const MODE_CURSOR: u32 = 0xA3;
    /// `DRM_IOCTL_MODE_GETGAMMA`, `DRM_IOWR(0xA4, struct drm_mode_crtc_lut)`.
    pub const MODE_GETGAMMA: u32 = 0xA4;
    /// `DRM_IOCTL_MODE_SETGAMMA`, `DRM_IOWR(0xA5, struct drm_mode_crtc_lut)`.
    pub const MODE_SETGAMMA: u32 = 0xA5;
    /// `DRM_IOCTL_MODE_GETENCODER`, `DRM_IOWR(0xA6, struct drm_mode_get_encoder)`.
    pub const MODE_GETENCODER: u32 = 0xA6;
    /// `DRM_IOCTL_MODE_GETCONNECTOR`, `DRM_IOWR(0xA7, struct drm_mode_get_connector)`.
    pub const MODE_GETCONNECTOR: u32 = 0xA7;
    /// `DRM_IOCTL_MODE_GETPROPERTY`, `DRM_IOWR(0xAA, struct drm_mode_get_property)`.
    pub const MODE_GETPROPERTY: u32 = 0xAA;
    /// `DRM_IOCTL_MODE_SETPROPERTY`, `DRM_IOWR(0xAB, struct drm_mode_connector_set_property)`.
    pub const MODE_SETPROPERTY: u32 = 0xAB;
    /// `DRM_IOCTL_MODE_GETPROPBLOB`, `DRM_IOWR(0xAC, struct drm_mode_get_blob)`.
    pub const MODE_GETPROPBLOB: u32 = 0xAC;
    /// `DRM_IOCTL_MODE_GETFB`, `DRM_IOWR(0xAD, struct drm_mode_fb_cmd)`.
    pub const MODE_GETFB: u32 = 0xAD;
    /// `DRM_IOCTL_MODE_ADDFB`, `DRM_IOWR(0xAE, struct drm_mode_fb_cmd)`.
    pub const MODE_ADDFB: u32 = 0xAE;
    /// `DRM_IOCTL_MODE_RMFB`, `DRM_IOWR(0xAF, unsigned int)`.
    pub const MODE_RMFB: u32 = 0xAF;
    /// `DRM_IOCTL_MODE_PAGE_FLIP`, `DRM_IOWR(0xB0, struct drm_mode_crtc_page_flip)`.
    pub const MODE_PAGE_FLIP: u32 = 0xB0;
    /// `DRM_IOCTL_MODE_DIRTYFB`, `DRM_IOWR(0xB1, struct drm_mode_fb_dirty_cmd)`.
    pub const MODE_DIRTYFB: u32 = 0xB1;
    /// `DRM_IOCTL_MODE_CREATE_DUMB`, `DRM_IOWR(0xB2, struct drm_mode_create_dumb)`.
    pub const MODE_CREATE_DUMB: u32 = 0xB2;
    /// `DRM_IOCTL_MODE_MAP_DUMB`, `DRM_IOWR(0xB3, struct drm_mode_map_dumb)`.
    pub const MODE_MAP_DUMB: u32 = 0xB3;
    /// `DRM_IOCTL_MODE_DESTROY_DUMB`, `DRM_IOWR(0xB4, struct drm_mode_destroy_dumb)`.
    pub const MODE_DESTROY_DUMB: u32 = 0xB4;
    /// `DRM_IOCTL_MODE_GETPLANERESOURCES`, `DRM_IOWR(0xB5, struct drm_mode_get_plane_res)`.
    pub const MODE_GETPLANERESOURCES: u32 = 0xB5;
    /// `DRM_IOCTL_MODE_GETPLANE`, `DRM_IOWR(0xB6, struct drm_mode_get_plane)`.
    pub const MODE_GETPLANE: u32 = 0xB6;
    /// `DRM_IOCTL_MODE_SETPLANE`, `DRM_IOWR(0xB7, struct drm_mode_set_plane)`.
    pub const MODE_SETPLANE: u32 = 0xB7;
    /// `DRM_IOCTL_MODE_ADDFB2`, `DRM_IOWR(0xB8, struct drm_mode_fb_cmd2)`.
    pub const MODE_ADDFB2: u32 = 0xB8;
    /// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`, `DRM_IOWR(0xB9, struct drm_mode_obj_get_properties)`.
    pub const MODE_OBJ_GETPROPERTIES: u32 = 0xB9;
    /// `DRM_IOCTL_MODE_OBJ_SETPROPERTY`, `DRM_IOWR(0xBA, struct drm_mode_obj_set_property)`.
    pub const MODE_OBJ_SETPROPERTY: u32 = 0xBA;
    /// `DRM_IOCTL_MODE_CURSOR2`, `DRM_IOWR(0xBB, struct drm_mode_cursor2)`.
    pub const MODE_CURSOR2: u32 = 0xBB;
    /// `DRM_IOCTL_MODE_ATOMIC`, `DRM_IOWR(0xBC, struct drm_mode_atomic)`.
    pub const MODE_ATOMIC: u32 = 0xBC;
    /// `DRM_IOCTL_MODE_CREATEPROPBLOB`, `DRM_IOWR(0xBD, struct drm_mode_create_blob)`.
    pub const MODE_CREATEPROPBLOB: u32 = 0xBD;
    /// `DRM_IOCTL_MODE_DESTROYPROPBLOB`, `DRM_IOWR(0xBE, struct drm_mode_destroy_blob)`.
    pub const MODE_DESTROYPROPBLOB: u32 = 0xBE;

    /// `DRM_IOCTL_SYNCOBJ_CREATE`, `DRM_IOWR(0xBF, struct drm_syncobj_create)`.
    pub const SYNCOBJ_CREATE: u32 = 0xBF;
    /// `DRM_IOCTL_SYNCOBJ_DESTROY`, `DRM_IOWR(0xC0, struct drm_syncobj_destroy)`.
    pub const SYNCOBJ_DESTROY: u32 = 0xC0;
    /// `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD`, `DRM_IOWR(0xC1, struct drm_syncobj_handle)`.
    pub const SYNCOBJ_HANDLE_TO_FD: u32 = 0xC1;
    /// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE`, `DRM_IOWR(0xC2, struct drm_syncobj_handle)`.
    pub const SYNCOBJ_FD_TO_HANDLE: u32 = 0xC2;
    /// `DRM_IOCTL_SYNCOBJ_WAIT`, `DRM_IOWR(0xC3, struct drm_syncobj_wait)`.
    pub const SYNCOBJ_WAIT: u32 = 0xC3;
    /// `DRM_IOCTL_SYNCOBJ_RESET`, `DRM_IOWR(0xC4, struct drm_syncobj_array)`.
    pub const SYNCOBJ_RESET: u32 = 0xC4;
    /// `DRM_IOCTL_SYNCOBJ_SIGNAL`, `DRM_IOWR(0xC5, struct drm_syncobj_array)`.
    pub const SYNCOBJ_SIGNAL: u32 = 0xC5;
    /// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`, `DRM_IOWR(0xCA, struct drm_syncobj_timeline_wait)`.
    pub const SYNCOBJ_TIMELINE_WAIT: u32 = 0xCA;
    /// `DRM_IOCTL_SYNCOBJ_QUERY`, `DRM_IOWR(0xCB, struct drm_syncobj_timeline_array)`.
    pub const SYNCOBJ_QUERY: u32 = 0xCB;
    /// `DRM_IOCTL_SYNCOBJ_TRANSFER`, `DRM_IOWR(0xCC, struct drm_syncobj_transfer)`.
    pub const SYNCOBJ_TRANSFER: u32 = 0xCC;
    /// `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`, `DRM_IOWR(0xCD, struct drm_syncobj_timeline_array)`.
    pub const SYNCOBJ_TIMELINE_SIGNAL: u32 = 0xCD;
    /// `DRM_IOCTL_MODE_GETFB2`, `DRM_IOWR(0xCE, struct drm_mode_fb_cmd2)`.
    pub const MODE_GETFB2: u32 = 0xCE;
    /// `DRM_IOCTL_SYNCOBJ_EVENTFD`, `DRM_IOWR(0xCF, struct drm_syncobj_eventfd)`.
    ///
    /// Added in linux-6.6. The argument is 24 bytes; that size is asserted in
    /// [`crate::uapi`] because the size is part of the encoded ioctl number.
    pub const SYNCOBJ_EVENTFD: u32 = 0xCF;
}
