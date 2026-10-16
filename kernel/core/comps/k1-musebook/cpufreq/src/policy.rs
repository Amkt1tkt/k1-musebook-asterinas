// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: CPU frequency governor and thermal-limit policy.

//! Governors, user min/max, thermal cap. Pure selection is ktested;
//! applying a target talks to [`crate::clock`] / [`crate::voltage`].

use crate::opp::{
    self, Opp, STABLE_KHZ, allowed_max_khz, allowed_min_khz, clamp_to_allowed, lookup,
};

/// Linux `scaling_governor` names we accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Governor {
    Performance,
    Powersave,
    Userspace,
}

impl Governor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Performance => "performance",
            Self::Powersave => "powersave",
            Self::Userspace => "userspace",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "performance" => Some(Self::Performance),
            "powersave" => Some(Self::Powersave),
            "userspace" => Some(Self::Userspace),
            _ => None,
        }
    }
}

/// Software view of one policy (both K1 clusters share dcdc1 + the same OPP).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub gov: Governor,
    pub min_khz: u32,
    pub max_khz: u32,
    pub setspeed_khz: u32,
    pub cur_khz: u32,
    pub thermal_cap_khz: u32,
}

impl Policy {
    pub fn conservative_default() -> Self {
        Self {
            gov: Governor::Userspace,
            min_khz: allowed_min_khz(),
            max_khz: allowed_max_khz(),
            setspeed_khz: STABLE_KHZ,
            cur_khz: STABLE_KHZ,
            thermal_cap_khz: allowed_max_khz(),
        }
    }

    pub fn target_khz(&self) -> u32 {
        let want = match self.gov {
            Governor::Performance => self.max_khz,
            Governor::Powersave => self.min_khz,
            Governor::Userspace => self.setspeed_khz,
        };
        let hw_max = self.max_khz.min(self.thermal_cap_khz);
        // Thermal cooling overrides a user minimum that is above the cap.
        // Keep the interval ordered: the generic clamp normalizes both bounds.
        let effective_min = self.min_khz.min(hw_max);
        clamp_to_allowed(want, effective_min, hw_max)
    }

    pub fn target_opp(&self) -> &'static Opp {
        lookup(self.target_khz()).unwrap_or_else(|| opp::floor_allowed(self.target_khz()))
    }
}

/// `k1-x_thermal_cooling.dtsi` cluster0 trips → cooling state → kHz cap.
///
/// Linux `cpufreq_cooling` state 0 is the highest OPP. With the 1.8 GHz
/// turbo stripped (K1), the six DT cooling states collapse to:
///
/// | trip | °C | states | cap (K1) |
/// |---|---|---|---|
/// | 0 | 75 | 0..0 | 1600 (no extra throttle) |
/// | 1 | 85 | 1..1 | 1228.8 |
/// | 2 | 95 | 2..3 | 1000 |
/// | 3 | 105 | 4..5 | 614.4 |
///
/// Hysteresis is 5 °C on every trip. `level` is the last applied step
/// (0 = none … 3 = min). Dropping a level requires `temp <= trip - hyst`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThermalStep {
    pub trip_mc: i32,
    pub hyst_mc: i32,
    pub cap_khz: u32,
}

/// Passive trips only. Critical 115 °C is logged by the sampler; we already
/// sit at min from trip 3.
pub(crate) const THERMAL_STEPS: &[ThermalStep] = &[
    ThermalStep {
        trip_mc: 75_000,
        hyst_mc: 5_000,
        cap_khz: 1_600_000,
    },
    ThermalStep {
        trip_mc: 85_000,
        hyst_mc: 5_000,
        cap_khz: 1_228_800,
    },
    ThermalStep {
        trip_mc: 95_000,
        hyst_mc: 5_000,
        cap_khz: 1_000_000,
    },
    ThermalStep {
        trip_mc: 105_000,
        hyst_mc: 5_000,
        cap_khz: 614_400,
    },
];

/// Critical trip in the same dtsi (`type = "critical"`). No reboot here.
pub const THERMAL_CRITICAL_MC: i32 = 115_000;

