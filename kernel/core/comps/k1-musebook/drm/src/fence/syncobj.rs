// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Named, user-visible fence containers.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c>
//! and <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_syncobj.h>.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    vec::Vec,
};

use aster_softirq::BottomHalfDisabled;
use ostd::sync::SpinLock;
use spin::Once;

use super::{
    Fence, FenceCallback, FenceContext, FenceState, FenceStatus, FenceWaiter, SoftwareFence,
};
use crate::{
    error::{DrmError, DrmResult},
    prime::Eventfd,
};

/// A sync-object handle, valid only within the [`DrmFile`] that issued it.
///
/// Handle `0` is never valid, matching the GEM handle space and matching
/// user space's use of zero as "no object".
///
/// [`DrmFile`]: crate::file::DrmFile
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyncObjHandle(pub u32);

/// The largest number of sync objects one file may hold at a time.
///
/// Divergence: Linux bounds this only by memory, through an `idr`. Every
/// object here is a small allocation an unprivileged client can create in a
/// tight loop, so there is a ceiling. Mesa keeps one sync object per
/// in-flight submission plus a handful of long-lived ones; a compositor plus
/// its clients stays orders of magnitude below this.
pub const MAX_SYNCOBJS_PER_FILE: usize = 4096;

/// What a wait on a timeline point is waiting for.
///
/// The three cases are genuinely different to a caller, which is why this is
/// not an `Option`: "the timeline is already past this point" and "nothing has
/// been submitted that would reach it" both have no fence to wait on, but the
/// first means *return now* and the second means *block, or fail, depending on
/// `WAIT_FOR_SUBMIT`*. Collapsing them is how a client ends up waiting forever
/// for a point that went by.
#[derive(Clone, Debug)]
pub enum TimelinePoint {
    /// The timeline has already passed the point; a wait on it is satisfied.
    Passed,
    /// The fence that will signal the point once it completes.
    Pending(Arc<dyn Fence>),
    /// Nothing that would reach the point has been submitted yet.
    Unsubmitted,
}

/// A timeline completion that includes unfinished work from earlier points.
///
/// A supplied fence completing out of order cannot advance the timeline past
/// preceding work. This fence signals only after both prerequisites complete,
/// and propagates the first failed prerequisite in submission order.
#[derive(Debug)]
pub struct TimelineFence {
    state: FenceState,
    prerequisites: Vec<Arc<dyn Fence>>,
    propagation: Arc<TimelinePropagation>,
}

impl TimelineFence {
    fn new(
        previous: Arc<dyn Fence>,
        submitted: Arc<dyn Fence>,
        point: u64,
        propagation: Arc<TimelinePropagation>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: FenceState::new(FenceContext::new(), point),
            prerequisites: alloc::vec![previous, submitted],
            propagation,
        })
    }

    /// Returns all work whose completion this timeline point requires.
    ///
    /// A driver translating this fence into hardware waits must preserve both
    /// prerequisites, recursively expanding an earlier timeline fence if needed.
    pub fn prerequisites(&self) -> &[Arc<dyn Fence>] {
        &self.prerequisites
    }

    // Register after publishing and releasing the sync-object lock. Registration
    // on an already-completed source may run this callback inline.
    fn enable_signaling(self: &Arc<Self>) -> DrmResult<()> {
        for fence in &self.prerequisites {
            let weak = Arc::downgrade(self);
            fence.add_callback(Arc::new(move |_| {
                if let Some(chain) = weak.upgrade() {
                    chain
                        .propagation
                        .enqueue(TimelineWork::Check(chain.clone()));
                }
            }))?;
        }
        self.propagation.enqueue(TimelineWork::Check(self.clone()));
        Ok(())
    }

    fn try_signal(&self) {
        if self.state.is_signaled() || self.prerequisites.iter().any(|fence| !fence.is_signaled()) {
            return;
        }
        let error = self.prerequisites.iter().find_map(|fence| fence.error());
        self.state.signal(error);
    }
}

