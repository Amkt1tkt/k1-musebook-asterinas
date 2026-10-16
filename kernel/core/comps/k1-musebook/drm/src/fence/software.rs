// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The fence state machine, and the fence a creator signals explicitly.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-fence.c>.
//!
//! Everything subtle about fences is in [`FenceState`]. The rules it
//! implements are stated in the [module documentation](super) of the parent
//! module; the comments here explain *how*, not *what*.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use aster_softirq::BottomHalfDisabled;
use ostd::sync::SpinLock;

use super::{Fence, FenceContext, FenceId, FenceStatus, FenceWaiter};
use crate::error::{DrmError, DrmResult};

/// A function run when a fence signals.
///
/// Invoked with the terminal status, so that a consumer chaining work off a
/// fence learns whether the work it was waiting for actually succeeded without
/// having to keep a reference to the fence itself.
///
/// Runs in whatever context called [`FenceState::signal`], which may be a
/// bottom half: it must not sleep. Hard interrupt callers are not supported.
pub type FenceCallback = Arc<dyn Fn(FenceStatus) + Send + Sync>;

/// The largest number of dependencies one fence may declare.
///
/// Divergence: Linux imposes no limit; `drm_sched_job_add_dependency` grows an
/// `xarray`. The cycle check in [`FenceState::add_dependency`] walks the whole
/// reachable graph, so an unbounded fan-out is an unbounded amount of work
/// under [`GRAPH`], done on behalf of an unprivileged client. PowerVR's
/// `SUBMIT_JOBS` carries at most a handful of sync operations per job, so this
/// is orders of magnitude above any real request.
pub const MAX_DEPENDENCIES: usize = 64;

/// The largest number of fences the cycle check will visit before giving up.
///
/// With [`MAX_DEPENDENCIES`] edges per node this bounds the check at a few
/// thousand fence-lock acquisitions. Exceeding it is reported as
/// [`DrmError::WouldDeadlock`] rather than accepted, because "the graph is too
/// tangled to prove acyclic" and "the graph has a cycle" have the same right
/// answer: refuse the edge.
const MAX_GRAPH_NODES: usize = 4096;

/// Serializes dependency-graph edits.
///
/// One global lock rather than a per-fence one, because the check and the
/// insertion must be atomic together: two CPUs adding `A → B` and `B → A` at
/// the same moment would otherwise each walk a still-acyclic graph and both
/// succeed, which is precisely the cycle rule C7 exists to prevent.
///
/// Lock order: this lock is above every per-fence lock. A thread holding it
/// takes fence locks while walking; nothing takes it while holding one.
pub(crate) static GRAPH: SpinLock<()> = SpinLock::new(());

/// The shared state machine behind every fence.
///
/// Hold one of these in a fence implementation and forward [`Fence`]'s methods
/// to it. It owns the terminal status, the waiter list, the callback list and
/// the dependency edges, and it is the only place in the crate that implements
/// the signal/attach race of rule C3.
#[derive(Debug)]
pub struct FenceState {
    context: FenceContext,
    seqno: u64,
    /// A lock-free mirror of `inner.status.is_signaled()`.
    ///
    /// [`is_signaled`](Self::is_signaled) is on the fast path of every wait
    /// loop and of the cycle check, both of which run over whole arrays of
    /// fences. Written under `inner` with `Release` and read with `Acquire`,
    /// so a reader that observes `true` also observes everything the signaller
    /// did before the transition.
    signaled: AtomicBool,
    // Process attachment may be interrupted by a GPU completion bottom half.
    inner: SpinLock<Inner, BottomHalfDisabled>,
}

struct Inner {
    status: FenceStatus,
    /// Attached waiters and the index each was registered under.
    ///
    /// A `Vec`, not a map: a fence has a handful of waiters at most, and the
    /// operations are "append", "remove by identity" and "drain", all of which
    /// a short vector does better than a tree.
    waiters: Vec<(usize, Arc<FenceWaiter>)>,
    callbacks: Vec<FenceCallback>,
    /// Fences that must signal before this one may; see rule C7.
    dependencies: Vec<Arc<dyn Fence>>,
}

// A callback is a closure and closures are not `Debug`, so they are counted
// rather than printed. Nothing else here is interesting enough to hand-write.
impl core::fmt::Debug for Inner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inner")
            .field("status", &self.status)
            .field("waiters", &self.waiters.len())
            .field("callbacks", &self.callbacks.len())
            .field("dependencies", &self.dependencies.len())
            .finish()
    }
}

