// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: sleepable GPU recovery and deferred resource cleanup.

//! Sleepable recovery and deferred resource cleanup.
//!
//! Firmware cleanup, resets, and runtime-PM transitions may take mutexes and wait
//! for firmware acknowledgement. They cannot run in the interrupt's taskless
//! deferred half or in arbitrary destructors. This worker owns those operations
//! and periodically checks completion, timeouts, and firmware progress.
//!
//! `aster-core` creates the worker thread and calls [`run`]. A bare OSTD task lacks
//! the `Thread` data required by the kernel scheduler; the driver cannot create
//! that higher-layer type itself.
//!
//! [`request_wake`] and [`enqueue`] wake the worker without sleeping. A timer also
//! wakes it every [`WATCHDOG_INTERVAL_MS`] and when an autosuspend deadline is
//! due, so progress does not depend on another ioctl or GPU interrupt.

use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use aster_softirq::BottomHalfDisabled;
use ostd::{
    sync::{SpinLock, WaitQueue},
    timer::TIMER_FREQ,
};
use spin::Once;

use crate::{
    device::PvrDevice,
    error::PvrResult,
    fw::heap::FwObject,
    power::{self, WATCHDOG_INTERVAL_MS},
    render::{
        free_list::FreeList,
        hwrt::{HwrtData, HwrtDataset},
    },
    sched::{context::Context, job::HwrtRef, queue::TimelineUfo},
};

/// A firmware structure whose owner has gone, waiting to be released properly.
///
/// The three kinds are the three that need the GPU powered and the firmware's
/// acknowledgement to release: upstream reaches all of them through
/// `pvr_fw_structure_cleanup()`, which sends a KCCB cleanup command and waits.
/// Address spaces are not here, because freeing one touches no firmware
/// structure.
pub enum Doomed {
    /// A render, compute or transfer context.
    Context(Arc<Context>),
    /// A parameter-manager free list.
    FreeList(Arc<FreeList>),
    /// A hardware render target dataset.
    Hwrt(Arc<HwrtDataset>),
}

impl Doomed {
    /// Releases the firmware structure. Runs only on the service thread.
    fn destroy(&self) -> PvrResult<()> {
        match self {
            Self::Context(context) => context.destroy(),
            Self::FreeList(free_list) => free_list.destroy(),
            Self::Hwrt(hwrt) => hwrt.destroy(),
        }
    }

    /// What to call it in a log line.
    fn kind(&self) -> &'static str {
        match self {
            Self::Context(_) => "context",
            Self::FreeList(_) => "free list",
            Self::Hwrt(_) => "render target dataset",
        }
    }
}

enum RetiredOwner {
    Hwrt(HwrtRef),
    Ufo(Arc<TimelineUfo>),
    Context(Arc<Context>),
    FreeList(Arc<FreeList>),
    HwrtData(Arc<HwrtData>),
    Firmware { object: FwObject, mapped: bool },
}

struct RetiredResource {
    owner: RetiredOwner,
    after_stop: Option<u64>,
}

/// Per-device state for the service thread.
pub struct ServiceState {
    /// Where the thread sleeps.
    work: WaitQueue,
    /// Set by every wake source, cleared by the thread. A flag rather than a
    /// count: the thread's pass is idempotent, so two wakes and one wake ask for
    /// the same thing.
    pending: AtomicBool,
    /// Firmware structures waiting to be released, in the order their owner gave
    /// them up. **The order is the contract**: a context may still be running work
    /// that names a free list, so contexts are released first, exactly as
    /// `PvrFile::teardown` and upstream's `pvr_drm_driver_postclose()` order them.
    doomed: SpinLock<VecDeque<Doomed>, BottomHalfDisabled>,
    /// References and allocations handed off by completion or registry lookup.
    retired_resources: SpinLock<VecDeque<RetiredResource>, BottomHalfDisabled>,
}

impl ServiceState {
    /// Creates the state a device starts with. No thread yet.
    pub const fn new() -> Self {
        Self {
            work: WaitQueue::new(),
            pending: AtomicBool::new(false),
            doomed: SpinLock::new(VecDeque::new()),
            retired_resources: SpinLock::new(VecDeque::new()),
        }
    }
}

impl Default for ServiceState {
    fn default() -> Self {
        Self::new()
    }
}

/// The device the timer callback wakes.
///
/// A `Once` for the same reason [`crate::power::install`] and
/// [`crate::driver::register`] use one: this driver supports one GPU. Holding a
/// strong reference is deliberate — the device outlives the boot, and a `Weak`
/// here would only add a failure case that cannot happen.
static SERVICED: Once<Arc<PvrDevice>> = Once::new();

/// Timer ticks between two watchdog passes.
///
/// `TIMER_FREQ` is in hertz, so this is the interval upstream's
/// `queue_delayed_work(..., msecs_to_jiffies(WATCHDOG_TIME_MS))` asks for.
const TICKS_PER_PASS: u64 = TIMER_FREQ * WATCHDOG_INTERVAL_MS / 1000;