impl Fence for TimelineFence {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn context(&self) -> FenceContext {
        self.state.context()
    }
    fn seqno(&self) -> u64 {
        self.state.seqno()
    }
    fn status(&self) -> FenceStatus {
        self.state.status()
    }
    fn dependencies(&self) -> Vec<Arc<dyn Fence>> {
        if self.state.is_signaled() {
            Vec::new()
        } else {
            self.prerequisites.to_vec()
        }
    }
    fn attach(&self, waiter: &Arc<FenceWaiter>, index: usize) -> bool {
        self.state.attach(waiter, index)
    }
    fn detach(&self, waiter: &Arc<FenceWaiter>) {
        self.state.detach(waiter);
    }
    fn waiter_count(&self) -> usize {
        self.state.waiter_count()
    }
    fn add_callback(&self, callback: FenceCallback) -> DrmResult<()> {
        self.state.add_callback(callback)
    }
    fn is_signaled(&self) -> bool {
        self.state.is_signaled()
    }
}

impl Drop for TimelineFence {
    fn drop(&mut self) {
        // An abandoned chain may be arbitrarily long. Iterative retirement also
        // prevents a last reference from recursively destroying its ancestors.
        let retired = core::mem::take(&mut self.prerequisites);
        self.propagation.enqueue(TimelineWork::Retire(retired));
    }
}

// Completion or destruction can cascade through arbitrarily many points,
// including chains transferred between sync objects. One shared trampoline
// bounds stack use across these objects. The lock only protects queue state;
// fence callbacks and last-reference destruction run outside it.
#[derive(Debug)]
struct TimelinePropagation {
    inner: SpinLock<PropagationInner, BottomHalfDisabled>,
}

#[derive(Debug)]
enum TimelineWork {
    Check(Arc<TimelineFence>),
    Retire(Vec<Arc<dyn Fence>>),
}

#[derive(Debug, Default)]
struct PropagationInner {
    pending: VecDeque<TimelineWork>,
    draining: bool,
}

impl TimelinePropagation {
    fn shared() -> Arc<Self> {
        static PROPAGATION: Once<Arc<TimelinePropagation>> = Once::new();
        PROPAGATION
            .call_once(|| {
                Arc::new(Self {
                    inner: SpinLock::new(PropagationInner::default()),
                })
            })
            .clone()
    }

    fn enqueue(&self, work: TimelineWork) {
        {
            let mut inner = self.inner.lock();
            inner.pending.push_back(work);
            if inner.draining {
                return;
            }
            inner.draining = true;
        }
        loop {
            let next = {
                let mut inner = self.inner.lock();
                match inner.pending.pop_front() {
                    Some(next) => Some(next),
                    None => {
                        inner.draining = false;
                        None
                    }
                }
            };
            let Some(next) = next else {
                return;
            };
            match next {
                TimelineWork::Check(next) => next.try_signal(),
                TimelineWork::Retire(retired) => drop(retired),
            }
        }
    }
}

/// A named, user-visible fence container.
///
/// A *binary* sync object holds at most one fence, replaced on each signal. A
/// *timeline* sync object holds a monotonically increasing set of
/// `(point, fence)` pairs, and a wait on point N is satisfied by the fence at
/// the first submitted point at or above N. Both live in one type because
/// user space does: the same handle may be used either way, and Linux
/// represents both with a `dma_fence_chain` whose degenerate case is the
/// binary object.
///
/// Point zero is the binary fence, never a timeline point. See rule C6 in the
/// [module documentation](super).
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_syncobj.h>.
#[derive(Debug)]
pub struct SyncObj {
    inner: SpinLock<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// The binary fence, i.e. point zero.
    fence: Option<Arc<dyn Fence>>,
    /// Submitted timeline points, ordered.
    points: BTreeMap<u64, Arc<dyn Fence>>,
    /// The highest point ever submitted, which never decreases while the
    /// timeline exists. Rule C6 is enforced against this rather than against
    /// the largest key still present, so that pruning cannot let a point be
    /// reused.
    last_submitted: u64,
    /// Completed prefix retained after its fences have been pruned.
    last_signaled: u64,
    /// Waiters blocked because the object had no fence for the point they
    /// asked about, woken when anything is installed.
    ///
    /// This is the `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT` machinery, and it
    /// has to live on the object rather than on a fence for the obvious
    /// reason: there is no fence to wait on yet. Upstream keeps the same list
    /// in `drm_syncobj::cb_list`.
    submit_waiters: Vec<(usize, Arc<FenceWaiter>)>,
    /// Eventfds waiting for a point to be submitted and/or signalled.
    ///
    /// `SYNCOBJ_EVENTFD` does not block: it parks an entry here, the way
    /// linux-6.6 parks one on `drm_syncobj::ev_fd_list`, and the next
    /// [`replace_fence`](SyncObj::replace_fence) / [`add_point`] retries it.
    /// A reset leaves the watches in place — they are waiting for a fence
    /// that has not been submitted yet, not for the one that was just
    /// cleared.
    eventfds: Vec<EventfdWatch>,
}