impl FenceState {
    /// Creates an unsignalled fence state.
    pub fn new(context: FenceContext, seqno: u64) -> Self {
        Self {
            context,
            seqno,
            signaled: AtomicBool::new(false),
            inner: SpinLock::new(Inner {
                status: FenceStatus::Pending,
                waiters: Vec::new(),
                callbacks: Vec::new(),
                dependencies: Vec::new(),
            }),
        }
    }

    /// Creates a state that has already reached `status`.
    ///
    /// `DRM_SYNCOBJ_CREATE_SIGNALED` needs this, and so does every place that
    /// has to hand out a fence for work that is already done.
    pub fn new_signaled(context: FenceContext, seqno: u64, status: FenceStatus) -> Self {
        let state = Self::new(context, seqno);
        if status.is_signaled() {
            state.signal(status.error());
        }
        state
    }

    /// Returns the context this fence belongs to.
    pub fn context(&self) -> FenceContext {
        self.context
    }

    /// Returns this fence's sequence number within its context.
    pub fn seqno(&self) -> u64 {
        self.seqno
    }

    /// Returns this fence's identity.
    pub fn id(&self) -> FenceId {
        FenceId {
            context: self.context,
            seqno: self.seqno,
        }
    }

    /// Returns whether the fence has signalled, without taking the lock.
    pub fn is_signaled(&self) -> bool {
        self.signaled.load(Ordering::Acquire)
    }

    /// Returns the fence's status.
    pub fn status(&self) -> FenceStatus {
        self.inner.lock().status
    }

    /// Performs the one transition a fence has, waking every waiter and
    /// running every callback.
    ///
    /// Returns whether *this* call performed it; a second call is a no-op, by
    /// rule C1.
    ///
    /// # Why the notifications happen outside the lock
    ///
    /// Two reasons, and both are load-bearing. A callback may do anything
    /// short of sleeping, including touching this very fence, so running one
    /// under the fence's own lock would deadlock. And a waiter's wake-up takes
    /// the wait queue's lock, which must not nest inside a fence lock or two
    /// fences signalling into one waiter could order the two locks both ways.
    ///
    /// Moving the notifications out does not open a window, because the status
    /// is already terminal when the lock is released: an [`attach`] that
    /// arrives in that window sees the terminal state and reports it inline
    /// rather than joining a list nobody will walk again. That is rule C3.
    ///
    /// [`attach`]: Self::attach
    pub fn signal(&self, error: Option<DrmError>) -> bool {
        let (waiters, callbacks, dependencies, status) = {
            let mut inner = self.inner.lock();
            if inner.status.is_signaled() {
                return false;
            }

            inner.status = match error {
                Some(error) => FenceStatus::Failed(error),
                None => FenceStatus::Signaled,
            };
            // Release, so that a reader of the lock-free mirror sees a fully
            // written `Inner` behind it.
            self.signaled.store(true, Ordering::Release);

            // The dependency edges are dead weight once the fence has
            // completed: nothing will consult them again, and holding them
            // keeps whole finished job graphs alive.

            (
                core::mem::take(&mut inner.waiters),
                core::mem::take(&mut inner.callbacks),
                core::mem::take(&mut inner.dependencies),
                inner.status,
            )
        };

        // Dropping a dependency may invoke a driver destructor.
        drop(dependencies);
        for (index, waiter) in waiters {
            waiter.notify(index);
        }
        for callback in callbacks {
            callback(status);
        }

        true
    }

    /// Attaches a waiter; see [`Fence::attach`].
    pub fn attach(&self, waiter: &Arc<FenceWaiter>, index: usize) -> bool {
        {
            let mut inner = self.inner.lock();
            if !inner.status.is_signaled() {
                inner.waiters.push((index, waiter.clone()));
                return false;
            }
        }

        // Already terminal, so nobody will ever walk the list again. Notify
        // inline instead, outside the lock for the reasons in `signal`.
        waiter.notify(index);
        true
    }

    /// Detaches a waiter; see [`Fence::detach`].
    pub fn detach(&self, waiter: &Arc<FenceWaiter>) {
        self.inner
            .lock()
            .waiters
            .retain(|(_, attached)| !Arc::ptr_eq(attached, waiter));
    }

