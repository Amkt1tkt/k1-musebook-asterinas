// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Tests for the fence contract and the sync-object ioctls.
//!
//! Every rule in the parent module's contract has at least one test here, and
//! the test is named in the rule. That pairing is the point: a scheduler built
//! on this module reasons from the contract, so a rule with no test is a
//! promise nobody checked, and a test with no rule is behaviour nobody agreed
//! to depend on.
//!
//! # How a blocking wait is tested without a second thread
//!
//! A ktest runs on one thread, so there is nobody to signal a fence while that
//! thread is parked. [`TestBlocker`] stands in for the other CPU: it is a
//! [`Blocker`] that, instead of sleeping, runs a scripted action at exactly
//! the point a real one would have gone to sleep, then reports whether the
//! wait became satisfied. That covers everything except the sleep itself —
//! attach/detach ordering, the no-lost-wakeup window, the deadline and
//! interruption paths, and what is left behind afterwards.

use alloc::{boxed::Box, vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ostd::{
    prelude::ktest,
    sync::{SpinLock, WaitQueue},
};
use ostd_pod::Pod;

use super::*;
use crate::{
    UserMem,
    device::{DriverFeatures, DriverInfo, DrmDevice, DrmDriver, DrmMinor},
    file::{DrmFile, Privilege},
    ioctl::CallerContext,
    prime::{Descriptors, DrmExport, Eventfd, ExportFlags},
    uapi::{ioctl_nr, sync::*},
};

/// Copies a structure into the byte payload the dispatcher takes.
fn payload_of<T: Pod>(value: &T) -> Vec<u8> {
    value.as_bytes().to_vec()
}

/// A driver that claims both sync-object capabilities and nothing else.
#[derive(Debug)]
struct SyncDriver;

impl DrmDriver for SyncDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "syncobj-test",
            date: "20260729",
            desc: "a driver with sync objects",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::RENDER
            .union(DriverFeatures::GEM)
            .union(DriverFeatures::SYNCOBJ)
            .union(DriverFeatures::SYNCOBJ_TIMELINE)
    }
}

/// A driver with sync objects but no timelines, to check the second gate.
#[derive(Debug)]
struct BinaryOnlyDriver;

impl DrmDriver for BinaryOnlyDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "syncobj-binary",
            date: "20260729",
            desc: "a driver with binary sync objects only",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::RENDER
            .union(DriverFeatures::GEM)
            .union(DriverFeatures::SYNCOBJ)
    }
}

/// A driver claiming neither sync-object bit, to check the first gate.
///
/// Keep its feature set independent of `SoftDriver` so changes to the
/// software display driver cannot change the permission test fixture.
#[derive(Debug)]
struct PlainDriver;

impl DrmDriver for PlainDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "syncobj-none",
            date: "20260802",
            desc: "a driver with no sync objects",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::RENDER.union(DriverFeatures::GEM)
    }
}

/// The base address of [`TestUser`]'s pretend address space.
///
/// Above zero, so that a handler which forgets to reject a null pointer fails
/// here rather than reading offset zero of the buffer.
const USER_BASE: u64 = 0x1000_0000;

/// A [`UserMem`] backed by a flat buffer, plus a descriptor table and a
/// blocker.
struct TestUser {
    memory: SpinLock<Vec<u8>>,
    descriptors: TestDescriptors,
    blocker: TestBlocker,
}

impl TestUser {
    fn new() -> Self {
        Self {
            memory: SpinLock::new(vec![0u8; 4096]),
            descriptors: TestDescriptors::new(),
            blocker: TestBlocker::new(),
        }
    }

    /// Stores an array at `USER_BASE + offset` and returns its address.
    fn put<T: Pod>(&self, offset: usize, values: &[T]) -> u64 {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.as_bytes().to_vec()).collect();
        self.memory.lock()[offset..offset + bytes.len()].copy_from_slice(&bytes);
        USER_BASE + offset as u64
    }

    /// Reads back an array written by a handler.
    fn get<T: Pod>(&self, offset: usize, count: usize) -> Vec<T> {
        let memory = self.memory.lock();
        memory[offset..offset + count * size_of::<T>()]
            .chunks_exact(size_of::<T>())
            .map(T::from_bytes)
            .collect()
    }

    fn range(&self, addr: u64, len: usize) -> DrmResult<core::ops::Range<usize>> {
        let start = usize::try_from(addr.checked_sub(USER_BASE).ok_or(DrmError::BadAddress)?)
            .map_err(|_| DrmError::BadAddress)?;
        let end = start.checked_add(len).ok_or(DrmError::BadAddress)?;
        if end > self.memory.lock().len() {
            return Err(DrmError::BadAddress);
        }
        Ok(start..end)
    }
}

impl TestUser {
    /// The caller context a handler is given, with everything this fixture can
    /// supply. Unlike a real in-kernel caller it has both halves, because the
    /// point of the fixture is to exercise the paths that need them.
    fn context(&self) -> CallerContext<'_> {
        CallerContext::new(self)
            .with_descriptors(&self.descriptors)
            .with_blocker(&self.blocker)
    }
}

impl UserMem for TestUser {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> DrmResult<()> {
        let range = self.range(addr, buf.len())?;
        buf.copy_from_slice(&self.memory.lock()[range]);
        Ok(())
    }

    fn write_bytes(&self, addr: u64, buf: &[u8]) -> DrmResult<()> {
        let range = self.range(addr, buf.len())?;
        self.memory.lock()[range].copy_from_slice(buf);
        Ok(())
    }
}

/// A descriptor table that is a `Vec` with the index as the descriptor.
pub(crate) struct TestDescriptors {
    open: SpinLock<Vec<Option<DrmExport>>>,
    /// Eventfds live in a separate table so their numbers cannot collide
    /// with [`Descriptors::install`]'s export descriptors.
    eventfds: SpinLock<Vec<Arc<TestEventfd>>>,
    /// The flags the last install was asked for, so a test can check that the
    /// ioctl passed them through.
    last_flags: SpinLock<Option<ExportFlags>>,
}

/// An eventfd stand-in whose increment is a counter a test can read.
pub(crate) struct TestEventfd {
    count: AtomicU64,
}

