// SPDX-License-Identifier: MPL-2.0

mod evdev;
mod fb;
mod mem;
// K1 MUSE Book --- (/dev/rtc0 for hwclock RTC_RD_TIME / RTC_SET_TIME)
mod rtc;
// K1 MUSE Book ---
pub(crate) mod misc;
mod model;
mod pty;
pub mod registry;
pub(crate) mod tty;
// K1 MUSE Book --- (DRM nodes and firmware loading; riscv64-only, crates are not on x86)
#[cfg(target_arch = "riscv64")]
mod dri;
#[cfg(target_arch = "riscv64")]
mod dri_export;
// The only firmware consumer is the PowerVR GPU, so the loader is also
// gated on the `pvr` feature to keep a `--no-default-features` check
// free of dead-code warnings.
#[cfg(all(target_arch = "riscv64", feature = "pvr"))]
mod firmware;
// K1 MUSE Book ---

use device_id::DeviceId;
pub(crate) use mem::{getrandom, geturandom};
pub(crate) use pty::{PtyMaster, PtySlave, new_pty_pair};
pub(crate) use registry::lookup;

use crate::{
    fs::{devtmpfs::DevtmpfsNodeMeta, file::PerOpenFileOps},
    prelude::*,
};

/// The abstraction of a device.
pub trait Device: Send + Sync + 'static {
    /// Returns the device type.
    fn type_(&self) -> DeviceType;

    /// Returns the device ID.
    fn id(&self) -> DeviceId;

    /// Returns the metadata that specifies a device inode to be created in devtmpfs, if any.
    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta>;

    /// Opens the device, returning a file-like object that the userspace can interact with by
    /// doing I/O.
    fn open(&self) -> Result<Box<dyn PerOpenFileOps>>;
}

impl Debug for dyn Device {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("type", &self.type_())
            .field("id", &self.id())
            .field("devtmpfs_meta", &self.devtmpfs_meta())
            .finish_non_exhaustive()
    }
}

/// Device type
#[derive(Debug)]
pub enum DeviceType {
    Char,
    Block,
}

pub(crate) fn init_in_first_kthread() {
    model::init_in_first_kthread();
    registry::init_in_first_kthread();
    mem::init_in_first_kthread();
    misc::init_in_first_kthread();
    evdev::init_in_first_kthread();
    // TODO: Transfer ownership of the boot framebuffer to DRM and skip registering the
    // legacy framebuffer device once DRM has initialized successfully.
    fb::init_in_first_kthread();
    // K1 MUSE Book --- (publish /dev/dri after every driver has registered)
    #[cfg(target_arch = "riscv64")]
    dri::init_in_first_kthread();
    // K1 MUSE Book ---
    // K1 MUSE Book --- (/dev/rtc0)
    rtc::init_in_first_kthread();
    // K1 MUSE Book ---
}

// K1 MUSE Book --- (firmware lives in initramfs; load after fs::init_in_first_kthread)
#[cfg(target_arch = "riscv64")]
pub(crate) fn load_firmware_in_first_kthread(path_resolver: &crate::fs::vfs::path::PathResolver) {
    dri::load_firmware_in_first_kthread(path_resolver);
}
// K1 MUSE Book ---

// K1 MUSE Book --- (exercise real mem registration before the K1 sysfs adapter)
#[cfg(ktest)]
pub(crate) fn init_memory_for_ktest() {
    static INIT: spin::Once<()> = spin::Once::new();
    INIT.call_once(|| {
        aster_device::init_for_ktest();
        mem::init_in_first_kthread();
    });
}
// K1 MUSE Book ---

/// Initializes device state after mounting rootfs.
pub(crate) fn init_in_first_process() -> Result<()> {
    tty::init_in_first_process()?;
    registry::init_in_first_process()?;

    Ok(())
}