    /// Returns how many waiters are attached.
    pub fn waiter_count(&self) -> usize {
        self.inner.lock().waiters.len()
    }

    /// Registers a callback; see [`Fence::add_callback`].
    pub fn add_callback(&self, callback: FenceCallback) -> DrmResult<()> {
        let status = {
            let mut inner = self.inner.lock();
            if !inner.status.is_signaled() {
                inner.callbacks.push(callback);
                return Ok(());
            }
            inner.status
        };

        callback(status);
        Ok(())
    }

    /// Returns the fences this one must wait for.
    pub fn dependencies(&self) -> Vec<Arc<dyn Fence>> {
        self.inner.lock().dependencies.clone()
    }

    /// Returns whether every declared dependency has signalled.
    ///
    /// Nothing calls this for you: a fence is not signalled by its
    /// dependencies completing. Rule C2 remains the creator's obligation, and
    /// this is the question a scheduler asks when deciding whether a job is
    /// runnable.
    pub fn dependencies_met(&self) -> bool {
        self.dependencies()
            .iter()
            .all(|dependency| dependency.is_signaled())
    }

    /// Records that this fence may not signal until `dependency` has.
    ///
    /// The edge is refused if it would close a cycle, which is rule C7 and the
    /// whole reason this function is not a `Vec::push`.
    ///
    /// An already-signalled dependency is dropped on the floor and reported as
    /// success: it cannot be part of a cycle and it constrains nothing.
    ///
    /// # Errors
    ///
    /// - [`DrmError::WouldDeadlock`] (`EDEADLK`) if this fence is reachable
    ///   from `dependency`, including the degenerate case of depending on
    ///   itself, or if the graph is too large to prove acyclic within
    ///   [`MAX_GRAPH_NODES`].
    /// - [`DrmError::InvalidArgument`] if this fence has already signalled. A
    ///   completed fence cannot acquire new obligations, and a scheduler that
    ///   tries has lost track of a job's lifetime.
    /// - [`DrmError::NoSpace`] once [`MAX_DEPENDENCIES`] edges are recorded.
    pub fn add_dependency(&self, dependency: &Arc<dyn Fence>) -> DrmResult<()> {
        if dependency.is_signaled() {
            return Ok(());
        }

        let id = self.id();
        if dependency.id() == id {
            return Err(DrmError::WouldDeadlock);
        }

        // Held across the walk *and* the insertion; see `GRAPH`.
        let _graph = GRAPH.lock();

        if self.is_signaled() {
            return Err(DrmError::InvalidArgument);
        }
        if reaches(dependency, id)? {
            return Err(DrmError::WouldDeadlock);
        }

        let mut inner = self.inner.lock();
        // Signalling does not need GRAPH and may finish during the walk.
        if inner.status.is_signaled() {
            return Err(DrmError::InvalidArgument);
        }
        if inner.dependencies.len() >= MAX_DEPENDENCIES {
            return Err(DrmError::NoSpace);
        }
        inner.dependencies.push(dependency.clone());

        Ok(())
    }
}

/// Returns whether `target` is reachable from `start` along dependency edges.
///
/// An explicit stack rather than recursion: the depth is bounded only by what
/// a client submitted, and kernel stacks are small. The visited set is keyed by
/// [`FenceId`] because that is a fence's identity (rule C8) and because
/// comparing `Arc<dyn Fence>` pointers would miss a diamond that reaches the
/// same fence through two different `Arc`s of one allocation.
///
/// Runs with [`GRAPH`] held and takes each visited fence's own lock through
/// [`Fence::dependencies`], which is the documented lock order.
fn reaches(start: &Arc<dyn Fence>, target: FenceId) -> DrmResult<bool> {
    let mut visited = BTreeSet::new();
    let mut stack = alloc::vec![start.clone()];

    while let Some(fence) = stack.pop() {
        let id = fence.id();
        if id == target {
            return Ok(true);
        }
        if !visited.insert(id) {
            continue;
        }
        if visited.len() > MAX_GRAPH_NODES {
            return Err(DrmError::WouldDeadlock);
        }

        // A signalled fence waits for nothing, so nothing beyond it can close
        // a cycle. Pruning here also keeps the walk out of the finished part
        // of a long-running client's graph.
        if fence.is_signaled() {
            continue;
        }

        stack.extend(fence.dependencies());
    }

    Ok(false)
}