/// Upstream's watchdog runs twice a second, and `TIMER_FREQ` is 1 kHz.
const _: () = assert!(TICKS_PER_PASS == 500);
/// Keep periodic service bounded by its cadence instead of doing a full pass
/// on every timer tick.
const _: () = assert!(TICKS_PER_PASS > 1);

/// Ticks seen since the last pass was asked for.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Asks the service thread to make a pass.
///
/// Callable from anywhere, including the interrupt handler's deferred half: it
/// sets a flag and wakes a wait queue, and does nothing else.
pub fn request_wake(device: &PvrDevice) {
    device.service.pending.store(true, Ordering::Release);
    device.service.work.wake_all();
}

/// Hands a firmware structure over to be released.
///
/// The caller must already have made it unreachable to user space (its handle
/// erased) and, for a context, have called [`Context::kill`] so nothing new is
/// queued. What is left — a power reference and a wait for the firmware — is what
/// this defers.
///
/// Ownership is retained even before the worker starts: absence of a consumer
/// does not establish that firmware has stopped using an allocation.
pub fn enqueue(device: &PvrDevice, doomed: Doomed) {
    device.service.doomed.lock().push_back(doomed);
    request_wake(device);
}

fn retire_owner(device: &PvrDevice, owner: RetiredOwner, after_stop: Option<u64>) {
    device
        .service
        .retired_resources
        .lock()
        .push_back(RetiredResource { owner, after_stop });
    request_wake(device);
}

/// Releases render targets on the worker; failed submitted work waits for a stop.
pub(crate) fn retire_hwrt(device: &PvrDevice, reference: HwrtRef, after_stop: Option<u64>) {
    retire_owner(device, RetiredOwner::Hwrt(reference), after_stop);
}

/// Retains a native dependency until completion or a confirmed stop after failure.
pub(crate) fn retire_ufo(device: &PvrDevice, reference: Arc<TimelineUfo>, after_stop: Option<u64>) {
    retire_owner(device, RetiredOwner::Ufo(reference), after_stop);
}

/// Transfers a registry lookup reference without running its destructor in a bottom half.
pub(crate) fn retire_context(device: &PvrDevice, reference: Arc<Context>) {
    retire_owner(device, RetiredOwner::Context(reference), None);
}

pub(crate) fn retire_free_list(device: &PvrDevice, reference: Arc<FreeList>) {
    retire_owner(device, RetiredOwner::FreeList(reference), None);
}

pub(crate) fn retire_hwrt_data(device: &PvrDevice, reference: Arc<HwrtData>) {
    retire_owner(device, RetiredOwner::HwrtData(reference), None);
}

/// Returns an unreferenced timeline allocation without taking heap or MMU locks here.
pub(crate) fn retire_firmware_object(device: &PvrDevice, object: FwObject, mapped: bool) {
    retire_owner(device, RetiredOwner::Firmware { object, mapped }, None);
}

/// Drops completed references in sleepable context, retaining abandoned GPU work.
pub(crate) fn drain_retired_resources(device: &PvrDevice) {
    let retired = {
        let mut pending = device.service.retired_resources.lock();
        core::mem::take(&mut *pending)
    };
    for entry in retired {
        if entry.after_stop.is_some_and(|epoch| {
            !device.quiesced.load(Ordering::Acquire)
                && epoch == device.quiescence_epoch.load(Ordering::Acquire)
        }) {
            device.service.retired_resources.lock().push_back(entry);
        } else {
            match entry.owner {
                RetiredOwner::Hwrt(reference) => drop(reference),
                RetiredOwner::Ufo(reference) => drop(reference),
                RetiredOwner::Context(reference) => drop(reference),
                RetiredOwner::FreeList(reference) => drop(reference),
                RetiredOwner::HwrtData(reference) => drop(reference),
                RetiredOwner::Firmware { object, mapped } => {
                    if let Some(fw) = device.fw.get() {
                        let mut heap = fw.heap.lock();
                        if mapped && let Err(err) = fw.mips.page_table.unmap(&object) {
                            ostd::error!("could not unmap a retired firmware allocation: {err:?}");
                        }
                        heap.free(&object);
                    }
                }
            }
        }
    }
}

/// Runs the service thread's body for one device, for ever.
///
/// `aster-core` calls this from a thread it created; see the module
/// documentation for why the thread cannot be created here.
///
/// Never returns. A device that is declared lost keeps its thread: the passes
/// become nearly free (every step checks), and the alternative is a thread that
/// exits and leaves the doomed queue with no drainer.
pub fn run(device: Arc<PvrDevice>) -> ! {
    if SERVICED.is_completed() {
        ostd::warn!("a GPU service thread is already installed; the timer wakes only the first");
    } else {
        SERVICED.call_once(|| device.clone());
        ostd::timer::register_callback_on_cpu(on_timer_tick);
        // Only now, because this is the thread that performs a postponed
        // power-down and the timer above is what wakes it for one. Enabling it
        // any earlier — at probe, where upstream does — would postpone
        // power-downs to nobody. See `crate::power::Pm::use_autosuspend`.
        power::use_autosuspend();
    }

    loop {
        // Sleeps until a wake source sets the flag. Taking the flag inside the
        // condition is what makes a wake that arrives during a pass produce
        // another pass rather than being lost.
        device.service.work.wait_until(|| {
            device
                .service
                .pending
                .swap(false, Ordering::AcqRel)
                .then_some(())
        });

        pass(&device);
    }
}