/// Advance / retreat `level` from a new sample. Returns the cap in kHz.
pub(crate) fn thermal_update(level: u8, temp_mc: i32) -> (u8, u32) {
    let mut lvl = level.min(THERMAL_STEPS.len() as u8);
    // Climb while the next trip is crossed.
    while (lvl as usize) < THERMAL_STEPS.len() && temp_mc >= THERMAL_STEPS[lvl as usize].trip_mc {
        lvl += 1;
    }
    // Drop while the current trip has fallen by its hysteresis.
    while lvl > 0 {
        let step = &THERMAL_STEPS[(lvl as usize) - 1];
        if temp_mc <= step.trip_mc - step.hyst_mc {
            lvl -= 1;
        } else {
            break;
        }
    }
    let cap = if lvl == 0 {
        allowed_max_khz()
    } else {
        THERMAL_STEPS[(lvl as usize) - 1].cap_khz
    };
    (lvl, cap)
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn userspace_obeys_setspeed_inside_min_max() {
        let mut p = Policy::conservative_default();
        p.gov = Governor::Userspace;
        p.setspeed_khz = 1_000_000;
        assert_eq!(p.target_khz(), 1_000_000);
        p.setspeed_khz = 1_800_000;
        assert_eq!(p.target_khz(), 1_600_000);
        p.min_khz = 1_000_000;
        p.max_khz = 1_228_800;
        p.setspeed_khz = 614_400;
        assert_eq!(p.target_khz(), 1_000_000);
    }

    #[ktest]
    fn performance_and_powersave() {
        let mut p = Policy::conservative_default();
        p.gov = Governor::Performance;
        assert_eq!(p.target_khz(), 1_600_000);
        p.gov = Governor::Powersave;
        assert_eq!(p.target_khz(), 614_400);
        p.thermal_cap_khz = 1_000_000;
        p.gov = Governor::Performance;
        assert_eq!(p.target_khz(), 1_000_000);
    }

    #[ktest]
    fn thermal_cap_overrides_user_minimum_for_every_governor() {
        let mut p = Policy::conservative_default();
        p.min_khz = 1_600_000;
        p.max_khz = 1_600_000;
        p.setspeed_khz = 1_600_000;
        for gov in [
            Governor::Performance,
            Governor::Powersave,
            Governor::Userspace,
        ] {
            p.gov = gov;
            for step in THERMAL_STEPS {
                p.thermal_cap_khz = step.cap_khz;
                assert_eq!(p.target_khz(), step.cap_khz);
            }
            p.thermal_cap_khz = allowed_max_khz();
            assert_eq!(p.target_khz(), p.min_khz);
        }
    }

    #[ktest]
    fn governor_parse_roundtrip() {
        assert_eq!(
            Governor::parse("performance").unwrap().as_str(),
            "performance"
        );
        assert_eq!(Governor::parse("  powersave\n"), Some(Governor::Powersave));
        assert_eq!(Governor::parse("ondemand"), None);
    }

    /// 80 °C sits between trip0 and trip1: after climbing to level 1
    /// (75 °C) we stay there until 70 °C (75 − 5).
    #[ktest]
    fn thermal_hysteresis_holds_a_level() {
        let (l, cap) = thermal_update(0, 80_000);
        assert_eq!(l, 1);
        assert_eq!(cap, 1_600_000);
        let (l, cap) = thermal_update(l, 72_000);
        assert_eq!(l, 1);
        assert_eq!(cap, 1_600_000);
        let (l, cap) = thermal_update(l, 70_000);
        assert_eq!(l, 0);
        assert_eq!(cap, 1_600_000);
    }

    #[ktest]
    fn thermal_ladder_matches_dtsi() {
        assert_eq!(thermal_update(0, 86_000), (2, 1_228_800));
        assert_eq!(thermal_update(0, 96_000), (3, 1_000_000));
        assert_eq!(thermal_update(0, 106_000), (4, 614_400));
        // Coming down from min: 105−5 = 100 °C releases trip 3.
        assert_eq!(thermal_update(4, 100_000), (3, 1_000_000));
    }
}
