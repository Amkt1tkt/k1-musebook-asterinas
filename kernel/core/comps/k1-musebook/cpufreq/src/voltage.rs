// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 DVFS clock / voltage helpers.

//! SPM8821 DCDC1 (CPU cluster rail) voltage helpers.
//!
//! Register and range from
//! `include/linux/mfd/spacemit/spm8821.h`:
//! - `SPM8821_BUCK1_VSEL_REG` = 0x48
//! - `SPM8821_BUCK_VSEL_MASK` = 0xff
//! - `SPM8821_BUCK_LINER_RANGE`:
//!   `REGULATOR_LINEAR_RANGE(500000, 0x0, 0xaa, 5000)` then
//!   `REGULATOR_LINEAR_RANGE(1375000, 0xab, 0xff, 25000)`
//!
//! I2C address `0x41` is the Muse Book `spm8821@41` node
//! (`k1-x_MUSE-Book.dts`). Transfers go through [`k1_musebook_i2c::pmic_read`] /
//! [`k1_musebook_i2c::pmic_write`], which already serialize TWSI8 against the
//! power-key driver.
//!
//! Ramp: `regulator-ramp-delay = <5000>` on `dcdc_1` (µV/µs).

/// `SPM8821_BUCK1_VSEL_REG`.
pub(crate) const BUCK1_VSEL_REG: u8 = 0x48;

/// First linear range: 500 mV + sel * 5 mV, sel ∈ [0, 0xaa].
const RANGE0_MIN_UV: u32 = 500_000;
const RANGE0_STEP_UV: u32 = 5_000;
const RANGE0_SEL_MAX: u8 = 0xaa;

/// Second linear range: 1375 mV + (sel - 0xab) * 25 mV, sel ∈ [0xab, 0xff].
const RANGE1_MIN_UV: u32 = 1_375_000;
const RANGE1_STEP_UV: u32 = 25_000;
const RANGE1_SEL_MIN: u8 = 0xab;

/// `regulator-ramp-delay` on Muse Book `dcdc_1`, µV/µs.
pub(crate) const RAMP_UV_PER_US: u32 = 5_000;

/// Convert millivolts to the DCDC1 vsel byte. `None` if out of range.
pub(crate) fn mv_to_vsel(mv: u32) -> Option<u8> {
    let uv = mv.checked_mul(1000)?;
    if uv < RANGE0_MIN_UV {
        return None;
    }
    if uv < RANGE1_MIN_UV {
        let steps = (uv - RANGE0_MIN_UV) / RANGE0_STEP_UV;
        if steps > u32::from(RANGE0_SEL_MAX) {
            return None;
        }
        // Reject voltages that are not on a 5 mV step.
        if !(uv - RANGE0_MIN_UV).is_multiple_of(RANGE0_STEP_UV) {
            return None;
        }
        return Some(steps as u8);
    }
    let steps = (uv - RANGE1_MIN_UV) / RANGE1_STEP_UV;
    let sel = u32::from(RANGE1_SEL_MIN).checked_add(steps)?;
    if sel > 0xff {
        return None;
    }
    if !(uv - RANGE1_MIN_UV).is_multiple_of(RANGE1_STEP_UV) {
        return None;
    }
    Some(sel as u8)
}

/// Inverse of [`mv_to_vsel`].
pub(crate) fn vsel_to_mv(sel: u8) -> u32 {
    if sel <= RANGE0_SEL_MAX {
        (RANGE0_MIN_UV + u32::from(sel) * RANGE0_STEP_UV) / 1000
    } else {
        (RANGE1_MIN_UV + u32::from(sel - RANGE1_SEL_MIN) * RANGE1_STEP_UV) / 1000
    }
}

/// Microseconds to wait after a voltage step (`regulator_set_voltage_time`).
pub(crate) fn ramp_delay_us(from_mv: u32, to_mv: u32) -> u64 {
    let from = u64::from(from_mv) * 1000;
    let to = u64::from(to_mv) * 1000;
    let delta = from.abs_diff(to);
    delta.div_ceil(u64::from(RAMP_UV_PER_US))
}

/// Write DCDC1. `false` if the bus NAKs / times out.
pub(crate) fn set_dcdc1_mv(mv: u32) -> bool {
    let Some(sel) = mv_to_vsel(mv) else {
        return false;
    };
    k1_musebook_i2c::pmic_write(BUCK1_VSEL_REG, sel)
}

/// Read DCDC1. `None` on I2C failure.
pub(crate) fn get_dcdc1_mv() -> Option<u32> {
    k1_musebook_i2c::pmic_read(BUCK1_VSEL_REG).map(vsel_to_mv)
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// The three voltages that appear in `clst_core_opp_table0`.
    #[ktest]
    fn opp_millivolts_land_on_range0() {
        assert_eq!(mv_to_vsel(950), Some(90)); // 0x5a
        assert_eq!(mv_to_vsel(1050), Some(110)); // 0x6e
        assert_eq!(mv_to_vsel(1160), Some(132)); // 0x84
        assert_eq!(vsel_to_mv(90), 950);
        assert_eq!(vsel_to_mv(110), 1050);
        assert_eq!(vsel_to_mv(132), 1160);
    }

    /// Range boundary: 0xaa is 1350 mV; 0xab is 1375 mV (25 mV step).
    #[ktest]
    fn range_break_at_0xaa() {
        assert_eq!(vsel_to_mv(0xaa), 1350);
        assert_eq!(vsel_to_mv(0xab), 1375);
        assert_eq!(mv_to_vsel(1350), Some(0xaa));
        assert_eq!(mv_to_vsel(1375), Some(0xab));
        assert_eq!(mv_to_vsel(1400), Some(0xac));
        assert_eq!(mv_to_vsel(499), None);
    }

    /// 950 → 1050 mV is 100 mV = 100_000 µV / 5000 µV/µs = 20 µs.
    #[ktest]
    fn ramp_matches_dts_delay() {
        assert_eq!(ramp_delay_us(950, 1050), 20);
        assert_eq!(ramp_delay_us(1050, 950), 20);
        assert_eq!(ramp_delay_us(950, 950), 0);
        assert_eq!(ramp_delay_us(950, 1160), 42);
    }
}