impl TestEventfd {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: AtomicU64::new(0),
        })
    }

    /// How many times [`Eventfd::signal`] has run.
    pub(crate) fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Eventfd for TestEventfd {
    fn signal(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

impl TestDescriptors {
    pub(crate) fn new() -> Self {
        Self {
            open: SpinLock::new(Vec::new()),
            eventfds: SpinLock::new(Vec::new()),
            last_flags: SpinLock::new(None),
        }
    }

    pub(crate) fn last_flags(&self) -> Option<ExportFlags> {
        *self.last_flags.lock()
    }

    /// Closes a descriptor, as user space would.
    pub(crate) fn close(&self, fd: i32) {
        self.open.lock()[fd as usize] = None;
    }

    /// Allocates an eventfd whose increments a test can observe.
    ///
    /// Numbers start at 1000 so they cannot collide with the 0, 1, 2, …
    /// sequence [`Descriptors::install`] hands out.
    pub(crate) fn add_eventfd(&self) -> (i32, Arc<TestEventfd>) {
        let eventfd = TestEventfd::new();
        let mut table = self.eventfds.lock();
        let fd = 1000 + table.len() as i32;
        table.push(eventfd.clone());
        (fd, eventfd)
    }
}

impl Descriptors for TestDescriptors {
    fn install(&self, export: DrmExport, flags: ExportFlags) -> DrmResult<i32> {
        *self.last_flags.lock() = Some(flags);

        let mut open = self.open.lock();
        open.push(Some(export));
        Ok(open.len() as i32 - 1)
    }

    fn resolve(&self, fd: i32) -> DrmResult<DrmExport> {
        usize::try_from(fd)
            .ok()
            .and_then(|fd| self.open.lock().get(fd).cloned())
            .flatten()
            .ok_or(DrmError::InvalidArgument)
    }

    fn eventfd(&self, fd: i32) -> DrmResult<Arc<dyn Eventfd>> {
        // An export descriptor is open but is not an eventfd: EINVAL, matching
        // `eventfd_ctx_fileget`. A number that is not open at all is EBADF.
        if usize::try_from(fd)
            .ok()
            .and_then(|fd| self.open.lock().get(fd).cloned())
            .flatten()
            .is_some()
        {
            return Err(DrmError::InvalidArgument);
        }

        let index = usize::try_from(fd.checked_sub(1000).ok_or(DrmError::BadFd)?)
            .map_err(|_| DrmError::BadFd)?;
        self.eventfds
            .lock()
            .get(index)
            .cloned()
            .map(|eventfd| eventfd as Arc<dyn Eventfd>)
            .ok_or(DrmError::BadFd)
    }
}

/// A [`Blocker`] that runs a scripted action instead of sleeping.
struct TestBlocker {
    now: AtomicU64,
    /// Run at the moment a real blocker would have parked the thread.
    action: SpinLock<Option<Box<dyn FnMut() + Send>>>,
    /// What to report when the action left the wait unsatisfied.
    outcome: SpinLock<DrmError>,
    calls: AtomicUsize,
}

impl TestBlocker {
    fn new() -> Self {
        Self {
            now: AtomicU64::new(0),
            action: SpinLock::new(None),
            outcome: SpinLock::new(DrmError::TimeExpired),
            calls: AtomicUsize::new(0),
        }
    }

    fn on_block(&self, action: impl FnMut() + Send + 'static) {
        *self.action.lock() = Some(Box::new(action));
    }

    fn report(&self, outcome: DrmError) {
        *self.outcome.lock() = outcome;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Blocker for TestBlocker {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }

    fn block(
        &self,
        _queue: &WaitQueue,
        ready: &mut dyn FnMut() -> bool,
        _deadline: Deadline,
        _interruptible: bool,
    ) -> DrmResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);

        if let Some(action) = self.action.lock().as_mut() {
            action();
        }

        if ready() {
            Ok(())
        } else {
            Err(*self.outcome.lock())
        }
    }
}

///
/// A blocker that neither sleeps nor lets anything happen.
struct IdleBlocker;

impl Blocker for IdleBlocker {
    fn block(
        &self,
        _queue: &WaitQueue,
        _ready: &mut dyn FnMut() -> bool,
        _deadline: Deadline,
        _interruptible: bool,
    ) -> DrmResult<()> {
        Err(DrmError::TimeExpired)
    }
}

fn fence_of(seqno: u64) -> Arc<SoftwareFence> {
    SoftwareFence::new(FenceContext::new(), seqno)
}

fn erase(fence: &Arc<SoftwareFence>) -> Arc<dyn Fence> {
    fence.clone()
}

fn device() -> Arc<DrmDevice> {
    DrmDevice::new_unregistered(Arc::new(SyncDriver), None, Some(128))
}

fn file(device: &Arc<DrmDevice>) -> Arc<DrmFile> {
    device
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap()
}

/// Issues an ioctl the way the character-device shell does.
fn ioctl<T: Pod>(
    device: &Arc<DrmDevice>,
    file: &DrmFile,
    user: &TestUser,
    nr: u32,
    args: T,
) -> DrmResult<T> {
    let mut payload = args.as_bytes().to_vec();
    device.ioctl(file, nr, &mut payload, &user.context())?;
    Ok(T::from_bytes(&payload))
}

// ---------------------------------------------------------------- rule C1 --

#[ktest]
fn a_fence_signals_once_and_the_state_is_terminal() {
    let fence = fence_of(7);

    assert_eq!(fence.status(), FenceStatus::Pending);
    assert!(!fence.is_signaled());
    assert_eq!(fence.error(), None);

    assert!(fence.signal(None));
    assert_eq!(fence.status(), FenceStatus::Signaled);
    assert!(fence.is_signaled());

    // Terminal, and idempotent rather than an error: a completion path and an
    // error path race by construction, and only the first may take effect.
    assert!(!fence.signal(None));
    assert!(!fence.signal(Some(DrmError::Io)));
    assert_eq!(fence.status(), FenceStatus::Signaled);
    assert_eq!(fence.error(), None);
}

#[ktest]
fn a_failed_fence_is_still_signalled() {
    let fence = fence_of(1);

    assert!(fence.signal(Some(DrmError::Io)));

    // Modelling failure as "never signals" is the easiest way to wedge a
    // pipeline, so a failed fence must satisfy every consumer and let them
    // observe the error separately.
    assert!(fence.is_signaled());
    assert_eq!(fence.status(), FenceStatus::Failed(DrmError::Io));
    assert_eq!(fence.error(), Some(DrmError::Io));

    // And the failure is terminal too: a late success does not overwrite it.
    assert!(!fence.signal(None));
    assert_eq!(fence.error(), Some(DrmError::Io));
}

#[ktest]
fn fence_contexts_are_unique() {
    // Identity is `(context, seqno)`, and both the cycle check and any waiter
    // that skips implied fences rely on it, so handing out one context twice
    // would break both.
    let contexts: Vec<_> = (0..8).map(|_| FenceContext::new()).collect();

    for (index, context) in contexts.iter().enumerate() {
        assert!(!contexts[..index].contains(context));
    }
}

// ---------------------------------------------------------------- rule C3 --

#[ktest]
fn signalling_wakes_every_waiter() {
    let fence = fence_of(1);
    let waiters: Vec<_> = (0..4).map(|_| FenceWaiter::new(1)).collect();

    for (index, waiter) in waiters.iter().enumerate() {
        assert!(!fence.attach(waiter, index));
    }
    assert_eq!(fence.waiter_count(), 4);
    for waiter in &waiters {
        assert!(!waiter.is_ready(true));
    }

    fence.signal(None);

    // Every one of them, not just the first. Losing a single wakeup is not a
    // slow path, it is one thread hung forever.
    for (index, waiter) in waiters.iter().enumerate() {
        assert!(waiter.is_ready(true), "waiter {index} was not woken");
        assert_eq!(waiter.first_signaled(), Some(index));
    }

    // And the list is empty afterwards, so a second signal — or a detach from
    // a wait that has since given up — walks nothing.
    assert_eq!(fence.waiter_count(), 0);
}

#[ktest]
fn attaching_to_an_already_signalled_fence_reports_it_immediately() {
    let fence = fence_of(1);
    fence.signal(None);

    let waiter = FenceWaiter::new(1);

    // The other half of the no-lost-wakeup rule. A caller that ignored this
    // return value would join a list nobody will ever walk again and sleep
    // forever.
    assert!(fence.attach(&waiter, 3));
    assert!(waiter.is_ready(true));
    assert_eq!(waiter.first_signaled(), Some(3));
    assert_eq!(fence.waiter_count(), 0);
}

#[ktest]
fn a_callback_runs_once_whichever_side_of_the_signal_it_arrives() {
    let before = Arc::new(AtomicUsize::new(0));
    let after = Arc::new(AtomicUsize::new(0));
    let status: Arc<SpinLock<Option<FenceStatus>>> = Arc::new(SpinLock::new(None));

    let fence = fence_of(1);

    let counter = before.clone();
    let seen = status.clone();
    fence
        .add_callback(Arc::new(move |status| {
            counter.fetch_add(1, Ordering::Relaxed);
            *seen.lock() = Some(status);
        }))
        .unwrap();
    assert_eq!(before.load(Ordering::Relaxed), 0);

    fence.signal(Some(DrmError::Io));
    assert_eq!(before.load(Ordering::Relaxed), 1);
    // The terminal status is handed to the callback, so a consumer chaining
    // work off the fence learns that the work failed without keeping a
    // reference to the fence itself.
    assert_eq!(*status.lock(), Some(FenceStatus::Failed(DrmError::Io)));

    // Registered after the fact: runs inline, exactly once.
    let counter = after.clone();
    fence
        .add_callback(Arc::new(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        }))
        .unwrap();
    assert_eq!(after.load(Ordering::Relaxed), 1);

    // A second signal is a no-op, so nothing runs twice.
    fence.signal(None);
    assert_eq!(before.load(Ordering::Relaxed), 1);
    assert_eq!(after.load(Ordering::Relaxed), 1);
}

// ------------------------------------------------------------ rules C4/C5 --

#[ktest]
fn waiting_for_a_signalled_fence_returns_without_blocking() {
    let fence = fence_of(1);
    fence.signal(None);

    let blocker = TestBlocker::new();
    let fences = vec![erase(&fence)];

    assert_eq!(
        wait_many(&blocker, &fences, true, Deadline::Never, true).unwrap(),
        0
    );
    // The fast path is not an optimization here: with no blocker able to make
    // progress, going to sleep would be a hang.
    assert_eq!(blocker.calls(), 0);
}

#[ktest]
fn waiting_reports_the_index_of_the_first_signalled_fence() {
    let fences: Vec<_> = (0..4).map(fence_of).collect();
    let erased: Vec<Arc<dyn Fence>> = fences.iter().map(erase).collect();

    fences[2].signal(None);

    let blocker = TestBlocker::new();
    assert_eq!(
        wait_many(&blocker, &erased, false, Deadline::Never, true).unwrap(),
        2
    );

    // `wait_all` is not satisfied by one of them, and the deadline decides.
    assert_eq!(
        wait_many(&blocker, &erased, true, Deadline::Immediate, true).unwrap_err(),
        DrmError::TimeExpired
    );

    for fence in &fences {
        fence.signal(None);
    }
    assert_eq!(
        wait_many(&blocker, &erased, true, Deadline::Never, true).unwrap(),
        0
    );
}

