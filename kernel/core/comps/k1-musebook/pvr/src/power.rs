// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: GPU runtime power references and bounded reset recovery.

//! Runtime power management.
//!
//! Ports `pvr_power.c` and `pvr_power.h` (785 lines).
//!
//! The GPU is powered down when idle and brought up on first use, which means
//! firmware boot happens lazily rather than at probe. Two consequences worth
//! stating: an ioctl may be the thing that boots the firmware, so it must be able
//! to report a boot failure; and the hang detector lives here, because deciding a
//! GPU is stuck and deciding to power it off are the same decision.
//!
//! "Idle" means idle for [`AUTOSUSPEND_DELAY_MS`], not idle for an instant. That
//! distinction is the difference between booting the firmware once per client and
//! booting it once per frame; it has its own section below.
//!
//! The platform-specific half — clocks, resets, the power domain — is in
//! [`crate::platform::k1`]. This module only sequences it.
//!
//! # Ordering is the whole problem
//!
//! Power domain, then clock, then reset — and the exact reverse on the way down.
//! Get it wrong and every control register reads back all-zeroes or all-ones,
//! with no error and no panic anywhere. That is why the sequence exists once, in
//! [`power_up`] and [`power_down`], behind a trait: a call site cannot assemble
//! its own variant, and [`sequence_is_a_palindrome`] can assert the two are
//! mirror images without any hardware.
//!
//! [`sequence_is_a_palindrome`]: tests::sequence_is_a_palindrome

use alloc::sync::Arc;
use core::{
    mem::offset_of,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

use ostd::sync::{Mutex, SpinLock};
use ostd_pod::FromZeros;
use spin::Once;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw,
    rogue::fwif::{RogueFwifKccbCmd, RogueFwifOsdata, RogueFwifSysdata},
    sched::abi,
};

/// One step of the power sequence.
///
/// Named so that a log line, and the ordering test, can talk about the sequence
/// rather than about six unrelated function calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Enable the GPU power domain.
    PowerDomainOn,
    /// Ungate and program `CLK_GPU`.
    ClockEnable,
    /// Release `RESET_GPU`.
    ResetDeassert,
    /// Assert `RESET_GPU`.
    ResetAssert,
    /// Gate `CLK_GPU`.
    ClockDisable,
    /// Disable the GPU power domain.
    PowerDomainOff,
}

impl Step {
    /// The step that undoes this one.
    ///
    /// The reverse sequence is derived from the forward one rather than written
    /// out a second time, so the two cannot drift apart.
    pub const fn inverse(self) -> Self {
        match self {
            Self::PowerDomainOn => Self::PowerDomainOff,
            Self::ClockEnable => Self::ClockDisable,
            Self::ResetDeassert => Self::ResetAssert,
            Self::ResetAssert => Self::ResetDeassert,
            Self::ClockDisable => Self::ClockEnable,
            Self::PowerDomainOff => Self::PowerDomainOn,
        }
    }

    /// What this step is doing, for the serial log.
    pub const fn description(self) -> &'static str {
        match self {
            Self::PowerDomainOn => "enabling the GPU power domain",
            Self::ClockEnable => "enabling CLK_GPU",
            Self::ResetDeassert => "releasing RESET_GPU",
            Self::ResetAssert => "asserting RESET_GPU",
            Self::ClockDisable => "gating CLK_GPU",
            Self::PowerDomainOff => "disabling the GPU power domain",
        }
    }
}

/// The order [`power_up`] performs its steps in.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_power_on_sequence_manual`, lines 272-314: clocks, `udelay(1)`, then
/// `reset_control_deassert`). The power domain comes first because on the K1 it
/// gates the clock controller's view of the GPU as well — see
/// [`crate::platform::k1`].
pub const POWER_UP_SEQUENCE: [Step; 3] =
    [Step::PowerDomainOn, Step::ClockEnable, Step::ResetDeassert];

/// Microseconds to wait between the last clock and the reset release.
///
/// Upstream's comment is the source and is worth repeating: the hardware manual
/// requires at least 32 clock cycles between de-asserting the clock generator's
/// reset and de-asserting the GPU reset, and one microsecond covers that for any
/// feasible GPU clock.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_power_on_sequence_manual`, lines 288-296: `udelay(1)`).
pub const CLOCK_TO_RESET_DELAY_US: u64 = 1;

/// How often the hang detector should be sampled, in milliseconds.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`WATCHDOG_TIME_MS`, line 30).
pub const WATCHDOG_INTERVAL_MS: u64 = 500;

const WATCHDOG_INTERVAL_NS: u64 = WATCHDOG_INTERVAL_MS * 1_000_000;

/// Consecutive stalled samples before the GPU is declared hung.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_watchdog_kccb_stalled`, line 179: `kccb_stall_count == 2`).
pub const STALL_POLLS_BEFORE_HUNG: u32 = 2;

/// How long the firmware has to acknowledge a power command, in microseconds.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`POWER_SYNC_TIMEOUT_US`, line 28). Used by the FORCED_IDLE / POW_OFF
/// handshake that [`Pm::settle`] runs before stopping the firmware core.
pub const POWER_SYNC_TIMEOUT_US: u64 = 1_000_000;

/// Microseconds between `power_sync` polls while waiting for that acknowledgement.
///
/// Upstream `readl_poll_timeout(..., 100, POWER_SYNC_TIMEOUT_US)` in
/// `pvr_power_send_command` (`pvr_power.c:62-63`).
const POWER_SYNC_POLL_INTERVAL_US: u64 = 100;

/// How long the GPU stays powered after its last use, in milliseconds.
///
/// Upstream `pvr_probe` uses a 50 ms runtime-PM autosuspend delay. Keeping the
/// device up across adjacent ioctl batches avoids repeating firmware boot for
/// short gaps between references.
pub const AUTOSUSPEND_DELAY_MS: u64 = 50;

/// Whether `delay_ns` has passed between two monotonic instants.
///
/// `now` before `last_busy` reports "not expired". That cannot happen with a
/// monotonic clock, and saturating rather than wrapping means a bug elsewhere
/// keeps the GPU powered instead of powering it down under a running client.
pub const fn autosuspend_expired(now_ns: u64, last_busy_ns: u64, delay_ns: u64) -> bool {
    now_ns.saturating_sub(last_busy_ns) >= delay_ns
}

/// The platform-specific half of the power sequence.
///
/// Upstream's equivalent is `struct pvr_power_sequence_ops`, one instance per
/// compatible string. Ours has one implementor, [`crate::platform::k1::K1Ops`],
/// plus a recording stand-in in the tests.
///
/// The down-going methods cannot fail. Upstream's `reset_control_assert` can, and
/// its `pvr_power_off_sequence_manual` returns that error after gating the clocks
/// anyway — there is nothing a caller could do differently, and stopping halfway
/// through a power-down is worse than finishing it.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.h>
/// (`struct pvr_power_sequence_ops`).
pub trait PlatformOps: Send + Sync {
    /// Enables the GPU power domain.
    fn power_domain_on(&self) -> PvrResult<()>;
    /// Enables the GPU clock.
    fn clock_enable(&self) -> PvrResult<()>;
    /// Releases the GPU reset.
    fn reset_deassert(&self) -> PvrResult<()>;
    /// Asserts the GPU reset.
    fn reset_assert(&self);
    /// Gates the GPU clock.
    fn clock_disable(&self);
    /// Disables the GPU power domain.
    fn power_domain_off(&self);

    /// The GPU core clock rate in Hz, if the platform can report it.
    ///
    /// Upstream reads `clk_get_rate(pvr_dev->core_clk)` and hands the value to
    /// the firmware, which derives its timers from it. `None` means the platform
    /// has no way to tell, which makes firmware timestamps wrong but does not
    /// stop it booting — see [`fw::FwInitParams::core_clock_hz`].
    fn core_clock_hz(&self) -> Option<u32> {
        None
    }
}

/// Runs one step and says so on the console.
///
/// Every step logs before it acts, so that a step which hangs the SoC is
/// identified by the last line printed rather than by the absence of one.
fn run(ops: &dyn PlatformOps, step: Step) -> PvrResult<()> {
    ostd::info!("power sequence: {}", step.description());
    match step {
        Step::PowerDomainOn => ops.power_domain_on(),
        Step::ClockEnable => ops.clock_enable(),
        Step::ResetDeassert => ops.reset_deassert(),
        Step::ResetAssert => {
            ops.reset_assert();
            Ok(())
        }
        Step::ClockDisable => {
            ops.clock_disable();
            Ok(())
        }
        Step::PowerDomainOff => {
            ops.power_domain_off();
            Ok(())
        }
    }
}

/// Powers the GPU up: domain, clock, reset, in that order.
///
/// On failure the steps already taken are undone in reverse, which is what
/// upstream's `err_*` labels do.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_power_on_sequence_manual`).
pub fn power_up(ops: &dyn PlatformOps) -> PvrResult<()> {
    for (index, step) in POWER_UP_SEQUENCE.iter().copied().enumerate() {
        // The delay belongs between the clock and the reset, and nowhere else.
        if step == Step::ResetDeassert {
            fw::delay_us(CLOCK_TO_RESET_DELAY_US);
        }

        if let Err(err) = run(ops, step) {
            ostd::error!(
                "power sequence: {} failed ({:?}); undoing {} completed step(s)",
                step.description(),
                err,
                index
            );
            for done in POWER_UP_SEQUENCE[..index].iter().rev() {
                let _ = run(ops, done.inverse());
            }
            return Err(err);
        }
    }

    ostd::info!("power sequence: GPU powered, clocked and out of reset");
    Ok(())
}

/// Powers the GPU down, in the exact reverse order of [`power_up`].
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_power_off_sequence_manual`, lines 316-327).
pub fn power_down(ops: &dyn PlatformOps) {
    for step in POWER_UP_SEQUENCE.iter().rev() {
        // Down-going steps are infallible, so the result cannot be anything but
        // `Ok`; discarding it here keeps that fact in one place.
        let _ = run(ops, step.inverse());
    }
    ostd::info!("power sequence: GPU in reset, clock gated, domain off");
}

/// What the hang detector concluded from one sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HangVerdict {
    /// The firmware executed commands since the last sample.
    Progressing,
    /// No command was executed, but the KCCB is empty. This port does not send
    /// upstream's idle HEALTH_CHECK command, so this verdict alone does not prove
    /// that idle firmware is responsive.
    Idle,
    /// Nothing was executed while commands were outstanding, but not yet for
    /// long enough to call it a hang.
    Suspect,
    /// Commands have been outstanding with no progress for
    /// [`STALL_POLLS_BEFORE_HUNG`] consecutive samples.
    Stalled,
}

/// Detects consecutive stalled KCCB samples within one firmware boot.
///
/// Service wakes also perform cleanup and need not be periodic. Only samples
/// separated by [`WATCHDOG_INTERVAL_MS`] may advance the hang history, matching
/// Linux's delayed watchdog work. A new boot starts with a fresh baseline.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.18/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_watchdog_kccb_stalled` and `pvr_watchdog_worker`).
#[derive(Debug, Default)]
pub struct HangDetector {
    generation: Option<u32>,
    last_sample_ns: u64,
    last_executed: u32,
    stall_polls: u32,
}

impl HangDetector {
    /// Creates a detector with no history.
    pub const fn new() -> Self {
        Self {
            generation: None,
            last_sample_ns: 0,
            last_executed: 0,
            stall_polls: 0,
        }
    }

    fn sample_due(&self, now_ns: u64, generation: u32) -> bool {
        self.generation != Some(generation)
            || now_ns.saturating_sub(self.last_sample_ns) >= WATCHDOG_INTERVAL_NS
    }

    /// Samples progress when due, or establishes a baseline for a new boot.
    ///
    /// Returns `None` without advancing the history when called too early.
    /// The first sample of a boot also returns `None`: it establishes the
    /// counter and time against which subsequent samples measure progress.
    pub fn sample(
        &mut self,
        now_ns: u64,
        generation: u32,
        executed: u32,
        kccb_idle: bool,
    ) -> Option<HangVerdict> {
        if !self.sample_due(now_ns, generation) {
            return None;
        }
        // A delayed worker takes one sample; it must not catch up missed polls.
        self.last_sample_ns = now_ns;
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.last_executed = executed;
            self.stall_polls = 0;
            return None;
        }

        if executed != self.last_executed {
            self.last_executed = executed;
            self.stall_polls = 0;
            return Some(HangVerdict::Progressing);
        }

        if kccb_idle {
            // Idle or an unavailable ring snapshot breaks consecutive evidence
            // of pending work. This port does not submit idle HEALTH_CHECKs.
            self.stall_polls = 0;
            return Some(HangVerdict::Idle);
        }

        self.stall_polls += 1;
        if self.stall_polls >= STALL_POLLS_BEFORE_HUNG {
            self.stall_polls = 0;
            Some(HangVerdict::Stalled)
        } else {
            Some(HangVerdict::Suspect)
        }
    }
}

/// Which of upstream's two resets is being asked for.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_power_reset`'s `hard_reset` argument, lines 511-609).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResetKind {
    /// Restart the firmware processor and clear its fault flags, keeping the
    /// GPU powered and the firmware image in place.
    Soft,
    /// Power-cycle the GPU, reload the firmware image and rebuild the rings.
    Hard,
}

