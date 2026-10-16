// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Fences and sync objects.
//!
//!
//! This module provides the fence semantics shared by the PowerVR scheduler
//! and dma-buf users. It corresponds to Linux's `dma_fence` and sync objects.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-fence.c>
//! and <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c>.
//!
//! Fence state and waiter methods support process and bottom-half contexts.
//! They must not be called from hard interrupt handlers. GPU IRQ handlers
//! schedule completion work; callbacks run after the fence lock is released.
//!
//! # The contract
//!
//! Schedulers rely on these eight rules for completion ordering and deadlock
//! avoidance. Each rule names the [`ktest`] that verifies it.
//!
//! [`ktest`]: self::test
//!
//! ## C1. A fence is a one-way latch
//!
//! A fence starts [`Pending`] and moves exactly once, to [`Signaled`] or to
//! [`Failed`]. That state is terminal: there is no un-signal, no reset, and no
//! second transition. [`FenceState::signal`] returns whether *this* call was
//! the one that performed the transition, so a completion path and an error
//! path may both call it and only the first takes effect. It is deliberately
//! idempotent rather than an error, because those two paths race by
//! construction — a job that times out while its interrupt is already in
//! flight is normal, not a bug.
//!
//! A failed fence is *signalled*. Consumers must make progress on it and
//! observe the error through [`Fence::error`]; they must not keep waiting.
//! Modelling failure as "never signals" is the single easiest way to wedge a
//! pipeline, which is why [`DrmError`] rides along with the transition instead
//! of replacing it.
//!
//! Pinned by `a_fence_signals_once_and_the_state_is_terminal`.
//!
//! [`Pending`]: FenceStatus::Pending
//! [`Signaled`]: FenceStatus::Signaled
//! [`Failed`]: FenceStatus::Failed
//!
//! ## C2. Every fence must eventually signal
//!
//! This is an obligation on whoever created the fence, and this module cannot
//! enforce it. A fence that never signals deadlocks every consumer downstream
//! of it, including consumers in other processes that imported the buffer it
//! is attached to. Hardware that can hang must therefore be timed out by its
//! driver, and the timeout must end in [`FenceState::signal`] with
//! [`DrmError::Io`] — never in simply abandoning the fence.
//!
//! ## C3. Signalling wakes every waiter, and no wakeup may be lost
//!
//! [`FenceState::signal`] performs the state transition under the fence's
//! lock, then releases the lock and only afterwards wakes the waiters and runs
//! the callbacks. That order is what closes the race: a waiter that arrives
//! *before* the transition is on the list and gets woken, and one that arrives
//! *after* it sees the terminal state from [`FenceState::attach`] and never
//! sleeps at all. There is no window in which a waiter is neither.
//!
//! Losing one wakeup is not a slow path, it is a permanent hang of that
//! waiter, so this is the rule the tests spend the most effort on.
//!
//! Pinned by `signalling_wakes_every_waiter` and
//! `attaching_to_an_already_signalled_fence_reports_it_immediately`.
//!
//! ## C4. A wait that gives up leaves nothing behind
//!
//! A waiter is attached to every fence in the set it is waiting on, and the
//! attachment is owned by an [`Attachment`] guard whose destructor detaches it
//! from all of them. Timing out, being interrupted, and succeeding therefore
//! all leave the fences with an empty waiter list. A stale waiter is worse
//! than a leak: the fence holds a strong reference to it, so the memory of a
//! long-gone wait stays live and the eventual signal walks a list that grows
//! without bound.
//!
//! [`Fence::waiter_count`] exists so that a test — and a driver chasing a
//! leak — can assert this rather than assume it.
//!
//! Pinned by `a_timed_out_wait_leaves_no_waiter_behind` and
//! `an_interrupted_wait_leaves_no_waiter_behind`.
//!
//! ## C5. Waiting has an outcome for every exit
//!
//! [`wait_many`] returns:
//!
//!  - `Ok(index)` when the wait was satisfied. `index` is the first fence that
//!    signalled, which is what `SYNCOBJ_WAIT` reports in `first_signaled`.
//!  - `Err(`[`DrmError::TimeExpired`]`)` when the deadline passed first. That
//!    is `ETIME`, not `ETIMEDOUT`: Vulkan turns the former into `VK_TIMEOUT`
//!    and anything else into an error.
//!  - `Err(`[`DrmError::Interrupted`]`)` when a signal arrived first, and only
//!    if the caller asked for an interruptible wait.
//!
//! A fence that signalled with an error still *satisfies* a wait: the wait
//! returns `Ok`, and it is the caller's job to inspect [`Fence::error`]. This
//! matches `dma_fence_wait_timeout`, which reports the remaining time rather
//! than the fence's status.
//!
//! Every wait a user-space process can reach must be interruptible. An
//! uninterruptible wait on a wedged GPU makes the calling process unkillable,
//! and there is no way out of that short of a reboot.
//!
//! Pinned by `waiting_for_an_already_failed_fence_succeeds_with_an_error`.
//!
//! ## C6. Timeline points are strictly increasing
//!
//! A timeline sync object accepts a point only if it is greater than every
//! point it has ever been given; a smaller or repeated point is
//! [`DrmError::InvalidArgument`] and is *never* silently reordered. Point zero
//! is not a timeline point at all — it names the object's binary fence.
//!
//! A wait on a point that the timeline has already passed returns
//! immediately. That is not an optimization: a client that submits point 5 and
//! then waits on point 3 must not block, because nothing will ever signal
//! point 3 again.
//!
//! Pinned by `timeline_points_only_move_forwards` and
//! `a_wait_on_a_passed_timeline_point_returns_at_once`.
//!
//! ## C7. Dependency cycles are refused, not deadlocked
//!
//! A fence may be declared to depend on other fences through
//! [`FenceState::add_dependency`], which is how a scheduler records "this job
//! waits for those". Before the edge is added, the graph is walked from the
//! proposed dependency: if this fence is reachable from it, the edge would
//! close a cycle and the call fails with [`DrmError::WouldDeadlock`]
//! (`EDEADLK`).
//!
//! The walk and the insertion happen under one global lock, so two CPUs adding
//! `A → B` and `B → A` at the same time cannot both pass the check. That lock
//! is ordered *above* the per-fence locks — a thread holding it may take a
//! fence lock, never the other way round — and it is only ever taken on the
//! submission path, which is not hot.
//!
//! An already-signalled dependency is dropped rather than recorded. It cannot
//! participate in a cycle, since it will never wait for anything, and keeping
//! it would pin dead fences in memory for the lifetime of the job.
//!
//! Pinned by `a_dependency_cycle_is_refused`,
//! `a_fence_may_not_depend_on_itself` and
//! `a_longer_dependency_cycle_is_refused`.
//!
//! ## C8. Identity is the pair (context, sequence number)
//!
//! [`FenceId`] — a [`FenceContext`] plus a sequence number — identifies a
//! fence, exactly as `dma_fence` does. Two fences in one context are ordered
//! by their sequence numbers, which is what lets a waiter skip a fence it
//! knows a later one implies, and it is also what the cycle check compares.
//!
//! A driver that hands out one `(context, seqno)` pair twice breaks both. Use
//! [`FenceContext::new`] to obtain a context nobody else has, and number
//! within it.
//!
//! # What is *not* promised
//!
//! - **Callbacks do not run in any particular order** and they run in the
//!   context of whoever called [`FenceState::signal`], which may be an
//!   bottom half. They must not sleep or acquire locks held by a path that
//!   waits for their completion. They may inspect the invoking fence because
//!   its state lock has already been released.
//! - **A fence does not know when its dependencies have all signalled.**
//!   [`FenceState::dependencies_met`] answers the question on demand; nothing
//!   signals the fence for you. Rule C2 is still the creator's obligation.
//! - **There is no priority inheritance and no deadline propagation.**
//!   Linux 6.8 added `dma_fence` deadline hints; linux-6.6, which this tracks,
//!   has none.