#[ktest]
fn a_wait_satisfied_while_blocked_succeeds() {
    let fence = fence_of(1);
    let fences = vec![erase(&fence)];

    let blocker = TestBlocker::new();
    let signaller = fence.clone();
    // Stands in for the CPU that completes the work while this thread sleeps.
    blocker.on_block(move || {
        signaller.signal(None);
    });

    assert_eq!(
        wait_many(&blocker, &fences, true, Deadline::At(1_000_000), true).unwrap(),
        0
    );
    assert_eq!(blocker.calls(), 1);
    assert_eq!(fence.waiter_count(), 0);
}

#[ktest]
fn a_timed_out_wait_leaves_no_waiter_behind() {
    let fences: Vec<_> = (0..3).map(fence_of).collect();
    let erased: Vec<Arc<dyn Fence>> = fences.iter().map(erase).collect();

    let blocker = TestBlocker::new();
    blocker.report(DrmError::TimeExpired);

    assert_eq!(
        wait_many(&blocker, &erased, true, Deadline::At(1), true).unwrap_err(),
        DrmError::TimeExpired
    );

    // Rule C4. A stale waiter is worse than a leak: the fence holds a strong
    // reference to a wait that is long gone, and the eventual signal walks a
    // list that only ever grows.
    for fence in &fences {
        assert_eq!(fence.waiter_count(), 0);
    }

    // And the fences are untouched — a wait that gave up did not consume them.
    for fence in &fences {
        assert!(!fence.is_signaled());
    }
}

#[ktest]
fn an_interrupted_wait_leaves_no_waiter_behind() {
    let fence = fence_of(1);
    let fences = vec![erase(&fence)];

    let blocker = TestBlocker::new();
    blocker.report(DrmError::Interrupted);

    assert_eq!(
        wait_many(&blocker, &fences, true, Deadline::Never, true).unwrap_err(),
        DrmError::Interrupted
    );
    assert_eq!(fence.waiter_count(), 0);
}

#[ktest]
fn a_fence_that_signals_as_the_deadline_expires_still_satisfies_the_wait() {
    let fence = fence_of(1);
    let fences = vec![erase(&fence)];

    let blocker = TestBlocker::new();
    let signaller = fence.clone();
    blocker.on_block(move || {
        signaller.signal(None);
    });
    // The blocker reports a timeout regardless, as a real one would if the
    // timer fired in the same instant the interrupt arrived.
    blocker.report(DrmError::TimeExpired);

    // Work that is demonstrably done must not be reported as a timeout, or
    // user space goes hunting a hang that is not there.
    assert_eq!(
        wait_many(&blocker, &fences, true, Deadline::At(1), true).unwrap(),
        0
    );
}

#[ktest]
fn waiting_for_an_already_failed_fence_succeeds_with_an_error() {
    let fence = fence_of(1);
    fence.signal(Some(DrmError::Io));
    let fences = vec![erase(&fence)];

    // The wait is satisfied; inspecting the outcome is a separate step, which
    // is what `dma_fence_wait_timeout` does too.
    assert_eq!(
        wait_many(&IdleBlocker, &fences, true, Deadline::Never, true).unwrap(),
        0
    );
    assert_eq!(fences[0].error(), Some(DrmError::Io));
}

#[ktest]
fn an_immediate_deadline_never_blocks() {
    let fence = fence_of(1);
    let fences = vec![erase(&fence)];

    let blocker = TestBlocker::new();
    assert_eq!(
        wait_many(&blocker, &fences, true, Deadline::Immediate, true).unwrap_err(),
        DrmError::TimeExpired
    );
    assert_eq!(blocker.calls(), 0);
    assert_eq!(fence.waiter_count(), 0);
}