/// A reset requested from a context that cannot sleep.
///
/// The service worker and the next [`PowerRef::take`] consume this request from
/// a sleepable context. Coalescing preserves the strongest pending reset; the
/// requesting interrupt or fence callback does not take transition locks.
#[derive(Debug)]
pub struct ResetRequest(AtomicU32);

/// No reset outstanding.
const RESET_REQUEST_NONE: u32 = 0;
/// A soft reset outstanding.
const RESET_REQUEST_SOFT: u32 = 1;
/// A hard reset outstanding.
const RESET_REQUEST_HARD: u32 = 2;

impl ResetRequest {
    /// Creates an empty request slot.
    pub const fn new() -> Self {
        Self(AtomicU32::new(RESET_REQUEST_NONE))
    }

    /// Records a request, keeping the more drastic one if two arrive.
    pub fn request(&self, kind: ResetKind) {
        let wanted = match kind {
            ResetKind::Soft => RESET_REQUEST_SOFT,
            ResetKind::Hard => RESET_REQUEST_HARD,
        };
        self.0.fetch_max(wanted, Ordering::Release);
    }

    /// Takes the outstanding request, leaving none.
    pub fn take(&self) -> Option<ResetKind> {
        match self.0.swap(RESET_REQUEST_NONE, Ordering::Acquire) {
            RESET_REQUEST_SOFT => Some(ResetKind::Soft),
            RESET_REQUEST_HARD => Some(ResetKind::Hard),
            _ => None,
        }
    }

    /// Whether a reset is waiting for a context that may sleep.
    pub fn is_pending(&self) -> bool {
        self.0.load(Ordering::Acquire) != RESET_REQUEST_NONE
    }
}

impl Default for ResetRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// One step of the reset sequence.
///
/// Named for the same reason [`Step`] is: the order is the whole problem, and a
/// test has to be able to talk about it. Getting it wrong here does not produce
/// an error — it produces a firmware that boots against rings whose offsets the
/// host and the firmware disagree about, which looks like a GPU that accepts
/// work and renders nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetStep {
    /// Stop every queue, so nothing new reaches the firmware. Upstream
    /// `pvr_queue_device_pre_reset()`.
    StopQueues,
    /// Stop the firmware processor. Upstream `pvr_power_fw_disable()`.
    FwStop,
    /// Record that the firmware is no longer running. Upstream's
    /// `fw_dev.initialised = false`.
    ClearBooted,
    /// Power the GPU down. Upstream `pvr_power_device_suspend()`.
    PowerDown,
    /// Reload the firmware image and re-run its initialisers. Upstream
    /// `pvr_fw_hard_reset()`.
    FwHardReset,
    /// Power the GPU back up. Upstream `pvr_power_device_resume()`.
    PowerUp,
    /// Put the KCCB and FWCCB back to their initial state and re-publish their
    /// addresses. Part of upstream's `pvr_fw_hard_reset()`; see [`ResetOps`].
    RebuildRings,
    /// Clear `ROGUE_FWIF_HWR_FW_FAULT | ROGUE_FWIF_HWR_RESTART_REQUESTED`. The
    /// soft path's only action, upstream `pvr_power_reset()` lines 563-566.
    ClearHwrFlags,
    /// Acknowledge any pending firmware interrupt. Upstream `pvr_fw_irq_clear()`.
    IrqClear,
    /// Start the firmware and wait for it to report itself booted. Upstream
    /// `pvr_power_fw_enable()`.
    FwStart,
    /// Restart every queue. Upstream `pvr_queue_device_post_reset()`.
    StartQueues,
    /// Give up on the GPU. Upstream `pvr_device_lost()`.
    DeclareLost,
}

/// The operations [`run_reset`] sequences.
///
/// Behind a trait for the reason [`PlatformOps`] is: the order cannot be checked
/// on hardware — a wrong one produces no error, only a GPU that renders nothing
/// — so it has to be checkable here, against a recording stand-in.
///
/// [`RebuildRings`] has no single upstream counterpart because upstream gets it
/// for free: `pvr_fw_hard_reset()` walks every firmware object and zeroes the
/// ones without `PVR_BO_FW_NO_CLEAR_ON_RESET`, which includes the KCCB and
/// FWCCB control structures, and then re-runs each object's initialiser. This
/// port's [`crate::fw::FwDevice::hard_reset`] only owns the driver's own
/// structures, so the rings are put back by [`crate::sched::reinit`] instead.
/// Skipping it would leave the host's read and write offsets where the dead
/// firmware left them while the new firmware starts again from zero.
///
/// [`RebuildRings`]: ResetStep::RebuildRings
pub trait ResetOps {
    /// Whether the device has already been declared lost.
    fn is_lost(&self) -> bool;
    /// [`ResetStep::StopQueues`].
    fn stop_queues(&self);
    /// [`ResetStep::FwStop`].
    ///
    /// `hard_reset` skips the FORCED_IDLE / POW_OFF handshake, matching
    /// upstream `pvr_power_fw_disable`.
    fn fw_stop(&self, hard_reset: bool) -> PvrResult<()>;
    /// [`ResetStep::ClearBooted`].
    fn clear_booted(&self);
    /// [`ResetStep::PowerDown`].
    fn power_down(&self);
    /// [`ResetStep::FwHardReset`].
    fn fw_hard_reset(&self) -> PvrResult<()>;
    /// [`ResetStep::PowerUp`].
    fn power_up(&self) -> PvrResult<()>;
    /// [`ResetStep::RebuildRings`].
    fn rebuild_rings(&self) -> PvrResult<()>;
    /// [`ResetStep::ClearHwrFlags`].
    fn clear_hwr_flags(&self) -> PvrResult<()>;
    /// [`ResetStep::IrqClear`].
    fn irq_clear(&self) -> PvrResult<()>;
    /// [`ResetStep::FwStart`].
    fn fw_start(&self) -> PvrResult<()>;
    /// [`ResetStep::StartQueues`].
    fn start_queues(&self);
    /// [`ResetStep::DeclareLost`].
    fn declare_lost(&self);
}

/// Runs the reset sequence, escalating a failed soft reset to a hard one.
///
/// Ports the body of upstream `pvr_power_reset()` (lines 525-608) — everything
/// between the power reference and the semaphore, both of which belong to the
/// caller ([`Pm::reset`]) because they are not part of the order this function
/// exists to get right.
///
/// Three properties are upstream's and are what the tests pin:
///
/// - a soft reset that fails is retried as a hard one (`hard_reset = true` and
///   round the `do {} while (err)` again);
/// - a hard reset that fails means the device is lost;
/// - the queues are restarted on *both* exits, including the lost one, because
///   the jobs on them still have fences that must be terminated — fence-completion contract.
pub fn run_reset(ops: &dyn ResetOps, kind: ResetKind) -> PvrResult<()> {
    // Upstream checks `pvr_dev->lost` immediately after taking the semaphore:
    // there is nothing to reset, and the queues have already been failed.
    if ops.is_lost() {
        return Err(PvrError::DeviceLost);
    }

    // Divergence: upstream brackets the whole sequence in `disable_irq()`. A
    // `MappedIrqLine` has no masking operation, so instead the deferred half is
    // told to stand off — see `Pm::is_resetting`. Recorded in `ABI.md`
    // compatibility limits, next to the same divergence on the suspend path.
    let mut hard = kind == ResetKind::Hard;
    let mut queues_stopped = false;

    let outcome = loop {
        if hard {
            ops.stop_queues();
            queues_stopped = true;
        }

        let mut result = ops.fw_stop(hard);
        if result.is_ok() {
            if hard {
                ops.clear_booted();
                ops.power_down();
                // From here to the end of the hard path every failure is fatal:
                // upstream's `goto err_device_lost`, and for the same reason —
                // the GPU is powered down with no firmware in it.
                if let Err(err) = ops
                    .fw_hard_reset()
                    .and_then(|()| ops.power_up())
                    .and_then(|()| ops.rebuild_rings())
                {
                    break Err(err);
                }
            } else {
                result = ops.clear_hwr_flags();
            }

            result = result
                .and_then(|()| ops.irq_clear())
                .and_then(|()| ops.fw_start());
        }

        match result {
            Ok(()) => break Ok(()),
            // A failed hard reset is the end of the road.
            Err(err) if hard => break Err(err),
            Err(_) => {
                ostd::error!("soft GPU reset did not take; escalating to a hard reset");
                hard = true;
            }
        }
    };

    if outcome.is_err() {
        // A failed boot may leave the core executing. Stop it before publishing lost.
        // A failed stop still forces platform reset; resource owners separately require
        // confirmed quiescence before releasing DMA backing.
        let _ = ops.fw_stop(true);
        ops.clear_booted();
        ops.power_down();
        ops.declare_lost();
    }
    if queues_stopped {
        ops.start_queues();
    }
    outcome
}

/// Reference-counted power state for one GPU.
///
/// Upstream leans on Linux's runtime PM, whose reference count and
/// `->runtime_resume`/`->runtime_suspend` callbacks this reproduces:
///
/// - the count going from zero to one powers the GPU up;
/// - the count going back to zero stops the firmware and powers it down;
/// - every count in between is a no-op, so the callers can be as sloppy about
///   pairing as upstream's `pvr_power_get`/`pvr_power_put` sites are.
///
/// # Locking
///
/// [`Self::transition`] is a sleeping mutex and is never taken in interrupt
/// context: resume may wait for firmware boot. Watchdog sampling tries the reset
/// read lock before the transition lock, keeping the firmware snapshot stable
/// without waiting for recovery. Its history spin lock is released before
/// firmware reads, logging or recovery requests.
pub struct Pm {
    ops: Arc<dyn PlatformOps>,
    /// Serializes power transitions.
    transition: Mutex<()>,
    /// Outstanding runtime-PM references.
    references: AtomicU32,
    /// Whether the power sequence has been run and not undone.
    powered: AtomicBool,
    /// Whether a reset is running. The stand-in for upstream's `disable_irq()`.
    resetting: AtomicBool,
    /// Sampling cadence and hang history, maintained by the service worker.
    watchdog: SpinLock<HangDetector>,
    /// Monotonic nanoseconds of the last thing that used the GPU.
    ///
    /// Upstream's `dev->power.last_busy`, maintained by
    /// `pm_runtime_mark_last_busy()`. Written from anywhere, interrupt context
    /// included, which is why it is one atomic and not part of the state behind
    /// [`Self::transition`].
    last_busy: AtomicU64,
    /// How long a power-down waits after the last use, in nanoseconds.
    ///
    /// A field rather than [`AUTOSUSPEND_DELAY_MS`] read at the point of use, for
    /// the same reason upstream has one per device
    /// (`pm_runtime_set_autosuspend_delay()`): the delay has to be settable. Tests
    /// move it so they can stand on either side of the delay without depending
    /// on how far into boot they run — the clock behind [`crate::sched::now_ns`]
    /// cannot be moved, and a test that waited out 50 ms of it would be a race
    /// rather than a statement about the code. Production installs
    /// [`AUTOSUSPEND_DELAY_MS`] unless `k1.gpuidle=` asked for something else.
    delay_ns: AtomicU64,
    /// Whether a power-down may be postponed until [`Self::delay_ns`] has
    /// passed.
    ///
    /// Upstream's `pm_runtime_use_autosuspend()`, called once in `pvr_probe()`.
    /// Here it is turned on by [`crate::service::run`] instead, and the reason
    /// is the whole trick: a postponed power-down has to be performed by
    /// *somebody*, upstream's somebody is the runtime-PM worker, and this port's
    /// is the service thread. Until that thread exists, the count reaching zero
    /// is the last event that will ever happen, so postponing would mean leaving
    /// the GPU powered for good — which is what the probe path would do.
    autosuspend: AtomicBool,
}

impl Pm {
    /// Creates a power manager over a platform, initially powered down.
    pub fn new(ops: Arc<dyn PlatformOps>) -> Self {
        Self {
            ops,
            transition: Mutex::new(()),
            references: AtomicU32::new(0),
            powered: AtomicBool::new(false),
            resetting: AtomicBool::new(false),
            watchdog: SpinLock::new(HangDetector::new()),
            // Upstream's `pm_runtime_mark_last_busy()` in `pvr_probe()`, which
            // is there so that the first suspend is judged against the probe
            // rather than against a zero that is already long expired.
            last_busy: AtomicU64::new(crate::sched::now_ns()),
            delay_ns: AtomicU64::new(crate::knobs::autosuspend_delay_ns()),
            autosuspend: AtomicBool::new(false),
        }
    }

