// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux power_supply sysfs attribute formatting.

//! Linux `/sys/class/power_supply` attribute text.
//!
//! Names, units, and the `status` spellings are the power_supply class
//! ABI — see `power_supply_sysfs.c` — not something this port invented.
//! The UPower stub on this board reads those files; a different string
//! or unit silently becomes "no battery" or a wrong icon.
//!
//! This module only formats a [`BatterySnapshot`]. It does not publish
//! sysfs nodes: the device model lives in `aster-kernel` and is
//! initialized after Bootstrap component init, so this crate cannot
//! hang `BAT0` during [`crate::init`].

use alloc::{format, string::String};
use core::fmt::Write;

#[cfg(ktest)]
use crate::convert::Capacity;
use crate::convert::{BatterySnapshot, ChargeStatus, capacity_to_micro};

/// Linux `POWER_SUPPLY_TYPE_BATTERY` text.
pub const TYPE: &str = "Battery";

/// The pack answered at least once. A node with `present=0` is how
/// Linux says "slot empty"; we do not create the node in that case.
pub const PRESENT: &str = "1";

/// Snapshot has no chemistry word. Inventing `Li-ion` would be a
/// guess: SBS `Chemistry()` (0x22) is not sampled.
pub const TECHNOLOGY_UNKNOWN: &str = "Unknown";

/// Files that do not depend on `CAPACITY_MODE`.
const COMMON_ATTRS: [&str; 8] = [
    "type",
    "present",
    "status",
    "capacity",
    "voltage_now",
    "current_now",
    "temp",
    "technology",
];

/// Linux `power_supply_status_text[]`. The serial log uses
/// [`ChargeStatus::as_str`] (`not_charging`); sysfs does not.
pub fn status_text(status: ChargeStatus) -> &'static str {
    match status {
        ChargeStatus::Charging => "Charging",
        ChargeStatus::Discharging => "Discharging",
        ChargeStatus::NotCharging => "Not charging",
        ChargeStatus::Full => "Full",
        ChargeStatus::Unknown => "Unknown",
    }
}

/// Charge vs energy file names. Linux's SBS driver exposes one pair;
/// both at once makes UPower pick an arbitrary unit.
pub fn capacity_pair(mode_watt: bool) -> [&'static str; 2] {
    if mode_watt {
        ["energy_now", "energy_full"]
    } else {
        ["charge_now", "charge_full"]
    }
}

/// Attribute names published for one capacity mode, `uevent` excluded.
pub fn attr_names(mode_watt: bool) -> [&'static str; 10] {
    let [now, full] = capacity_pair(mode_watt);
    [
        COMMON_ATTRS[0],
        COMMON_ATTRS[1],
        COMMON_ATTRS[2],
        COMMON_ATTRS[3],
        COMMON_ATTRS[4],
        COMMON_ATTRS[5],
        COMMON_ATTRS[6],
        COMMON_ATTRS[7],
        now,
        full,
    ]
}

/// One sysfs attribute: value plus newline, matching Linux `sprintf`.
pub fn format_attr(snap: BatterySnapshot, name: &str) -> Option<String> {
    match name {
        "type" => Some(line_str(TYPE)),
        "present" => Some(line_str(PRESENT)),
        "status" => Some(line_str(status_text(snap.status))),
        "capacity" => Some(line_u32(u32::from(snap.soc_percent))),
        "voltage_now" => Some(line_u32(snap.voltage_uv)),
        "current_now" => Some(line_i32(snap.current_ua)),
        "temp" => Some(line_i32(snap.temp_deci_celsius())),
        "technology" => Some(line_str(TECHNOLOGY_UNKNOWN)),
        // File names are pinned at publication. If the gauge changes mode,
        // reject the old pair instead of labelling energy as charge or vice versa.
        "charge_now" if !snap.capacity_mode_watt => {
            Some(line_u32(capacity_to_micro(snap.remaining)))
        }
        "energy_now" if snap.capacity_mode_watt => {
            Some(line_u32(capacity_to_micro(snap.remaining)))
        }
        "charge_full" if !snap.capacity_mode_watt => Some(line_u32(capacity_to_micro(snap.full))),
        "energy_full" if snap.capacity_mode_watt => Some(line_u32(capacity_to_micro(snap.full))),
        _ => None,
    }
}

/// Linux `power_supply_uevent`: `POWER_SUPPLY_NAME` plus one
/// `POWER_SUPPLY_<ATTR>` per property. `libudev` refuses a device
/// with no `uevent` file (`sd_device_new_from_syspath`); the keys
/// are what a udev-less reader would use instead of the attr files.
pub fn format_uevent(snap: BatterySnapshot) -> String {
    let mut text = String::from("POWER_SUPPLY_NAME=BAT0\n");
    for name in attr_names(snap.capacity_mode_watt) {
        let Some(value) = format_attr(snap, name) else {
            continue;
        };
        let value = value.trim_end_matches('\n');
        // Writing into a `String` cannot fail.
        let _ = writeln!(text, "POWER_SUPPLY_{}={}", name.to_ascii_uppercase(), value);
    }
    text
}

fn line_str(value: &str) -> String {
    format!("{}\n", value)
}

fn line_u32(value: u32) -> String {
    format!("{}\n", value)
}

fn line_i32(value: i32) -> String {
    format!("{}\n", value)
}