#[ktest]
fn waiting_on_an_empty_set_is_rejected() {
    // `drm_syncobj_wait_ioctl` refuses a zero count for the same reason: there
    // is no meaningful `first_signaled` to report.
    assert_eq!(
        wait_many(&IdleBlocker, &[], false, Deadline::Never, true).unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn an_absolute_deadline_in_the_past_is_a_poll() {
    assert_eq!(Deadline::from_timeout_nsec(0), Deadline::Immediate);
    assert_eq!(Deadline::from_timeout_nsec(-1), Deadline::Immediate);
    assert_eq!(Deadline::from_timeout_nsec(i64::MIN), Deadline::Immediate);
    assert_eq!(Deadline::from_timeout_nsec(42), Deadline::At(42));
    assert_eq!(
        Deadline::from_timeout_nsec(i64::MAX),
        Deadline::At(i64::MAX as u64)
    );
}

// ---------------------------------------------------------------- rule C6 --

#[ktest]
fn timeline_points_only_move_forwards() {
    let syncobj = SyncObj::new(false);

    syncobj.add_point(10, fence_of(1)).unwrap();
    syncobj.add_point(20, fence_of(2)).unwrap();
    assert_eq!(syncobj.last_submitted_point(), 20);

    // Backwards, repeated, and zero are all refused rather than reordered: a
    // client that numbered its work wrongly has to find out, because silently
    // accepting the point makes its synchronization stop meaning anything.
    for point in [0, 1, 10, 20] {
        assert_eq!(
            syncobj.add_point(point, fence_of(3)).unwrap_err(),
            DrmError::InvalidArgument,
            "point {point} was accepted after 20"
        );
    }
    assert_eq!(syncobj.last_submitted_point(), 20);

    // Forwards still works, and the refusals left nothing behind.
    syncobj.add_point(21, fence_of(4)).unwrap();
    assert_eq!(syncobj.last_submitted_point(), 21);
}

#[ktest]
fn a_wait_on_a_passed_timeline_point_returns_at_once() {
    let syncobj = SyncObj::new(false);
    let first = fence_of(1);
    let second = fence_of(2);

    syncobj.add_point(5, erase(&first)).unwrap();
    syncobj.add_point(10, erase(&second)).unwrap();

    // Everything up to the highest submitted point resolves to the first
    // submitted point at or above it, because points signal in order.
    assert!(matches!(syncobj.point_state(1), TimelinePoint::Pending(_)));
    assert!(matches!(syncobj.point_state(5), TimelinePoint::Pending(_)));
    assert!(matches!(syncobj.point_state(6), TimelinePoint::Pending(_)));
    assert!(matches!(
        syncobj.point_state(11),
        TimelinePoint::Unsubmitted
    ));

    first.signal(None);
    second.signal(None);
    syncobj.prune();

    // With the fences gone, a wait on a point the timeline has passed must
    // still return at once. Nothing will ever signal point 3 again, so
    // reporting it as unsubmitted would hang the caller for good.
    for point in [1, 5, 6, 10] {
        assert!(
            matches!(syncobj.point_state(point), TimelinePoint::Passed),
            "point {point} did not read as passed"
        );
    }
    assert!(matches!(
        syncobj.point_state(11),
        TimelinePoint::Unsubmitted
    ));
}

#[ktest]
fn a_timeline_reports_the_highest_contiguously_signalled_point() {
    let syncobj = SyncObj::new(false);
    let first = fence_of(1);
    let second = fence_of(2);
    let third = fence_of(3);

    syncobj.add_point(1, erase(&first)).unwrap();
    syncobj.add_point(2, erase(&second)).unwrap();
    syncobj.add_point(3, erase(&third)).unwrap();

    assert_eq!(syncobj.signaled_point(), 0);
    assert_eq!(syncobj.last_submitted_point(), 3);

    // Out-of-order completion must not move the value: a timeline value is a
    // promise that everything up to it is done, and a client comparing values
    // relies on exactly that.
    third.signal(None);
    assert_eq!(syncobj.signaled_point(), 0);

    first.signal(None);
    assert_eq!(syncobj.signaled_point(), 1);

    second.signal(None);
    assert_eq!(syncobj.signaled_point(), 3);
}

#[ktest]
fn a_binary_sync_object_holds_one_fence_and_reports_no_timeline_value() {
    let syncobj = SyncObj::new(false);
    assert!(syncobj.fence().is_none());
    assert!(matches!(syncobj.point_state(0), TimelinePoint::Unsubmitted));

    let first = fence_of(1);
    syncobj.replace_fence(Some(erase(&first)));
    assert!(matches!(syncobj.point_state(0), TimelinePoint::Pending(_)));

    // Replacing does not wait for or cancel what was there; whoever was
    // already waiting on the old fence keeps waiting on it.
    let second = fence_of(2);
    syncobj.replace_fence(Some(erase(&second)));
    assert!(Arc::ptr_eq(&syncobj.fence().unwrap(), &erase(&second)));
    assert!(!first.is_signaled());

    // A binary object has no timeline value, matching upstream, whose query
    // reports zero for anything that is not a chain.
    syncobj.signal();
    assert_eq!(syncobj.signaled_point(), 0);
    assert!(syncobj.fence().unwrap().is_signaled());

    syncobj.reset();
    assert!(syncobj.fence().is_none());
}

#[ktest]
fn a_sync_object_created_signalled_is_signalled() {
    // User space uses this to mean "no work to wait for", so a client that
    // never signals the object must still make progress.
    let syncobj = SyncObj::new(true);
    assert!(syncobj.fence().unwrap().is_signaled());
}

#[ktest]
fn a_submission_wakes_the_waiters_that_had_nothing_to_wait_on() {
    let syncobj = SyncObj::new(false);
    let waiter = FenceWaiter::new(1);

    assert!(!syncobj.attach_submit(&waiter, 0, 1));
    assert_eq!(syncobj.submit_waiter_count(), 1);
    assert!(!waiter.is_ready(false));

    syncobj.add_point(1, fence_of(1)).unwrap();
    assert!(waiter.is_ready(false));
    assert_eq!(syncobj.submit_waiter_count(), 0);

    // With something already installed there is nothing to wait for, and the
    // waiter is told inline — the same shape as rule C3, for the same reason.
    let late = FenceWaiter::new(1);
    assert!(syncobj.attach_submit(&late, 2, 1));
    assert_eq!(late.first_signaled(), Some(2));
    assert_eq!(syncobj.submit_waiter_count(), 0);
}

// ---------------------------------------------------------------- rule C7 --

#[ktest]
fn a_fence_may_not_depend_on_itself() {
    let fence = fence_of(1);
    let erased = erase(&fence);

    assert_eq!(
        fence.state().add_dependency(&erased).unwrap_err(),
        DrmError::WouldDeadlock
    );
    assert!(fence.dependencies().is_empty());
}

#[ktest]
fn a_dependency_cycle_is_refused() {
    let a = fence_of(1);
    let b = fence_of(2);

    // A waits for B: fine, the graph is still acyclic.
    a.state().add_dependency(&erase(&b)).unwrap();
    assert_eq!(a.dependencies().len(), 1);

    // B waits for A: that closes the cycle, and both would hang forever
    // waiting for the other. Refusing it is the whole of rule C7.
    assert_eq!(
        b.state().add_dependency(&erase(&a)).unwrap_err(),
        DrmError::WouldDeadlock
    );
    assert!(b.dependencies().is_empty());

    // The refusal did not damage the edge that was already there.
    assert!(Arc::ptr_eq(&a.dependencies()[0], &erase(&b)));
}

#[ktest]
fn a_longer_dependency_cycle_is_refused() {
    let fences: Vec<_> = (1..=5).map(fence_of).collect();

    // A chain A ← B ← C ← D ← E, added one link at a time.
    for pair in fences.windows(2) {
        pair[0].state().add_dependency(&erase(&pair[1])).unwrap();
    }

    // Closing it anywhere is a cycle, and the walk has to follow the whole
    // chain to see it — a check that only looked one edge deep would let this
    // through and deadlock the lot.
    assert_eq!(
        fences[4]
            .state()
            .add_dependency(&erase(&fences[0]))
            .unwrap_err(),
        DrmError::WouldDeadlock
    );
    assert_eq!(
        fences[3]
            .state()
            .add_dependency(&erase(&fences[1]))
            .unwrap_err(),
        DrmError::WouldDeadlock
    );

    assert!(fences[4].dependencies().is_empty());
}

#[ktest]
fn a_dependency_diamond_is_allowed() {
    let top = fence_of(1);
    let left = fence_of(2);
    let right = fence_of(3);
    let bottom = fence_of(4);

    // top ← {left, right} ← bottom. Two paths reach `bottom`, which the walk
    // must recognize as a diamond rather than mistake for a cycle.
    left.state().add_dependency(&erase(&bottom)).unwrap();
    right.state().add_dependency(&erase(&bottom)).unwrap();
    top.state().add_dependency(&erase(&left)).unwrap();
    top.state().add_dependency(&erase(&right)).unwrap();

    assert_eq!(top.dependencies().len(), 2);
    assert!(!top.state().dependencies_met());

    bottom.signal(None);
    left.signal(None);
    right.signal(None);
    assert!(top.state().dependencies_met());
}

#[ktest]
fn a_signalled_dependency_is_not_recorded() {
    let fence = fence_of(1);
    let done = fence_of(2);
    done.signal(None);

    // It cannot be part of a cycle and it constrains nothing, so recording it
    // would only pin a finished job graph in memory.
    fence.state().add_dependency(&erase(&done)).unwrap();
    assert!(fence.dependencies().is_empty());
    assert!(fence.state().dependencies_met());
}

#[ktest]
fn a_signalled_fence_takes_on_no_new_dependencies() {
    let fence = fence_of(1);
    let other = fence_of(2);
    fence.signal(None);

    assert_eq!(
        fence.state().add_dependency(&erase(&other)).unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn signalling_drops_the_dependency_edges() {
    let fence = fence_of(1);
    let dependency = fence_of(2);

    fence.state().add_dependency(&erase(&dependency)).unwrap();
    assert_eq!(fence.dependencies().len(), 1);

    // Nothing consults them again, and holding them keeps whole finished job
    // graphs alive.
    fence.signal(None);
    assert!(fence.dependencies().is_empty());
}

#[ktest]
fn dependencies_are_bounded() {
    let fence = fence_of(1);

    let dependencies: Vec<_> = (0..MAX_DEPENDENCIES + 1).map(|_| fence_of(1)).collect();
    for dependency in dependencies.iter().take(MAX_DEPENDENCIES) {
        fence.state().add_dependency(&erase(dependency)).unwrap();
    }

    assert_eq!(
        fence
            .state()
            .add_dependency(&erase(&dependencies[MAX_DEPENDENCIES]))
            .unwrap_err(),
        DrmError::NoSpace
    );
}

// -------------------------------------------------------------- fence sets --

#[ktest]
fn a_writer_waits_for_the_readers_a_reader_does_not() {
    let set = FenceSet::new();
    let write = fence_of(1);
    let read = fence_of(2);

    set.add_write(erase(&write));
    set.add_read(erase(&read));

    // A reader only has to see the last write land.
    assert_eq!(set.read_dependencies().len(), 1);
    // A writer may not overwrite data a reader is still fetching.
    assert_eq!(set.write_dependencies().len(), 2);
    assert!(!set.is_idle());

    // A new write supersedes the reads: a writer that has already waited for
    // them makes them redundant, and keeping them would make every later
    // writer wait for reads that finished long ago.
    let second_write = fence_of(3);
    set.add_write(erase(&second_write));
    assert_eq!(set.write_dependencies().len(), 1);

    second_write.signal(None);
    assert!(set.is_idle());
    set.prune();
    assert!(set.write_dependencies().is_empty());
}

// ------------------------------------------------------- the sync-obj table --

#[ktest]
fn sync_object_handles_are_dense_and_never_zero() {
    let table = SyncObjTable::new();
    let objects: Vec<_> = (0..3).map(|_| SyncObj::new(false)).collect();

    let handles: Vec<_> = objects
        .iter()
        .map(|object| table.insert(object.clone()).unwrap())
        .collect();
    assert_eq!(
        handles,
        vec![SyncObjHandle(1), SyncObjHandle(2), SyncObjHandle(3)]
    );

    // A bad handle is `ENOENT` here, not the `EINVAL` GEM uses on its delete
    // path. Both are what Linux returns, so the inconsistency is ABI.
    assert_eq!(table.get(SyncObjHandle(0)).unwrap_err(), DrmError::NotFound);
    table.remove(SyncObjHandle(2)).unwrap();
    assert_eq!(
        table.remove(SyncObjHandle(2)).unwrap_err(),
        DrmError::NotFound
    );
    assert_eq!(table.insert(SyncObj::new(false)).unwrap(), SyncObjHandle(2));

    // All or nothing, so a wait naming one bad handle among many fails
    // outright rather than waiting on the good ones.
    assert!(table.get_many(&[1, 3]).is_ok());
    assert_eq!(table.get_many(&[1, 99]).unwrap_err(), DrmError::NotFound);
}

// ----------------------------------------------------------------- ioctls --

#[ktest]
fn the_sync_object_ioctls_round_trip() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    let created = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate {
            handle: 0,
            flags: 0,
        },
    )
    .unwrap();
    assert_ne!(created.handle, 0);
    assert_eq!(file.syncobjs().len(), 1);

    let signalled = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate {
            handle: 0,
            flags: create_flags::SIGNALED,
        },
    )
    .unwrap();

    // Waiting on the signalled one returns at once and names it; waiting on
    // the empty one is `EINVAL`, as `drm_syncobj_find_fence` reports for an
    // object with no fence and no `WAIT_FOR_SUBMIT`.
    let handles = user.put(0, &[signalled.handle]);
    let wait = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_WAIT,
        SyncobjWait {
            handles,
            timeout_nsec: 0,
            count_handles: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(wait.first_signaled, 0);

    let handles = user.put(0, &[created.handle]);
    let args = SyncobjWait {
        handles,
        timeout_nsec: 0,
        count_handles: 1,
        ..Default::default()
    };
    assert_eq!(
        ioctl(&device, &file, &user, ioctl_nr::nr::SYNCOBJ_WAIT, args).unwrap_err(),
        DrmError::InvalidArgument
    );

    // `SYNCOBJ_SIGNAL` fills it in, and the same wait then succeeds.
    let array = SyncobjArray {
        handles,
        count_handles: 1,
        pad: 0,
    };
    ioctl(&device, &file, &user, ioctl_nr::nr::SYNCOBJ_SIGNAL, array).unwrap();
    ioctl(&device, &file, &user, ioctl_nr::nr::SYNCOBJ_WAIT, args).unwrap();

    // `SYNCOBJ_RESET` empties it again.
    ioctl(&device, &file, &user, ioctl_nr::nr::SYNCOBJ_RESET, array).unwrap();
    assert_eq!(
        ioctl(&device, &file, &user, ioctl_nr::nr::SYNCOBJ_WAIT, args).unwrap_err(),
        DrmError::InvalidArgument
    );

    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_DESTROY,
        SyncobjDestroy {
            handle: created.handle,
            pad: 0,
        },
    )
    .unwrap();
    assert_eq!(file.syncobjs().len(), 1);
}

/// Mesa's `VkFence` capability probe, in the order and with the arguments it
/// uses.
///
/// Worth its own test despite the coverage above, which waits with no flags at
/// all: Mesa sets `WAIT_ALL`, and that bit selects a different branch of both
/// `wait_syncobjs` and `satisfied`. Nothing exercised it, and the consequence
/// of getting it wrong is not a failed wait but a missing feature —
/// `vk_drm_syncobj_get_type_from_provider` grants `VK_SYNC_FEATURE_CPU_WAIT`
/// only if this exact call succeeds, and `vk_fence.c` then refuses to create
/// any fence at all, reporting `VK_ERROR_INVALID_EXTERNAL_HANDLE` for a fence
/// that has nothing external about it.
///
/// Reference: `src/vulkan/runtime/vk_drm_syncobj.c`,
/// `vk_drm_syncobj_get_type_from_provider`.
#[ktest]
fn mesas_fence_capability_probe_succeeds() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    let created = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate {
            handle: 0,
            flags: create_flags::SIGNALED,
        },
    )
    .unwrap();
    // The handle has to come back out. Mesa abandons the fence type without
    // ever waiting if the create reports failure, so a handle that never
    // reached user space looks exactly like a refused wait.
    assert_ne!(created.handle, 0);

    let handles = user.put(0, &[created.handle]);
    let wait = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_WAIT,
        SyncobjWait {
            handles,
            timeout_nsec: 0,
            count_handles: 1,
            flags: wait_flags::WAIT_ALL,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(wait.first_signaled, 0);
}