/// One `SYNCOBJ_EVENTFD` registration.
///
/// [`eventfd`](EventfdWatch::eventfd) is a trait object because this crate
/// cannot name the kernel's `EventFile`; see [`Eventfd`].
struct EventfdWatch {
    point: u64,
    wait_available: bool,
    eventfd: Arc<dyn Eventfd>,
}

impl core::fmt::Debug for EventfdWatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventfdWatch")
            .field("point", &self.point)
            .field("wait_available", &self.wait_available)
            .finish_non_exhaustive()
    }
}

/// What to do with a watch once the object's lock is dropped.
enum EventfdAction {
    /// The eventfd should be incremented now.
    Signal(Arc<dyn Eventfd>),
    /// A fence exists; increment when *it* signals.
    WaitFence(Arc<dyn Fence>, Arc<dyn Eventfd>),
}

impl SyncObj {
    /// Creates a sync object, optionally already signalled.
    ///
    /// `DRM_SYNCOBJ_CREATE_SIGNALED` asks for the latter, and user space relies
    /// on it to represent "no work to wait for".
    pub fn new(signaled: bool) -> Arc<Self> {
        let fence: Option<Arc<dyn Fence>> = signaled.then(|| SoftwareFence::new_signaled() as _);

        Arc::new(Self {
            inner: SpinLock::new(Inner {
                fence,
                points: BTreeMap::new(),
                last_submitted: 0,
                last_signaled: 0,
                submit_waiters: Vec::new(),
                eventfds: Vec::new(),
            }),
        })
    }

    /// Returns the binary fence, if one is installed.
    pub fn fence(&self) -> Option<Arc<dyn Fence>> {
        self.inner.lock().fence.clone()
    }

    /// Replaces the binary fence.
    ///
    /// The previous fence is simply dropped: a sync object is a *container*,
    /// not a queue, so replacing it does not cancel anything and does not wait
    /// for what was there. Whoever was already waiting on the old fence keeps
    /// waiting on the old fence, which is upstream's behaviour and what makes
    /// `SYNCOBJ_RESET` safe to call while a wait is in flight.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L347>
    /// (`drm_syncobj_replace_fence`).
    pub fn replace_fence(&self, fence: Option<Arc<dyn Fence>>) {
        let installed = fence.is_some();

        let (old_fence, waiters, eventfds) = {
            let mut inner = self.inner.lock();
            let old_fence = core::mem::replace(&mut inner.fence, fence);
            let waiters = if installed {
                core::mem::take(&mut inner.submit_waiters)
            } else {
                Vec::new()
            };
            let eventfds = take_ready_eventfds(&mut inner);
            (old_fence, waiters, eventfds)
        };

        drop(old_fence);
        notify_all(waiters);
        fire_eventfds(eventfds);
    }

    /// Installs an already-signalled binary fence, as `SYNCOBJ_SIGNAL` does.
    pub fn signal(&self) {
        self.replace_fence(Some(SoftwareFence::new_signaled()));
    }

    /// Empties the object, as `SYNCOBJ_RESET` does.
    ///
    /// Timeline points go too, and `last_submitted` returns to zero: upstream
    /// resets a timeline object by replacing its whole chain with `NULL`, so a
    /// reset timeline restarts its numbering. A client that resets an object
    /// other clients are still waiting on has not cancelled their waits — see
    /// [`replace_fence`](Self::replace_fence).
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1330>
    /// (`drm_syncobj_reset_ioctl`).
    pub fn reset(&self) {
        let retired = {
            let mut inner = self.inner.lock();
            let retired = (inner.fence.take(), core::mem::take(&mut inner.points));
            inner.last_submitted = 0;
            inner.last_signaled = 0;
            retired
        };
        // Driver fences can release callbacks and resources in their destructor.
        drop(retired);
    }

    /// Returns the highest point ever submitted on the timeline.
    ///
    /// What `SYNCOBJ_QUERY` reports with `DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED`.
    pub fn last_submitted_point(&self) -> u64 {
        self.inner.lock().last_submitted
    }