#[cfg(ktest)]
pub fn ktest_discharging_mah() -> BatterySnapshot {
    // Sample with 27.0 C, a signed discharge current and charge in mAh.
    BatterySnapshot {
        soc_percent: 88,
        voltage_uv: 8_290_000,
        current_ua: -130_000,
        temp_decikelvin: 3001,
        status: ChargeStatus::Discharging,
        remaining: Capacity::MilliAmpHour(4309),
        full: Capacity::MilliAmpHour(4918),
        time_to_empty_min: Some(2394),
        time_to_full_min: None,
        battery_status_raw: crate::convert::BATTERY_DISCHARGING,
        capacity_mode_watt: false,
    }
}

#[cfg(ktest)]
pub fn ktest_not_charging_mwh() -> BatterySnapshot {
    BatterySnapshot {
        soc_percent: 100,
        voltage_uv: 8_400_000,
        current_ua: 0,
        temp_decikelvin: 3001,
        status: ChargeStatus::NotCharging,
        remaining: Capacity::MilliWattHour(36_000),
        full: Capacity::MilliWattHour(36_000),
        time_to_empty_min: None,
        time_to_full_min: None,
        battery_status_raw: 0,
        capacity_mode_watt: true,
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Sysfs status is the Linux title-case table, not the serial
    /// log's snake_case. `Not charging` has a space; UPower matches
    /// that literal.
    #[ktest]
    fn status_matches_linux_power_supply_text() {
        assert_eq!(status_text(ChargeStatus::Charging), "Charging");
        assert_eq!(status_text(ChargeStatus::Discharging), "Discharging");
        assert_eq!(status_text(ChargeStatus::NotCharging), "Not charging");
        assert_eq!(status_text(ChargeStatus::Full), "Full");
        assert_eq!(status_text(ChargeStatus::Unknown), "Unknown");
        assert_ne!(
            status_text(ChargeStatus::NotCharging),
            ChargeStatus::NotCharging.as_str()
        );
        assert!(!status_text(ChargeStatus::NotCharging).contains('_'));
    }

    /// mAh → `charge_*` in µAh. mWh → `energy_*` in µWh. The two
    /// pairs must not be advertised together.
    #[ktest]
    fn capacity_mode_selects_exactly_one_pair() {
        let mah = attr_names(false);
        assert!(mah.contains(&"charge_now"));
        assert!(mah.contains(&"charge_full"));
        assert!(!mah.contains(&"energy_now"));
        assert!(!mah.contains(&"energy_full"));

        let mwh = attr_names(true);
        assert!(mwh.contains(&"energy_now"));
        assert!(mwh.contains(&"energy_full"));
        assert!(!mwh.contains(&"charge_now"));
        assert!(!mwh.contains(&"charge_full"));

        assert_ne!(capacity_pair(false), capacity_pair(true));
    }

    /// Board-log snapshot → the files UPower was told to read.
    #[ktest]
    fn discharging_mah_formats_linux_units() {
        let snap = ktest_discharging_mah();
        assert_eq!(format_attr(snap, "type").unwrap(), "Battery\n");
        assert_eq!(format_attr(snap, "present").unwrap(), "1\n");
        assert_eq!(format_attr(snap, "status").unwrap(), "Discharging\n");
        assert_eq!(format_attr(snap, "capacity").unwrap(), "88\n");
        assert_eq!(format_attr(snap, "voltage_now").unwrap(), "8290000\n");
        assert_eq!(format_attr(snap, "current_now").unwrap(), "-130000\n");
        assert_eq!(format_attr(snap, "temp").unwrap(), "270\n");
        assert_eq!(format_attr(snap, "technology").unwrap(), "Unknown\n");
        assert_eq!(format_attr(snap, "charge_now").unwrap(), "4309000\n");
        assert_eq!(format_attr(snap, "charge_full").unwrap(), "4918000\n");
        assert!(format_attr(snap, "scope").is_none());
        assert!(format_attr(snap, "current_now").unwrap().starts_with('-'));
    }

    /// Zero current + `Not charging` (space), energy pair in µWh,
    /// temperature still 0.1 °C.
    #[ktest]
    fn not_charging_mwh_formats_energy_pair() {
        let snap = ktest_not_charging_mwh();
        assert_eq!(format_attr(snap, "status").unwrap(), "Not charging\n");
        assert_eq!(format_attr(snap, "current_now").unwrap(), "0\n");
        assert_eq!(format_attr(snap, "energy_now").unwrap(), "36000000\n");
        assert_eq!(format_attr(snap, "energy_full").unwrap(), "36000000\n");
        assert_eq!(format_attr(snap, "temp").unwrap(), "270\n");
        assert_ne!(format_attr(snap, "status").unwrap(), "Not_charging\n");
        assert_ne!(format_attr(snap, "status").unwrap(), "not_charging\n");
    }

    /// uevent repeats the live properties so a reader that only
    /// parses that file still sees the same numbers as the attr files.
    #[ktest]
    fn uevent_carries_power_supply_keys() {
        let text = format_uevent(ktest_discharging_mah());
        assert!(text.starts_with("POWER_SUPPLY_NAME=BAT0\n"));
        assert!(text.contains("POWER_SUPPLY_STATUS=Discharging\n"));
        assert!(text.contains("POWER_SUPPLY_CAPACITY=88\n"));
        assert!(text.contains("POWER_SUPPLY_CURRENT_NOW=-130000\n"));
        assert!(text.contains("POWER_SUPPLY_CHARGE_NOW=4309000\n"));
        assert!(!text.contains("POWER_SUPPLY_ENERGY_NOW="));
    }
}