/// The other answer the same call can give, which decides `vkWaitForFences`.
///
/// An unsignalled object waited on with `WAIT_FOR_SUBMIT` and a deadline in the
/// past is [`DrmError::TimeExpired`] — `ETIME`, which Vulkan turns into
/// `VK_TIMEOUT` — and not [`DrmError::TimedOut`], however much that reads like
/// it. Without `WAIT_FOR_SUBMIT` an object with no fence is `EINVAL` instead,
/// which the test above this one covers.
#[ktest]
fn a_wait_all_that_reaches_its_deadline_expires_rather_than_timing_out() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    let created = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate {
            handle: 0,
            flags: 0,
        },
    )
    .unwrap();

    let handles = user.put(0, &[created.handle]);
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_WAIT,
            SyncobjWait {
                handles,
                timeout_nsec: 0,
                count_handles: 1,
                flags: wait_flags::WAIT_ALL | wait_flags::WAIT_FOR_SUBMIT,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::TimeExpired
    );
}

#[ktest]
fn a_timeline_signals_queries_and_transfers_through_the_ioctls() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    let timeline = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;

    let handles = user.put(0, &[timeline]);
    let points = user.put(64, &[7u64]);
    let timeline_array = SyncobjTimelineArray {
        handles,
        points,
        count_handles: 1,
        flags: 0,
    };

    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
        timeline_array,
    )
    .unwrap();

    // The query writes the value into the caller's own `points` array, so the
    // point that was signalled is overwritten by the answer.
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_QUERY,
        timeline_array,
    )
    .unwrap();
    assert_eq!(user.get::<u64>(64, 1), vec![7]);

    // Rule C6 through the ioctl: a point that does not move forwards is
    // refused rather than reordered.
    user.put(64, &[7u64]);
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
            timeline_array,
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // A wait on a point the timeline has passed returns at once, and one on a
    // point it has not reached is unsubmitted.
    user.put(64, &[3u64]);
    let wait = SyncobjTimelineWait {
        handles,
        points,
        timeout_nsec: 0,
        count_handles: 1,
        ..Default::default()
    };
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_WAIT,
        wait,
    )
    .unwrap();

    user.put(64, &[99u64]);
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_TIMELINE_WAIT,
            wait,
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // A transfer of a point past the end of the timeline has no fence to copy,
    // which `dma_fence_chain_find_seqno` reports as `EINVAL` upstream.
    let destination = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_TRANSFER,
            SyncobjTransfer {
                src_handle: timeline,
                dst_handle: destination,
                src_point: 99,
                dst_point: 0,
                flags: 0,
                pad: 0,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // A point that is still pending copies across, and a zero destination
    // point means the binary fence rather than a timeline point.
    let pending = SyncObj::new(false);
    let fence = fence_of(1);
    pending.add_point(4, erase(&fence)).unwrap();
    let source = file.syncobjs().insert(pending).unwrap();

    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_TRANSFER,
        SyncobjTransfer {
            src_handle: source.0,
            dst_handle: destination,
            src_point: 1,
            dst_point: 0,
            flags: 0,
            pad: 0,
        },
    )
    .unwrap();

    let copied = file.syncobjs().get(SyncObjHandle(destination)).unwrap();
    assert!(Arc::ptr_eq(&copied.fence().unwrap(), &erase(&fence)));
}

#[ktest]
fn a_wait_for_submit_blocks_instead_of_failing() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    let handle = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    let object = file.syncobjs().get(SyncObjHandle(handle)).unwrap();

    // Mesa sets this flag whenever it waits on a sync object another thread is
    // still filling in, so an empty object must be waited for rather than
    // refused.
    let submitter = object.clone();
    user.blocker.on_block(move || {
        submitter.signal();
    });

    let handles = user.put(0, &[handle]);
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_WAIT,
        SyncobjWait {
            handles,
            timeout_nsec: 1_000_000,
            count_handles: 1,
            flags: wait_flags::WAIT_FOR_SUBMIT,
            ..Default::default()
        },
    )
    .unwrap();

    // Rule C4 on the submission side too: nothing is left attached.
    assert_eq!(object.submit_waiter_count(), 0);
}

#[ktest]
fn the_sync_object_ioctls_reject_what_they_do_not_understand() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    // Unknown flag bits, so that a field which gains a meaning later cannot
    // have been silently ignored in the meantime.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_CREATE,
            SyncobjCreate {
                handle: 0,
                flags: 0x8000,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // A non-zero padding field.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_DESTROY,
            SyncobjDestroy { handle: 1, pad: 1 },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // Zero handles, which has no `first_signaled` to report, and more than the
    // wait path will hold attachments for at once.
    for count in [0, MAX_WAIT_HANDLES as u32 + 1] {
        assert_eq!(
            ioctl(
                &device,
                &file,
                &user,
                ioctl_nr::nr::SYNCOBJ_WAIT,
                SyncobjWait {
                    handles: USER_BASE,
                    count_handles: count,
                    ..Default::default()
                },
            )
            .unwrap_err(),
            DrmError::InvalidArgument
        );
    }

    // A handle that was never created.
    let handles = user.put(0, &[42u32]);
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_WAIT,
            SyncobjWait {
                handles,
                count_handles: 1,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );
}