    /// Allows power-downs to be postponed by [`AUTOSUSPEND_DELAY_MS`].
    ///
    /// Ports `pm_runtime_use_autosuspend()`, which upstream calls once in
    /// `pvr_probe()`. Called by [`crate::service::run`] instead, and the reason is
    /// the whole trick: a postponed power-down has to be performed by *somebody*,
    /// upstream's somebody is the runtime-PM worker, and this port's is the
    /// service thread. Before that thread exists the count reaching zero is the
    /// last event that will ever happen, so postponing would leave the GPU powered
    /// for good — which is why the probe path uses [`Self::put_now`].
    ///
    /// Idempotent, and there is no way back: nothing here ever stops servicing.
    pub fn use_autosuspend(&self) {
        self.autosuspend.store(true, Ordering::Release);
        // Default path: identical log to the pre-knob driver. `k1.gpuidle=off`
        // skips it (the `[k1-gpu]` breadcrumb already named the policy).
        let delay_ms = self.delay_ns.load(Ordering::Acquire) / 1_000_000;
        if crate::knobs::gpu_idle().is_off() {
            return;
        }
        ostd::info!(
            "runtime PM: the GPU will stay powered for {} ms after the last use",
            if delay_ms == AUTOSUSPEND_DELAY_MS {
                AUTOSUSPEND_DELAY_MS
            } else {
                delay_ms
            }
        );
    }

    /// Records that the GPU was used just now.
    ///
    /// Ports `pm_runtime_mark_last_busy()`. One relaxed atomic store and nothing
    /// else, so the interrupt handler's deferred half may call it — which is
    /// where upstream's per-event call is.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
    /// (`pvr_device_irq_thread_handler`, line 222).
    pub fn mark_last_busy(&self) {
        self.last_busy
            .store(crate::sched::now_ns(), Ordering::Release);
    }

    /// Whether a power-down is waiting only for the autosuspend delay to pass.
    ///
    /// Three atomic loads and a clock read, no locks, so the timer callback can
    /// ask on every tick: it is what lets the service thread be woken
    /// [`AUTOSUSPEND_DELAY_MS`] after the last use rather than at the next
    /// watchdog pass, which is ten times coarser.
    pub fn suspend_is_due(&self) -> bool {
        self.powered.load(Ordering::Acquire)
            && self.references.load(Ordering::Acquire) == 0
            && self.delay_expired()
    }

    /// Whether the autosuspend delay has passed since the last use.
    fn delay_expired(&self) -> bool {
        autosuspend_expired(
            crate::sched::now_ns(),
            self.last_busy.load(Ordering::Acquire),
            self.delay_ns.load(Ordering::Acquire),
        )
    }

    /// Moves the autosuspend delay, so that a test can stand on either side of it.
    ///
    /// Zero means every power-down is due at once; [`u64::MAX`] means none ever
    /// is. See [`Self::delay_ns`] for why this exists rather than a hook that
    /// backdates the clock.
    #[cfg(any(test, ktest))]
    fn set_delay_ns(&self, delay_ns: u64) {
        self.delay_ns.store(delay_ns, Ordering::Release);
    }

    /// The autosuspend delay in nanoseconds. For tests.
    #[cfg(any(test, ktest))]
    fn delay_ns(&self) -> u64 {
        self.delay_ns.load(Ordering::Acquire)
    }

    /// The platform this manages.
    pub fn ops(&self) -> &Arc<dyn PlatformOps> {
        &self.ops
    }

    /// Outstanding references.
    pub fn references(&self) -> u32 {
        self.references.load(Ordering::Acquire)
    }

    /// Whether the GPU is powered.
    pub fn is_powered(&self) -> bool {
        self.powered.load(Ordering::Acquire)
    }

    /// Whether a reset is in progress.
    ///
    /// The interrupt handler's deferred half consults this and stands off, which
    /// is as close as this port gets to upstream's `disable_irq()` around the
    /// reset: the firmware structures the deferred half reads are being zeroed
    /// and re-initialised while this is true.
    pub fn is_resetting(&self) -> bool {
        self.resetting.load(Ordering::Acquire)
    }

    /// Takes a reference, powering the GPU up if this is the first one.
    ///
    /// `device` is `None` before there is a [`PvrDevice`] to boot firmware for:
    /// the platform has to power the GPU up in order to read its BVNC, and that
    /// read is what decides whether a device exists at all.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
    /// (`pvr_power_device_resume`).
    pub fn get(&self, device: Option<&PvrDevice>) -> PvrResult<()> {
        let _transition = self.transition.lock();
        if let Some(device) = device {
            device.check_alive()?;
            // Platform reset release can itself restart the processor, before fw::start.
            device.quiesced.store(false, Ordering::Release);
        }

        let references = self.take_reference()?;
        ostd::debug!("power reference {} taken", references);

        // Upstream resumes the firmware only once `fw_dev.initialised` is set,
        // and this is the same condition: the firmware image is installed by
        // `platform::k1::attach`, which happens after the register window is
        // known to be good.
        if let Some(device) = device
            && let Some(fw) = device.fw.get()
            && !fw.booted.load(Ordering::Acquire)
        {
            // The firmware reads the KCCB, FWCCB and return-slot addresses out
            // of `osinit` as it starts, so the rings have to exist and their
            // addresses have to be published before it runs. Booting without
            // this leaves those fields zero and the firmware stops inside its
            // own initialisation, which `BootDiagnosis` reports as
            // `DiedDuringInit` -- a symptom several steps removed from the
            // cause. Installing twice is `Busy` and means another caller
            // already did it.
            match crate::sched::init(device) {
                Ok(()) | Err(PvrError::Busy) => {}
                Err(err) => {
                    ostd::error!("submission path could not be prepared: {:?}", err);
                    drop(_transition);
                    self.put_now(Some(device));
                    return Err(err);
                }
            }

            if let Err(err) = fw::boot(device) {
                ostd::error!("firmware did not boot: {:?}", err);
                drop(_transition);
                self.put_now(Some(device));
                return Err(err);
            }
        }

        Ok(())
    }

    /// Adds one to the reference count, running the power sequence if the GPU is
    /// down. The transition lock must already be held.
    ///
    /// The count is incremented before the sequence runs and undone if it fails,
    /// so a concurrent [`Self::put_deferred`] — which takes no lock — can never
    /// observe a reference that has been promised but not yet counted.
    fn take_reference(&self) -> PvrResult<u32> {
        let references = self.references.fetch_add(1, Ordering::AcqRel) + 1;
        if !self.powered.load(Ordering::Acquire) {
            if let Err(err) = power_up(&*self.ops) {
                self.references.fetch_sub(1, Ordering::AcqRel);
                return Err(err);
            }
            self.powered.store(true, Ordering::Release);
        }
        Ok(references)
    }

    /// Takes a reference on behalf of a job, without booting the firmware.
    ///
    /// Ports upstream `pvr_job_get_pm_ref()`, minus the resume. Upstream's is a
    /// full `pm_runtime_resume_and_get()` because it runs from
    /// `pvr_queue_run_job()`, which the scheduler may call on a device nothing
    /// else is holding up. Here the only caller is
    /// [`crate::sched::queue::Queue::push`], which is reached from
    /// `SUBMIT_JOBS` and therefore always runs inside the ioctl's own
    /// [`PowerRef`] — the firmware is already booted, and this can only ever be
    /// the second or later reference. Booting from here would put a second boot
    /// site on a path that is not the documented one.
    ///
    /// Finding the GPU down anyway means the ioctl reference is missing, which
    /// is a bug in this driver rather than something a client can provoke. The
    /// sequence is run regardless, because a job kicked at an unpowered GPU is
    /// worse than a GPU that is powered when it need not be.
    pub fn get_for_job(&self) -> PvrResult<()> {
        let _transition = self.transition.lock();

        if !self.powered.load(Ordering::Acquire) {
            ostd::error!("a job took a power reference on a GPU nothing was holding up");
        }
        self.take_reference()?;
        Ok(())
    }

    /// Releases a reference without powering anything down.
    ///
    /// The half of [`Self::put`] that is legal in interrupt and fence-callback
    /// context: it touches one atomic and takes no lock, so it can run wherever
    /// a job's fence is terminated. The power-down it may have become due is
    /// left to [`Self::settle`].
    ///
    /// This is not an invention of the port. Upstream's `pvr_power_put()` is
    /// `pm_runtime_put()` — the *asynchronous* form, which only marks the device
    /// idle and hands the suspend to a worker — which is exactly why upstream
    /// can call it from `pvr_queue_signal_done_fences()` under a spinlock.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.h>
    /// (`pvr_power_put`).
    pub fn put_deferred(&self) {
        // Measure autosuspend from this release, not the preceding batch's last
        // use. `put` may settle synchronously, without upstream's worker delay.
        self.mark_last_busy();

        loop {
            let references = self.references.load(Ordering::Acquire);
            let Some(remaining) = references.checked_sub(1) else {
                // A count that goes negative means a `put` without a `get`, which
                // is a driver bug that would otherwise show up much later as a GPU
                // that powered off under a running client.
                ostd::error!("unbalanced power put; the reference count is already zero");
                return;
            };
            if self
                .references
                .compare_exchange_weak(references, remaining, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                ostd::debug!("power reference released; {} remain", remaining);
                return;
            }
        }
    }

    /// Performs the power-down the deferred releases have left outstanding, once
    /// the autosuspend delay has passed.
    ///
    /// Sleeps, so it runs only where sleeping is allowed: an ioctl handler
    /// letting go of its own [`PowerRef`], or a [`crate::service`] pass. A count
    /// that reaches zero from an interrupt therefore leaves the GPU powered until
    /// one of those happens — the conservative direction, and recorded in
    /// `ABI.md` compatibility limits.
    ///
    /// Returning without powering down is the normal case, not a failure: see
    /// [`AUTOSUSPEND_DELAY_MS`]. The power-down is then performed by whichever
    /// service pass first finds the delay expired, and the timer wakes that pass
    /// as soon as it is — see [`Self::suspend_is_due`].
    pub fn settle(&self, device: Option<&PvrDevice>) {
        self.power_off_if_idle(device, false);
    }

    /// Performs the power-down now, ignoring the autosuspend delay.
    ///
    /// Ports `pm_runtime_put_sync_suspend()`, which upstream uses on the paths
    /// where nothing will come back to finish an asynchronous one — its
    /// `pvr_device_gpu_init()` error label, for instance. Here the same is true
    /// of the whole probe and attach path: it runs before there is a service
    /// thread, so a postponed power-down would never be performed at all.
    pub fn put_now(&self, device: Option<&PvrDevice>) {
        self.put_deferred();
        self.power_off_if_idle(device, true);
    }

    /// The body of [`Self::settle`] and [`Self::put_now`].
    ///
    /// `ignore_delay` is the only difference between them, and it is checked
    /// *inside* the transition lock together with the count, so that a reference
    /// taken between a caller's decision and the sequence cannot be lost.
    fn power_off_if_idle(&self, device: Option<&PvrDevice>, ignore_delay: bool) {
        let _transition = self.transition.lock();

        if self.references.load(Ordering::Acquire) != 0 || !self.powered.load(Ordering::Acquire) {
            return;
        }

        // Postponing is only correct if something will come back to do it; see
        // the field's documentation. When nothing will, both tests below are
        // skipped rather than honoured, because "the GPU stays powered for ever"
        // is a worse answer than "it powers down sooner than upstream would".
        // `ignore_delay` is the same choice made by the caller: upstream's
        // `pm_runtime_put_sync_suspend()` reaches `rpm_suspend` directly and
        // consults neither the delay nor `->runtime_idle`.
        if !ignore_delay && self.autosuspend.load(Ordering::Acquire) {
            if !self.delay_expired() {
                return;
            }

            // Upstream's runtime-idle callback refuses suspension while firmware
            // work remains. A zero host reference count does not imply an empty KCCB;
            // cleanup can still be awaiting firmware acknowledgement.
            if device.is_some_and(|device| !firmware_is_idle(device)) {
                report_suspend_refused(device);
                return;
            }
        }

        // Stops the firmware before the platform power-down sequence.
        //
        // A soft stop sends FORCED_IDLE and POW_OFF and waits for acknowledgement;
        // a hard stop skips that handshake. Handshake failure is reported but this port
        // still stops the firmware core, unlike upstream's powered-on error return.
        // Callers must respect reset → transition → heap → queue → KCCB lock ordering.
        if let Some(device) = device
            && let Some(fw) = device.fw.get()
            && fw.booted.load(Ordering::Acquire)
        {
            if let Err(err) = fw_disable_device(device, false) {
                ostd::error!("could not stop the firmware cleanly: {:?}", err);
            }
            fw.booted.store(false, Ordering::Release);
        }

        // Divergence: upstream also calls `disable_irq()` here, so that a late
        // interrupt cannot arrive against an unpowered GPU. `MappedIrqLine` has
        // no masking operation, so instead the handler tolerates reading a
        // powered-down GPU — see `crate::platform::k1::handle_irq`. Recorded in
        // `ABI.md` compatibility limits.
        self.powered.store(false, Ordering::Release);
        power_down(&*self.ops);

        // The next power cycle gets fresh reports: the ring it comes back with is
        // not the one the last report described, and a refusal that has been
        // resolved by this very power-down should not silence the next one.
        STALL_REPORTED.store(false, Ordering::Relaxed);
        SUSPEND_REFUSED_REPORTED.store(false, Ordering::Relaxed);
    }

