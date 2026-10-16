// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `/sys/class/power_supply/BAT0` on the device model.

//! `/sys/class/power_supply/BAT0`.
//!
//! A parentless power_supply device is the same shape as an input device:
//! [`register_virtual_device`] puts it at
//! `/sys/devices/virtual/power_supply/BAT0`, and `/sys/class/power_supply/BAT0`
//! is a relative symlink. Linux does the same for a class device with no
//! physical parent (`virtual_device_parent` in `drivers/base/core.c`).
//!
//! The node is created only when [`k1_musebook_power::battery`] is `Some`.
//! Attributes that move are live: each `cat` calls `battery()` again.

use core::sync::atomic::{AtomicBool, Ordering};

use aster_systree::SysStr;

use super::{LiveAttr, VirtualDeviceInfo, register_virtual_device};
use crate::prelude::*;

/// Linux class name. `libudev` / UPower look up this exact string.
const CLASS: &str = "power_supply";

/// The directory name consumed by the UPower shim.
const NAME: &str = "BAT0";

static PUBLISHED: AtomicBool = AtomicBool::new(false);

/// Publishes `BAT0` if a sample exists. Returns whether the node is
/// present afterwards. Idempotent: a second call does not create a
/// second directory. The power sampler retries publication after its first
/// valid late sample; once published, attributes read the current cached sample.
pub(crate) fn publish() -> bool {
    let Some(snap) = k1_musebook_power::battery() else {
        return false;
    };
    if PUBLISHED.swap(true, Ordering::AcqRel) {
        return true;
    }

    match register(snap.capacity_mode_watt) {
        Ok(()) => {
            info!("[k1-batt] sysfs /sys/class/power_supply/BAT0");
            true
        }
        Err(err) => {
            PUBLISHED.store(false, Ordering::Release);
            warn!("failed to publish /sys/class/power_supply/BAT0: {:?}", err);
            false
        }
    }
}

type CapacityReader = fn() -> Option<String>;

fn register(mode_watt: bool) -> Result<()> {
    let [now, full] = k1_musebook_power::sysfs::capacity_pair(mode_watt);
    let (read_now, read_full): (CapacityReader, CapacityReader) = if mode_watt {
        (attr_energy_now, attr_energy_full)
    } else {
        (attr_charge_now, attr_charge_full)
    };
    // Read values on demand so registration cannot freeze the initial I2C
    // sample into a static attribute.
    let live_attrs: Vec<LiveAttr> = vec![
        (SysStr::from("type"), attr_type),
        (SysStr::from("present"), attr_present),
        (SysStr::from("status"), attr_status),
        (SysStr::from("capacity"), attr_capacity),
        (SysStr::from("voltage_now"), attr_voltage_now),
        (SysStr::from("current_now"), attr_current_now),
        (SysStr::from("temp"), attr_temp),
        (SysStr::from("technology"), attr_technology),
        (SysStr::from("uevent"), attr_uevent),
        (SysStr::from(now), read_now),
        (SysStr::from(full), read_full),
    ];

    register_virtual_device(VirtualDeviceInfo {
        class: SysStr::from(CLASS),
        name: SysStr::from(NAME),
        // Overwritten by the live `uevent` entry.
        uevent: String::new(),
        attrs: Vec::new(),
        attr_groups: Vec::new(),
        live_attrs,
        live_rw_attrs: Vec::new(),
    })?;
    Ok(())
}

fn snap_attr(name: &str) -> Option<String> {
    k1_musebook_power::battery().and_then(|snap| k1_musebook_power::sysfs::format_attr(snap, name))
}

fn attr_type() -> Option<String> {
    snap_attr("type")
}

fn attr_present() -> Option<String> {
    snap_attr("present")
}

fn attr_status() -> Option<String> {
    snap_attr("status")
}

fn attr_capacity() -> Option<String> {
    snap_attr("capacity")
}

