// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The two PRIME ioctls.
//!
//! Registered in [`CORE_IOCTLS`](crate::ioctl::CORE_IOCTLS)
//! with `DRM_RENDER_ALLOW`, which both carry upstream: PRIME exists precisely
//! so that a render client with no access to the display can hand a finished
//! frame to whoever does.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L655>.

use alloc::sync::Arc;

use super::{DrmExport, ExportFlags};
use crate::{
    device::{DriverFeatures, DrmDevice},
    error::{DrmError, DrmResult},
    file::DrmFile,
    gem::GemHandle,
    ioctl::{CallerContext, payload_read, payload_write},
    uapi::{DrmPrimeHandle, sync::prime_flags},
};

/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`: shares a buffer as a file descriptor.
///
/// The handle is unaffected. User space normally closes it immediately
/// afterwards and relies on the descriptor keeping the buffer alive, which is
/// why [`DmaBuf`] holds a strong reference to the object.
///
/// Divergence: Linux ignores every bit of `flags` other than `DRM_CLOEXEC` and
/// `DRM_RDWR`. Unknown bits are refused here, for the same reason
/// `MODE_CREATE_DUMB` refuses unknown flags: the field is one user space
/// always zeroes the rest of, so keeping it meaningful costs nothing now and
/// leaves room later.
///
/// [`DmaBuf`]: super::DmaBuf
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_prime.c#L400>
/// (`drm_prime_handle_to_fd_ioctl`).
pub fn prime_handle_to_fd(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_prime(device)?;

    let mut args = payload_read::<DrmPrimeHandle>(payload)?;
    if args.flags & !prime_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let descriptors = caller.descriptors().ok_or(DrmError::NotSupported)?;
    let device = file.device().ok_or(DrmError::NoDevice)?;

    let object = file.handles().get(GemHandle(args.handle))?;
    let buffer = super::export(&device, &object)?;

    args.fd = descriptors.install(
        DrmExport::Buffer(buffer),
        ExportFlags {
            cloexec: args.flags & prime_flags::CLOEXEC != 0,
            read_write: args.flags & prime_flags::RDWR != 0,
        },
    )?;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: takes a shared buffer into this file.
///
/// Importing the same descriptor twice returns the same handle, because that
/// is what user space uses to recognize a buffer it already holds. The
/// descriptor stays open; closing it is the caller's business and does not
/// affect the handle.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_prime.c#L316>
/// (`drm_prime_fd_to_handle_ioctl`, whose `drm_gem_prime_fd_to_handle` does
/// the deduplication through `drm_prime_lookup_buf_handle`).
pub fn prime_fd_to_handle(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_prime(device)?;

    let mut args = payload_read::<DrmPrimeHandle>(payload)?;
    // Divergence: Linux documents `flags` as unused in this direction and does
    // not look at it. Refused when non-zero here, so that a client passing
    // `DRM_CLOEXEC` by mistake finds out rather than believing it took effect.
    if args.flags != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let descriptors = caller.descriptors().ok_or(DrmError::NotSupported)?;
    let device = file.device().ok_or(DrmError::NoDevice)?;

    let DrmExport::Buffer(buffer) = descriptors.resolve(args.fd)? else {
        // A descriptor that is open but is not a buffer. `dma_buf_get` reports
        // `EINVAL` for this, not `ENOTTY`.
        return Err(DrmError::InvalidArgument);
    };

    let handle = match file.handles().find_import(&buffer) {
        Some(handle) => handle,
        None => {
            // Driver work stays outside the table lock; installation rechecks
            // after it so concurrent foreign imports return one live handle.
            let object = super::import(&device, &buffer)?;
            file.handles().insert_import(&buffer, object)?
        }
    };

    args.handle = handle.0;

    payload_write(payload, &args)
}

/// Refuses an ioctl on a driver that does not do GEM.
///
/// Divergence: Linux gates the PRIME ioctls on `DRIVER_PRIME`, a feature bit
/// it removed in 5.4 in favour of always-on PRIME support, leaving the check
/// as `DRIVER_GEM`. There is no [`DriverFeatures::PRIME`] here for the same
/// reason: a driver with buffer objects can share them, and one without has
/// nothing to share.
///
/// [`DriverFeatures::PRIME`]: crate::device::DriverFeatures
fn require_prime(device: &DrmDevice) -> DrmResult<()> {
    if !device.driver().features().contains(DriverFeatures::GEM) {
        return Err(DrmError::NotSupported);
    }
    Ok(())
}

/// Returns the buffer behind a descriptor, for callers outside the ioctl path.
///
/// Submission paths that receive file descriptors can resolve the same
/// exported buffer as the PRIME ioctls.
pub fn buffer_of_fd(caller: &CallerContext, fd: i32) -> DrmResult<Arc<super::DmaBuf>> {
    let descriptors = caller.descriptors().ok_or(DrmError::NotSupported)?;

    match descriptors.resolve(fd)? {
        DrmExport::Buffer(buffer) => Ok(buffer),
        _ => Err(DrmError::InvalidArgument),
    }
}