    /// Returns the highest point the timeline has actually reached.
    ///
    /// A point counts as reached only when it and every earlier submitted
    /// point have signalled, because a timeline value is a promise that
    /// everything up to it is done. Out-of-order completion is therefore
    /// invisible here, which is the property a client comparing values relies
    /// on.
    ///
    /// Divergence: upstream reports 0 for a binary sync object, since its
    /// fence is not a chain and carries no timeline value. Reproduced exactly,
    /// including for a signalled binary object.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1424>
    /// (`drm_syncobj_query_ioctl`).
    pub fn signaled_point(&self) -> u64 {
        completed_point(&self.inner.lock())
    }

    /// Submits a timeline point.
    ///
    /// # Errors
    ///
    /// [`DrmError::InvalidArgument`] for point zero, which names the binary
    /// fence rather than a timeline point, and for any point that is not
    /// strictly greater than every point submitted so far. Rule C6: a point
    /// that went backwards is never quietly reordered, because a client that
    /// numbered its work wrongly must find out rather than see its
    /// synchronization silently stop meaning anything.
    pub fn add_point(&self, point: u64, fence: Arc<dyn Fence>) -> DrmResult<()> {
        loop {
            let (submitted, previous, retired) = {
                let mut inner = self.inner.lock();
                if point == 0 || point <= inner.last_submitted {
                    return Err(DrmError::InvalidArgument);
                }
                let retired = take_completed_points(&mut inner);
                let previous = inner
                    .points
                    .last_key_value()
                    .map(|(_, fence)| fence.clone());
                (inner.last_submitted, previous, retired)
            };
            drop(retired);

            // A completed prefix needs no wrapper, preserving native driver
            // fences. Callback registration can invoke a driver's hook inline,
            // so prepare it outside the sync-object lock and propagate failure.
            let chain = previous.as_ref().map(|previous| {
                TimelineFence::new(
                    previous.clone(),
                    fence.clone(),
                    point,
                    TimelinePropagation::shared(),
                )
            });
            if let Some(chain) = &chain {
                chain.enable_signaling()?;
            }

            let (waiters, eventfds) = {
                let mut inner = self.inner.lock();
                let tail = inner.points.last_key_value().map(|(_, fence)| fence);
                let same_tail = match (tail, previous.as_ref()) {
                    (None, None) => true,
                    (Some(tail), Some(previous)) => Arc::ptr_eq(tail, previous),
                    _ => false,
                };
                // A reset or another submission may have changed the prefix
                // during registration. Rebuild against its new obligations.
                if inner.last_submitted != submitted || !same_tail {
                    drop(inner);
                    continue;
                }
                inner.last_submitted = point;
                let stored: Arc<dyn Fence> = match chain {
                    Some(chain) => chain,
                    None => fence.clone(),
                };
                inner.points.insert(point, stored);
                (
                    core::mem::take(&mut inner.submit_waiters),
                    take_ready_eventfds(&mut inner),
                )
            };
            notify_all(waiters);
            fire_eventfds(eventfds);
            return Ok(());
        }
    }

    /// Submits a timeline point that is already signalled, as
    /// `SYNCOBJ_TIMELINE_SIGNAL` does.
    pub fn signal_point(&self, point: u64) -> DrmResult<()> {
        self.add_point(point, SoftwareFence::new_signaled())
    }

    /// Registers an eventfd to be incremented when `point` reaches `flags`.
    ///
    /// Does not block. If the point is already in the requested state the
    /// eventfd is incremented before this returns; otherwise the watch stays
    /// on the object and is retried from [`replace_fence`](Self::replace_fence)
    /// and [`add_point`](Self::add_point). That is linux-6.6
    /// `drm_syncobj_add_eventfd`: the ioctl always succeeds once the handle
    /// and the eventfd are valid, even if nothing has been submitted yet.
    ///
    /// `wait_available` is `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE`: increment
    /// as soon as a fence exists, without waiting for it to signal.
    ///
    /// Actions run after the object's lock is dropped. A fence callback
    /// that already-signalled runs inline, and [`Eventfd::signal`] must not
    /// take this lock — the same order as [`notify_all`].
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L309>
    /// (`drm_syncobj_add_eventfd`) and
    /// <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1375>
    /// (`syncobj_eventfd_entry_func`).
    pub fn add_eventfd(&self, point: u64, wait_available: bool, eventfd: Arc<dyn Eventfd>) {
        let actions = {
            let mut inner = self.inner.lock();
            inner.eventfds.push(EventfdWatch {
                point,
                wait_available,
                eventfd,
            });
            take_ready_eventfds(&mut inner)
        };

        fire_eventfds(actions);
    }

