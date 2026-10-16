// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SBS battery register decoding and unit conversion.

//! SBS word → SI-unit conversions from the vendor `sbs-battery.c`.
//!
//! Kept free of I2C so the ktests can pin the numbers without a board.

/// SBS `BatteryMode()` command. `sbs-battery.c` `BATTERY_MODE_OFFSET`.
pub const CMD_BATTERY_MODE: u8 = 0x03;
/// SBS `Temperature()` — 0.1 K. `sbs_data[REG_TEMPERATURE]`.
pub const CMD_TEMPERATURE: u8 = 0x08;
/// SBS `Voltage()` — mV. `sbs_data[REG_VOLTAGE]`.
pub const CMD_VOLTAGE: u8 = 0x09;
/// SBS `Current()` — signed mA. `sbs_data[REG_CURRENT_NOW]`.
pub const CMD_CURRENT: u8 = 0x0a;
/// SBS `RelativeStateOfCharge()` — percent. `sbs_data[REG_CAPACITY]`.
pub const CMD_SOC: u8 = 0x0d;
/// SBS `RemainingCapacity()` — mAh or 10 mWh. `sbs_data[REG_REMAINING_CAPACITY]`.
pub const CMD_REMAINING: u8 = 0x0f;
/// SBS `FullChargeCapacity()` — same units as remaining. `sbs_data[REG_FULL_CHARGE_CAPACITY]`.
pub const CMD_FULL: u8 = 0x10;
/// SBS `RunTimeToEmpty()` — minutes. `sbs_data[REG_TIME_TO_EMPTY_NOW]`.
pub const CMD_TIME_TO_EMPTY: u8 = 0x11;
/// SBS `AverageTimeToFull()` — minutes. `sbs_data[REG_TIME_TO_FULL_AVG]`.
pub const CMD_TIME_TO_FULL: u8 = 0x13;
/// SBS `BatteryStatus()`. `sbs_data[REG_STATUS]`.
pub const CMD_STATUS: u8 = 0x16;

/// `BATTERY_MODE_CAPACITY_MASK` — `sbs-battery.c` bit 15.
pub const BATTERY_MODE_CAPACITY_MASK: u16 = 1 << 15;

/// `BATTERY_INITIALIZED` — `sbs-battery.c`.
pub const BATTERY_INITIALIZED: u16 = 0x80;
/// `BATTERY_DISCHARGING` — `sbs-battery.c`.
pub const BATTERY_DISCHARGING: u16 = 0x40;
/// `BATTERY_FULL_CHARGED` — `sbs-battery.c`.
pub const BATTERY_FULL_CHARGED: u16 = 0x20;
/// `BATTERY_FULL_DISCHARGED` — `sbs-battery.c` (capacity_level only).
pub const BATTERY_FULL_DISCHARGED: u16 = 0x10;

/// `BASE_UNIT_CONVERSION` in `sbs_unit_adjustment`.
pub const BASE_UNIT_CONVERSION: i32 = 1000;
/// `BATTERY_MODE_CAP_MULT_WATT` = 10 × 1000. 10 mWh → µWh.
pub const BATTERY_MODE_CAP_MULT_WATT: i32 = 10 * BASE_UNIT_CONVERSION;
/// `TEMP_KELVIN_TO_CELSIUS` in `sbs_unit_adjustment`: 0.1 K → 0.1 °C.
pub const TEMP_KELVIN_TO_CELSIUS: i32 = 2731;
/// SBS `TimeToEmpty`/`TimeToFull` sentinel: not (dis)charging. Linux
/// does not filter this; the spec does.
pub const SBS_TIME_UNKNOWN: u16 = 65535;

/// How remaining / full capacity should be labelled.
///
/// `CAPACITY_MODE` (`BatteryMode` bit 15) selects the *same* two
/// registers (0x0F / 0x10). Linux writes the bit to force a unit, then
/// restores it (`sbs_get_battery_capacity`). We only *read* the bit:
/// writing it also touches charger-broadcast enable (bit 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capacity {
    /// `CAPACITY_MODE=0`. The SBS word is already mAh.
    MilliAmpHour(u16),
    /// `CAPACITY_MODE=1`. The SBS word is 10 mWh; this holds mWh (`raw * 10`).
    MilliWattHour(u32),
}

/// Linux `POWER_SUPPLY_STATUS_*` after `sbs_get_battery_property` +
/// `sbs_status_correct`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChargeStatus {
    Charging,
    Discharging,
    Full,
    NotCharging,
    Unknown,
}

impl ChargeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Full => "full",
            Self::NotCharging => "not_charging",
            Self::Unknown => "unknown",
        }
    }
}

