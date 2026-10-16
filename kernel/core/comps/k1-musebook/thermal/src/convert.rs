// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 thermal sensor conversion and trip thresholds.

//! Pure conversions taken from the vendor BSP / K1 manual.
//!
//! Kept free of MMIO so the ktests can pin the numbers without a board.

/// Celsius = raw − this. BSP `drivers/thermal/k1x-thermal.h`
/// `TEMPERATURE_OFFSET` and `k1x_thermal_get_temp`.
pub const TEMPERATURE_OFFSET: i32 = 278;

/// First passive trip on both clusters, millidegrees in
/// `k1-x_thermal_cooling.dtsi` `cls0-trip-point0` / `cls1-trip-point0`.
pub const TRIP_PASSIVE_0_C: i32 = 75;

/// Last passive trip (`cls0-trip-point3`), 105 °C. Above this Linux
/// asks cpufreq for the deepest cooling state. The K1 cpufreq policy uses
/// this trip for its minimum-frequency cap.
pub const TRIP_PASSIVE_3_C: i32 = 105;

/// Commented-out hardware emergency-reboot threshold in `k1-x.dtsi`
/// (`emergent_reboot_threshold = <110>`). The write path in
/// `k1x-thermal.c` is `#if 0`; silicon default is unknown until we
/// read `REG_EMERGENT_REBOOT_TEMP_THR` on the board.
pub const EMERGENT_REBOOT_C: i32 = 110;

/// `type = "critical"` trip on both clusters (115 °C). Linux then calls
/// `thermal_zone_device_critical` → `hw_protection_shutdown` (orderly
/// poweroff), not a hardware reset. The K1 cpufreq sampler logs this trip;
/// automatic critical-temperature shutdown is not implemented.
pub const TRIP_CRITICAL_C: i32 = 115;

/// 16-bit SoC WDT input clock. Manual §10.1.3 and U-Boot
/// `spacemit_wdt.c` `WDT_CLK_FREQ`.
pub const WDT_HZ: u32 = 256;

/// Linux `SPACEMIT_WATCHDOG_MAX_TIMEOUT`: the match register is 16 bits
/// and the driver programs `timeout << 8`, so 255 s is the largest
/// value that still fits (`255 << 8 == 0xff00 ≤ 0xffff`).
pub const WDT_MAX_TIMEOUT_S: u32 = 255;

/// SPM8821 / P1 PMIC WDT scale field max. U-Boot `spacemit_pmic_wdt.c`
/// and P1 datasheet table 7-72: 1 / 4 / 8 / **16** s.
pub const PMIC_WDT_MAX_TIMEOUT_S: u32 = 16;

/// BJT data register → °C. Inverse of the BSP `set_trips` encode.
pub fn raw_to_celsius(raw: u16) -> i32 {
    i32::from(raw) - TEMPERATURE_OFFSET
}

/// °C → the 16-bit field written to a TSEN threshold register.
pub fn celsius_to_raw(celsius: i32) -> u16 {
    (celsius + TEMPERATURE_OFFSET) as u16
}

/// Seconds → WDT match ticks. Linux `spa_wdt_set_timeout`:
/// `tick = timeout << DEFAULT_SHIFT` with `DEFAULT_SHIFT = 8`
/// (256 Hz). Same as `seconds * WDT_HZ`.
pub fn wdt_seconds_to_match_ticks(seconds: u32) -> u32 {
    seconds << 8
}

/// Match ticks → whole seconds (truncates the sub-second remainder).
pub fn wdt_match_ticks_to_seconds(ticks: u32) -> u32 {
    ticks >> 8
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// The BSP subtracts 278 and then multiplies by 1000 for millidegrees.
    /// 278 raw is therefore 0 °C, not an ADC mid-scale.
    #[ktest]
    fn raw_278_is_zero_celsius() {
        assert_eq!(raw_to_celsius(278), 0);
        assert_eq!(celsius_to_raw(0), 278);
    }

    /// Cooling-dtsi trip 0 is 75 °C; the encode in `set_trips` is
    /// `celsius + 278`.
    #[ktest]
    fn cluster_passive0_is_raw_353() {
        assert_eq!(celsius_to_raw(TRIP_PASSIVE_0_C), 353);
        assert_eq!(raw_to_celsius(353), 75);
    }

    /// Critical software trip is 115 °C, not the commented 110 °C
    /// emergency-reboot figure. Mixing them up would make a "we hit
    /// critical" line lie about which threshold fired.
    #[ktest]
    fn critical_115_is_not_the_commented_emergent_110() {
        assert_eq!(celsius_to_raw(TRIP_CRITICAL_C), 393);
        assert_eq!(raw_to_celsius(393), 115);
        assert_eq!(celsius_to_raw(EMERGENT_REBOOT_C), 388);
        assert_eq!(celsius_to_raw(TRIP_PASSIVE_3_C), 383);
        assert_ne!(
            celsius_to_raw(TRIP_CRITICAL_C),
            celsius_to_raw(EMERGENT_REBOOT_C)
        );
    }

    /// Linux programs `timeout << 8`. 100 s (the driver's expire time)
    /// must be 0x6400, and the 16-bit match register cannot represent
    /// the 28 minutes seen on the board (~1680 s).
    #[ktest]
    fn soc_wdt_cannot_span_twenty_eight_minutes() {
        assert_eq!(wdt_seconds_to_match_ticks(100), 0x6400);
        assert_eq!(wdt_match_ticks_to_seconds(0xffff), WDT_MAX_TIMEOUT_S);
        assert!(WDT_MAX_TIMEOUT_S < 28 * 60);
        assert!(PMIC_WDT_MAX_TIMEOUT_S < WDT_MAX_TIMEOUT_S);
        assert_eq!(wdt_seconds_to_match_ticks(WDT_MAX_TIMEOUT_S), 0xff00);
    }
}