    /// Reports what a wait on `point` is waiting for.
    ///
    /// Point zero asks about the binary fence: it is [`Pending`] with that
    /// fence, or [`Unsubmitted`] if the object is empty. A signalled fence is
    /// still reported as [`Pending`] — a wait on it succeeds immediately
    /// anyway, and folding it into [`Passed`] would lose the fence's error
    /// status, which the waiter has to see.
    ///
    /// [`Pending`]: TimelinePoint::Pending
    /// [`Unsubmitted`]: TimelinePoint::Unsubmitted
    /// [`Passed`]: TimelinePoint::Passed
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-fence-chain.c#L96>
    /// (`dma_fence_chain_find_seqno`).
    pub fn point_state(&self, point: u64) -> TimelinePoint {
        point_state_locked(&self.inner.lock(), point)
    }

    /// Returns the fence at a timeline point, or the binary fence for point
    /// zero.
    ///
    /// `None` covers both "already passed" and "not submitted", so callers
    /// that must tell them apart want [`point_state`](Self::point_state).
    pub fn fence_at(&self, point: u64) -> Option<Arc<dyn Fence>> {
        match self.point_state(point) {
            TimelinePoint::Pending(fence) => Some(fence),
            TimelinePoint::Passed | TimelinePoint::Unsubmitted => None,
        }
    }

    /// Attaches a waiter until a fence covering `point` is installed.
    ///
    /// The counterpart of [`Fence::attach`] for the case where there is no
    /// fence yet, which is what `DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT` asks
    /// about. Returns `true` if this point is already submitted, in which case
    /// nothing was attached and the caller was notified inline — the same
    /// no-lost-wakeup shape as rule C3, for the same reason.
    pub fn attach_submit(&self, waiter: &Arc<FenceWaiter>, index: usize, point: u64) -> bool {
        {
            let mut inner = self.inner.lock();
            if matches!(
                point_state_locked(&inner, point),
                TimelinePoint::Unsubmitted
            ) {
                inner.submit_waiters.push((index, waiter.clone()));
                return false;
            }
        }

        waiter.notify(index);
        true
    }

    /// Detaches a waiter attached by [`attach_submit`](Self::attach_submit).
    pub fn detach_submit(&self, waiter: &Arc<FenceWaiter>) {
        self.inner
            .lock()
            .submit_waiters
            .retain(|(_, attached)| !Arc::ptr_eq(attached, waiter));
    }

    /// Returns how many submission waiters are attached.
    ///
    /// Diagnostic, for the same reason [`Fence::waiter_count`] is: rule C4
    /// applies here too.
    pub fn submit_waiter_count(&self) -> usize {
        self.inner.lock().submit_waiters.len()
    }

    /// Drops the timeline points whose fences have signalled and which no
    /// later wait can name.
    ///
    /// The completed prefix is recorded separately, so pruning does not change
    /// [`Self::signaled_point`] or make an earlier wait resolve to a later fence.
    /// New submissions also prune the prefix to bound retained completed work.
    ///
    /// [`signaled_point`]: Self::signaled_point
    pub fn prune(&self) {
        let retired = take_completed_points(&mut self.inner.lock());
        drop(retired);
    }
}

/// Returns the completed prefix, including points already retired.
fn completed_point(inner: &Inner) -> u64 {
    let mut value = inner.last_signaled;
    for (point, fence) in &inner.points {
        if !fence.is_signaled() {
            break;
        }
        value = *point;
    }
    value
}

/// Detaches the completed prefix under the same lock as reset and submission.
/// The returned fences must be dropped after that lock is released.
fn take_completed_points(inner: &mut Inner) -> BTreeMap<u64, Arc<dyn Fence>> {
    let signaled = completed_point(inner);
    if signaled == inner.last_signaled {
        return BTreeMap::new();
    }
    inner.last_signaled = signaled;
    let Some(next) = signaled.checked_add(1) else {
        return core::mem::take(&mut inner.points);
    };
    let pending = inner.points.split_off(&next);
    core::mem::replace(&mut inner.points, pending)
}