#[ktest]
fn a_driver_without_the_feature_refuses_the_sync_object_ioctls() {
    let user = TestUser::new();

    // No `DRIVER_SYNCOBJ` at all: every one of the twelve is refused, or user
    // space that believed `DRM_CAP_SYNCOBJ` would find them working anyway and
    // stop asking.
    let plain = DrmDevice::new_unregistered(Arc::new(PlainDriver), None, Some(0));
    let file = plain
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();

    for nr in [
        ioctl_nr::nr::SYNCOBJ_CREATE,
        ioctl_nr::nr::SYNCOBJ_DESTROY,
        ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
        ioctl_nr::nr::SYNCOBJ_FD_TO_HANDLE,
        ioctl_nr::nr::SYNCOBJ_WAIT,
        ioctl_nr::nr::SYNCOBJ_RESET,
        ioctl_nr::nr::SYNCOBJ_SIGNAL,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_WAIT,
        ioctl_nr::nr::SYNCOBJ_QUERY,
        ioctl_nr::nr::SYNCOBJ_TRANSFER,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
    ] {
        let mut payload = [0u8; 48];
        assert_eq!(
            plain
                .ioctl(&file, nr, &mut payload, &user.context())
                .unwrap_err(),
            DrmError::NotSupported,
            "ioctl {nr:#04x} was answered without DRIVER_SYNCOBJ"
        );
    }

    // With binary sync objects but no timelines, only the five timeline
    // ioctls are refused. `SYNCOBJ_EVENTFD` is among them: linux-6.6 gates
    // it on `DRIVER_SYNCOBJ_TIMELINE`, not just `SYNCOBJ`.
    let binary = DrmDevice::new_unregistered(Arc::new(BinaryOnlyDriver), None, Some(1));
    let file = binary
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();

    let mut payload = [0u8; 48];
    binary
        .ioctl(
            &file,
            ioctl_nr::nr::SYNCOBJ_CREATE,
            &mut payload,
            &user.context(),
        )
        .unwrap();

    for nr in [
        ioctl_nr::nr::SYNCOBJ_TIMELINE_WAIT,
        ioctl_nr::nr::SYNCOBJ_QUERY,
        ioctl_nr::nr::SYNCOBJ_TRANSFER,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
    ] {
        let mut payload = [0u8; 48];
        assert_eq!(
            binary
                .ioctl(&file, nr, &mut payload, &user.context())
                .unwrap_err(),
            DrmError::NotSupported,
            "ioctl {nr:#04x} was answered without DRIVER_SYNCOBJ_TIMELINE"
        );
    }
}

#[ktest]
fn a_sync_object_survives_a_round_trip_through_a_descriptor() {
    let device = device();
    let exporter = file(&device);
    let importer = file(&device);
    let user = TestUser::new();

    let handle = ioctl(
        &device,
        &exporter,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;

    let exported = ioctl(
        &device,
        &exporter,
        &user,
        ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
        SyncobjHandle {
            handle,
            ..Default::default()
        },
    )
    .unwrap();
    // Upstream's `drm_syncobj_create_fd` always asks for close-on-exec.
    assert_eq!(user.descriptors.last_flags(), Some(ExportFlags::SYNC));

    let imported = ioctl(
        &device,
        &importer,
        &user,
        ioctl_nr::nr::SYNCOBJ_FD_TO_HANDLE,
        SyncobjHandle {
            fd: exported.fd,
            ..Default::default()
        },
    )
    .unwrap();

    // One object, two handles: a signal through one holder is seen by the
    // other, which is the only reason to share a sync object at all.
    let here = exporter.syncobjs().get(SyncObjHandle(handle)).unwrap();
    let there = importer
        .syncobjs()
        .get(SyncObjHandle(imported.handle))
        .unwrap();
    assert!(Arc::ptr_eq(&here, &there));

    here.signal();
    assert!(there.fence().unwrap().is_signaled());

    // Importing the same descriptor twice yields a second, distinct handle
    // onto the one object -- `drm_syncobj_fd_to_handle` allocates
    // unconditionally, with none of the lookup PRIME import does. An importer
    // that pairs each import with a destroy depends on getting two.
    let again = ioctl(
        &device,
        &importer,
        &user,
        ioctl_nr::nr::SYNCOBJ_FD_TO_HANDLE,
        SyncobjHandle {
            fd: exported.fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(again.handle, imported.handle);
    assert_eq!(importer.syncobjs().len(), 2);

    // Still the same object behind both, so closing one leaves the other
    // holding it.
    let third = importer
        .syncobjs()
        .get(SyncObjHandle(again.handle))
        .unwrap();
    assert!(Arc::ptr_eq(&here, &third));

    importer
        .syncobjs()
        .remove(SyncObjHandle(imported.handle))
        .unwrap();
    assert!(importer.syncobjs().get(SyncObjHandle(again.handle)).is_ok());
}

#[ktest]
fn a_sync_file_is_a_snapshot_not_a_container() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    let source = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate {
            handle: 0,
            flags: create_flags::SIGNALED,
        },
    )
    .unwrap()
    .handle;
    let destination = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;

    let exported = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
        SyncobjHandle {
            handle: source,
            flags: handle_flags::SYNC_FILE,
            ..Default::default()
        },
    )
    .unwrap();

    // A sync file goes into an existing handle, because it is a fence rather
    // than a container and has nowhere of its own to live.
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_FD_TO_HANDLE,
        SyncobjHandle {
            handle: destination,
            flags: handle_flags::SYNC_FILE,
            fd: exported.fd,
            pad: 0,
        },
    )
    .unwrap();

    let objects = file.syncobjs();
    let source_object = objects.get(SyncObjHandle(source)).unwrap();
    let destination_object = objects.get(SyncObjHandle(destination)).unwrap();
    assert!(Arc::ptr_eq(
        &source_object.fence().unwrap(),
        &destination_object.fence().unwrap()
    ));

    // The snapshot does not follow later changes to the exporting object.
    source_object.reset();
    assert!(destination_object.fence().unwrap().is_signaled());

    // And an object with no fence has no snapshot to hand out.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
            SyncobjHandle {
                handle: source,
                flags: handle_flags::SYNC_FILE,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn the_descriptor_ioctls_need_a_descriptor_table() {
    /// A caller with an address space but no process behind it.
    struct NoDescriptors;

    impl UserMem for NoDescriptors {
        fn read_bytes(&self, _addr: u64, _buf: &mut [u8]) -> DrmResult<()> {
            Err(DrmError::BadAddress)
        }

        fn write_bytes(&self, _addr: u64, _buf: &[u8]) -> DrmResult<()> {
            Err(DrmError::BadAddress)
        }
    }

    let device = device();
    let file = file(&device);

    let mut payload = payload_of(&SyncobjHandle::default());
    assert_eq!(
        device
            .ioctl(
                &file,
                ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
                &mut payload,
                &CallerContext::new(&NoDescriptors)
            )
            .unwrap_err(),
        DrmError::NotSupported
    );

    // `SYNCOBJ_EVENTFD` looks up the handle before the descriptor table, so
    // a zero handle would be `ENOENT` and would not prove this path. A real
    // object with no process behind the caller is `EOPNOTSUPP`.
    let user = TestUser::new();
    let handle = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    let mut payload = payload_of(&SyncobjEventfd {
        handle,
        ..Default::default()
    });
    assert_eq!(
        device
            .ioctl(
                &file,
                ioctl_nr::nr::SYNCOBJ_EVENTFD,
                &mut payload,
                &CallerContext::new(&NoDescriptors)
            )
            .unwrap_err(),
        DrmError::NotSupported
    );
}

#[ktest]
fn syncobj_eventfd_rejects_unknown_flags_and_nonzero_pad() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();
    let handle = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    let (fd, eventfd) = user.descriptors.add_eventfd();

    // `WAIT_FOR_SUBMIT` is `1 << 1`. It is *not* `WAIT_AVAILABLE` (`1 << 2`),
    // and this ioctl always waits for submit by parking on the object, so
    // the bit is `EINVAL`. A probe that used the wrong constant would
    // otherwise look like "the kernel has no eventfd ioctl".
    for flags in [
        wait_flags::WAIT_ALL,
        wait_flags::WAIT_FOR_SUBMIT,
        wait_flags::WAIT_AVAILABLE | wait_flags::WAIT_ALL,
        0x8000,
    ] {
        assert_eq!(
            ioctl(
                &device,
                &file,
                &user,
                ioctl_nr::nr::SYNCOBJ_EVENTFD,
                SyncobjEventfd {
                    handle,
                    flags,
                    fd,
                    ..Default::default()
                },
            )
            .unwrap_err(),
            DrmError::InvalidArgument,
            "flags {flags:#x} must be EINVAL"
        );
    }

    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_EVENTFD,
            SyncobjEventfd {
                handle,
                fd,
                pad: 1,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
    assert_eq!(eventfd.count(), 0);
}

#[ktest]
fn syncobj_eventfd_reports_enoent_before_examining_the_fd() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    // smithay's `supports_syncobj_eventfd` calls this with handle 0 and the
    // DRM node's own fd, and treats `ENOENT` as "the ioctl exists". Checking
    // the eventfd first would turn that into `EBADF` / `EINVAL` and the
    // probe would conclude the kernel cannot do it.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_EVENTFD,
            SyncobjEventfd {
                handle: 0,
                fd: -1,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );

    let (fd, eventfd) = user.descriptors.add_eventfd();
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_EVENTFD,
            SyncobjEventfd {
                handle: 0,
                fd,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );
    assert_eq!(eventfd.count(), 0);
}

