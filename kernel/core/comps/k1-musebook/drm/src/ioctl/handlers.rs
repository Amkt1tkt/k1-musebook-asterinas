// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The ioctls the DRM core answers on every driver's behalf.
//!
//! These are the ones whose answers come from the core's own
//! bookkeeping — driver identity, capabilities, mastership — rather than from any
//! hardware. GEM, mode setting and PRIME handlers live with their subsystems.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c>.

use super::{CallerContext, copy_out_string, copy_out_string_whole, payload_read, payload_write};
use crate::{
    device::{DriverInfo, DrmDevice},
    error::{DrmError, DrmResult},
    file::DrmFile,
    uapi::{DrmGetCap, DrmSetClientCap, DrmSetVersion, DrmUnique, DrmVersion},
};

/// `DRM_IOCTL_VERSION`: reports the driver's identity and interface version.
///
/// The three strings use a two-stage protocol. Each is a `(pointer, capacity)`
/// pair on input and a length on output, and the kernel always reports the full
/// length of its value — truncating the copy if the buffer is short, and skipping
/// it entirely when the capacity is zero. `libdrm`'s `drmGetVersion` depends on
/// exactly that: it calls this ioctl once with all three capacities set to zero
/// purely to learn the lengths, allocates, then calls it again.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L510>
/// (`drm_version`).
pub fn version(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<DrmVersion>(payload)?;
    let info = device.driver().info();

    args.version_major = info.major;
    args.version_minor = info.minor;
    args.version_patchlevel = info.patchlevel;

    // Linux copies the argument structure back to user space even when a handler
    // fails, so the lengths gathered before a faulting buffer still get through.
    // Preserve that by updating `payload` before propagating the error.
    let copied = copy_version_strings(caller, &mut args, &info);
    payload_write(payload, &args)?;
    copied
}

fn copy_version_strings(
    caller: &CallerContext,
    args: &mut DrmVersion,
    info: &DriverInfo,
) -> DrmResult<()> {
    args.name_len = copy_out_string(caller, args.name, args.name_len, info.name)?;
    args.date_len = copy_out_string(caller, args.date, args.date_len, info.date)?;
    args.desc_len = copy_out_string(caller, args.desc, args.desc_len, info.desc)?;
    Ok(())
}

/// `DRM_IOCTL_GET_UNIQUE`: reports the device's bus ID.
///
/// Unlike `VERSION`, this copy is all or nothing: the string is written only if
/// the caller's buffer can hold all of it, and the full length is reported either
/// way.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L116>
/// (`drm_getunique`).
pub fn get_unique(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<DrmUnique>(payload)?;

    // Empty until the client has asked for it through `SET_VERSION`, which is
    // what upstream does: `drm_master::unique` starts `NULL` and only
    // `drm_set_busid` fills it in. See [`DrmFile::busid_set`] for why answering
    // unconditionally hides the device from `libdrm`'s open-by-name path.
    let unique = if file.busid_set() {
        device.unique()
    } else {
        ""
    };
    args.unique_len = copy_out_string_whole(caller, args.unique, args.unique_len, unique)?;

    payload_write(payload, &args)
}

/// The DRM interface version this implementation speaks.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_internal.h>
/// (`DRM_IF_MAJOR` / `DRM_IF_MINOR`).
const IF_MAJOR: i32 = 1;
const IF_MINOR: i32 = 4;

/// `DRM_IOCTL_SET_VERSION`: negotiates the interface version, and with it the
/// bus ID.
///
/// The version negotiation matters less than its side effect. Asking for
/// interface 1.1 or later is what makes `GET_UNIQUE` start answering with the
/// device's bus ID -- upstream calls `drm_set_busid` from here, and nothing else
/// ever sets `drm_master::unique`. See [`DrmFile::busid_set`].
///
/// A `-1` in either half means "leave that one alone". Both halves are answered
/// with what is actually in force regardless of what was asked, which is how a
/// client learns what it got.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L382>
/// (`drm_setversion`).
pub fn set_version(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<DrmSetVersion>(payload)?;
    let info = device.driver().info();
    let mut result = Ok(());

    if args.drm_di_major != -1 {
        if args.drm_di_major != IF_MAJOR || args.drm_di_minor < 0 || args.drm_di_minor > IF_MINOR {
            result = Err(DrmError::InvalidArgument);
        } else if args.drm_di_minor >= 1 {
            file.set_busid();
        }
    }

    if result.is_ok()
        && args.drm_dd_major != -1
        && (args.drm_dd_major != info.major
            || args.drm_dd_minor < 0
            || args.drm_dd_minor > info.minor)
    {
        result = Err(DrmError::InvalidArgument);
    }

    // Written back even on the error paths, exactly as upstream does: the reply
    // is how the caller finds out which versions it should have asked for.
    args.drm_di_major = IF_MAJOR;
    args.drm_di_minor = IF_MINOR;
    args.drm_dd_major = info.major;
    args.drm_dd_minor = info.minor;
    payload_write(payload, &args)?;

    result
}

/// `DRM_IOCTL_GET_CAP`: asks what the device supports.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L231>
/// (`drm_getcap`).
pub fn get_cap(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<DrmGetCap>(payload)?;

    // Linux zeroes the answer before deciding, so that the structure it copies
    // back on failure carries no stale value.
    let result = device.capability(args.capability);
    args.value = result.unwrap_or(0);
    payload_write(payload, &args)?;

    result.map(|_| ())
}

/// `DRM_IOCTL_SET_CLIENT_CAP`: opts this file into an extended view of the device.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L306>
/// (`drm_setclientcap`).
pub fn set_client_cap(
    _device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<DrmSetClientCap>(payload)?;
    file.set_client_cap(args.capability, args.value)
}

/// `DRM_IOCTL_SET_MASTER`: claims the right to program the display.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_auth.c#L248>
/// (`drm_setmaster_ioctl`).
pub fn set_master(
    device: &DrmDevice,
    file: &DrmFile,
    _payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    device.set_master(file, caller)
}

/// `DRM_IOCTL_DROP_MASTER`: gives up the right to program the display.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_auth.c#L299>
/// (`drm_dropmaster_ioctl`).
pub fn drop_master(
    device: &DrmDevice,
    file: &DrmFile,
    _payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    device.drop_master(file, caller)
}