/// Notifies a drained waiter list, outside the object's lock.
///
/// Same rule as [`FenceState::signal`]: the wake path takes the wait queue's
/// lock, which must never nest inside an object's.
///
/// [`FenceState::signal`]: super::FenceState::signal
fn notify_all(waiters: Vec<(usize, Arc<FenceWaiter>)>) {
    for (index, waiter) in waiters {
        waiter.notify(index);
    }
}

/// The body of [`SyncObj::point_state`], for callers that already hold the lock.
fn point_state_locked(inner: &Inner, point: u64) -> TimelinePoint {
    if point == 0 {
        return match &inner.fence {
            Some(fence) => TimelinePoint::Pending(fence.clone()),
            None => TimelinePoint::Unsubmitted,
        };
    }

    if point <= inner.last_signaled {
        return TimelinePoint::Passed;
    }

    // The first submitted point at or above the one asked for is the one
    // that will satisfy the wait: points signal in order, so anything
    // later implies it.
    match inner.points.range(point..).next() {
        Some((_, fence)) => TimelinePoint::Pending(fence.clone()),
        // Nothing submitted at or above it. Either the timeline has run
        // past it — in which case the wait is already satisfied — or the
        // work simply has not been submitted yet.
        None if point <= inner.last_submitted => TimelinePoint::Passed,
        None => TimelinePoint::Unsubmitted,
    }
}

/// Pulls every watch that can make progress off the list.
///
/// The vec is taken first so the classify step can borrow `inner` without
/// fighting `retain`. Watches that are still unsubmitted go back.
fn take_ready_eventfds(inner: &mut Inner) -> Vec<EventfdAction> {
    let watches = core::mem::take(&mut inner.eventfds);
    let mut ready = Vec::new();
    for watch in watches {
        match classify_watch(inner, &watch) {
            None => inner.eventfds.push(watch),
            Some(action) => ready.push(action),
        }
    }
    ready
}

/// Decides whether a watch can fire given the object's current contents.
///
/// Mirrors linux-6.6 `syncobj_eventfd_entry_func`: no fence yet keeps the
/// entry; a point the timeline has already passed is a stub that is already
/// signalled; `WAIT_AVAILABLE` fires as soon as a fence exists.
fn classify_watch(inner: &Inner, watch: &EventfdWatch) -> Option<EventfdAction> {
    match point_state_locked(inner, watch.point) {
        TimelinePoint::Unsubmitted => None,
        TimelinePoint::Passed => Some(EventfdAction::Signal(watch.eventfd.clone())),
        TimelinePoint::Pending(_) if watch.wait_available => {
            Some(EventfdAction::Signal(watch.eventfd.clone()))
        }
        TimelinePoint::Pending(fence) => {
            Some(EventfdAction::WaitFence(fence, watch.eventfd.clone()))
        }
    }
}

/// Runs the actions collected under the object's lock, outside that lock.
fn fire_eventfds(actions: Vec<EventfdAction>) {
    for action in actions {
        match action {
            EventfdAction::Signal(eventfd) => eventfd.signal(),
            EventfdAction::WaitFence(fence, eventfd) => {
                // Already-signalled fences run the callback inline, which is
                // the immediate increment linux-6.6 gets from
                // `dma_fence_add_callback` returning `-ENOENT`.
                let _ = fence.add_callback(Arc::new(move |_status| {
                    eventfd.signal();
                }));
            }
        }
    }
}

/// Keeps a waiter attached to a set of sync objects awaiting submission.
///
/// The submission-side twin of [`Attachment`], and it exists for the same
/// reason: a wait that gives up must not leave a waiter on an object, or the
/// next submission walks a list of threads that are long gone.
///
/// [`Attachment`]: super::Attachment
#[derive(Debug)]
pub struct SubmitAttachment<'a> {
    waiter: Arc<FenceWaiter>,
    objects: &'a [(Arc<SyncObj>, u64)],
}

impl<'a> SubmitAttachment<'a> {
    /// Attaches `waiter` to every object in `objects`.
    pub fn new(waiter: Arc<FenceWaiter>, objects: &'a [(Arc<SyncObj>, u64)]) -> Self {
        for (index, (object, point)) in objects.iter().enumerate() {
            object.attach_submit(&waiter, index, *point);
        }

        Self { waiter, objects }
    }
}

impl Drop for SubmitAttachment<'_> {
    fn drop(&mut self) {
        for (object, _) in self.objects {
            object.detach_submit(&self.waiter);
        }
    }
}