#[ktest]
fn syncobj_eventfd_signals_when_the_fence_is_ready() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    // Already signalled: increment before the ioctl returns, the way
    // `dma_fence_add_callback` returning `-ENOENT` does upstream.
    let signaled = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate {
            handle: 0,
            flags: create_flags::SIGNALED,
        },
    )
    .unwrap()
    .handle;
    let (fd, eventfd) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle: signaled,
            fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(eventfd.count(), 1);

    // Empty object: park, then fire from `SYNCOBJ_SIGNAL`.
    let pending = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    let (fd, eventfd) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle: pending,
            fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(eventfd.count(), 0);

    let handles = user.put(0, &[pending]);
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_SIGNAL,
        SyncobjArray {
            handles,
            count_handles: 1,
            pad: 0,
        },
    )
    .unwrap();
    assert_eq!(eventfd.count(), 1);

    // A fence already installed but not yet signalled fires at signal, not
    // at registration.
    let fence = fence_of(1);
    let object = SyncObj::new(false);
    object.replace_fence(Some(erase(&fence)));
    let handle = file.syncobjs().insert(object).unwrap().0;
    let (fd, eventfd) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle,
            fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(eventfd.count(), 0);
    fence.signal(None);
    assert_eq!(eventfd.count(), 1);
}

#[ktest]
fn syncobj_eventfd_wait_available_means_submitted_not_signalled() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();

    // A fence exists but has not signalled: `WAIT_AVAILABLE` increments now;
    // the default wait still needs the signal.
    let fence = fence_of(1);
    let object = SyncObj::new(false);
    object.replace_fence(Some(erase(&fence)));
    let handle = file.syncobjs().insert(object).unwrap().0;

    let (available_fd, available) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle,
            flags: wait_flags::WAIT_AVAILABLE,
            fd: available_fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(available.count(), 1);

    let (on_signal_fd, on_signal) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle,
            fd: on_signal_fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(on_signal.count(), 0);
    fence.signal(None);
    assert_eq!(on_signal.count(), 1);
    // The available watch already fired; signalling must not increment it
    // again.
    assert_eq!(available.count(), 1);

    // Nothing submitted yet: both watches park. `WAIT_AVAILABLE` fires when
    // a fence appears, even an unsignalled one; the default wait still
    // waits for that fence to signal.
    let empty = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    let object = file.syncobjs().get(SyncObjHandle(empty)).unwrap();
    let (available_fd, available) = user.descriptors.add_eventfd();
    let (on_signal_fd, on_signal) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle: empty,
            flags: wait_flags::WAIT_AVAILABLE,
            fd: available_fd,
            ..Default::default()
        },
    )
    .unwrap();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle: empty,
            fd: on_signal_fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(available.count(), 0);
    assert_eq!(on_signal.count(), 0);

    let later = fence_of(2);
    object.replace_fence(Some(erase(&later)));
    assert_eq!(available.count(), 1);
    assert_eq!(on_signal.count(), 0);
    later.signal(None);
    assert_eq!(on_signal.count(), 1);

    // A wait on a point already implied by a later signalled point fires
    // at once: the fence at 5 satisfies a wait on 3, and it is already
    // signalled, so the callback runs inline.
    let timeline = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;
    let handles = user.put(256, &[timeline]);
    let points = user.put(320, &[5u64]);
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
        SyncobjTimelineArray {
            handles,
            points,
            count_handles: 1,
            flags: 0,
        },
    )
    .unwrap();
    let (fd, eventfd) = user.descriptors.add_eventfd();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_EVENTFD,
        SyncobjEventfd {
            handle: timeline,
            point: 3,
            fd,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(eventfd.count(), 1);
}

#[ktest]
fn syncobj_eventfd_rejects_a_descriptor_that_is_not_an_eventfd() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();
    let handle = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_CREATE,
        SyncobjCreate::default(),
    )
    .unwrap()
    .handle;

    // Closed / never-opened is `EBADF` (`eventfd_ctx_fdget`).
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_EVENTFD,
            SyncobjEventfd {
                handle,
                fd: -1,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::BadFd
    );
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_EVENTFD,
            SyncobjEventfd {
                handle,
                fd: 7,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::BadFd
    );

    // Open, but a DRM export rather than an eventfd: `EINVAL`
    // (`eventfd_ctx_fileget`).
    let exported = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
        SyncobjHandle {
            handle,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::SYNCOBJ_EVENTFD,
            SyncobjEventfd {
                handle,
                fd: exported.fd,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn layouts_match_linux() {
    // Sizes taken from `include/uapi/drm/drm.h` on an LP64 target. A mismatch
    // here means the ioctl encoding in `kernel/core/src/device/dri.rs` disagrees
    // with what user space sends, and the argument is silently truncated.
    assert_eq!(size_of::<SyncobjCreate>(), 8);
    assert_eq!(size_of::<SyncobjDestroy>(), 8);
    assert_eq!(size_of::<SyncobjHandle>(), 16);
    assert_eq!(size_of::<SyncobjTransfer>(), 32);
    // Linux 6.8 appended `deadline_nsec`, making these 40 / 48 bytes.
    // Keep runtime assertions consistent with the `uapi::sizes` checks.
    assert_eq!(size_of::<SyncobjWait>(), 40);
    assert_eq!(size_of::<SyncobjTimelineWait>(), 48);
    assert_eq!(size_of::<SyncobjArray>(), 16);
    assert_eq!(size_of::<SyncobjTimelineArray>(), 24);
    assert_eq!(size_of::<SyncobjEventfd>(), 24);

    // Eight-byte alignment throughout, which is what makes the sizes above
    // free of implicit tail padding.
    assert_eq!(align_of::<SyncobjWait>(), 8);
    assert_eq!(align_of::<SyncobjTimelineArray>(), 8);
    assert_eq!(align_of::<SyncobjEventfd>(), 8);
}

#[ktest]
fn an_older_point_does_not_prewake_a_future_submission_waiter() {
    let object = SyncObj::new(false);
    object.signal_point(1).unwrap();
    let waiter = FenceWaiter::new(1);
    assert!(!object.attach_submit(&waiter, 0, 3));
    assert!(!waiter.is_ready(false));
    assert_eq!(object.submit_waiter_count(), 1);
    // Any new submission wakes a recheck, but an insufficient one must not
    // cause the next attachment to self-wake forever.
    object.signal_point(2).unwrap();
    assert!(waiter.is_ready(false));
    let next = FenceWaiter::new(1);
    assert!(!object.attach_submit(&next, 0, 3));
    assert!(!next.is_ready(false));
    object.signal_point(3).unwrap();
    assert!(next.is_ready(false));
    let late = FenceWaiter::new(1);
    assert!(object.attach_submit(&late, 0, 3));
    assert_eq!(object.submit_waiter_count(), 0);
    // A timeline point cannot satisfy an empty binary-fence wait either.
    let binary = FenceWaiter::new(1);
    assert!(!object.attach_submit(&binary, 0, 0));
    object.detach_submit(&binary);
    assert_eq!(object.submit_waiter_count(), 0);
}

#[ktest]
fn pruning_preserves_the_timeline_value_and_passed_waits() {
    let object = SyncObj::new(false);
    let first = fence_of(1);
    let second = fence_of(2);
    object.add_point(5, erase(&first)).unwrap();
    object.add_point(10, erase(&second)).unwrap();
    first.signal(None);
    object.prune();
    assert_eq!(object.signaled_point(), 5);
    assert_eq!(object.last_submitted_point(), 10);
    assert!(matches!(object.point_state(3), TimelinePoint::Passed));
    assert!(matches!(object.point_state(6), TimelinePoint::Pending(_)));
    second.signal(None);
    object.prune();
    object.prune();
    assert_eq!(object.signaled_point(), 10);
    assert!(matches!(object.point_state(10), TimelinePoint::Passed));
    object.signal_point(20).unwrap();
    assert_eq!(object.signaled_point(), 20);
    object.reset();
    object.signal_point(1).unwrap();
    object.prune();
    assert_eq!(object.signaled_point(), 1);
    assert_eq!(object.last_submitted_point(), 1);
    assert!(matches!(object.point_state(2), TimelinePoint::Unsubmitted));
}

#[ktest]
fn submitting_after_a_completed_point_releases_its_fence() {
    let object = SyncObj::new(false);
    let fence = fence_of(1);
    let weak = Arc::downgrade(&fence);
    object.add_point(1, erase(&fence)).unwrap();
    fence.signal(None);
    drop(fence);
    assert!(weak.upgrade().is_some());
    object.signal_point(2).unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(object.signaled_point(), 2);
    assert!(matches!(object.point_state(1), TimelinePoint::Passed));
}

#[ktest]
fn a_later_timeline_fence_cannot_complete_before_earlier_work() {
    let object = SyncObj::new(false);
    let first = fence_of(1);
    let second = fence_of(2);
    object.add_point(1, erase(&first)).unwrap();
    object.add_point(2, erase(&second)).unwrap();
    let chain = object.fence_at(2).unwrap();
    assert!(chain.as_any().is::<TimelineFence>());
    second.signal(None);
    assert!(!chain.is_signaled());
    assert_eq!(
        wait_many(
            &IdleBlocker,
            &[chain.clone()],
            true,
            Deadline::Immediate,
            true
        ),
        Err(DrmError::TimeExpired)
    );
    first.signal(Some(DrmError::Io));
    assert!(chain.is_signaled());
    assert_eq!(chain.error(), Some(DrmError::Io));
    assert_eq!(object.signaled_point(), 2);
    object.signal_point(3).unwrap();
    assert!(!object.fence_at(3).unwrap().as_any().is::<TimelineFence>());
}

#[ktest]
fn long_timeline_completion_and_abandonment_do_not_recurse() {
    const POINTS: u64 = 2048;
    for complete in [false, true] {
        let object = SyncObj::new(false);
        let first = fence_of(1);
        object.add_point(1, erase(&first)).unwrap();
        for point in 2..=POINTS {
            object.signal_point(point).unwrap();
        }
        let tail = object.fence_at(POINTS).unwrap();
        assert!(!tail.is_signaled());
        // Leave only the last external reference over a deep pending chain.
        object.reset();
        if complete {
            first.signal(None);
            assert!(tail.is_signaled());
        }
        drop(tail);
        // The source callback owns only a weak wrapper reference.
        assert_eq!(Arc::strong_count(&first), 1);
    }
}

#[ktest]
fn timeline_propagation_is_iterative_across_sync_objects() {
    let first = fence_of(1);
    let mut tail: Arc<dyn Fence> = erase(&first);
    for _ in 1..=2048 {
        let object = SyncObj::new(false);
        object.add_point(1, tail).unwrap();
        object.signal_point(2).unwrap();
        tail = object.fence_at(2).unwrap();
        drop(object);
        assert!(!tail.is_signaled());
        // Each point uses a distinct SyncObj, as cross-object transfer can do.
    }
    first.signal(None);
    assert!(tail.is_signaled());
    drop(tail);
    assert_eq!(Arc::strong_count(&first), 1);
}

struct TransferBlocker {
    object: Arc<SyncObj>,
    submit: bool,
    outcome: DrmError,
    calls: AtomicUsize,
}

impl Blocker for TransferBlocker {
    fn now_ns(&self) -> u64 {
        123
    }
    fn block(
        &self,
        _: &WaitQueue,
        ready: &mut dyn FnMut() -> bool,
        deadline: Deadline,
        interruptible: bool,
    ) -> DrmResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert_eq!(deadline, Deadline::At(5_000_000_123));
        assert!(interruptible);
        assert_eq!(self.object.submit_waiter_count(), 1);
        if self.submit {
            self.object.signal_point(7).unwrap();
        }
        if ready() { Ok(()) } else { Err(self.outcome) }
    }
}

#[ktest]
fn transfer_waits_for_submission_with_a_bounded_interruptible_deadline() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();
    for (submit, outcome) in [
        (true, DrmError::TimeExpired),
        (false, DrmError::TimeExpired),
        (false, DrmError::Interrupted),
    ] {
        let source = SyncObj::new(false);
        let destination = SyncObj::new(false);
        let source_handle = file.syncobjs().insert(source.clone()).unwrap();
        let destination_handle = file.syncobjs().insert(destination.clone()).unwrap();
        let args = SyncobjTransfer {
            src_handle: source_handle.0,
            dst_handle: destination_handle.0,
            src_point: 7,
            dst_point: 0,
            flags: wait_flags::WAIT_FOR_SUBMIT,
            pad: 0,
        };
        let blocker = TransferBlocker {
            object: source.clone(),
            submit,
            outcome,
            calls: AtomicUsize::new(0),
        };
        let result = device.ioctl(
            &file,
            ioctl_nr::nr::SYNCOBJ_TRANSFER,
            &mut payload_of(&args),
            &user.context().with_blocker(&blocker),
        );
        if submit {
            assert_eq!(result, Ok(()));
            assert!(destination.fence().unwrap().is_signaled());
        } else {
            assert_eq!(result, Err(outcome));
            assert!(destination.fence().is_none());
        }
        assert_eq!(blocker.calls.load(Ordering::Relaxed), 1);
        assert_eq!(source.submit_waiter_count(), 0);
    }
}