use alloc::{sync::Arc, vec::Vec};

mod set;
mod software;
mod syncobj;
mod wait;

pub mod ioctl;
/// Shared with [`crate::prime::test`], which needs the same fake descriptor
/// table.
#[cfg(ktest)]
pub(crate) mod test;

pub(crate) use self::wait::MAX_WAIT_HANDLES;
pub use self::{
    set::FenceSet,
    software::{FenceCallback, FenceState, MAX_DEPENDENCIES, SoftwareFence},
    syncobj::{
        MAX_SYNCOBJS_PER_FILE, SubmitAttachment, SyncObj, SyncObjHandle, SyncObjTable,
        TimelineFence, TimelinePoint,
    },
    wait::{Attachment, Blocker, Deadline, FenceWaiter, wait_many},
};
use crate::error::{DrmError, DrmResult};

/// Identifies the ordered stream a fence belongs to.
///
/// Fences within one context are ordered by sequence number, which lets waiters
/// skip fences they know are already implied by a later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FenceContext(pub u64);

impl Default for FenceContext {
    /// Allocates a fresh context, the same as [`FenceContext::new`].
    ///
    /// There is no sensible "default" context: rule C8 makes the number part
    /// of a fence's identity, so handing out the same one twice would be
    /// actively wrong.
    fn default() -> Self {
        Self::new()
    }
}

impl FenceContext {
    /// Allocates a context no other caller has been given.
    ///
    /// Rule C8 makes `(context, seqno)` the identity of a fence, so a driver
    /// that invents context numbers instead of asking for them risks colliding
    /// with another driver's and confusing both the cycle check and any waiter
    /// that skips implied fences.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-fence.c#L92>
    /// (`dma_fence_context_alloc`).
    pub fn new() -> Self {
        use core::sync::atomic::{AtomicU64, Ordering};

        /// Contexts are never reused. At one context per submission context
        /// per client, exhausting 2^64 of them is not reachable.
        static NEXT: AtomicU64 = AtomicU64::new(1);

        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The identity of a fence.
///
/// See rule C8: a driver must not hand out one of these twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FenceId {
    /// The stream the fence belongs to.
    pub context: FenceContext,
    /// Its position within that stream.
    pub seqno: u64,
}

/// Where a fence is in its one and only transition.
///
/// [`Signaled`](Self::Signaled) and [`Failed`](Self::Failed) are both terminal
/// and both count as *signalled*: a consumer of a failed fence makes progress
/// and observes the error, it does not keep waiting. See rule C1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FenceStatus {
    /// The fence has not completed yet.
    Pending,
    /// The fence completed successfully.
    Signaled,
    /// The fence completed, but the work behind it failed.
    Failed(DrmError),
}

