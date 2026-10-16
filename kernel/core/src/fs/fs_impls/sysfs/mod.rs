// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book --- (shared sysfs device registration)
mod device;
// K1 MUSE Book ---
// K1 MUSE Book --- (PWM14 /sys/class/backlight)
mod backlight;
// K1 MUSE Book ---
mod fs;
mod inode;
mod kernel;
#[cfg(ktest)]
mod test;
// K1 MUSE Book --- (BAT0 / thermal_zone0 / cpufreq policy0 on the device model)
#[cfg(all(target_arch = "riscv64", feature = "cpufreq"))]
mod cpufreq;
#[cfg(target_arch = "riscv64")]
mod power_supply;
#[cfg(target_arch = "riscv64")]
mod thermal;
// K1 MUSE Book ---

use aster_systree::SysNode;
pub(crate) use aster_systree::primary_tree as systree_singleton;
use fs::SysFsType;

// K1 MUSE Book --- (shared device-model API)
#[expect(unused_imports)]
pub(crate) use self::device::{
    AttrDir, AttrDirInfo, AttrFile, AttrGroup, CharDevice, CharDeviceInfo, LiveAttr, LiveRwAttr,
    PlatformDevice, PlatformDeviceInfo, VirtualDevice, VirtualDeviceInfo, class_index,
    register_attr_dir, register_platform_device, register_virtual_device, system_devices_dir,
};
// K1 MUSE Book ---
use crate::{fs::vfs::registry, prelude::*};

// This method should be called during kernel file system initialization,
// _after_ `aster_systree::init`.
pub(crate) fn init() {
    // K1 MUSE Book --- (device model owns /sys/devices before other sysfs nodes)
    // registry::register(&SysFsType).unwrap();
    //
    // kernel::init();
    registry::register(&SysFsType).unwrap();

    device::init();
    kernel::init();
    // K1 MUSE Book ---
    // K1 MUSE Book --- (device-model class nodes; BAT0 waits for a first sample)
    #[cfg(target_arch = "riscv64")]
    {
        let _ = power_supply::publish();
        let _ = thermal::publish();
        #[cfg(feature = "cpufreq")]
        let _ = cpufreq::publish();
    }
    // K1 MUSE Book ---
    // K1 MUSE Book --- (publish /sys/class/backlight after the device model exists)
    backlight::publish();
    // K1 MUSE Book ---
}

// K1 MUSE Book --- (retry BAT0 after a late I2C sample in first_kthread)
/// Publishes `/sys/class/power_supply/BAT0` if a sample exists.
#[cfg(target_arch = "riscv64")]
pub(crate) fn publish_battery() -> bool {
    power_supply::publish()
}
// K1 MUSE Book ---

/// Registers a new kernel `SysNode`.
pub(crate) fn register_kernel_sysnode(config_obj: Arc<dyn SysNode>) -> Result<()> {
    kernel::register(config_obj)
}

/// Unregisters a kernel `SysNode`.
#[expect(dead_code)]
pub(crate) fn unregister_kernel_sysnode(name: &str) -> Result<()> {
    kernel::unregister(name)
}