    /// Resets the GPU, and declares it lost if the reset fails.
    ///
    /// Ports upstream `pvr_power_reset()`'s frame: the power reference, the
    /// reset semaphore and the power release. The sequence between them is
    /// [`run_reset`].
    ///
    /// # Locking
    ///
    /// Three locks, taken in this order and released in the reverse:
    ///
    /// 1. a **power reference** (upstream's `pvr_power_get_clear()`), so that
    ///    nothing can power the GPU down mid-reset. Taken *before* the two locks
    ///    below, as upstream takes it before `down_write()`, because
    ///    [`Self::get`] itself takes [`Self::transition`] and holding that
    ///    across the acquisition would deadlock;
    /// 2. [`crate::device::PvrDevice::reset_lock`] for **writing** — upstream's
    ///    `down_write(&pvr_dev->reset_sem)`. This is what excludes a concurrent
    ///    submission or firmware cleanup;
    /// 3. [`Self::transition`], which excludes a concurrent [`Self::get`] — and
    ///    therefore a concurrent firmware boot — for the whole sequence. Nothing
    ///    inside the sequence may take it again, which is why the sequence uses
    ///    [`Self::suspend_for_reset`] and [`Self::resume_for_reset`] rather than
    ///    [`Self::put`] and [`Self::get`].
    ///
    /// The order relative to everything else in the driver is therefore
    /// **reset lock → power transition → firmware heap → queue → KCCB**, which
    /// extends [`crate::sched::queue`]'s lock order upwards rather than inserting
    /// anything between its three.
    ///
    /// Divergence: upstream's `pvr_power_get_clear()` retries after clearing a
    /// runtime-PM error, and `WARN_ON`s the failure and carries on. There is no
    /// runtime-PM error state to clear here, and continuing without a power
    /// reference would mean resetting a GPU another thread may power down
    /// half-way through, so a failure to take the reference is returned instead.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
    /// (`pvr_power_reset`, lines 511-609).
    pub fn reset(&self, device: &PvrDevice, kind: ResetKind) -> PvrResult<()> {
        self.get(Some(device))?;

        // Before anything is overwritten. A hard reset restores the firmware's
        // code and data from the driver's copies, which throws away the boot
        // stage counter and the MIPS exception state — on a GPU that has never
        // rendered, that is the only account of what went wrong. The trace
        // buffers survive a reset by design; these two do not.
        if kind == ResetKind::Hard {
            ostd::debug!("[k1-gpu] hard GPU reset requested; state before the reset:");
            fw::BootDiagnosis::collect(device).log();
        }

        let outcome = {
            let _serialise = device.reset_lock.write();
            let _transition = self.transition.lock();
            self.resetting.store(true, Ordering::Release);
            let outcome = run_reset(&DeviceReset { device, pm: self }, kind);
            self.resetting.store(false, Ordering::Release);
            outcome
        };

        self.put(Some(device));

        match &outcome {
            Ok(()) => ostd::debug!("[k1-gpu] GPU reset complete; the firmware is up again"),
            Err(err) => {
                ostd::debug!("[k1-gpu] GPU reset failed ({:?}); device lost", err)
            }
        }
        outcome
    }

    /// Stops an unrecoverable device without attempting to restart firmware.
    ///
    /// Reset exclusion precedes the transition lock, as in [`Self::reset`].
    /// Cleanup can release DMA only if the stop sequence confirmed quiescence.
    fn lose_device(&self, device: &PvrDevice) {
        let _serialise = device.reset_lock.write();
        let _transition = self.transition.lock();
        self.resetting.store(true, Ordering::Release);
        crate::sched::queue::stop_all(device);
        if self.is_powered()
            && let Err(err) = fw_disable_device(device, true)
        {
            ostd::error!("GPU stop failed ({err:?}); retaining firmware cleanup owners");
        }
        if let Some(fw) = device.fw.get() {
            fw.booted.store(false, Ordering::Release);
        }
        self.suspend_for_reset();
        device.declare_lost();
        self.resetting.store(false, Ordering::Release);
        crate::sched::queue::start_all(device);
    }

    /// Powers the GPU down as part of a reset, without touching the count.
    ///
    /// Ports the `pvr_power_device_suspend()` call inside upstream
    /// `pvr_power_reset()`, which reaches the same `->power_off` hook the
    /// runtime-PM path does. Legal only from [`Self::reset`], which holds
    /// [`Self::transition`]; the reference it also holds is what stops any other
    /// caller from observing the GPU powered down.
    fn suspend_for_reset(&self) {
        // Upstream's `pvr_power_device_suspend()` would stop the firmware first;
        // the reset sequence has already done that, which is why upstream clears
        // `initialised` immediately before calling it.
        self.powered.store(false, Ordering::Release);
        power_down(&*self.ops);
    }

    /// Powers the GPU back up as part of a reset. See [`Self::suspend_for_reset`].
    ///
    /// Ports the `pvr_power_device_resume()` call inside upstream
    /// `pvr_power_reset()`. The firmware is started separately, by the
    /// [`ResetStep::FwStart`] step, because upstream's resume skips its own
    /// `pvr_power_fw_enable()` while `initialised` is false.
    fn resume_for_reset(&self) -> PvrResult<()> {
        power_up(&*self.ops)?;
        self.powered.store(true, Ordering::Release);
        Ok(())
    }

    /// Releases a reference, letting the GPU power down when the last one goes.
    ///
    /// The two halves together, for a caller that may sleep. "Letting" rather
    /// than "powering": once [`Self::use_autosuspend`] has been called the
    /// power-down waits for [`AUTOSUSPEND_DELAY_MS`], which is what
    /// `pvr_power_put()` gets from being the asynchronous `pm_runtime_put()`.
    /// [`Self::put_now`] is the variant that does not wait.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
    /// (`pvr_power_device_suspend`).
    pub fn put(&self, device: Option<&PvrDevice>) {
        self.put_deferred();
        self.settle(device);
    }

    /// Samples the hang detector and asks for a reset when it has stalled.
    ///
    /// Runs from the service thread. Records a reset request for the next service
    /// pass so timeout and completion processing retain a consistent pass order.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
    /// (`pvr_watchdog_worker`, lines 206-239).
    pub fn check_for_hang(&self, device: &PvrDevice) {
        // Match the reset -> power-transition lock order. A busy transition is
        // not hang evidence; skip it without advancing the sampling history.
        let Some(_reset) = device.reset_lock.try_read() else {
            return;
        };
        let Some(_transition) = self.transition.try_lock() else {
            return;
        };
        if device.check_alive().is_err() || device.reset_request.is_pending() {
            return;
        }
        // Upstream's `pm_runtime_get_if_in_use() <= 0` bail-out: a suspended GPU
        // is not a hung one, and its firmware structures must not be read.
        if !self.is_powered() {
            return;
        }
        let Some(fw) = device.fw.get() else {
            return;
        };
        if !fw.booted.load(Ordering::Acquire) {
            return;
        }

        let generation = fw.generation.load(Ordering::Acquire);
        if !self
            .watchdog
            .lock()
            .sample_due(crate::sched::now_ns(), generation)
        {
            return;
        }

        let executed = match fw
            .mem
            .osdata
            .read_val::<u32>(offset_of!(RogueFwifOsdata, kccb_cmds_executed))
        {
            Ok(executed) => executed,
            Err(err) => {
                ostd::warn!("could not read the firmware's command counter: {:?}", err);
                return;
            }
        };

        let idle = kccb_is_idle(device);
        let Some(verdict) =
            self.watchdog
                .lock()
                .sample(crate::sched::now_ns(), generation, executed, idle)
        else {
            return;
        };
        match verdict {
            HangVerdict::Progressing | HangVerdict::Idle => {}
            HangVerdict::Suspect => {
                ostd::warn!(
                    "firmware has executed {} commands and made no progress since the last poll",
                    executed
                );
                // On the *first* suspicion, not only at the stall: by the time a
                // hard reset runs it has reloaded the firmware image and rebuilt
                // the rings, so the ring that caused the verdict is gone. This is
                // the only point at which the evidence still exists.
                report_stall_evidence(device, executed);
            }
            HangVerdict::Stalled => {
                // Upstream's `pvr_watchdog_worker()` calls
                // `pvr_power_reset(pvr_dev, true)` here and only declares the
                // device lost if that fails. It can, because its watchdog is a
                // workqueue worker. This port records the reset for the next
                // service pass; IRQ-side reset requests use the same mechanism.
                ostd::debug!(
                    "[k1-gpu] firmware stalled at {} executed commands for {} polls; \
                     asking for a hard reset",
                    executed,
                    STALL_POLLS_BEFORE_HUNG
                );
                ostd::error!(
                    "firmware stalled at {} executed commands for {} polls",
                    executed,
                    STALL_POLLS_BEFORE_HUNG
                );
                device.reset_request.request(ResetKind::Hard);
            }
        }
    }
}

/// [`ResetOps`] over a real device.
///
/// Every method is one line of upstream `pvr_power_reset()` pointed at this
/// port's equivalent; the sequencing is in [`run_reset`], which is why this
/// type has no logic of its own.
struct DeviceReset<'a> {
    device: &'a PvrDevice,
    pm: &'a Pm,
}

impl ResetOps for DeviceReset<'_> {
    fn is_lost(&self) -> bool {
        self.device.check_alive().is_err()
    }

    fn stop_queues(&self) {
        crate::sched::queue::stop_all(self.device);
    }

    fn fw_stop(&self, hard_reset: bool) -> PvrResult<()> {
        fw_disable_device(self.device, hard_reset)
    }

    fn clear_booted(&self) {
        if let Some(fw) = self.device.fw.get() {
            fw.booted.store(false, Ordering::Release);
        }
    }

    fn power_down(&self) {
        self.pm.suspend_for_reset();
    }

    fn fw_hard_reset(&self) -> PvrResult<()> {
        let fw = self.device.fw.get().ok_or(PvrError::Unsupported)?;
        fw.hard_reset(self.device)
    }

    fn power_up(&self) -> PvrResult<()> {
        self.device.quiesced.store(false, Ordering::Release);
        self.pm.resume_for_reset()
    }

    fn rebuild_rings(&self) -> PvrResult<()> {
        crate::sched::reinit(self.device)
    }

    fn clear_hwr_flags(&self) -> PvrResult<()> {
        let fw = self.device.fw.get().ok_or(PvrError::Unsupported)?;
        let at = offset_of!(RogueFwifSysdata, hwr_state_flags);
        let flags = fw.mem.sysdata.read_val::<u32>(at)?;
        fw.mem.sysdata.write_val(
            at,
            &(flags & !(abi::HWR_FW_FAULT | abi::HWR_RESTART_REQUESTED)),
        )
    }

    fn irq_clear(&self) -> PvrResult<()> {
        fw::mips::irq_clear(self.device)
    }

    fn fw_start(&self) -> PvrResult<()> {
        // Successful boot advances the firmware generation; the service
        // worker establishes a fresh watchdog baseline on its next pass.
        fw::boot(self.device)
    }

    fn start_queues(&self) {
        crate::sched::queue::start_all(self.device);
    }

    fn declare_lost(&self) {
        self.device.declare_lost();
    }
}

/// Whether the firmware is idle, given its own account and the ring's.
///
/// The decision half of [`firmware_is_idle`], separated because it is the part a
/// test can drive: the two inputs come from firmware memory that a host-side test
/// has no way to fabricate.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.c>
/// (`pvr_power_is_idle`, lines 152-163).
const fn firmware_is_idle_from(pow_state: u32, kccb_empty: bool) -> bool {
    pow_state == abi::POW_STATE_IDLE && kccb_empty
}

/// Whether the firmware may be stopped without losing work in flight.
///
/// Ports upstream `pvr_power_is_idle()`, which its `->runtime_idle` callback
/// `pvr_power_device_idle()` turns into an `-EBUSY` that refuses the suspend.
///
/// Two questions, because either alone is wrong. `pow_state` is the firmware's own
/// account of what it is doing; upstream's comment says why it does not suffice —
/// it "can be out of date if a KCCB command has been submitted but the FW hasn't
/// started processing it yet". The ring covers exactly that window.
///
/// # Why this does not reuse [`kccb_is_idle`]
///
/// Because the safe answer on a contended lock is the *opposite* one, and reusing
/// it would have inverted the guarantee silently. For the hang detector, "idle"
/// means "do not declare a hang", so contention reporting idle is the
/// conservative answer. Here "idle" means "go ahead and stop the firmware", and a
/// contended KCCB means another CPU is submitting to it *right now* — the one
/// moment the GPU must not be stopped. So everything this cannot determine —
/// contention, no ring, unreadable firmware memory — reports **not idle**, and the
/// next service pass asks again 500 ms later.
fn firmware_is_idle(device: &PvrDevice) -> bool {
    // No firmware, or one that was never started: nothing is executing, and the
    // caller is about to power down a GPU that is not running anything.
    let Some(fw) = device.fw.get() else {
        return true;
    };
    if !fw.booted.load(Ordering::Acquire) {
        return true;
    }

    let Ok(pow_state) = fw
        .mem
        .sysdata
        .read_val::<u32>(offset_of!(RogueFwifSysdata, pow_state))
    else {
        ostd::warn!("could not read the firmware's power state; leaving the GPU powered");
        return false;
    };

    // `used == 0` rather than `is_empty()`, and `is_some_and` rather than
    // `unwrap_or(true)`: a snapshot that could not be taken must not read as an
    // empty ring. See the section above.
    let kccb_empty = crate::sched::ccb::snapshot(device).is_some_and(|kccb| kccb.used == 0);

    firmware_is_idle_from(pow_state, kccb_empty)
}

