// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: safe platform contract for PowerVR hardware access.

//! Platform integration.
//!
//! Owner: the platform layer. The equivalent of upstream's per-compatible-string
//! `pvr_device_data`.

#[cfg(target_arch = "riscv64")]
pub mod k1;

#[cfg(target_arch = "riscv64")]
use alloc::string::ToString;
use alloc::vec::Vec;

use component::ComponentInitError;
use k1_musebook_drm::PlatformIdentity;

/// Probes the GPU on the current platform.
///
/// A platform where the GPU is absent must return `Ok(())`, not an error: this runs
/// during component init, and failing here would take down the whole boot for a
/// device nobody asked for.
pub fn probe() -> Result<(), ComponentInitError> {
    #[cfg(target_arch = "riscv64")]
    return k1::probe();

    #[cfg(not(target_arch = "riscv64"))]
    Ok(())
}

/// The firmware image this platform's GPU needs, relative to `/lib/firmware`.
///
/// [`probe`] stops before loading it, because component initialization runs
/// before there is a file system. The kernel shell reads the file once the root
/// is mounted and hands the bytes back through [`attach`]; this is the name it
/// asks for.
///
/// `None` on a platform with no GPU, which tells the caller there is nothing to
/// read.
pub fn firmware_name() -> Option<&'static str> {
    #[cfg(target_arch = "riscv64")]
    return Some(k1::FIRMWARE_NAME);

    #[cfg(not(target_arch = "riscv64"))]
    None
}

/// Finishes bring-up with the firmware image the caller read on the driver's
/// behalf.
///
/// The second half of [`probe`]. On success a DRM device has been registered
/// and its render node is ready to be published; the caller does that, because
/// device nodes and sysfs belong to the kernel shell.
pub fn attach(image: Vec<u8>) -> Result<(), crate::error::PvrError> {
    #[cfg(target_arch = "riscv64")]
    return k1::attach(image).map(|device| {
        ATTACHED.call_once(|| device);
    });

    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = image;
        Err(crate::error::PvrError::Unsupported)
    }
}

/// The device [`attach`] brought up, for [`run_service`].
///
/// One GPU, so a `Once`, as in [`crate::power::install`] and
/// [`crate::driver::register`].
static ATTACHED: spin::Once<alloc::sync::Arc<crate::device::PvrDevice>> = spin::Once::new();

/// Runs the driver's sleepable work for ever, on a thread the caller created.
///
/// The portable entry point [`crate::service`] describes: this crate cannot create
/// a thread that the kernel's scheduler will run, so `aster-core` makes one and
/// calls this from it. Nothing else about the work moves out of the driver.
///
/// Returns immediately if no GPU was attached, which is the caller's cue that the
/// thread has nothing to do. It never returns otherwise.
pub fn run_service() {
    let Some(device) = ATTACHED.get() else {
        ostd::warn!("no attached GPU; the service thread has nothing to run");
        return;
    };
    crate::service::run(device.clone())
}

/// How this GPU appears in sysfs, which is how `libdrm` finds it at all.
///
/// `drmGetDevices2` enumerates DRM devices by reading `/sys`, not by asking the
/// kernel, so a device without these entries is skipped without a word. The
/// `of_fullname` must differ from every other DRM device's — see
/// [`k1::GPU_OF_FULLNAME`] and `ABI.md` device identity contract.
///
/// `None` on a platform with no GPU node, which is what tells
/// `kernel/core/src/device/dri.rs` not to publish anything.
pub fn identity() -> Option<PlatformIdentity> {
    #[cfg(target_arch = "riscv64")]
    return Some(PlatformIdentity {
        name: k1::GPU_DEVICE_NAME.to_string(),
        of_fullname: k1::GPU_OF_FULLNAME.to_string(),
        of_compatible: k1::GPU_OF_COMPATIBLE
            .iter()
            .map(|compatible| compatible.to_string())
            .collect::<Vec<_>>(),
    });

    #[cfg(not(target_arch = "riscv64"))]
    None
}
