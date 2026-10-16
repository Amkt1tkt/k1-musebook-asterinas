// SPDX-License-Identifier: MPL-2.0

pub(crate) use cpu_clock::*;
pub(crate) use system_wide::*;

mod cpu_clock;
mod system_wide;

pub(super) fn init() {
    system_wide::init();
}

// K1 MUSE Book --- (re-sample CLOCK_REALTIME_COARSE after clock_settime)
pub(super) fn refresh_coarse_realtime() {
    system_wide::refresh_coarse_realtime();
}
// K1 MUSE Book ---