/// The operations [`fw_disable`] performs, so the order can be asserted without
/// a GPU.
///
/// Sending a `POW` command and polling `power_sync` are firmware-memory
/// operations; stopping the core is a register sequence. Production drives a
/// [`PvrDevice`]; the tests drive a recorder.
trait FwDisableOps {
    fn write_power_sync(&mut self, value: u32) -> PvrResult<()>;
    fn read_power_sync(&mut self) -> PvrResult<u32>;
    fn send_pow(&mut self, command: &RogueFwifKccbCmd) -> PvrResult<()>;
    fn stop_core(&mut self) -> PvrResult<()>;
    fn power_sync_timeout_us(&self) -> u64 {
        POWER_SYNC_TIMEOUT_US
    }
}

/// Production backend for [`fw_disable`].
struct DeviceFwDisable<'a> {
    device: &'a PvrDevice,
}

impl FwDisableOps for DeviceFwDisable<'_> {
    fn write_power_sync(&mut self, value: u32) -> PvrResult<()> {
        let fw = self.device.fw.get().ok_or(PvrError::Unsupported)?;
        fw.mem.power_sync.write_val(0, &value)
    }

    fn read_power_sync(&mut self) -> PvrResult<u32> {
        let fw = self.device.fw.get().ok_or(PvrError::Unsupported)?;
        fw.mem.power_sync.read_val(0)
    }

    fn send_pow(&mut self, command: &RogueFwifKccbCmd) -> PvrResult<()> {
        crate::sched::ccb::send(self.device, command).map(|_| ())
    }

    fn stop_core(&mut self) -> PvrResult<()> {
        fw::startstop::stop(self.device)
    }
}

/// `ROGUE_FWIF_KCCB_CMD_POW` / `ROGUE_FWIF_POW_FORCED_IDLE_REQ` /
/// `ROGUE_FWIF_POWER_FORCE_IDLE`. Upstream `pvr_power_request_idle`.
fn forced_idle_command() -> RogueFwifKccbCmd {
    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = abi::KCCB_CMD_POW;
    let pow = cmd.cmd_data.pow_data_mut();
    pow.pow_type = abi::POW_FORCED_IDLE_REQ;
    *pow.power_req_data.pow_request_type_mut() = abi::POWER_FORCE_IDLE;
    cmd
}

/// `ROGUE_FWIF_KCCB_CMD_POW` / `ROGUE_FWIF_POW_OFF_REQ` / `forced = true`.
/// Upstream `pvr_power_request_pwr_off`.
fn pwr_off_command() -> RogueFwifKccbCmd {
    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = abi::KCCB_CMD_POW;
    let pow = cmd.cmd_data.pow_data_mut();
    pow.pow_type = abi::POW_OFF_REQ;
    *pow.power_req_data.forced_mut() = 1;
    cmd
}

/// Ports `pvr_power_send_command`: zero `power_sync`, send, wait for non-zero.
fn send_power_command(ops: &mut dyn FwDisableOps, command: &RogueFwifKccbCmd) -> PvrResult<()> {
    ops.write_power_sync(0)?;
    ops.send_pow(command)?;
    wait_for_power_sync(ops)
}

/// Upstream `readl_poll_timeout(..., value != 0, 100, POWER_SYNC_TIMEOUT_US)`.
///
/// A zero timeout still performs one read, matching [`PvrDevice::poll32`].
fn wait_for_power_sync(ops: &mut dyn FwDisableOps) -> PvrResult<()> {
    let timeout_us = ops.power_sync_timeout_us();
    let mut waited_us = 0u64;
    loop {
        if ops.read_power_sync()? != 0 {
            return Ok(());
        }
        if waited_us >= timeout_us {
            return Err(PvrError::Timeout);
        }
        fw::delay_us(POWER_SYNC_POLL_INTERVAL_US);
        waited_us = waited_us.saturating_add(POWER_SYNC_POLL_INTERVAL_US);
    }
}

/// Ports `pvr_power_fw_disable`.
///
/// A hard reset skips the KCCB handshake and stops the core immediately. A soft
/// stop sends FORCED_IDLE and POW_OFF. If either acknowledgement times out, this
/// port reports the error and still stops the core; upstream returns the error
/// and leaves the device powered.
///
/// FORCED_IDLE is required even when `firmware_is_idle` reports POW_IDLE.
/// Upstream orders the same handshake after its runtime-idle check and before
/// the platform power-down sequence.
fn fw_disable(ops: &mut dyn FwDisableOps, hard_reset: bool) -> PvrResult<()> {
    if !hard_reset {
        if let Err(err) = send_power_command(ops, &forced_idle_command()) {
            ostd::debug!(
                "[k1-gpu] firmware did not acknowledge FORCED_IDLE ({:?}); \
                 stopping the core anyway",
                err
            );
        } else if let Err(err) = send_power_command(ops, &pwr_off_command()) {
            ostd::debug!(
                "[k1-gpu] firmware did not acknowledge POW_OFF ({:?}); \
                 stopping the core anyway",
                err
            );
        }
    }

    ops.stop_core()
}

/// [`fw_disable`] against a live device.
fn fw_disable_device(device: &PvrDevice, hard_reset: bool) -> PvrResult<()> {
    fw_disable(&mut DeviceFwDisable { device }, hard_reset)
}

/// Reports the state behind a suspected firmware hang.
///
/// Ring offsets, pending command types, and firmware power state distinguish
/// outstanding submissions from a firmware that has not acknowledged idle.
/// Reporting is bounded per power cycle; these snapshots do not establish a
/// hardware root cause by themselves.
fn report_stall_evidence(device: &PvrDevice, executed: u32) {
    if !aster_cmdline::diag_enabled("gpu") {
        return;
    }
    if STALL_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let pow_state = device.fw.get().and_then(|fw| {
        fw.mem
            .sysdata
            .read_val::<u32>(offset_of!(RogueFwifSysdata, pow_state))
            .ok()
    });

    let pow_state = pow_state.unwrap_or(u32::MAX);

    match crate::sched::ccb::snapshot(device) {
        Some(kccb) => {
            ostd::debug!(
                "[k1-gpu] hang evidence: kccb_cmds_executed={} read_offset={} write_offset={} \
                 used={} reserved={} next_cmd=0x{:08x} ({}) pow_state={} ({})",
                executed,
                kccb.read_offset,
                kccb.write_offset,
                kccb.used,
                kccb.reserved,
                kccb.next_cmd_type.unwrap_or(0),
                kccb.next_cmd_type
                    .map_or("ring is empty", abi::kccb_cmd_name),
                pow_state,
                abi::pow_state_name(pow_state)
            );
            // The whole queue, not just its head. What the driver asked for in
            // what order is the other half of a stall: a `KICK` waiting behind
            // an unfinished cache flush is a different fault from a `KICK` the
            // firmware simply will not take.
            if kccb.pending_len > 1 {
                let mut queue = alloc::string::String::new();
                for (index, cmd_type) in kccb.pending[..kccb.pending_len].iter().enumerate() {
                    use core::fmt::Write as _;
                    let slot = (kccb.read_offset as usize + index) as u32;
                    let _ = write!(queue, " [{slot}]={}", abi::kccb_cmd_name(*cmd_type));
                }
                ostd::debug!(
                    "[k1-gpu] hang evidence: unconsumed commands, oldest first:{}",
                    queue
                );
            }
        }
        // Worth a line of its own: a ring that cannot be read is itself the
        // finding, and without this the report would simply be absent.
        None => ostd::debug!(
            "[k1-gpu] hang evidence: kccb_cmds_executed={} pow_state={} ({}); the kernel CCB \
             could not be read (no ring, contended lock, or unreadable firmware memory)",
            executed,
            pow_state,
            abi::pow_state_name(pow_state)
        ),
    }
}

/// Whether [`report_stall_evidence`] has spoken since the GPU was last powered.
static STALL_REPORTED: AtomicBool = AtomicBool::new(false);

/// Says once that a power-down was refused because the firmware is not idle.
///
/// Worth a line because of how this fails if it fails: a firmware that never
/// reports [`abi::POW_STATE_IDLE`] again leaves the GPU powered for good, and the
/// only other symptom is the *absence* of the `[k1-ccu]` power-down lines. An
/// absence is not something anyone notices. With this, the refusal names itself
/// and says what it is waiting for.
///
/// At most once per power cycle, like [`report_stall_evidence`] and for the same
/// reason: the service thread asks again every 500 ms, and the answer does not
/// change between two of them.
fn report_suspend_refused(device: Option<&PvrDevice>) {
    if SUSPEND_REFUSED_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }

    let pow_state = device
        .and_then(|device| device.fw.get())
        .and_then(|fw| {
            fw.mem
                .sysdata
                .read_val::<u32>(offset_of!(RogueFwifSysdata, pow_state))
                .ok()
        })
        .unwrap_or(u32::MAX);

    ostd::debug!(
        "[k1-gpu] not powering the GPU down: the firmware is not idle (pow_state={} ({})). \
         Retrying every {} ms; if the power-down lines never appear again, this is why.",
        pow_state,
        abi::pow_state_name(pow_state),
        WATCHDOG_INTERVAL_MS
    );
}

/// Whether [`report_suspend_refused`] has spoken since the GPU was last powered.
static SUSPEND_REFUSED_REPORTED: AtomicBool = AtomicBool::new(false);

/// Samples KCCB idleness without waiting behind a submitting thread.
/// An unavailable snapshot is treated as idle for watchdog purposes, avoiding a
/// hang diagnosis based on missing evidence. Suspend uses the opposite fallback.
pub(crate) fn kccb_is_idle(device: &PvrDevice) -> bool {
    // No outstanding KCCB command means the firmware has no pending host command
    // to execute; an idle watchdog sample must not treat that as stalled work.
    let Some(kccb) = device.kccb.get() else {
        return true;
    };
    let Some(kccb) = kccb.try_lock() else {
        return true;
    };
    kccb.ccb().is_empty().unwrap_or(true)
}

/// The power manager, installed by the platform once it has found a GPU.
static PM: Once<Pm> = Once::new();

/// Installs the power manager for the GPU this kernel found.
///
/// Called by [`crate::platform::k1::probe`] before it touches a control
/// register, since reading one requires the GPU to be powered. A second call is
/// ignored: like [`crate::driver::register`], this driver supports one GPU.
pub fn install(pm: Pm) {
    if PM.is_completed() {
        ostd::warn!("a power manager is already installed; ignoring the second");
        return;
    }
    PM.call_once(|| pm);
}

/// Stops the GPU before publishing an unrecoverable error.
///
/// Must run in a sleepable context without holding a reset, queue or VM lock.
pub(crate) fn lose_device(device: &PvrDevice) {
    if let Some(pm) = PM.get() {
        pm.lose_device(device);
    } else {
        // No platform can perform a stop; the quiescence state remains unchanged.
        device.declare_lost();
    }
}

/// The installed power manager.
pub fn manager() -> Option<&'static Pm> {
    PM.get()
}

/// The installed power manager, or an error naming what is missing.
fn require_manager() -> PvrResult<&'static Pm> {
    PM.get().ok_or_else(|| {
        ostd::error!("no power manager: the platform has not probed a GPU");
        PvrError::Unsupported
    })
}

/// Powers up the GPU and boots the firmware if it is not already running.
///
/// Idempotent and reference counted: every path that touches the GPU calls this
/// and releases it afterwards.
///
/// Prefer [`PowerRef::take`] at call sites that can use a scope guard; the raw
/// pair is for the few places that need an explicit mismatch (for example the
/// probe path that powers up before a [`PvrDevice`] exists).
pub fn get(device: &PvrDevice) -> PvrResult<()> {
    device.check_alive()?;
    require_manager()?.get(Some(device))
}

/// Releases a reference taken by [`get`].
pub fn put(device: &PvrDevice) {
    if let Some(pm) = PM.get() {
        pm.put(Some(device));
    }
}

/// Resets the GPU, declaring it lost if the reset fails.
///
/// The port of upstream `pvr_power_reset()`. **Sleeps**, for as long as a
/// firmware boot takes: legal from an ioctl, never from the interrupt handler's
/// deferred half or a fence callback. Those use [`request_reset`].
///
/// The difference from [`crate::device::PvrDevice::declare_lost`] is what user
/// space sees. After a successful reset only the contexts that had work in
/// flight are faulty, and every other client carries on; after `declare_lost`
/// every ioctl on the device returns `EIO` until the driver is reloaded.
pub fn reset(device: &PvrDevice, kind: ResetKind) -> PvrResult<()> {
    require_manager()?.reset(device, kind)
}

/// Records a reset request and wakes the sleepable service worker.
///
/// [`PowerRef::take`] also processes a pending request before accepting more work.
pub fn request_reset(device: &PvrDevice, kind: ResetKind) {
    device.reset_request.request(kind);
    crate::service::request_wake(device);
}

/// Lets the GPU power down if the deferred releases have left nothing holding it.
///
/// The other half of [`Pm::put_deferred`], which only decrements. Sleeps, so this
/// is the service thread's and the ioctl path's to call, never the deferred
/// half's. Does nothing until the autosuspend delay has passed; see
/// [`AUTOSUSPEND_DELAY_MS`].
pub fn settle(device: &PvrDevice) {
    if let Some(pm) = PM.get() {
        pm.settle(Some(device));
    }
}