/// The timer callback. Runs in interrupt context, so it only counts and wakes.
///
/// Two reasons to ask for a pass, and they are deliberately not merged into one
/// counter. The watchdog cadence is periodic and coarse; an autosuspend that has
/// come due is a one-off and ten times finer, and it must not reset the cadence,
/// because that would let a stream of short power-downs postpone the hang check
/// indefinitely.
fn on_timer_tick() {
    let cadence_reached = TICKS.fetch_add(1, Ordering::Relaxed) + 1 >= TICKS_PER_PASS;
    if cadence_reached {
        TICKS.store(0, Ordering::Relaxed);
    } else if !power::suspend_is_due() {
        // The common case, once per millisecond: three atomic loads and a clock
        // read, which is why the check is allowed to live on this path at all.
        return;
    }

    if let Some(device) = SERVICED.get() {
        request_wake(device);
    }
}

/// Performs sleepable driver work in this order:
///
/// 1. Clean up resources transferred by closing files, retaining them until done.
/// 2. Process a pending reset before judging the device's progress.
/// 3. Retire completions, including any whose interrupt was missed.
/// 4. Check remaining jobs for timeout.
/// 5. Sample the firmware watchdog against the updated queue state.
/// 6. Settle deferred power releases once the autosuspend delay has expired.
fn pass(device: &Arc<PvrDevice>) {
    drain_doomed(device);

    if let Err(err) = power::service_pending_reset(device) {
        ostd::error!("a GPU reset asked for from interrupt context failed: {err:?}");
    }

    crate::sched::queue::process_all(device);
    crate::sched::queue::check_all_timeouts(device);

    power::check_for_hang(device);
    power::settle(device);
    drain_retired_resources(device);
    crate::mm::mmu::release_after_stop(device);
}

/// Whether a notification survives until the worker consumes it.
#[cfg(ktest)]
pub(crate) fn wake_requested(device: &PvrDevice) -> bool {
    device.service.pending.load(Ordering::Acquire)
}

/// What is waiting to be released, oldest first. For tests.
#[cfg(any(test, ktest))]
pub(crate) fn doomed_kinds(device: &PvrDevice) -> alloc::vec::Vec<&'static str> {
    device
        .service
        .doomed
        .lock()
        .iter()
        .map(Doomed::kind)
        .collect()
}

/// Releases every firmware structure waiting for it, under one power reference.
///
/// One reference for the whole batch rather than one each: taking it may boot the
/// firmware, and a client that closes a file with a context, a free list and a
/// dataset on it should not pay for that three times.
pub(crate) fn drain_doomed(device: &Arc<PvrDevice>) {
    if device.service.doomed.lock().is_empty() {
        return;
    }

    // A lost device has no firmware to tell, and `PowerRef::take` refuses one.
    // A separate successful stop is required: the error flag does not stop DMA.
    if device.check_alive().is_err() {
        if !device.quiesced.load(Ordering::Acquire) {
            // Firmware may still reach these allocations. A reboot releases the quarantine.
            return;
        }
        // Destructors take heap and MIPS locks; detach the batch before dropping it.
        let doomed = {
            let mut queued = device.service.doomed.lock();
            core::mem::take(&mut *queued)
        };
        let dropped = doomed.len();
        drop(doomed);
        ostd::warn!("device is lost; released {dropped} firmware structure(s) without asking it");
        return;
    }

    let _power = match power::PowerRef::take(device) {
        Ok(power) => power,
        Err(err) => {
            // Left in the queue: the next pass tries again. A structure the
            // firmware still knows about must not be freed just because the GPU
            // could not be powered this time.
            ostd::error!("could not power the GPU to release firmware structures: {err:?}");
            return;
        }
    };

    // Upstream takes `down_read(&pvr_dev->reset_sem)` around the same cleanup
    // command, in `pvr_fw_structure_cleanup()`.
    let _serialise_against_reset = device.reset_lock.read();

    // One at a time, and the lock is not held across `destroy()`: that sends a
    // KCCB command and waits for the firmware, and an enqueue from a closing file
    // must not block behind it.
    loop {
        let Some(doomed) = device.service.doomed.lock().pop_front() else {
            return;
        };
        if let Err(err) = doomed.destroy() {
            ostd::error!(
                "{} cleanup failed ({err:?}); retaining its allocations",
                doomed.kind()
            );
            device.service.doomed.lock().push_front(doomed);
            device.reset_request.request(power::ResetKind::Hard);
            request_wake(device);
            return;
        }
    }
}
