// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Blocking on fences.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-fence-array.c>
//! (`dma_fence_wait_any_timeout`, which is the shape of [`wait_many`]).
//!
//! # Why the sleep itself is somebody else's job
//!
//! Putting a thread to sleep with a deadline, and letting a signal cut that
//! sleep short, both need machinery a comp crate cannot see: the timer
//! subsystem lives in `kernel/core/src/time/` and signal delivery in
//! `kernel/core/src/process/signal/`. So this module owns the fence-side
//! bookkeeping — who is waiting on what, and how a wakeup reaches them —
//! and delegates the actual sleep to a [`Blocker`] the character-device shell
//! supplies, exactly as it supplies [`UserMem`] for the address space.
//!
//! [`UserMem`]: crate::ioctl::UserMem

use alloc::sync::Arc;

use aster_softirq::BottomHalfDisabled;
use ostd::sync::{SpinLock, WaitQueue};

use super::Fence;
use crate::error::{DrmError, DrmResult};

/// The largest number of handles a single wait ioctl may name.
///
/// Divergence: Linux bounds `SYNCOBJ_WAIT` only by `kmalloc_array` failing.
/// Each handle here costs a `Vec` slot, a fence reference and an attachment on
/// that fence, all held for the duration of a sleep an unprivileged client
/// controls. Mesa waits on a handful of sync objects per submission; a
/// thousand is far beyond any real caller.
pub(crate) const MAX_WAIT_HANDLES: usize = 1024;

/// When a wait gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deadline {
    /// Do not sleep at all: report the current state and return.
    ///
    /// This is what `timeout_nsec <= 0` means to `SYNCOBJ_WAIT`, and it is the
    /// only deadline this crate can honour without a [`Blocker`].
    Immediate,
    /// Give up at an absolute monotonic time, in nanoseconds since boot.
    ///
    /// Absolute rather than relative because that is what the ioctls carry:
    /// `drm_syncobj_wait::timeout_nsec` is an absolute `CLOCK_MONOTONIC`
    /// value, so a wait that is restarted after a signal resumes against the
    /// original deadline instead of starting its timeout over.
    At(u64),
    /// Never give up.
    Never,
}

impl Deadline {
    /// Interprets a `timeout_nsec` field from one of the wait ioctls.
    ///
    /// A non-positive value is a deadline in the past, which upstream's
    /// `drm_timeout_abs_to_jiffies` turns into a zero-length wait, i.e. a
    /// poll.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L969>
    /// (`drm_timeout_abs_to_jiffies`).
    pub fn from_timeout_nsec(timeout_nsec: i64) -> Self {
        match u64::try_from(timeout_nsec) {
            Ok(0) | Err(_) => Self::Immediate,
            Ok(deadline) => Self::At(deadline),
        }
    }
}

/// The scheduling services a blocking wait needs.
///
/// Implemented by the kernel crate, which is the only place that can reach the
/// timer subsystem and the calling thread's pending signals. A handler that
/// cannot get one — because the caller did not supply it — must degrade to
/// [`Deadline::Immediate`] rather than spin.
pub trait Blocker {
    /// Returns the current monotonic time in nanoseconds since boot.
    ///
    /// The default reads the jiffy counter, whose resolution is one timer tick
    /// (a millisecond). An implementation with access to a finer clock should
    /// override it: the ioctls speak nanoseconds and user space rounds its own
    /// deadlines assuming the kernel does too.
    fn now_ns(&self) -> u64 {
        let elapsed = ostd::timer::Jiffies::elapsed().as_duration();
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
    }

    /// Sleeps on `queue` until `ready` holds, the deadline passes, or — when
    /// `interruptible` — a signal arrives.
    ///
    /// The implementation must enqueue on `queue` *before* evaluating `ready`
    /// on each iteration, or a fence that signals in between is a wakeup lost
    /// and a thread hung forever. `WaitQueue::wait_until` and its timeout
    /// variants in the kernel crate already do this.
    ///
    /// # Errors
    ///
    /// - [`DrmError::TimeExpired`] if the deadline passed first.
    /// - [`DrmError::Interrupted`] if a signal arrived first.
    fn block(
        &self,
        queue: &WaitQueue,
        ready: &mut dyn FnMut() -> bool,
        deadline: Deadline,
        interruptible: bool,
    ) -> DrmResult<()>;
}

/// The rendezvous between a blocked thread and the fences it waits on.
///
/// Shared: each fence in the set holds a reference for as long as the
/// [`Attachment`] lives, and the waiting thread holds one of its own. A fence
/// that signals calls [`notify`](Self::notify) and kicks the queue; the thread
/// re-evaluates and decides whether it is done.
pub struct FenceWaiter {
    inner: SpinLock<Inner, BottomHalfDisabled>,
    /// Woken by every fence this waiter is attached to.
    ///
    /// One queue per wait rather than one per fence, so that a wait on a set
    /// of fences sleeps once instead of once per fence.
    queue: WaitQueue,
}

// `WaitQueue` is not `Debug`, and a fence has to be, so the queue is described
// rather than printed.
impl core::fmt::Debug for FenceWaiter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FenceWaiter")
            .field("state", &*self.inner.lock())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Inner {
    /// How many notifications have arrived.
    ///
    /// Counts notifications, not distinct fences: a set that names one fence
    /// twice is notified twice, which is what makes the `wait_all` comparison
    /// against `total` come out right.
    notified: usize,
    /// The index the first notification carried.
    first: Option<usize>,
    /// How many notifications a `wait_all` needs.
    total: usize,
}