/// A file's sync-object table.
///
/// Sync objects are per-file, exactly as GEM handles are, and for the same
/// reason: the handle namespace belongs to the open file, so two clients may
/// hold the same number for different objects. Sharing one across processes
/// goes through `SYNCOBJ_HANDLE_TO_FD`, never through the number.
///
/// # Locking
///
/// A `SpinLock` holding a leaf: nothing under it calls back into the fence
/// machinery, and objects removed from the table are dropped after the lock is
/// released so that releasing their fences does not happen with it held.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1052>
/// (`drm_syncobj_open`, which allocates the equivalent `idr`).
#[derive(Debug)]
pub struct SyncObjTable {
    inner: SpinLock<TableInner>,
}

impl Default for SyncObjTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct TableInner {
    objects: BTreeMap<u32, Arc<SyncObj>>,
    /// Handles below `next` that have been closed and can be handed out again,
    /// so that the space stays dense the way Linux's `idr` keeps it.
    reusable: BTreeSet<u32>,
    /// The lowest handle never yet handed out. Starts at 1, because zero means
    /// "no object" to user space.
    next: u32,
}

impl SyncObjTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(TableInner {
                objects: BTreeMap::new(),
                reusable: BTreeSet::new(),
                next: 1,
            }),
        }
    }

    /// Installs an object and returns its new handle.
    ///
    /// The table takes its own reference, so an object shared through a file
    /// descriptor and then imported twice is one object with two handles.
    ///
    /// # Errors
    ///
    /// [`DrmError::NoSpace`] once the table holds [`MAX_SYNCOBJS_PER_FILE`].
    pub fn insert(&self, object: Arc<SyncObj>) -> DrmResult<SyncObjHandle> {
        let mut inner = self.inner.lock();

        if inner.objects.len() >= MAX_SYNCOBJS_PER_FILE {
            return Err(DrmError::NoSpace);
        }

        let handle = match inner.reusable.pop_first() {
            Some(handle) => handle,
            None => {
                let handle = inner.next;
                if handle == u32::MAX {
                    return Err(DrmError::NoSpace);
                }
                inner.next = handle + 1;
                handle
            }
        };
        inner.objects.insert(handle, object);

        Ok(SyncObjHandle(handle))
    }

    /// Resolves a handle.
    ///
    /// # Errors
    ///
    /// [`DrmError::NotFound`] (`ENOENT`) if the handle is not in this table,
    /// which is what `drm_syncobj_find` failing reports for every sync-object
    /// ioctl, including the one that destroys a handle. That is the opposite
    /// of GEM, where a bad handle is `EINVAL` on the delete path; both are
    /// ABI, so neither may be tidied up.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1301>
    /// (`drm_syncobj_destroy_ioctl`).
    pub fn get(&self, handle: SyncObjHandle) -> DrmResult<Arc<SyncObj>> {
        self.inner
            .lock()
            .objects
            .get(&handle.0)
            .cloned()
            .ok_or(DrmError::NotFound)
    }

    /// Removes a handle, dropping this table's reference to the object.
    ///
    /// # Errors
    ///
    /// [`DrmError::NotFound`] (`ENOENT`) if the handle is not in this table.
    pub fn remove(&self, handle: SyncObjHandle) -> DrmResult<()> {
        let object = {
            let mut inner = self.inner.lock();
            let object = inner.objects.remove(&handle.0).ok_or(DrmError::NotFound)?;
            inner.reusable.insert(handle.0);
            object
        };

        // Dropped outside the lock: this may be the last reference, and
        // releasing its fences may run their callbacks.
        drop(object);

        Ok(())
    }

    /// Returns how many handles are open.
    pub fn len(&self) -> usize {
        self.inner.lock().objects.len()
    }

    /// Returns whether no handle is open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolves several handles at once.
    ///
    /// All or nothing, matching `drm_syncobj_array_find`: a wait naming one
    /// bad handle among twenty fails outright rather than waiting on the
    /// nineteen good ones, because a client that miscounted has no way to tell
    /// a partial wait from a complete one.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1258>
    /// (`drm_syncobj_array_find`).
    pub fn get_many(&self, handles: &[u32]) -> DrmResult<Vec<Arc<SyncObj>>> {
        let inner = self.inner.lock();

        handles
            .iter()
            .map(|handle| inner.objects.get(handle).cloned().ok_or(DrmError::NotFound))
            .collect()
    }
}