impl FenceStatus {
    /// Returns whether the fence has completed, successfully or not.
    pub fn is_signaled(self) -> bool {
        !matches!(self, Self::Pending)
    }

    /// Returns the error the fence completed with, if any.
    pub fn error(self) -> Option<DrmError> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

/// A one-shot completion latch.
///
/// Implementors are expected to hold a [`FenceState`] and forward to it, the
/// way [`SoftwareFence`] does. The state machine is subtle enough — see rules
/// C1, C3 and C4 — that a second implementation of it is a second place for
/// the lost-wakeup bug to live.
/// Final references may be released from process or bottom-half context.
/// Implementations must not sleep during destruction; resource cleanup requiring
/// a sleepable lock must be deferred by the owning driver.
pub trait Fence: Send + Sync + core::fmt::Debug {
    /// Downcast escape hatch for recovering a concrete fence from
    /// `Arc<dyn Fence>`.
    ///
    /// Provides an explicit, object-safe downcast hook.
    /// PowerVR uses it to recognise its own job fences the way upstream's
    /// `pvr_queue_fence_is_native()` compares `dma_fence->ops` pointers.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Returns the context this fence belongs to.
    fn context(&self) -> FenceContext;

    /// Returns this fence's sequence number within its context.
    fn seqno(&self) -> u64;

    /// Returns the fence's status.
    fn status(&self) -> FenceStatus;

    /// Returns the fences that must signal before this one may.
    ///
    /// Walked by the cycle check in [`FenceState::add_dependency`], so an
    /// implementation must return the edges it actually recorded and must not
    /// take a lock that any other fence's method takes. See rule C7.
    fn dependencies(&self) -> Vec<Arc<dyn Fence>> {
        Vec::new()
    }

    /// Attaches `waiter`, to be notified with `index` when the fence signals.
    ///
    /// Returns `true` if the fence had *already* signalled, in which case
    /// nothing was attached and the waiter has been notified inline. That
    /// return value is the second half of rule C3, and a caller that ignores
    /// it sleeps forever.
    fn attach(&self, waiter: &Arc<FenceWaiter>, index: usize) -> bool;

    /// Detaches a waiter attached by [`attach`](Self::attach).
    ///
    /// Detaching a waiter that is not attached is a no-op, which is what lets
    /// [`Attachment`] detach unconditionally on the way out.
    fn detach(&self, waiter: &Arc<FenceWaiter>);

    /// Returns how many waiters are currently attached.
    ///
    /// Diagnostic: rule C4 says this must be back to zero once every wait has
    /// left, however it left.
    fn waiter_count(&self) -> usize;

    /// Registers a callback to run when the fence signals.
    ///
    /// Runs immediately, in the caller's context, if the fence is already
    /// signalled. Callbacks may run in bottom-half context, so they must not
    /// sleep; see "What is not promised" above.
    fn add_callback(&self, callback: FenceCallback) -> DrmResult<()>;

    /// Returns whether the fence has been signalled.
    fn is_signaled(&self) -> bool {
        self.status().is_signaled()
    }

    /// Returns the error the fence signalled with, if any.
    ///
    /// A signalled fence with an error still counts as signalled: consumers
    /// must make progress, they just observe the failure.
    fn error(&self) -> Option<DrmError> {
        self.status().error()
    }

    /// Returns the fence's identity; see rule C8.
    fn id(&self) -> FenceId {
        FenceId {
            context: self.context(),
            seqno: self.seqno(),
        }
    }
}

/// Waits for one fence.
///
/// A thin wrapper over [`wait_many`] with a single-element set, kept because
/// that is overwhelmingly the common case and because the borrow dance of
/// building a one-element slice is noise at every call site.
pub fn wait_one(
    blocker: &dyn Blocker,
    fence: &Arc<dyn Fence>,
    deadline: Deadline,
    interruptible: bool,
) -> DrmResult<()> {
    // Cheap enough to be worth not building a slice for.
    if fence.is_signaled() {
        return Ok(());
    }

    let fences = [fence.clone()];
    wait_many(blocker, &fences, true, deadline, interruptible).map(|_| ())
}

/// Returns whether every fence in a set has signalled.
pub fn all_signaled(fences: &[Arc<dyn Fence>]) -> bool {
    fences.iter().all(|fence| fence.is_signaled())
}

/// Drops the fences that have already signalled.
///
/// A signalled fence is a dependency nobody will ever wait for again, so
/// keeping it only pins memory. Upstream's scheduler prunes at the same points.
pub fn prune_signaled(fences: &mut Vec<Arc<dyn Fence>>) {
    fences.retain(|fence| !fence.is_signaled());
}