impl FenceWaiter {
    /// Creates a waiter expecting `total` notifications.
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(Inner {
                notified: 0,
                first: None,
                total,
            }),
            queue: WaitQueue::new(),
        })
    }

    /// Records that the fence registered under `index` has signalled, and
    /// wakes the waiting thread.
    ///
    /// Called from [`FenceState::signal`] with no fence lock held, and
    /// possibly from a bottom half, so it neither sleeps nor allocates.
    ///
    /// [`FenceState::signal`]: super::FenceState::signal
    pub fn notify(&self, index: usize) {
        {
            let mut inner = self.inner.lock();
            inner.notified += 1;
            inner.first.get_or_insert(index);
        }

        // Outside the lock: the wake path takes the queue's own lock, and
        // nesting the two would give two fences signalling into one waiter a
        // way to order them differently.
        self.queue.wake_all();
    }

    /// Returns whether the wait has been satisfied.
    pub fn is_ready(&self, wait_all: bool) -> bool {
        let inner = self.inner.lock();
        if wait_all {
            inner.notified >= inner.total
        } else {
            inner.first.is_some()
        }
    }

    /// Returns the index of the first fence that signalled, if any has.
    pub fn first_signaled(&self) -> Option<usize> {
        self.inner.lock().first
    }

    /// Returns the queue a [`Blocker`] must sleep on.
    pub fn queue(&self) -> &WaitQueue {
        &self.queue
    }
}

/// Keeps a waiter attached to a set of fences, and detaches it on the way out.
///
/// This type is rule C4 made structural: there is no path out of a wait —
/// success, timeout, interruption, or a `?` in between — that does not run
/// this destructor, so a fence can never be left holding a waiter whose thread
/// has gone.
#[derive(Debug)]
pub struct Attachment<'a> {
    waiter: Arc<FenceWaiter>,
    fences: &'a [Arc<dyn Fence>],
}

impl<'a> Attachment<'a> {
    /// Attaches `waiter` to every fence in `fences`.
    ///
    /// A fence that has already signalled notifies the waiter inline instead
    /// of recording an attachment; detaching from it later is a no-op, which
    /// is why the destructor does not need to remember which is which.
    pub fn new(waiter: Arc<FenceWaiter>, fences: &'a [Arc<dyn Fence>]) -> Self {
        for (index, fence) in fences.iter().enumerate() {
            fence.attach(&waiter, index);
        }

        Self { waiter, fences }
    }

    /// Returns the waiter being kept attached.
    pub fn waiter(&self) -> &Arc<FenceWaiter> {
        &self.waiter
    }
}

impl Drop for Attachment<'_> {
    fn drop(&mut self) {
        for fence in self.fences {
            fence.detach(&self.waiter);
        }
    }
}

/// Waits for a set of fences.
///
/// With `wait_all` set, returns once every fence has signalled; otherwise
/// returns as soon as one has. Either way the value returned is the index of
/// the first fence that signalled, which `SYNCOBJ_WAIT` reports back in
/// `first_signaled`.
///
/// This is the primitive behind `SYNCOBJ_WAIT` and `SYNCOBJ_TIMELINE_WAIT`,
/// and for scheduler fence waits. Its exact contract is rule C5 in the
/// [module documentation](super).
///
/// # Errors
///
/// - [`DrmError::InvalidArgument`] for an empty set, matching
///   `drm_syncobj_wait_ioctl`, which rejects `count_handles == 0`.
/// - [`DrmError::TimeExpired`] if the deadline passed first. With
///   [`Deadline::Immediate`] this is the answer whenever the set is not
///   already satisfied, and it is reached without touching the [`Blocker`].
/// - [`DrmError::Interrupted`] if a signal arrived first.
pub fn wait_many(
    blocker: &dyn Blocker,
    fences: &[Arc<dyn Fence>],
    wait_all: bool,
    deadline: Deadline,
    interruptible: bool,
) -> DrmResult<usize> {
    if fences.is_empty() {
        return Err(DrmError::InvalidArgument);
    }

    // Fast path, and the whole of the `Immediate` path: answer from the
    // fences' own state without building a waiter or touching the blocker.
    if let Some(index) = satisfied(fences, wait_all) {
        return Ok(index);
    }
    if deadline == Deadline::Immediate {
        return Err(DrmError::TimeExpired);
    }

    let waiter = FenceWaiter::new(fences.len());
    // Attach before sleeping. A fence that signals from here on notifies the
    // waiter, and one that had already signalled notifies it inline from
    // `Attachment::new`, so the sleep below cannot start on a set that is
    // already satisfied. Detached again by this guard's destructor, however
    // this function returns.
    let attachment = Attachment::new(waiter.clone(), fences);

    let result = blocker.block(
        waiter.queue(),
        &mut || waiter.is_ready(wait_all),
        deadline,
        interruptible,
    );

    // Checked before the error is consulted: a fence that signalled in the
    // same instant the deadline expired satisfied the wait, and reporting
    // `ETIMEDOUT` for work that is demonstrably done would send user space
    // looking for a hang that is not there.
    let index = satisfied(fences, wait_all);
    drop(attachment);

    match (index, result) {
        (Some(index), _) => Ok(index),
        (None, Err(error)) => Err(error),
        // The blocker returned success without the condition holding. That is
        // a bug in the shell rather than in any client, but reporting a
        // spurious wake as a timeout keeps the caller from treating unfinished
        // work as finished.
        (None, Ok(())) => Err(DrmError::TimeExpired),
    }
}

/// Returns the index of the first signalled fence, if the set is satisfied.
fn satisfied(fences: &[Arc<dyn Fence>], wait_all: bool) -> Option<usize> {
    let mut first = None;

    for (index, fence) in fences.iter().enumerate() {
        if fence.is_signaled() {
            first.get_or_insert(index);
            if !wait_all {
                return first;
            }
        } else if wait_all {
            return None;
        }
    }

    first
}