fn attr_voltage_now() -> Option<String> {
    snap_attr("voltage_now")
}

fn attr_current_now() -> Option<String> {
    snap_attr("current_now")
}

fn attr_temp() -> Option<String> {
    snap_attr("temp")
}

fn attr_technology() -> Option<String> {
    snap_attr("technology")
}

fn attr_charge_now() -> Option<String> {
    snap_attr("charge_now")
}

fn attr_charge_full() -> Option<String> {
    snap_attr("charge_full")
}

fn attr_energy_now() -> Option<String> {
    snap_attr("energy_now")
}

fn attr_energy_full() -> Option<String> {
    snap_attr("energy_full")
}

fn attr_uevent() -> Option<String> {
    k1_musebook_power::battery().map(k1_musebook_power::sysfs::format_uevent)
}

#[cfg(ktest)]
mod tests {
    use k1_musebook_power::{BatterySnapshot, Capacity, ChargeStatus};
    use ostd::prelude::ktest;

    use super::*;
    use crate::fs::{file::StatusFlags, sysfs::fs::SysFs, vfs::file_system::FileSystem};

    fn sample() -> BatterySnapshot {
        BatterySnapshot {
            soc_percent: 50,
            voltage_uv: 8_000_000,
            current_ua: -100_000,
            temp_decikelvin: 2981,
            status: ChargeStatus::Discharging,
            remaining: Capacity::MilliAmpHour(2500),
            full: Capacity::MilliAmpHour(5000),
            time_to_empty_min: None,
            time_to_full_min: None,
            battery_status_raw: 0x40,
            capacity_mode_watt: false,
        }
    }

    #[ktest]
    fn a_late_sample_publishes_bat0_and_preserves_capacity_units() {
        crate::time::clocks::init_for_ktest();
        crate::device::init_memory_for_ktest();
        k1_musebook_power::inject_snapshot_for_ktest(None);
        // Reuse the initialized device roots without registering sysfs twice.
        super::super::device::init();
        assert!(!publish());
        let root = SysFs::new_for_ktest().root_inode();
        let virtual_devices = root.lookup("devices").unwrap().lookup("virtual").unwrap();
        assert!(virtual_devices.lookup(CLASS).is_err());

        let saved = sample();
        k1_musebook_power::inject_snapshot_for_ktest(Some(saved));
        k1_musebook_power::register_battery_publisher(publish);
        let battery = virtual_devices.lookup(CLASS).unwrap().lookup(NAME).unwrap();
        let inode_number = battery.ino();
        assert!(publish());
        assert_eq!(
            virtual_devices
                .lookup(CLASS)
                .unwrap()
                .lookup(NAME)
                .unwrap()
                .ino(),
            inode_number
        );
        let now = battery.lookup("charge_now").unwrap();
        let full = battery.lookup("charge_full").unwrap();
        let mut bytes = [0u8; 32];
        let len = now
            .read_at(
                0,
                &mut VmWriter::from(&mut bytes[..]).to_fallible(),
                StatusFlags::empty(),
            )
            .unwrap();
        assert_eq!(&bytes[..len], b"2500000\n");

        k1_musebook_power::inject_snapshot_for_ktest(Some(BatterySnapshot {
            capacity_mode_watt: true,
            remaining: Capacity::MilliWattHour(20_000),
            full: Capacity::MilliWattHour(40_000),
            ..saved
        }));
        assert!(
            now.read_at(
                0,
                &mut VmWriter::from(&mut bytes[..]).to_fallible(),
                StatusFlags::empty()
            )
            .is_err()
        );
        assert!(
            full.read_at(
                0,
                &mut VmWriter::from(&mut bytes[..]).to_fallible(),
                StatusFlags::empty()
            )
            .is_err()
        );
        assert_eq!(battery.ino(), inode_number);
        k1_musebook_power::inject_snapshot_for_ktest(Some(saved));
    }
}
