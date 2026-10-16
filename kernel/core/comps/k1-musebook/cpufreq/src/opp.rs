// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: CPU operating points and regulator voltage requirements.

//! K1 CPU OPP table, copied from the vendor DT.
//!
//! Source of truth:
//! - BSP `arch/riscv/boot/dts/spacemit/k1-x_opp_table.dtsi` `clst_core_opp_table0`
//!   (K1 uses table 0; tables 1/2 are M1 dro-bin variants — see
//!   `drivers/cpufreq/spacemit-cpufreq.c` `FREQ_TABLE_*` / `FILTER_POINTS_*`).
//! - Mainline `arch/riscv/boot/dts/spacemit/k1-opp.dtsi` agrees on the
//!   non-turbo points and voltages; it omits 1.8 GHz.
//!
//! Voltage is `clst-supply = <&dcdc_1>` (SPM8821 DCDC1). Units here are
//! kHz and mV so sysfs and the regulator math share one table.

/// One operating point. Frequencies are kHz (Linux cpufreq sysfs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Opp {
    pub khz: u32,
    pub mv: u32,
    /// `turbo-mode` in the DT. K1 software-caps below this
    /// (`K1_MAX_FREQ_LIMITATION` = 1.6 GHz).
    pub turbo: bool,
    /// ACE / TCM target in kHz (`ace-hz` / `tcm-hz` in the DT). `None`
    /// means "leave the divider as core/2", which is the PRECHANGE
    /// safe value in `spacemit_processor_notifier`.
    pub ace_khz: Option<u32>,
    pub tcm_khz: Option<u32>,
}

/// K1 (not M1) OPP table: `clst_core_opp_table0`.
///
/// 1.8 GHz is present in the DT as `turbo-mode` but
/// `spacemit-cpufreq.c` `K1_MAX_FREQ_LIMITATION` (1600000 kHz) drops it
/// unless `product-id`/`wafer-id` decode as M1 (`PRODUCT_ID_M1`).
pub const K1_OPP_TABLE0: &[Opp] = &[
    Opp {
        khz: 614_400,
        mv: 950,
        turbo: false,
        ace_khz: Some(307_200),
        tcm_khz: Some(307_200),
    },
    Opp {
        khz: 819_000,
        mv: 950,
        turbo: false,
        ace_khz: Some(409_500),
        tcm_khz: Some(409_500),
    },
    Opp {
        khz: 1_000_000,
        mv: 950,
        turbo: false,
        ace_khz: Some(500_000),
        tcm_khz: Some(500_000),
    },
    Opp {
        khz: 1_228_800,
        mv: 950,
        turbo: false,
        ace_khz: Some(614_400),
        tcm_khz: Some(614_400),
    },
    Opp {
        khz: 1_600_000,
        mv: 1_050,
        turbo: false,
        ace_khz: Some(800_000),
        tcm_khz: Some(800_000),
    },
    // `opp1800000000` / `opp-microvolt = <1160000>` / `turbo-mode`.
    // Retained to recognize an existing firmware clock; never advertised on K1.
    Opp {
        khz: 1_800_000,
        mv: 1_160,
        turbo: true,
        ace_khz: Some(900_000),
        tcm_khz: Some(900_000),
    },
];

/// `spacemit-cpufreq.c` `K1_MAX_FREQ_LIMITATION`.
pub(crate) const K1_MAX_KHZ: u32 = 1_600_000;

/// `spacemit-cpufreq.c` `STABLE_FREQUENCY` (1.2 GHz). Used as the
/// conservative default setspeed. PLL3 retuning parks on PLL1/4 instead.
pub const STABLE_KHZ: u32 = 1_228_800;

/// `clock-latency-ns = <200000>` on every OPP.
pub(crate) const CLOCK_LATENCY_NS: u32 = 200_000;

/// OPPs a K1 policy may advertise (turbo stripped).
pub(crate) fn k1_available() -> impl DoubleEndedIterator<Item = &'static Opp> {
    K1_OPP_TABLE0
        .iter()
        .filter(|o| !o.turbo && o.khz <= K1_MAX_KHZ)
}

pub(crate) fn lookup(khz: u32) -> Option<&'static Opp> {
    K1_OPP_TABLE0.iter().find(|o| o.khz == khz)
}

/// Nearest OPP at or below `khz` that is allowed on K1.
pub(crate) fn floor_allowed(khz: u32) -> &'static Opp {
    k1_available()
        .rev()
        .find(|o| o.khz <= khz)
        .or_else(|| k1_available().next())
        .unwrap_or(&K1_OPP_TABLE0[0])
}

/// Lowest / highest allowed kHz.
pub fn allowed_min_khz() -> u32 {
    k1_available().map(|o| o.khz).min().unwrap_or(614_400)
}

pub fn allowed_max_khz() -> u32 {
    k1_available().map(|o| o.khz).max().unwrap_or(K1_MAX_KHZ)
}

pub(crate) fn is_allowed(khz: u32) -> bool {
    k1_available().any(|o| o.khz == khz)
}

/// Clamp `khz` into `[min, max]` then snap down to an allowed OPP.
pub(crate) fn clamp_to_allowed(khz: u32, min_khz: u32, max_khz: u32) -> u32 {
    let lo = min_khz.min(max_khz);
    let hi = max_khz.max(min_khz);
    let capped = khz.clamp(lo, hi);
    floor_allowed(capped).khz
}

/// Space-separated kHz list, Linux `scaling_available_frequencies`.
pub(crate) fn format_available() -> impl core::fmt::Display {
    AvailableFreqs
}

struct AvailableFreqs;

impl core::fmt::Display for AvailableFreqs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for opp in k1_available() {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{}", opp.khz)?;
        }
        Ok(())
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Table 0 voltages and the 1.6 GHz K1 cap come from the cited DT /
    /// `spacemit-cpufreq.c`. Mixing table 1's 1.8 GHz @ 1100 mV in here
    /// would under-volt a K1 bin.
    #[ktest]
    fn table0_matches_bsp_k1() {
        assert_eq!(lookup(614_400).unwrap().mv, 950);
        assert_eq!(lookup(819_000).unwrap().mv, 950);
        assert_eq!(lookup(1_000_000).unwrap().mv, 950);
        assert_eq!(lookup(1_228_800).unwrap().mv, 950);
        assert_eq!(lookup(1_600_000).unwrap().mv, 1_050);
        assert_eq!(lookup(1_800_000).unwrap().mv, 1_160);
        assert!(lookup(1_800_000).unwrap().turbo);
        assert!(!is_allowed(1_800_000));
        assert!(is_allowed(1_600_000));
        assert_eq!(allowed_max_khz(), 1_600_000);
        assert_eq!(allowed_min_khz(), 614_400);
    }

    #[ktest]
    fn clamp_snaps_down_to_an_opp() {
        assert_eq!(clamp_to_allowed(1_500_000, 614_400, 1_600_000), 1_228_800);
        assert_eq!(clamp_to_allowed(2_000_000, 614_400, 1_600_000), 1_600_000);
        assert_eq!(clamp_to_allowed(100_000, 614_400, 1_600_000), 614_400);
        assert_eq!(clamp_to_allowed(1_600_000, 614_400, 1_000_000), 1_000_000);
    }

    #[ktest]
    fn floor_of_stable_is_the_1g2_opp() {
        assert_eq!(floor_allowed(STABLE_KHZ).khz, 1_228_800);
        assert_eq!(floor_allowed(819_200).khz, 819_000);
    }
}
