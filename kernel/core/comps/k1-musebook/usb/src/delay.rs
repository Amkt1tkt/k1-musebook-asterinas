// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: busy-wait delay for USB PHY / host sequencing.

//! Busy-wait using the architectural cycle counter (same as `aster-mmc`).

use ostd::arch::{read_tsc, tsc_freq};

pub(crate) fn delay_us(us: u64) {
    ostd::mm::dma::dma_io_fence();
    let ticks = (tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}

pub(crate) fn delay_ms(ms: u64) {
    delay_us(ms.saturating_mul(1000));
}