/// A fence that the creator signals explicitly.
///
/// The common case for software-driven completions: page flips, the fences
/// `SYNCOBJ_SIGNAL` installs, and the CPU-side half of the PowerVR sync
/// implementation. A hardware fence is the same object with its
/// [`signal`](Self::signal) called from a completion bottom half instead.
#[derive(Debug)]
pub struct SoftwareFence {
    state: FenceState,
}

impl SoftwareFence {
    /// Creates an unsignalled fence.
    pub fn new(context: FenceContext, seqno: u64) -> Arc<Self> {
        Arc::new(Self {
            state: FenceState::new(context, seqno),
        })
    }

    /// Creates a fence that has already signalled successfully.
    ///
    /// Its context is a fresh one, so it collides with nothing; a caller that
    /// needs the fence to belong to an existing timeline should build it with
    /// [`new`](Self::new) and signal it.
    pub fn new_signaled() -> Arc<Self> {
        Arc::new(Self {
            state: FenceState::new_signaled(FenceContext::new(), 0, FenceStatus::Signaled),
        })
    }

    /// Signals the fence, waking every waiter and running every callback.
    ///
    /// Idempotent: signalling twice is a no-op rather than an error, because
    /// error paths and completion paths can race. Returns whether this call
    /// was the one that signalled.
    pub fn signal(&self, error: Option<DrmError>) -> bool {
        self.state.signal(error)
    }

    /// Returns the shared state, so that a caller can reach
    /// [`FenceState::add_dependency`] and the rest.
    pub fn state(&self) -> &FenceState {
        &self.state
    }
}

impl Fence for SoftwareFence {
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
        self.state.dependencies()
    }

    fn attach(&self, waiter: &Arc<FenceWaiter>, index: usize) -> bool {
        self.state.attach(waiter, index)
    }

    fn detach(&self, waiter: &Arc<FenceWaiter>) {
        self.state.detach(waiter)
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

#[cfg(ktest)]
mod regression {
    use alloc::sync::Weak;

    use ostd::prelude::ktest;

    use super::*;

    #[derive(Debug)]
    struct ReentrantDependency {
        state: FenceState,
        target: Weak<FenceState>,
        signal_during_walk: bool,
    }

    impl Drop for ReentrantDependency {
        fn drop(&mut self) {
            if let Some(target) = self.target.upgrade() {
                assert!(
                    target.inner.try_lock().is_some(),
                    "dependency destructor ran under its owner's lock"
                );
            }
        }
    }

    impl Fence for ReentrantDependency {
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
            // Deterministically place a legal completion between GRAPH's
            // first terminal check and insertion into the target's state.
            if self.signal_during_walk {
                self.target.upgrade().unwrap().signal(None);
            }
            Vec::new()
        }
        fn attach(&self, waiter: &Arc<FenceWaiter>, index: usize) -> bool {
            self.state.attach(waiter, index)
        }
        fn detach(&self, waiter: &Arc<FenceWaiter>) {
            self.state.detach(waiter)
        }
        fn waiter_count(&self) -> usize {
            self.state.waiter_count()
        }
        fn add_callback(&self, callback: FenceCallback) -> DrmResult<()> {
            self.state.add_callback(callback)
        }
    }

    fn dependency(target: &Arc<FenceState>, signal_during_walk: bool) -> Arc<dyn Fence> {
        Arc::new(ReentrantDependency {
            state: FenceState::new(FenceContext::new(), 1),
            target: Arc::downgrade(target),
            signal_during_walk,
        })
    }

    #[ktest]
    fn completion_during_graph_walk_cannot_add_a_new_dependency() {
        let target = Arc::new(FenceState::new(FenceContext::new(), 1));
        assert_eq!(
            target.add_dependency(&dependency(&target, true)),
            Err(DrmError::InvalidArgument)
        );
        assert!(target.is_signaled());
        assert!(target.dependencies().is_empty());
    }

    #[ktest]
    fn completed_dependencies_are_destroyed_outside_the_state_lock() {
        let target = Arc::new(FenceState::new(FenceContext::new(), 1));
        let edge = dependency(&target, false);
        target.add_dependency(&edge).unwrap();
        let weak = Arc::downgrade(&edge);
        drop(edge);
        assert!(target.signal(None));
        assert!(weak.upgrade().is_none());
    }
}
