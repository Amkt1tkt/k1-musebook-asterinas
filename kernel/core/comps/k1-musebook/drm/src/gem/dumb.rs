// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Dumb buffers: the driver-independent way to get a scanout buffer.
//!
//! A dumb buffer is the one allocation interface that needs no device-specific
//! user space. It is how a compositor gets a framebuffer before (or without)
//! any GPU driver. `drm_smoke` exercises the allocation and mapping path.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c>.

use ostd::mm::PAGE_SIZE;

use super::GemHandle;
use crate::{
    device::{DriverFeatures, DrmDevice, DumbCreateArgs},
    error::{DrmError, DrmResult},
    file::DrmFile,
    uapi::mode::{CreateDumb, DestroyDumb, MapDumb},
};

/// Creates a dumb buffer and installs a handle to it in `file`.
///
/// The arithmetic below is Linux's, overflow checks included: a client that
/// asks for a buffer whose pitch or size does not fit in 32 bits must get
/// `EINVAL` rather than a buffer smaller than it believes.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c#L60>
/// (`drm_mode_create_dumb`).
pub(super) fn create(device: &DrmDevice, file: &DrmFile, args: &mut CreateDumb) -> DrmResult<()> {
    require_gem(device)?;

    if args.width == 0 || args.height == 0 || args.bpp == 0 {
        return Err(DrmError::InvalidArgument);
    }

    // Divergence: Linux ignores `flags` in the core and leaves it to each
    // driver, most of which ignore it too. Rejecting what we do not understand
    // keeps the field available for a later meaning; every existing client
    // zeroes it.
    if args.flags != 0 {
        return Err(DrmError::InvalidArgument);
    }

    // Overflow checks for the 32-bit size calculation, in Linux's order.
    if args.bpp > u32::MAX - 8 {
        return Err(DrmError::InvalidArgument);
    }
    let cpp = args.bpp.div_ceil(8);
    if cpp > u32::MAX / args.width {
        return Err(DrmError::InvalidArgument);
    }
    let stride = cpp * args.width;
    if args.height > u32::MAX / stride {
        return Err(DrmError::InvalidArgument);
    }
    let size = args.height * stride;
    if (size as usize)
        .checked_next_multiple_of(PAGE_SIZE)
        .is_none()
    {
        return Err(DrmError::InvalidArgument);
    }

    // Zeroed before the driver runs, as Linux does, so that a driver which
    // forgets to set one of them cannot leak whatever user space happened to
    // leave in the structure back to itself as a valid-looking answer.
    args.handle = 0;
    args.pitch = 0;
    args.size = 0;

    let buffer = device.driver().dumb_create(DumbCreateArgs {
        width: args.width,
        height: args.height,
        bpp: args.bpp,
    })?;

    // The driver may round the pitch up for its hardware, but a buffer that
    // does not hold the requested image is a driver bug, and one that would
    // show up as user space writing past the end of its mapping.
    let needed = u64::from(buffer.pitch) * u64::from(args.height);
    if buffer.pitch < stride || buffer.size < needed || (buffer.object.size() as u64) < buffer.size
    {
        ostd::error!(
            "{} returned an undersized dumb buffer: {}x{}x{} needs {} bytes, got {}",
            device.driver().info().name,
            args.width,
            args.height,
            args.bpp,
            needed,
            buffer.size
        );
        return Err(DrmError::Io);
    }

    let handle = file.handles().insert(buffer.object)?;

    args.handle = handle.0;
    args.pitch = buffer.pitch;
    args.size = buffer.size;

    Ok(())
}

/// Returns the fake `mmap` offset of a dumb buffer, assigning one on first
/// call.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c#L119>
/// (`drm_mode_mmap_dumb_ioctl`) and
/// <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L314>
/// (`drm_gem_dumb_map_offset`).
pub(super) fn map(device: &DrmDevice, file: &DrmFile, args: &mut MapDumb) -> DrmResult<()> {
    require_gem(device)?;

    let object = file.handles().get(GemHandle(args.handle))?;
    if !object.is_mappable() {
        return Err(DrmError::InvalidArgument);
    }

    args.offset = device.gem().offsets().offset_of(&object)?;

    Ok(())
}

/// Closes a dumb buffer's handle.
///
/// Identical to `GEM_CLOSE`, down to reporting a bad handle as `EINVAL`;
/// Linux's implementation is literally a call to `drm_gem_handle_delete`. It
/// is a separate ioctl only because dumb buffers predate the rule that every
/// driver uses GEM handles.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_dumb_buffers.c#L136>
/// (`drm_mode_destroy_dumb`).
pub(super) fn destroy(device: &DrmDevice, file: &DrmFile, args: &DestroyDumb) -> DrmResult<()> {
    require_gem(device)?;

    file.handles().remove(GemHandle(args.handle))?;
    crate::live::note_gem_close();
    Ok(())
}

/// Refuses the request if the driver does not do GEM at all.
///
/// Divergence: Linux answers `ENOSYS` for a driver without `dumb_create` and
/// `EOPNOTSUPP` for one without `DRIVER_GEM`. `DrmError` has no `ENOSYS`, and
/// no user space distinguishes the two — `libdrm` only checks for failure — so
/// both are reported as `EOPNOTSUPP`.
fn require_gem(device: &DrmDevice) -> DrmResult<()> {
    if !device.driver().features().contains(DriverFeatures::GEM) {
        return Err(DrmError::NotSupported);
    }
    Ok(())
}