/// Allows power-downs to be postponed by [`AUTOSUSPEND_DELAY_MS`].
///
/// Called by [`crate::service::run`], because that thread is what performs a
/// postponed power-down. See [`Pm::use_autosuspend`].
pub fn use_autosuspend() {
    if let Some(pm) = PM.get() {
        pm.use_autosuspend();
    }
}

/// Records that the GPU was used just now, so the autosuspend delay starts here.
///
/// Callable from the interrupt handler's deferred half, which is where upstream's
/// per-event call is; see [`Pm::mark_last_busy`].
pub fn mark_last_busy() {
    if let Some(pm) = PM.get() {
        pm.mark_last_busy();
    }
}

/// Whether a power-down is waiting only for the autosuspend delay.
///
/// Read by the service thread's timer callback, in interrupt context, to decide
/// whether to ask for a pass before the watchdog cadence would. No power manager
/// means no GPU to power down.
pub fn suspend_is_due() -> bool {
    PM.get().is_some_and(Pm::suspend_is_due)
}

/// Runs a reset asked for by [`request_reset`], if one is outstanding.
///
/// Called from [`PowerRef::take`], which is the first thing every ioctl that
/// touches the firmware does — so the request is serviced before the caller
/// gives the GPU any more work, which is the whole point of servicing it here
/// rather than after — and from the service thread, which is what makes it happen
/// without an ioctl at all.
pub fn service_pending_reset(device: &PvrDevice) -> PvrResult<()> {
    let Some(kind) = device.reset_request.take() else {
        return Ok(());
    };
    ostd::debug!(
        "[k1-gpu] running a {:?} GPU reset asked for from interrupt context",
        kind
    );
    reset(device, kind)
}

/// A runtime-PM reference held for the lifetime of this value.
///
/// Ports the pairing of upstream `pvr_power_get()` / `pvr_power_put()`. Taking
/// one may power the GPU up and boot the firmware; dropping it releases the
/// reference, and when it was the last one the GPU powers down
/// [`AUTOSUSPEND_DELAY_MS`] later rather than immediately — so a client that
/// makes a burst of calls, or one call per frame, boots the firmware once.
///
/// Must not be taken while holding a queue or context lock: [`Pm::get`] takes a
/// sleeping mutex and can boot the firmware. Must not be taken from interrupt
/// or fence-callback context for the same reason — see [`crate::sched::ccb`].
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_power.h>
/// (`pvr_power_get`, `pvr_power_put`).
pub struct PowerRef<'a> {
    device: &'a PvrDevice,
}

impl<'a> PowerRef<'a> {
    /// Takes a reference, powering the GPU up if this is the first one.
    ///
    /// Also the point at which a reset asked for from interrupt context is run;
    /// see [`service_pending_reset`]. The reference is held across it, as
    /// upstream's `pvr_power_reset()` holds its own, and released with this
    /// guard if the reset fails.
    pub fn take(device: &'a PvrDevice) -> PvrResult<Self> {
        get(device)?;
        let reference = Self { device };
        service_pending_reset(device)?;
        Ok(reference)
    }
}

impl Drop for PowerRef<'_> {
    fn drop(&mut self) {
        put(self.device);
    }
}

/// A runtime-PM reference owned by a job, for as long as the job may be running.
///
/// Ports upstream's `pvr_job::has_pm_ref` and the
/// `pvr_job_get_pm_ref()` / `pvr_job_release_pm_ref()` pair that maintains it.
/// The point of it is that a job outlives the ioctl that submitted it: the
/// ioctl's own [`PowerRef`] is gone as soon as `SUBMIT_JOBS` returns, while the
/// firmware may still be running the job minutes later.
///
/// Two differences from [`PowerRef`], both forced by where each end lands in
/// this port:
///
/// - **Taking one does not boot the firmware** — see [`Pm::get_for_job`]. It is
///   taken in [`crate::sched::queue::Queue::push`], the last point on the
///   submission path that is allowed to sleep, and always inside the ioctl's
///   reference.
/// - **Dropping one does not power the GPU down.** It goes through
///   [`Pm::put_deferred`], because a job's fence is terminated from
///   [`crate::sched::queue::Queue::process`],
///   [`crate::sched::queue::Queue::check_timeout`] and
///   [`crate::sched::queue::Queue::fail_all`], and the first two of those are
///   reachable from a fence callback, which may be interrupt context. The
///   power-down happens at the first [`Pm::settle`] that finds the autosuspend
///   delay expired, which for a job that retires from an interrupt is a
///   [`crate::service`] pass.
///
/// Unlike [`PowerRef`] this borrows nothing, because it is stored inside a
/// queue's pending list and must outlive every stack frame on the submission
/// path.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_job.h>
/// (`pvr_job_get_pm_ref`, `pvr_job_release_pm_ref`).
pub struct JobPowerRef {
    /// Whether a reference was actually taken, so that [`Drop`] cannot release
    /// one that was never held.
    held: bool,
}

impl JobPowerRef {
    /// Takes a reference that lasts until the job retires.
    ///
    /// Must not be called with a queue lock held, and must not be called from
    /// interrupt or fence-callback context: [`Pm::get_for_job`] takes the
    /// sleeping transition mutex.
    pub fn take(device: &PvrDevice) -> PvrResult<Self> {
        device.check_alive()?;
        match PM.get() {
            Some(pm) => {
                pm.get_for_job()?;
                Ok(Self { held: true })
            }
            // No power manager means no runtime PM: nothing exists that could
            // power the GPU down under the job, so there is nothing to hold.
            // The same reasoning [`put`] already relies on, from the other side.
            None => Ok(Self { held: false }),
        }
    }

    /// Whether this guard is actually holding a reference.
    ///
    /// Diagnostic, and what the tests assert against.
    pub fn is_held(&self) -> bool {
        self.held
    }
}

impl Drop for JobPowerRef {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        if let Some(pm) = PM.get() {
            pm.put_deferred();
        }
    }
}

/// Takes a reference before there is a [`PvrDevice`] to attach it to.
///
/// The platform needs the GPU powered in order to read the BVNC that decides
/// whether a device exists; see [`Pm::get`].
pub fn get_unattached() -> PvrResult<()> {
    require_manager()?.get(None)
}

/// Releases a reference taken by [`get_unattached`], powering the GPU down now.
///
/// Not the autosuspend variant: this runs during probe and attach, before any
/// service thread exists, so a postponed power-down would never be performed.
/// It is also what puts the reverse sequence in the boot log, which is the only
/// place its order can be observed on hardware.
pub fn put_unattached() {
    if let Some(pm) = PM.get() {
        pm.put_now(None);
    }
}

/// Checks whether the GPU has stopped making progress.
///
/// The service worker may call this on every wake. The detector enforces the
/// sampling interval independently of cleanup, completion and reset wakeups.
pub fn check_for_hang(device: &PvrDevice) {
    if let Some(pm) = PM.get() {
        pm.check_for_hang(device);
    }
}

