// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: periodic sync of every mounted filesystem (`aster.writeback=`).

//! Periodic writeback of every mounted filesystem.
//!
//! Periodic full-filesystem sync bounds the amount of cached data left pending
//! between explicit sync, fsync, unmount, and shutdown operations. It reduces
//! data loss after an unexpected power cut; it does not make non-journaled
//! ext2 updates atomic or guarantee a consistent crash image.
//!
//! The flusher syncs the initial mount namespace every [`INTERVAL_SECS`].
//! `aster.writeback=0` disables this periodic work. Ordered shutdown pauses it
//! before the final sync and clean-marker protocol.

use core::{
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use ostd::sync::WaitQueue;

use crate::{fs::vfs::path::MountNamespace, prelude::*, thread::kernel_thread::ThreadOptions};

/// Seconds between full syncs. `0` disables the thread.
static INTERVAL_SECS: AtomicU32 = AtomicU32::new(5);
aster_cmdline::define_kv_param!("aster.writeback", INTERVAL_SECS);

/// A sleeping mutex shared only by this flusher and the reboot path.
/// The shutdown caller acquires it before taking filesystem locks. This waits
/// for an in-flight periodic sync and prevents a new one after marking clean.
static SYNC_SERIAL: Mutex<()> = Mutex::new(());

/// Pauses periodic writeback until the guard is dropped. Shutdown holds the
/// guard through the final power transition; an early sync error releases it.
pub(crate) fn pause_for_shutdown() -> MutexGuard<'static, ()> {
    SYNC_SERIAL.lock()
}

pub(crate) fn init_in_first_kthread() {
    let secs = INTERVAL_SECS.load(Ordering::Relaxed);
    if secs == 0 {
        info!("[writeback] periodic sync disabled by aster.writeback=0");
        return;
    }
    ThreadOptions::new(move || flusher(Duration::from_secs(u64::from(secs)))).spawn();
}

fn flusher(interval: Duration) {
    let sleep_queue = WaitQueue::new();
    let mut failures: u32 = 0;
    loop {
        // Sleep between attempts without busy-polling.
        let _ = sleep_queue.wait_until_or_timeout(|| -> Option<()> { None }, &interval);
        let _sync_guard = SYNC_SERIAL.lock();
        match MountNamespace::get_init_singleton().sync() {
            Ok(()) => failures = 0,
            Err(err) => {
                failures = failures.saturating_add(1);
                // Report the first three failures, then every sixty failed cycles.
                if failures <= 3 || failures.is_multiple_of(60) {
                    error!(
                        "[writeback] periodic sync failed ({} in a row): {:?}",
                        failures, err
                    );
                }
            }
        }
    }
}