/// One SBS sample, units in the field names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatterySnapshot {
    /// `RelativeStateOfCharge`, clamped to 0..=100 like Linux.
    pub soc_percent: u8,
    /// `Voltage()` mV × 1000.
    pub voltage_uv: u32,
    /// `Current()` mA × 1000. Discharge is **negative** (`s16` in the BSP).
    pub current_ua: i32,
    /// `Temperature()` as 0.1 K. Subtract [`TEMP_KELVIN_TO_CELSIUS`] for 0.1 °C.
    pub temp_decikelvin: u16,
    pub status: ChargeStatus,
    pub remaining: Capacity,
    pub full: Capacity,
    /// Minutes. `None` when the gauge reports [`SBS_TIME_UNKNOWN`].
    pub time_to_empty_min: Option<u16>,
    pub time_to_full_min: Option<u16>,
    /// Raw `BatteryStatus` (0x16).
    pub battery_status_raw: u16,
    /// `BatteryMode` bit 15 set → remaining/full are energy.
    pub capacity_mode_watt: bool,
}

impl BatterySnapshot {
    /// 0.1 °C, Linux `POWER_SUPPLY_PROP_TEMP`.
    pub fn temp_deci_celsius(self) -> i32 {
        temp_decikelvin_to_deci_celsius(self.temp_decikelvin)
    }

    /// Truncated °C for the `[k1-batt]` line.
    pub fn temp_celsius_trunc(self) -> i32 {
        deci_celsius_to_celsius_trunc(self.temp_deci_celsius())
    }
}

pub fn battery_mode_is_watt(mode: u16) -> bool {
    mode & BATTERY_MODE_CAPACITY_MASK != 0
}

pub fn temp_decikelvin_to_deci_celsius(decikelvin: u16) -> i32 {
    i32::from(decikelvin) - TEMP_KELVIN_TO_CELSIUS
}

pub fn deci_celsius_to_celsius_trunc(deci_c: i32) -> i32 {
    deci_c / 10
}

/// SBS current word → µA. The register is signed; the BSP casts `s16`
/// before `* 1000`.
pub fn current_word_to_ua(word: u16) -> i32 {
    i32::from(word as i16) * BASE_UNIT_CONVERSION
}

pub fn voltage_mv_to_uv(mv: u16) -> u32 {
    u32::from(mv) * BASE_UNIT_CONVERSION as u32
}

pub fn capacity_from_raw(raw: u16, mode_watt: bool) -> Capacity {
    if mode_watt {
        Capacity::MilliWattHour(u32::from(raw) * 10)
    } else {
        Capacity::MilliAmpHour(raw)
    }
}

/// Linux `sbs_unit_adjustment` for charge (µAh) or energy (µWh).
pub fn capacity_to_micro(cap: Capacity) -> u32 {
    match cap {
        Capacity::MilliAmpHour(mah) => u32::from(mah) * BASE_UNIT_CONVERSION as u32,
        Capacity::MilliWattHour(mwh) => mwh * BASE_UNIT_CONVERSION as u32,
    }
}

/// `sbs_get_battery_property` STATUS, then `sbs_status_correct`.
///
/// `FULL_DISCHARGED` is *not* used here — Linux only consults it for
/// `CAPACITY_LEVEL`. Current sign only rewrites Full, and a zero
/// current only rewrites a non-Full status.
pub fn decode_charge_status(battery_status: u16, current_ma: i16) -> ChargeStatus {
    let mut status = if battery_status & BATTERY_FULL_CHARGED != 0 {
        ChargeStatus::Full
    } else if battery_status & BATTERY_DISCHARGING != 0 {
        ChargeStatus::Discharging
    } else {
        ChargeStatus::Charging
    };

    if status != ChargeStatus::Full && current_ma == 0 {
        status = ChargeStatus::NotCharging;
    }
    if status == ChargeStatus::Full {
        if current_ma > 0 {
            status = ChargeStatus::Charging;
        } else if current_ma < 0 {
            status = ChargeStatus::Discharging;
        }
    }
    status
}

/// Linux clamps `>100` and forces 100 when Full, or when SoC ≥ 98 and
/// idle (`NOT_CHARGING`).
pub fn adjust_soc(raw_soc: i32, status: ChargeStatus) -> u8 {
    if status == ChargeStatus::Full {
        return 100;
    }
    if raw_soc >= 98 && status == ChargeStatus::NotCharging {
        return 100;
    }
    raw_soc.clamp(0, 100) as u8
}

