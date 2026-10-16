// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: TSEN initialization plus the 10 s sampler thread.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ostd::{arch::boot::DEVICE_TREE, sync::WaitQueue, timer::TIMER_FREQ};

use super::{k1_rst, k1_tsen};

static WAIT: WaitQueue = WaitQueue::new();
static PENDING: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU64 = AtomicU64::new(0);

/// `TIMER_FREQ` is 1000 Hz; this is the 10 s cadence of `[k1-therm]`.
const TICKS_PER_PRINT: u64 = TIMER_FREQ * 10;

fn is_k1() -> bool {
    DEVICE_TREE.get().is_some_and(|dt| {
        dt.find_compatible(&["spacemit,k1-x", "spacemit,k1"])
            .is_some()
    })
}

pub(crate) fn init() {
    if !is_k1() {
        return;
    }
    // WDT status first: `ccu::enable(Wdt)` deasserts reset, and the
    // sticky WTS bit is what we came for. TSEN initialization is independent
    // and can wait a couple of ms for the first sample.
    k1_rst::dump();
    k1_tsen::init();
}

fn on_timer() {
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_multiple_of(TICKS_PER_PRINT) {
        PENDING.store(true, Ordering::Release);
        WAIT.wake_all();
    }
}

pub(crate) fn cluster0_temp_mc() -> Option<i32> {
    k1_tsen::cluster_temp_mc(3)
}

pub(crate) fn cluster1_temp_mc() -> Option<i32> {
    k1_tsen::cluster_temp_mc(4)
}

pub(crate) fn cluster_hot_temp_mc() -> Option<i32> {
    match (cluster0_temp_mc(), cluster1_temp_mc()) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

pub(crate) fn run_loop() {
    if !is_k1() {
        // Return rather than park. Parking on a queue that nothing ever
        // wakes would strand a kernel thread for the whole uptime of
        // every non-K1 target.
        return;
    }
    ostd::timer::register_callback_on_cpu(on_timer);
    loop {
        WAIT.wait_until(|| PENDING.swap(false, Ordering::AcqRel).then_some(()));
        k1_tsen::tick();
    }
}
