// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The GEM and dumb buffer ioctls.
//!
//! Registered in [`CORE_IOCTLS`](crate::ioctl::CORE_IOCTLS)
//! with the flags Linux gives them, which are worth reading off the table in
//! `drm_ioctl.c` rather than guessing:
//!
//! | ioctl | Linux flags | why |
//! |---|---|---|
//! | `GEM_CLOSE` | `DRM_RENDER_ALLOW` | a render client owns buffers and must be able to free them |
//! | `GEM_FLINK` | `DRM_AUTH` | publishes a buffer device-wide, so not for arbitrary render clients |
//! | `GEM_OPEN` | `DRM_AUTH` | the other half of the same, unauthenticated capability |
//! | `MODE_CREATE_DUMB` | none | primary node only: a dumb buffer is a scanout buffer |
//! | `MODE_MAP_DUMB` | none | ditto |
//! | `MODE_DESTROY_DUMB` | none | ditto |
//!
//! Note that `GEM_FLINK` and `GEM_OPEN` carry no `DRM_RENDER_ALLOW`: flink is
//! the unauthenticated sharing mechanism PRIME replaced, and render nodes
//! exist precisely so that an unprivileged client cannot reach the
//! device-global namespaces.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L649>.

use super::{GemHandle, dumb};
use crate::{
    device::{DriverFeatures, DrmDevice},
    error::{DrmError, DrmResult},
    file::DrmFile,
    ioctl::{CallerContext, payload_read, payload_write},
    uapi::{
        DrmGemClose, DrmGemFlink, DrmGemOpen,
        mode::{CreateDumb, DestroyDumb, MapDumb},
    },
};

/// `DRM_IOCTL_GEM_CLOSE`: releases one handle.
///
/// Closing a handle is not destroying the buffer. The object survives while a
/// PRIME file descriptor, another file's handle, or a mapping still refers to
/// it, and user space relies on that: exporting a dma-buf and immediately
/// closing the handle is the normal way to hand a buffer to another process.
///
/// A handle that is not in the table is `EINVAL`, not `ENOENT`. That looks
/// like an accident of Linux's implementation — `idr_replace` returning an
/// error — but it is what user space sees, so it is ABI.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L804>
/// (`drm_gem_close_ioctl`).
pub fn gem_close(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    require_gem(device)?;

    let args = payload_read::<DrmGemClose>(payload)?;
    file.handles().remove(GemHandle(args.handle))?;
    crate::live::note_gem_close();
    Ok(())
}

/// `DRM_IOCTL_GEM_FLINK`: publishes a handle under a global name.
///
/// The name is device-wide and unauthenticated: anybody who can open the node
/// and guess the number gets the buffer. That is why PRIME exists and why this
/// interface is deprecated; it is implemented because parts of Mesa still take
/// this path.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L830>
/// (`drm_gem_flink_ioctl`).
pub fn gem_flink(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    require_gem(device)?;

    let mut args = payload_read::<DrmGemFlink>(payload)?;

    let object = file.handles().get(GemHandle(args.handle))?;
    args.name = device.gem().names().publish(&object)?;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_GEM_OPEN`: resolves a global name into a handle in this file.
///
/// The new handle holds its own reference, so the object outlives the file
/// that published it.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L880>
/// (`drm_gem_open_ioctl`).
pub fn gem_open(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    require_gem(device)?;

    let mut args = payload_read::<DrmGemOpen>(payload)?;

    let object = device
        .gem()
        .names()
        .lookup(args.name)
        .ok_or(DrmError::NotFound)?;
    let size = object.size() as u64;
    let handle = file.handles().insert(object)?;

    args.handle = handle.0;
    args.size = size;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_CREATE_DUMB`: allocates a buffer suitable for scanout.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c#L99>
/// (`drm_mode_create_dumb_ioctl`).
pub fn mode_create_dumb(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<CreateDumb>(payload)?;

    let result = dumb::create(device, file, &mut args);

    // The outputs are written back even on failure, matching `drm_ioctl`,
    // which copies the argument structure out regardless of the handler's
    // verdict. They have been zeroed by then, so a client that ignores the
    // error still sees no handle rather than a stale one.
    payload_write(payload, &args)?;
    result
}

/// `DRM_IOCTL_MODE_MAP_DUMB`: returns the fake `mmap` offset of a dumb buffer.
///
/// The offset is not a position in any file. It is a device-wide token that
/// says which buffer a subsequent `mmap` of the DRM node is about; see
/// [`MmapOffsets`](super::MmapOffsets) for the encoding. Repeated calls for
/// one handle return the same value, which user space depends on.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c#L119>
/// (`drm_mode_mmap_dumb_ioctl`).
pub fn mode_map_dumb(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<MapDumb>(payload)?;

    dumb::map(device, file, &mut args)?;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_DESTROY_DUMB`: closes a dumb buffer's handle.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c#L145>
/// (`drm_mode_destroy_dumb_ioctl`).
pub fn mode_destroy_dumb(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<DestroyDumb>(payload)?;

    dumb::destroy(device, file, &args)
}

/// Refuses an ioctl on a driver that does not implement GEM.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L810>
/// (`drm_core_check_feature(dev, DRIVER_GEM)`).
fn require_gem(device: &DrmDevice) -> DrmResult<()> {
    if !device.driver().features().contains(DriverFeatures::GEM) {
        return Err(DrmError::NotSupported);
    }
    Ok(())
}