pub fn sbs_time_minutes(raw: u16) -> Option<u16> {
    if raw == SBS_TIME_UNKNOWN {
        None
    } else {
        Some(raw)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Linux subtracts 2731 from the 0.1 K word. 2731 is 0.0 °C, not
    /// an ADC midpoint, and 3041 is the 31 °C in the `[k1-batt]` example.
    #[ktest]
    fn sbs_temp_0p1k_to_celsius() {
        assert_eq!(temp_decikelvin_to_deci_celsius(2731), 0);
        assert_eq!(deci_celsius_to_celsius_trunc(0), 0);
        assert_eq!(temp_decikelvin_to_deci_celsius(3041), 310);
        assert_eq!(deci_celsius_to_celsius_trunc(310), 31);
        assert_eq!(temp_decikelvin_to_deci_celsius(0), -2731);
        assert_ne!(TEMP_KELVIN_TO_CELSIUS, 273);
    }

    /// `sbs_data[REG_CURRENT_NOW]` range is signed. 0xFB50 is −1200 mA
    /// (the example's −1.20 A), not an unsigned 64 A discharge.
    #[ktest]
    fn sbs_current_is_signed_discharge_negative() {
        assert_eq!(current_word_to_ua(0xfb50), -1_200_000);
        assert_eq!(current_word_to_ua(0x04b0), 1_200_000);
        assert_eq!(current_word_to_ua(0), 0);
        assert_eq!(current_word_to_ua(0x8000), -32_768_000);
        assert_ne!(current_word_to_ua(0xfb50), i32::from(0xfb50u16) * 1000);
    }

    /// Voltage is unsigned mV × 1000. 12340 mV is the example's 12.34 V.
    #[ktest]
    fn sbs_voltage_mv_to_uv() {
        assert_eq!(voltage_mv_to_uv(12340), 12_340_000);
        assert_eq!(voltage_mv_to_uv(0), 0);
    }

    /// Bit 15 off → mAh, `* 1000` µAh. Bit 15 on → 10 mWh word, printed
    /// as mWh (`* 10`) and exported as µWh (`* 10000` from the raw word).
    #[ktest]
    fn capacity_mode_switches_mah_vs_mwh() {
        assert!(!battery_mode_is_watt(0));
        assert!(battery_mode_is_watt(BATTERY_MODE_CAPACITY_MASK));
        assert_eq!(capacity_from_raw(4200, false), Capacity::MilliAmpHour(4200));
        assert_eq!(capacity_to_micro(Capacity::MilliAmpHour(4200)), 4_200_000);
        assert_eq!(capacity_from_raw(420, true), Capacity::MilliWattHour(4200));
        assert_eq!(capacity_to_micro(Capacity::MilliWattHour(4200)), 4_200_000);
        assert_ne!(
            capacity_from_raw(4200, false),
            capacity_from_raw(4200, true)
        );
    }

    /// STATUS uses FULL_CHARGED then DISCHARGING then Charging.
    /// `sbs_status_correct` only rewrites Full (by current sign) and
    /// non-Full + zero current. A discharging bit with *positive*
    /// current stays discharging — the BSP does not flip it.
    #[ktest]
    fn battery_status_bits_and_current_correct() {
        assert_eq!(
            decode_charge_status(BATTERY_DISCHARGING, -500),
            ChargeStatus::Discharging
        );
        assert_eq!(
            decode_charge_status(BATTERY_DISCHARGING, 500),
            ChargeStatus::Discharging
        );
        assert_eq!(
            decode_charge_status(BATTERY_FULL_CHARGED, 0),
            ChargeStatus::Full
        );
        assert_eq!(
            decode_charge_status(BATTERY_FULL_CHARGED, 100),
            ChargeStatus::Charging
        );
        assert_eq!(
            decode_charge_status(BATTERY_FULL_CHARGED, -100),
            ChargeStatus::Discharging
        );
        assert_eq!(decode_charge_status(0, 0), ChargeStatus::NotCharging);
        assert_eq!(decode_charge_status(0, 100), ChargeStatus::Charging);
        assert_eq!(
            decode_charge_status(BATTERY_FULL_DISCHARGED, -100),
            ChargeStatus::Charging
        );
        assert_ne!(
            decode_charge_status(BATTERY_FULL_CHARGED, 0),
            ChargeStatus::NotCharging
        );
    }

    /// Full → 100. Idle + ≥98 → 100. 105 is clamped. Discharging 98
    /// stays 98 (the idle-only bump).
    #[ktest]
    fn soc_clamp_and_full_bump() {
        assert_eq!(adjust_soc(87, ChargeStatus::Discharging), 87);
        assert_eq!(adjust_soc(105, ChargeStatus::Discharging), 100);
        assert_eq!(adjust_soc(50, ChargeStatus::Full), 100);
        assert_eq!(adjust_soc(98, ChargeStatus::NotCharging), 100);
        assert_eq!(adjust_soc(98, ChargeStatus::Discharging), 98);
        assert_eq!(adjust_soc(-1, ChargeStatus::Charging), 0);
    }

    /// 65535 is the SBS "not calculating" sentinel. 203 is the example.
    /// 0 minutes is a real empty-now reading, not unknown.
    #[ktest]
    fn time_to_empty_sentinel_is_65535() {
        assert_eq!(sbs_time_minutes(203), Some(203));
        assert_eq!(sbs_time_minutes(0), Some(0));
        assert_eq!(sbs_time_minutes(SBS_TIME_UNKNOWN), None);
        assert_ne!(sbs_time_minutes(SBS_TIME_UNKNOWN), Some(SBS_TIME_UNKNOWN));
    }
}
