// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: thermal-zone helpers forwarding to k1-musebook-thermal.

//! Thermal-zone helpers. The TSEN MMIO lives in `k1-musebook-thermal`
//! (`IoMem` is exclusive); this module only formats and forwards.

/// Linux `/sys/class/thermal/thermal_zone0/temp` — millidegrees, newline.
pub fn format_temp_mc(mc: i32) -> alloc::string::String {
    alloc::format!("{mc}\n")
}

/// Hottest cluster, millidegrees. `None` if TSEN never came up.
pub(crate) fn cluster_hot_temp_mc() -> Option<i32> {
    k1_musebook_thermal::cluster_hot_temp_mc()
}

/// Zone 0 is cluster0 (bjt3), matching `thermal-sensors = <&thermal 3>`.
pub(crate) fn zone0_temp_mc() -> Option<i32> {
    k1_musebook_thermal::cluster0_temp_mc().or_else(k1_musebook_thermal::cluster_hot_temp_mc)
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn sysfs_temp_is_millidegrees_with_newline() {
        assert_eq!(format_temp_mc(75_000), "75000\n");
        assert_eq!(format_temp_mc(0), "0\n");
        assert_eq!(format_temp_mc(-5_000), "-5000\n");
    }
}