/// Recording platform hooks verify power-sequence order and reference counts.
/// They do not emulate MMIO or validate the electrical sequence on hardware.
#[cfg(any(test, ktest))]
mod tests {
    use alloc::{vec, vec::Vec};

    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// A platform that records what it was asked to do, in order.
    struct Recorder {
        steps: SpinLock<Vec<Step>>,
        /// A step that fails, for the unwind test.
        fail_at: Option<Step>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                steps: SpinLock::new(Vec::new()),
                fail_at: None,
            })
        }

        fn failing_at(step: Step) -> Arc<Self> {
            Arc::new(Self {
                steps: SpinLock::new(Vec::new()),
                fail_at: Some(step),
            })
        }

        fn record(&self, step: Step) -> PvrResult<()> {
            self.steps.lock().push(step);
            if self.fail_at == Some(step) {
                return Err(PvrError::Timeout);
            }
            Ok(())
        }

        fn steps(&self) -> Vec<Step> {
            self.steps.lock().clone()
        }
    }

    impl PlatformOps for Recorder {
        fn power_domain_on(&self) -> PvrResult<()> {
            self.record(Step::PowerDomainOn)
        }

        fn clock_enable(&self) -> PvrResult<()> {
            self.record(Step::ClockEnable)
        }

        fn reset_deassert(&self) -> PvrResult<()> {
            self.record(Step::ResetDeassert)
        }

        fn reset_assert(&self) {
            let _ = self.record(Step::ResetAssert);
        }

        fn clock_disable(&self) {
            let _ = self.record(Step::ClockDisable);
        }

        fn power_domain_off(&self) {
            let _ = self.record(Step::PowerDomainOff);
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn power_up_is_domain_then_clock_then_reset() {
        let ops = Recorder::new();
        power_up(&*ops).unwrap();
        assert_eq!(
            ops.steps(),
            vec![Step::PowerDomainOn, Step::ClockEnable, Step::ResetDeassert]
        );
    }

    /// Check reverse power-down ordering independently of the power-up path.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn sequence_is_a_palindrome() {
        let ops = Recorder::new();
        power_up(&*ops).unwrap();
        let up = ops.steps();

        let ops = Recorder::new();
        power_down(&*ops);
        let down = ops.steps();

        assert_eq!(up.len(), down.len());
        let mirrored: Vec<Step> = up.iter().rev().map(|step| step.inverse()).collect();
        assert_eq!(down, mirrored);
        // Spelled out as well, so that a reader does not have to trust
        // `inverse()` to know what the hardware is asked to do.
        assert_eq!(
            down,
            vec![Step::ResetAssert, Step::ClockDisable, Step::PowerDomainOff]
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_failed_step_unwinds_the_completed_ones() {
        // Releasing reset fails: the clock and the domain must both be undone,
        // in reverse, and nothing must be left half-enabled.
        let ops = Recorder::failing_at(Step::ResetDeassert);
        assert_eq!(power_up(&*ops), Err(PvrError::Timeout));
        assert_eq!(
            ops.steps(),
            vec![
                Step::PowerDomainOn,
                Step::ClockEnable,
                Step::ResetDeassert,
                Step::ClockDisable,
                Step::PowerDomainOff,
            ]
        );

        // The first step failing leaves nothing to undo.
        let ops = Recorder::failing_at(Step::PowerDomainOn);
        assert_eq!(power_up(&*ops), Err(PvrError::Timeout));
        assert_eq!(ops.steps(), vec![Step::PowerDomainOn]);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn only_the_first_reference_powers_up_and_only_the_last_powers_down() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());

        assert!(!pm.is_powered());
        pm.get(None).unwrap();
        assert_eq!(pm.references(), 1);
        assert!(pm.is_powered());
        assert_eq!(ops.steps().len(), 3);

        pm.get(None).unwrap();
        pm.get(None).unwrap();
        assert_eq!(pm.references(), 3);
        // Still three steps: the second and third references touched nothing.
        assert_eq!(ops.steps().len(), 3);

        pm.put(None);
        pm.put(None);
        assert_eq!(pm.references(), 1);
        assert!(pm.is_powered());
        assert_eq!(ops.steps().len(), 3);

        pm.put(None);
        assert_eq!(pm.references(), 0);
        assert!(!pm.is_powered());
        assert_eq!(
            ops.steps(),
            vec![
                Step::PowerDomainOn,
                Step::ClockEnable,
                Step::ResetDeassert,
                Step::ResetAssert,
                Step::ClockDisable,
                Step::PowerDomainOff,
            ]
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_unbalanced_put_does_not_power_down_twice() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());

        pm.get(None).unwrap();
        pm.put(None);
        assert_eq!(pm.references(), 0);

        // One more `put` than `get`. The count must not wrap round to
        // `u32::MAX`, which would make every later `put` a no-op and leave the
        // GPU powered for good.
        pm.put(None);
        assert_eq!(pm.references(), 0);
        assert_eq!(ops.steps().len(), 6);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_failed_power_up_leaves_no_reference_behind() {
        let ops = Recorder::failing_at(Step::ClockEnable);
        let pm = Pm::new(ops.clone());

        assert_eq!(pm.get(None), Err(PvrError::Timeout));
        assert_eq!(pm.references(), 0);
        assert!(!pm.is_powered());
    }

    /// A deferred release decrements the reference without sleeping. The service
    /// worker performs any resulting power transition after the autosuspend delay.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_deferred_put_leaves_the_gpu_up_until_something_settles_it() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());

        pm.get(None).unwrap();
        assert_eq!(ops.steps().len(), 3);

        pm.put_deferred();
        assert_eq!(pm.references(), 0);
        assert!(pm.is_powered());
        assert_eq!(ops.steps().len(), 3);

        pm.settle(None);
        assert!(!pm.is_powered());
        assert_eq!(ops.steps().len(), 6);

        // Settling twice is not a second power-down. The count is already zero,
        // so a `settle` that did not check `powered` as well would run the
        // sequence again on every ioctl for the rest of the device's life.
        pm.settle(None);
        assert_eq!(ops.steps().len(), 6);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_job_reference_taken_before_the_settle_keeps_the_gpu_up() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());

        pm.get(None).unwrap();
        pm.put_deferred();

        // The ioctl has gone but a job is still in flight. The GPU was never
        // actually powered down, so taking the job's reference must not run the
        // sequence a second time either.
        pm.get_for_job().unwrap();
        assert_eq!(pm.references(), 1);
        pm.settle(None);
        assert!(pm.is_powered());
        assert_eq!(ops.steps().len(), 3);

        pm.put(None);
        assert!(!pm.is_powered());
        assert_eq!(ops.steps().len(), 6);
    }

    /// Check the autosuspend delay using explicit timestamp arithmetic.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn the_autosuspend_delay_expires_exactly_once_it_has_elapsed() {
        const MS: u64 = 1_000_000;
        let delay = AUTOSUSPEND_DELAY_MS * MS;

        // Upstream's number, asserted here because every wiring test below
        // overrides the delay and so cannot notice a `Pm::new` that installed
        // none at all.
        assert_eq!(AUTOSUSPEND_DELAY_MS, 50);
        assert_eq!(crate::knobs::DEFAULT_IDLE_MS, AUTOSUSPEND_DELAY_MS);
        assert_eq!(
            crate::knobs::autosuspend_delay_ns(),
            AUTOSUSPEND_DELAY_MS * MS
        );
        assert_eq!(Pm::new(Recorder::new()).delay_ns(), delay);

        assert!(!autosuspend_expired(0, 0, delay));
        assert!(!autosuspend_expired(delay - 1, 0, delay));
        assert!(autosuspend_expired(delay, 0, delay));
        assert!(autosuspend_expired(u64::MAX, 0, delay));

        // Measured from `last_busy`, not from boot.
        assert!(!autosuspend_expired(1_000 * MS, 1_000 * MS, delay));
        assert!(autosuspend_expired(1_000 * MS + delay, 1_000 * MS, delay));

        // A clock that appears to run backwards keeps the GPU powered rather than
        // powering it down under whatever is using it.
        assert!(!autosuspend_expired(0, u64::MAX, delay));

        // The two delays the wiring tests below use, so that "at once" and "never"
        // mean what they are relied on to mean.
        assert!(autosuspend_expired(0, 0, 0));

        // "Never" is a statement about every uptime this machine can reach, not
        // about the arithmetic: a delay of `u64::MAX` does expire at exactly
        // `now == u64::MAX`, because the comparison is `>=`. A century in
        // nanoseconds is 3.2e18 and `u64::MAX` is 1.8e19, so the distinction
        // cannot be reached by a running system — but it is reachable by a test,
        // and the first version of this one asserted the false half of it.
        const CENTURY_NS: u64 = 100 * 365 * 24 * 60 * 60 * 1_000_000_000;
        assert!(!autosuspend_expired(CENTURY_NS, 0, u64::MAX));
    }

    /// Verify both sides of the autosuspend deadline: no early power-down, then
    /// a power-down once due. Testing only the first case would also accept a GPU
    /// that never powers down.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn with_autosuspend_the_last_reference_going_does_not_power_down_yet() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());
        pm.use_autosuspend();
        pm.set_delay_ns(u64::MAX);

        pm.get(None).unwrap();
        assert_eq!(ops.steps().len(), 3);

        pm.put(None);
        assert_eq!(pm.references(), 0);
        assert!(pm.is_powered());
        assert_eq!(ops.steps().len(), 3);
        assert!(!pm.suspend_is_due());

        // A second client arriving inside the delay is the case this exists for:
        // it must not have to wait for a power sequence, let alone a firmware boot.
        pm.get(None).unwrap();
        assert_eq!(ops.steps().len(), 3);
        pm.put(None);
        assert_eq!(ops.steps().len(), 3);

        // Once the delay is behind us the power-down is due, and a pass performs it.
        pm.set_delay_ns(0);
        assert!(pm.suspend_is_due());
        pm.settle(None);
        assert!(!pm.is_powered());
        assert_eq!(ops.steps().len(), 6);
        assert!(!pm.suspend_is_due());
    }

    /// A reference taken inside the delay window cancels the power-down.
    ///
    /// The delay is not a timer that fires regardless: the count is re-read under
    /// the transition lock, so a `settle` that arrives once the delay is up but
    /// after a new reference too must do nothing.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_reference_taken_during_the_delay_cancels_the_power_down() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());
        pm.use_autosuspend();
        pm.set_delay_ns(u64::MAX);

        pm.get(None).unwrap();
        pm.put(None);

        pm.set_delay_ns(0);
        assert!(pm.suspend_is_due());

        pm.get(None).unwrap();
        assert!(!pm.suspend_is_due());
        pm.settle(None);
        assert!(pm.is_powered());
        assert_eq!(ops.steps().len(), 3);
    }

    /// Without a service thread nothing would ever perform a postponed
    /// power-down, so the delay is skipped instead of honoured.
    ///
    /// This is what keeps the probe path — `power::put_unattached`, which runs
    /// before any thread exists — from leaving the GPU powered for good, and it is
    /// why every other test in this module needs no `use_autosuspend` call.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn without_autosuspend_the_last_reference_going_powers_down_at_once() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());
        pm.set_delay_ns(u64::MAX);

        pm.get(None).unwrap();
        pm.put(None);
        assert!(!pm.is_powered());
        assert_eq!(ops.steps().len(), 6);
    }

    /// `put_now` is the probe path's variant and ignores the delay even when
    /// autosuspend is on.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn put_now_powers_down_inside_the_delay() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());
        pm.use_autosuspend();
        pm.set_delay_ns(u64::MAX);

        pm.get(None).unwrap();
        pm.put_now(None);
        assert!(!pm.is_powered());
        assert_eq!(ops.steps().len(), 6);
    }

    /// Upstream's suspend gate, whose surprising half is which state it accepts.
    ///
    /// `POW_OFF` is documented upstream as "idle and ready to full power down" and
    /// is nevertheless **not** accepted by `pvr_power_is_idle()`; only `POW_IDLE`
    /// is. Pinned here because the names invite the opposite conclusion, and
    /// because getting it wrong in the permissive direction means stopping a
    /// firmware that still has work — which produces no error, only lost work.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn only_a_firmware_that_reports_pow_idle_with_an_empty_ring_may_be_stopped() {
        assert!(firmware_is_idle_from(abi::POW_STATE_IDLE, true));

        // Both halves are required.
        assert!(!firmware_is_idle_from(abi::POW_STATE_IDLE, false));
        assert!(!firmware_is_idle_from(abi::POW_STATE_ON, true));
        assert!(!firmware_is_idle_from(abi::POW_STATE_ON, false));

        // The two states that sound idle and are not accepted.
        assert!(!firmware_is_idle_from(abi::POW_STATE_OFF, true));
        assert!(!firmware_is_idle_from(abi::POW_STATE_FORCED_IDLE, true));

        // A value the firmware should never report reads as busy, not as idle:
        // an unreadable or garbled state must not authorise stopping the core.
        assert!(!firmware_is_idle_from(u32::MAX, true));
    }

    /// Stand-in for the firmware half of [`super::fw_disable`].
    ///
    /// `ack` is whether a send makes `power_sync` non-zero. The wait timeout is
    /// zero so a silent firmware fails on the first poll rather than spinning
    /// for [`POWER_SYNC_TIMEOUT_US`].
    struct HandshakeRecorder {
        sent: Vec<RogueFwifKccbCmd>,
        ack: bool,
        power_sync: u32,
        stopped: bool,
    }

    impl HandshakeRecorder {
        fn acknowledging() -> Self {
            Self {
                sent: Vec::new(),
                ack: true,
                power_sync: 0,
                stopped: false,
            }
        }

        fn silent() -> Self {
            Self {
                ack: false,
                ..Self::acknowledging()
            }
        }
    }

    impl FwDisableOps for HandshakeRecorder {
        fn write_power_sync(&mut self, value: u32) -> PvrResult<()> {
            self.power_sync = value;
            Ok(())
        }

        fn read_power_sync(&mut self) -> PvrResult<u32> {
            Ok(self.power_sync)
        }

        fn send_pow(&mut self, command: &RogueFwifKccbCmd) -> PvrResult<()> {
            self.sent.push(*command);
            if self.ack {
                self.power_sync = 1;
            }
            Ok(())
        }

        fn stop_core(&mut self) -> PvrResult<()> {
            self.stopped = true;
            Ok(())
        }

        fn power_sync_timeout_us(&self) -> u64 {
            0
        }
    }

    fn assert_forced_idle(cmd: &RogueFwifKccbCmd) {
        assert_eq!(cmd.cmd_type, abi::KCCB_CMD_POW);
        let pow = cmd.cmd_data.pow_data();
        assert_eq!(pow.pow_type, abi::POW_FORCED_IDLE_REQ);
        assert_eq!(
            *pow.power_req_data.pow_request_type(),
            abi::POWER_FORCE_IDLE
        );
    }

    fn assert_pwr_off(cmd: &RogueFwifKccbCmd) {
        assert_eq!(cmd.cmd_type, abi::KCCB_CMD_POW);
        let pow = cmd.cmd_data.pow_data();
        assert_eq!(pow.pow_type, abi::POW_OFF_REQ);
        assert_eq!(*pow.power_req_data.forced(), 1);
    }

    /// Check both power commands in the order required by the firmware protocol.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn the_firmware_disable_handshake_sends_forced_idle_then_power_off() {
        assert_eq!(POWER_SYNC_TIMEOUT_US, 1_000_000);
        assert_eq!(POWER_SYNC_POLL_INTERVAL_US, 100);

        let mut ops = HandshakeRecorder::acknowledging();
        assert_eq!(fw_disable(&mut ops, false), Ok(()));
        assert_eq!(ops.sent.len(), 2);
        assert_forced_idle(&ops.sent[0]);
        assert_pwr_off(&ops.sent[1]);
        assert!(ops.stopped);
    }

    /// A zero `power_sync` eventually returns timeout while still stopping the core.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_power_sync_that_never_becomes_nonzero_times_out_and_still_stops_the_core() {
        let mut ops = HandshakeRecorder::silent();
        assert_eq!(fw_disable(&mut ops, false), Ok(()));
        assert_eq!(ops.sent.len(), 1);
        assert_forced_idle(&ops.sent[0]);
        assert!(ops.stopped);
    }

    /// Upstream `pvr_power_fw_disable` skips both commands when `hard_reset`.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_hard_reset_sends_neither_power_command() {
        let mut ops = HandshakeRecorder::acknowledging();
        assert_eq!(fw_disable(&mut ops, true), Ok(()));
        assert!(ops.sent.is_empty());
        assert!(ops.stopped);
    }

    /// A power-down is not due while the GPU is already down.
    ///
    /// Read by the timer callback on every tick, so a version that ignored
    /// `powered` would ask for a pass a thousand times a second on an idle
    /// device — the cost being a wake of the service thread each time.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_powered_down_gpu_never_has_a_suspend_due() {
        let ops = Recorder::new();
        let pm = Pm::new(ops.clone());
        pm.use_autosuspend();
        pm.set_delay_ns(0);

        assert!(!pm.is_powered());
        assert!(!pm.suspend_is_due());

        // And it is due again as soon as one is taken and given back, so the
        // assertion above is about `powered` and not about a delay that never
        // expires.
        pm.get(None).unwrap();
        pm.put_deferred();
        assert!(pm.suspend_is_due());
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn progress_resets_the_stall_count() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;

        assert_eq!(detector.sample(0, 1, 10, false), None);
        assert_eq!(
            detector.sample(interval, 1, 10, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample(2 * interval, 1, 11, false),
            Some(HangVerdict::Progressing)
        );
        assert_eq!(
            detector.sample(3 * interval, 1, 11, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample(4 * interval, 1, 11, false),
            Some(HangVerdict::Stalled)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_idle_kccb_is_never_a_hang() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;
        assert_eq!(detector.sample(0, 1, 7, true), None);
        let idle_polls = u64::from(STALL_POLLS_BEFORE_HUNG) * 10;
        for poll in 1..=idle_polls {
            assert_eq!(
                detector.sample(poll * interval, 1, 7, true),
                Some(HangVerdict::Idle)
            );
        }
        assert_eq!(
            detector.sample((idle_polls + 1) * interval, 1, 7, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample((idle_polls + 2) * interval, 1, 7, false),
            Some(HangVerdict::Stalled)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_stall_is_declared_once_and_then_starts_over() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;
        assert_eq!(detector.sample(0, 1, 3, false), None);
        for poll in 1..=4 {
            let verdict = if poll % 2 == 0 {
                HangVerdict::Stalled
            } else {
                HangVerdict::Suspect
            };
            assert_eq!(detector.sample(poll * interval, 1, 3, false), Some(verdict));
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn watchdog_extra_wakes_do_not_advance_sample_history() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;
        assert_eq!(detector.sample(0, 1, 10, false), None);

        // Completion and autosuspend wakes can arrive each millisecond. Even
        // different counter/idle inputs before the deadline must be ignored.
        for now in (1..interval).step_by(1_000_000) {
            assert!(!detector.sample_due(now, 1));
            assert_eq!(detector.sample(now, 1, 99, true), None);
        }
        assert_eq!(detector.sample(interval - 1, 1, 99, true), None);
        assert!(detector.sample_due(interval, 1));
        assert_eq!(
            detector.sample(interval, 1, 10, false),
            Some(HangVerdict::Suspect)
        );
        for now in (interval + 1..2 * interval).step_by(1_000_000) {
            assert_eq!(detector.sample(now, 1, 99, true), None);
        }
        assert_eq!(
            detector.sample(2 * interval, 1, 10, false),
            Some(HangVerdict::Stalled)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn watchdog_late_wakes_do_not_catch_up_missed_samples() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;
        assert_eq!(detector.sample(0, 1, 10, false), None);
        let late = 10 * interval;
        assert_eq!(
            detector.sample(late, 1, 10, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(detector.sample(late, 1, 10, false), None);
        assert_eq!(detector.sample(late + interval - 1, 1, 10, false), None);
        // A clock value before the previous sample cannot wrap into expiry.
        assert_eq!(detector.sample(late - 1, 1, 10, false), None);
        assert_eq!(
            detector.sample(late + interval, 1, 10, false),
            Some(HangVerdict::Stalled)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn watchdog_new_boot_discards_old_suspicion() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;
        assert_eq!(detector.sample(0, 1, 10, false), None);
        assert_eq!(
            detector.sample(interval, 1, 10, false),
            Some(HangVerdict::Suspect)
        );

        // The new firmware may reach exactly the old counter value. Its boot
        // generation, not the counter alone, identifies a fresh baseline.
        let rebooted = interval + 1;
        assert!(detector.sample_due(rebooted, 2));
        assert_eq!(detector.sample(rebooted, 2, 10, false), None);
        assert_eq!(detector.sample(rebooted + interval - 1, 2, 10, false), None);
        assert_eq!(
            detector.sample(rebooted + interval, 2, 10, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample(rebooted + 2 * interval, 2, 10, false),
            Some(HangVerdict::Stalled)
        );

        // A zero command counter still needs two full intervals after baseline.
        let rebooted = rebooted + 2 * interval + 1;
        assert_eq!(detector.sample(rebooted, 3, 0, false), None);
        assert_eq!(
            detector.sample(rebooted + interval, 3, 0, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample(rebooted + 2 * interval, 3, 0, false),
            Some(HangVerdict::Stalled)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn watchdog_idle_breaks_consecutive_stall_evidence() {
        let mut detector = HangDetector::new();
        let interval = WATCHDOG_INTERVAL_NS;
        assert_eq!(detector.sample(0, 1, 10, false), None);
        assert_eq!(
            detector.sample(interval, 1, 10, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample(2 * interval, 1, 10, true),
            Some(HangVerdict::Idle)
        );
        assert_eq!(
            detector.sample(3 * interval, 1, 10, false),
            Some(HangVerdict::Suspect)
        );
        assert_eq!(
            detector.sample(4 * interval, 1, 10, false),
            Some(HangVerdict::Stalled)
        );
    }

    /// A device that records the reset steps it is asked to take, in order.
    ///
    /// The counterpart of [`Recorder`], and for the same reason: the reset order
    /// cannot be checked against hardware, because a wrong one produces a GPU
    /// that accepts commands and renders nothing.
    struct ResetRecorder {
        steps: SpinLock<Vec<ResetStep>>,
        lost: AtomicBool,
        /// How many more times [`ResetOps::fw_start`] should fail.
        failing_starts: SpinLock<u32>,
        /// Whether reloading the firmware image should fail.
        fail_hard_reset: bool,
    }

    impl ResetRecorder {
        fn new() -> Self {
            Self {
                steps: SpinLock::new(Vec::new()),
                lost: AtomicBool::new(false),
                failing_starts: SpinLock::new(0),
                fail_hard_reset: false,
            }
        }

        /// A device whose firmware refuses to boot `times` times over.
        fn with_failing_starts(times: u32) -> Self {
            Self {
                failing_starts: SpinLock::new(times),
                ..Self::new()
            }
        }

        /// A device whose firmware image cannot be reloaded.
        fn with_failing_hard_reset() -> Self {
            Self {
                fail_hard_reset: true,
                ..Self::new()
            }
        }

        fn record(&self, step: ResetStep) {
            self.steps.lock().push(step);
        }

        fn steps(&self) -> Vec<ResetStep> {
            self.steps.lock().clone()
        }
    }

    impl ResetOps for ResetRecorder {
        fn is_lost(&self) -> bool {
            self.lost.load(Ordering::Acquire)
        }

        fn stop_queues(&self) {
            self.record(ResetStep::StopQueues);
        }

        fn fw_stop(&self, _hard_reset: bool) -> PvrResult<()> {
            self.record(ResetStep::FwStop);
            Ok(())
        }

        fn clear_booted(&self) {
            self.record(ResetStep::ClearBooted);
        }

        fn power_down(&self) {
            self.record(ResetStep::PowerDown);
        }

        fn fw_hard_reset(&self) -> PvrResult<()> {
            self.record(ResetStep::FwHardReset);
            if self.fail_hard_reset {
                return Err(PvrError::BadFirmware);
            }
            Ok(())
        }

        fn power_up(&self) -> PvrResult<()> {
            self.record(ResetStep::PowerUp);
            Ok(())
        }

        fn rebuild_rings(&self) -> PvrResult<()> {
            self.record(ResetStep::RebuildRings);
            Ok(())
        }

        fn clear_hwr_flags(&self) -> PvrResult<()> {
            self.record(ResetStep::ClearHwrFlags);
            Ok(())
        }

        fn irq_clear(&self) -> PvrResult<()> {
            self.record(ResetStep::IrqClear);
            Ok(())
        }

        fn fw_start(&self) -> PvrResult<()> {
            self.record(ResetStep::FwStart);
            let mut failing = self.failing_starts.lock();
            if *failing > 0 {
                *failing -= 1;
                return Err(PvrError::Timeout);
            }
            Ok(())
        }

        fn start_queues(&self) {
            self.record(ResetStep::StartQueues);
        }

        fn declare_lost(&self) {
            self.record(ResetStep::DeclareLost);
            self.lost.store(true, Ordering::Release);
        }
    }

    /// The hard path, step for step against upstream `pvr_power_reset()`.
    ///
    /// Lines 542-584: `pvr_queue_device_pre_reset()`, `pvr_power_fw_disable()`,
    /// `initialised = false`, `pvr_power_device_suspend()`,
    /// `pvr_fw_hard_reset()`, `pvr_power_device_resume()`,
    /// `pvr_fw_irq_clear()`, `pvr_power_fw_enable()`,
    /// `pvr_queue_device_post_reset()`. The one step with no upstream line is
    /// [`ResetStep::RebuildRings`], which upstream gets from zeroing every
    /// firmware object inside `pvr_fw_hard_reset()`; it belongs after the GPU is
    /// powered again and before the firmware is started, because that is the
    /// window in which the rings may be written and must already be correct.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_hard_reset_follows_the_upstream_sequence() {
        let ops = ResetRecorder::new();
        assert_eq!(run_reset(&ops, ResetKind::Hard), Ok(()));
        assert_eq!(
            ops.steps(),
            vec![
                ResetStep::StopQueues,
                ResetStep::FwStop,
                ResetStep::ClearBooted,
                ResetStep::PowerDown,
                ResetStep::FwHardReset,
                ResetStep::PowerUp,
                ResetStep::RebuildRings,
                ResetStep::IrqClear,
                ResetStep::FwStart,
                ResetStep::StartQueues,
            ]
        );
        assert!(!ops.is_lost());
    }

    /// The soft path does four things and leaves the GPU powered.
    ///
    /// Upstream lines 562-571: no `pvr_queue_device_pre_reset()` (it is inside
    /// `if (hard_reset)`), no power cycle and no image reload — only the two
    /// fault flags, the interrupt acknowledgement and the restart. The absence
    /// of [`ResetStep::StopQueues`] is upstream's, not an omission: a soft reset
    /// is the answer to the firmware's own restart request, and the queues it
    /// would stop are the ones whose work it is trying to preserve.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_soft_reset_only_clears_the_fault_flags() {
        let ops = ResetRecorder::new();
        assert_eq!(run_reset(&ops, ResetKind::Soft), Ok(()));
        assert_eq!(
            ops.steps(),
            vec![
                ResetStep::FwStop,
                ResetStep::ClearHwrFlags,
                ResetStep::IrqClear,
                ResetStep::FwStart,
            ]
        );
    }

    /// Upstream lines 577-581: a soft reset that fails sets `hard_reset = true`
    /// and goes round the `do {} while (err)` again.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_failed_soft_reset_escalates_to_a_hard_one() {
        // The firmware refuses to come back exactly once, which is what the soft
        // attempt sees; the hard attempt then succeeds.
        let ops = ResetRecorder::with_failing_starts(1);
        assert_eq!(run_reset(&ops, ResetKind::Soft), Ok(()));

        let steps = ops.steps();
        assert_eq!(
            &steps[..4],
            &[
                ResetStep::FwStop,
                ResetStep::ClearHwrFlags,
                ResetStep::IrqClear,
                ResetStep::FwStart,
            ]
        );
        // Everything after the failed start is the hard sequence, and the device
        // is *not* lost: escalation is a retry, not a diagnosis.
        assert_eq!(
            &steps[4..],
            &[
                ResetStep::StopQueues,
                ResetStep::FwStop,
                ResetStep::ClearBooted,
                ResetStep::PowerDown,
                ResetStep::FwHardReset,
                ResetStep::PowerUp,
                ResetStep::RebuildRings,
                ResetStep::IrqClear,
                ResetStep::FwStart,
                ResetStep::StartQueues,
            ]
        );
        assert!(!ops.is_lost());
    }

    /// Upstream lines 574-575 and 594-601: a hard reset that fails is
    /// `err_device_lost`, and the queues are still restarted on the way out.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_failed_hard_reset_declares_the_device_lost() {
        // A firmware that never comes back: the hard attempt has nowhere to
        // escalate to.
        let ops = ResetRecorder::with_failing_starts(u32::MAX);
        assert_eq!(run_reset(&ops, ResetKind::Hard), Err(PvrError::Timeout));
        assert!(ops.is_lost());

        let steps = ops.steps();
        // The device is declared lost, and only then are the queues restarted —
        // upstream's `err_device_lost` label does exactly these two, in this
        // order, because the jobs on those queues still hold fences that must be
        // signalled (fence-completion contract) even though nothing will ever run them.
        assert_eq!(
            &steps[steps.len() - 5..],
            &[
                ResetStep::FwStop,
                ResetStep::ClearBooted,
                ResetStep::PowerDown,
                ResetStep::DeclareLost,
                ResetStep::StartQueues,
            ]
        );
        // One attempt only: a hard reset does not escalate to itself.
        assert_eq!(
            steps.iter().filter(|s| **s == ResetStep::FwStart).count(),
            1
        );
    }

    /// A failure before the firmware is even started is fatal in the same way.
    ///
    /// Upstream line 556: `pvr_fw_hard_reset()` failing is `goto
    /// err_device_lost` with the GPU powered down and no firmware in it, so
    /// there is nothing left to retry.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_firmware_image_that_cannot_be_reloaded_is_fatal() {
        let ops = ResetRecorder::with_failing_hard_reset();
        assert_eq!(run_reset(&ops, ResetKind::Hard), Err(PvrError::BadFirmware));
        assert_eq!(
            ops.steps(),
            vec![
                ResetStep::StopQueues,
                ResetStep::FwStop,
                ResetStep::ClearBooted,
                ResetStep::PowerDown,
                ResetStep::FwHardReset,
                ResetStep::FwStop,
                ResetStep::ClearBooted,
                ResetStep::PowerDown,
                ResetStep::DeclareLost,
                ResetStep::StartQueues,
            ]
        );
    }

    /// Upstream lines 525-528: `pvr_dev->lost` is checked as soon as the
    /// semaphore is held, and a lost device is `-EIO` rather than a reset.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_lost_device_is_not_reset() {
        let ops = ResetRecorder::new();
        ops.lost.store(true, Ordering::Release);
        assert_eq!(run_reset(&ops, ResetKind::Hard), Err(PvrError::DeviceLost));
        assert!(ops.steps().is_empty());
    }

    /// The deferred request keeps the more drastic of two.
    ///
    /// A hard reset does everything a soft one does, so downgrading a pending
    /// hard request to a soft one would answer the firmware's restart request
    /// and leave the hang the watchdog found.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_deferred_reset_request_keeps_the_more_drastic_kind() {
        let request = ResetRequest::new();
        assert!(!request.is_pending());
        assert_eq!(request.take(), None);

        request.request(ResetKind::Soft);
        request.request(ResetKind::Hard);
        assert!(request.is_pending());
        assert_eq!(request.take(), Some(ResetKind::Hard));
        // Taking it clears it: the next caller that may sleep must not reset a
        // GPU nobody asked it to.
        assert!(!request.is_pending());
        assert_eq!(request.take(), None);

        request.request(ResetKind::Hard);
        request.request(ResetKind::Soft);
        assert_eq!(request.take(), Some(ResetKind::Hard));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn every_step_has_an_inverse_that_undoes_it() {
        for step in [
            Step::PowerDomainOn,
            Step::ClockEnable,
            Step::ResetDeassert,
            Step::ResetAssert,
            Step::ClockDisable,
            Step::PowerDomainOff,
        ] {
            assert_eq!(step.inverse().inverse(), step);
            assert_ne!(step.inverse(), step);
        }
    }
}