#[ktest]
fn transfer_of_a_pruned_point_preserves_completion() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();
    let source = SyncObj::new(false);
    let destination = SyncObj::new(false);
    source.signal_point(7).unwrap();
    source.prune();
    let source_handle = file.syncobjs().insert(source).unwrap();
    let destination_handle = file.syncobjs().insert(destination.clone()).unwrap();
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_TRANSFER,
        SyncobjTransfer {
            src_handle: source_handle.0,
            dst_handle: destination_handle.0,
            src_point: 3,
            dst_point: 0,
            flags: 0,
            pad: 0,
        },
    )
    .unwrap();
    assert!(destination.fence().unwrap().is_signaled());
}

#[derive(Debug)]
struct RegistrationFence {
    state: FenceState,
    reset: Option<alloc::sync::Weak<SyncObj>>,
    reject: bool,
    retirement: Option<(
        alloc::sync::Weak<FenceSet>,
        Arc<core::sync::atomic::AtomicBool>,
    )>,
}

impl Drop for RegistrationFence {
    fn drop(&mut self) {
        if let Some((set, dropped)) = &self.retirement {
            if let Some(set) = set.upgrade() {
                let _ = set.is_idle();
            }
            dropped.store(true, Ordering::Relaxed);
        }
    }
}

impl Fence for RegistrationFence {
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
        if let Some(object) = self.reset.as_ref().and_then(alloc::sync::Weak::upgrade) {
            object.reset();
        }
        if self.reject {
            return Err(DrmError::NoSpace);
        }
        self.state.add_callback(callback)
    }
}

#[ktest]
fn timeline_registration_failure_does_not_publish_a_point() {
    let object = SyncObj::new(false);
    let first = fence_of(1);
    object.add_point(1, erase(&first)).unwrap();
    let rejected = Arc::new(RegistrationFence {
        state: FenceState::new(FenceContext::new(), 2),
        reset: None,
        reject: true,
        retirement: None,
    });
    assert_eq!(object.add_point(2, rejected), Err(DrmError::NoSpace));
    assert_eq!(object.last_submitted_point(), 1);
    assert!(matches!(object.point_state(2), TimelinePoint::Unsubmitted));
    object.signal_point(2).unwrap();
    first.signal(None);
    assert_eq!(object.signaled_point(), 2);
}

#[ktest]
fn reset_during_callback_registration_rechecks_the_timeline_prefix() {
    let object = SyncObj::new(false);
    let first = fence_of(1);
    object.add_point(1, erase(&first)).unwrap();
    let second = Arc::new(RegistrationFence {
        state: FenceState::new(FenceContext::new(), 2),
        reset: Some(Arc::downgrade(&object)),
        reject: false,
        retirement: None,
    });
    object.add_point(2, second.clone()).unwrap();
    assert_eq!(object.last_submitted_point(), 2);
    assert!(!object.fence_at(2).unwrap().as_any().is::<TimelineFence>());
    second.state.signal(None);
    assert_eq!(object.signaled_point(), 2);
    assert!(!first.is_signaled());
}

#[ktest]
fn replacing_or_pruning_buffer_fences_releases_them_after_unlocking() {
    for operation in 0..4 {
        let set = Arc::new(FenceSet::new());
        let dropped = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let observed = Arc::new(RegistrationFence {
            state: FenceState::new_signaled(FenceContext::new(), 1, FenceStatus::Signaled),
            reset: None,
            reject: false,
            retirement: Some((Arc::downgrade(&set), dropped.clone())),
        });
        if operation == 0 {
            set.add_write(observed);
        } else {
            set.add_read(observed);
        }
        match operation {
            0 | 1 => set.add_write(fence_of(2)),
            2 => set.add_read(fence_of(2)),
            3 => set.prune(),
            _ => unreachable!(),
        }
        assert!(dropped.load(Ordering::Relaxed));
    }
}

#[ktest]
fn timeline_signal_validates_aliased_handles_before_applying_a_batch() {
    let device = device();
    let file = file(&device);
    let user = TestUser::new();
    let object = SyncObj::new(false);
    let first = file.syncobjs().insert(object.clone()).unwrap();
    let alias = file.syncobjs().insert(object.clone()).unwrap();
    let handles = user.put(0, &[first.0, alias.0]);
    let points = user.put(64, &[7u64, 7]);
    let args = SyncobjTimelineArray {
        handles,
        points,
        count_handles: 2,
        flags: 0,
    };
    for invalid in [[7u64, 7], [9, 7]] {
        user.put(64, &invalid);
        assert_eq!(
            ioctl(
                &device,
                &file,
                &user,
                ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
                args
            )
            .unwrap_err(),
            DrmError::InvalidArgument
        );
        assert_eq!(object.last_submitted_point(), 0);
    }
    user.put(64, &[7u64, 9]);
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
        args,
    )
    .unwrap();
    assert_eq!(object.last_submitted_point(), 9);
    assert_eq!(object.signaled_point(), 9);
}
