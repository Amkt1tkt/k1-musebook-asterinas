// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR explicit synchronization on DRM fence objects.

//! Synchronisation operations on submitted work.
//!
//! Ports `pvr_sync.c` and `pvr_sync.h` (373 lines).
//!
//! Each job carries a list of sync operations, each naming a sync object — plain
//! or timeline — and saying whether the job waits on it or signals it. This
//! module resolves those into `k1-musebook-drm`'s fence primitives; it owns no fence
//! implementation of its own.
//!
//! Preparation has three ordered phases for each job:
//!
//! 1. Collect signal points and their previous fences into the batch's signal array.
//! 2. Resolve waits against the cached batch fence or the current sync object.
//! 3. Replace each signaled point's cached fence with this job's fence.
//!
//! A job waiting on a point it also signals therefore sees the previous fence,
//! while later jobs see this job. Batch dependencies point backwards; fence-graph
//! checks also reject externally introduced cycles.
//!
//! All operation arrays are validated before use. Preparing a batch publishes no
//! fences; dropping a failed preparation leaves sync objects untouched.
//! The final signal-array installation follows successful queue insertion.
//!
//! Failed fences remain signaled error dependencies rather than successful waits.
//! Timeline resolution distinguishes passed points from unsubmitted points.
//! No WAIT_FOR_SUBMIT behavior is requested: an unsubmitted wait is rejected.

use alloc::{sync::Arc, vec::Vec};

use k1_musebook_drm::fence::{Fence, SyncObj, SyncObjHandle, SyncObjTable, TimelinePoint};
use ostd_pod::Pod;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    rogue::fwif::FwAddr,
    sched::queue::{JobFence, TimelineUfo},
    uapi,
};

/// Handle types a sync operation may name.
///
/// Values are ABI, from `DRM_PVR_SYNC_OP_FLAG_HANDLE_TYPE_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleType {
    /// A plain sync object.
    Syncobj,
    /// A timeline sync object, where the operation also carries a point.
    TimelineSyncobj,
}

impl HandleType {
    /// Reads the handle-type field of a `drm_pvr_sync_op`'s flags.
    fn from_flags(flags: u32) -> PvrResult<Self> {
        match flags & uapi::SyncOpFlags::HANDLE_TYPE_MASK.bits() {
            uapi::SYNC_OP_HANDLE_TYPE_SYNCOBJ => Ok(Self::Syncobj),
            v if v == uapi::SyncOpFlags::HANDLE_TYPE_TIMELINE_SYNCOBJ.bits() => {
                Ok(Self::TimelineSyncobj)
            }
            _ => Err(PvrError::InvalidArgument),
        }
    }
}

/// One validated sync operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    /// The sync object's handle in the calling file.
    pub handle: u32,
    /// The timeline point, or zero for a binary sync object.
    pub point: u64,
    /// What kind of object the handle names.
    pub kind: HandleType,
    /// Whether the job signals the object rather than waiting on it.
    pub signal: bool,
}

/// One job's validated sync operations.
///
/// A newtype rather than a bare `Vec` so that "validated" is visible in the
/// type: [`SignalArray`] and [`dependencies`] both assume every operation they
/// are handed has already been through [`Self::parse`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Ops(Vec<Op>);

impl Ops {
    /// Parses and validates a job's sync-operation array.
    ///
    /// `raw` is the caller's array image, exactly `array.count * array.stride`
    /// bytes; the stride negotiation goes through [`crate::uapi::obj_array`],
    /// which is the only implementation of that rule.
    ///
    /// Every element is checked before any is returned. Upstream validates each
    /// operation at the point it processes it, which is enough for it because
    /// nothing is published until the end; validating up front is strictly
    /// stronger and makes "the whole array is refused" a property of this
    /// function rather than of its callers.
    ///
    /// Ports upstream `pvr_check_sync_op()`, applied to the whole array.
    pub fn parse(array: &uapi::ObjArray, raw: &[u8]) -> PvrResult<Self> {
        let elements = uapi::obj_array(
            array,
            uapi::SyncOp::MIN_STRIDE,
            size_of::<uapi::SyncOp>(),
            raw,
        )?;

        let mut ops = Vec::with_capacity(array.count as usize);
        for chunk in elements.as_chunks::<{ size_of::<uapi::SyncOp>() }>().0 {
            let raw = <uapi::SyncOp as Pod>::from_bytes(chunk);

            // Upstream's first check: an unknown flag bit is a newer client
            // asking for a behaviour this driver does not implement, and
            // guessing is the `CONVENTIONS.md` command-stream contract mistake.
            if raw.flags & !uapi::SyncOpFlags::FLAGS_MASK != 0 {
                return Err(PvrError::InvalidArgument);
            }

            let kind = HandleType::from_flags(raw.flags)?;
            // A plain sync object has no points, so a non-zero value is a
            // client that meant to set the timeline handle-type bit. Refused
            // rather than ignored: the two mean different things and the
            // difference is which fence the job ends up waiting for.
            if kind == HandleType::Syncobj && raw.value != 0 {
                return Err(PvrError::InvalidArgument);
            }

            ops.push(Op {
                handle: raw.handle,
                point: raw.value,
                kind,
                signal: raw.flags & uapi::SyncOpFlags::SIGNAL.bits() != 0,
            });
        }

        Ok(Self(ops))
    }

    /// The operations, in the order the client gave them.
    pub fn as_slice(&self) -> &[Op] {
        &self.0
    }

    /// Whether the job carries no sync operations at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One `(object, point)` pair a batch signals, including aliased handles.
struct Entry {
    /// The handle, as the client named it.
    handle: u32,
    /// Other handles collected for this same object and point.
    aliases: Vec<u32>,
    /// The point, or zero for the binary fence.
    point: u64,
    /// The object the handle resolved to.
    object: Arc<SyncObj>,
    /// The fence that will be installed at [`Self::point`].
    ///
    /// Starts as whatever the object already has there, which may be nothing,
    /// and is replaced by each job in the batch that signals this pair. That is
    /// what makes the array a cache as well as a to-do list — see the module
    /// documentation.
    fence: Option<Arc<dyn Fence>>,
    /// Index within the batch of the job whose fence [`Self::fence`] is.
    ///
    /// `None` while the fence is still the one the object had before the batch
    /// started. Knowing which job produced a fence is what lets the submission
    /// path turn the dependency into a firmware-side wait instead of a
    /// driver-side one; see [`Wait::producer`].
    producer: Option<usize>,
}

/// Every sync object a `SUBMIT_JOBS` batch will signal.
///
/// Upstream keeps this in an `xarray` local to `pvr_submit_jobs()`; the shape is
/// the same and so is the lifetime — it exists for one ioctl and is discarded
/// afterwards, whether or not the submission succeeded.
///
/// Ports upstream `struct pvr_sync_signal` and the four functions over it.
#[derive(Default)]
pub struct SignalArray {
    entries: Vec<Entry>,
}

impl SignalArray {
    /// An array for a batch that has not been looked at yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct `(object, point)` pairs the batch signals.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the batch signals nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Phase 1: records the signal operations of one job.
    ///
    /// Ports upstream `pvr_sync_signal_array_collect_ops()` and the
    /// `pvr_sync_signal_array_get()` behind it. Signalling one pair from two
    /// jobs in a batch produces one entry, which is what makes the later job's
    /// fence the one that finally lands there.
    ///
    /// # Errors
    ///
    /// [`PvrError::NoSuchHandle`] for a handle this file does not hold —
    /// upstream's `drm_syncobj_find()` miss, which it reports as `-EINVAL`.
    ///
    /// [`PvrError::InvalidArgument`] for a timeline point the object has
    /// already been given. Fence timeline-ordering contract makes that an error at install time,
    /// and install time is *after* the jobs are committed, where nothing may
    /// fail — so it is checked here instead, while refusing the batch is still
    /// free. A client that races another submission on the same object can
    /// still lose this check later; see [`Self::push_fences`].
    pub fn collect(&mut self, syncobjs: &SyncObjTable, ops: &Ops) -> PvrResult<()> {
        for op in ops.as_slice().iter().filter(|op| op.signal) {
            if self.find(op.handle, op.point).is_some() {
                continue;
            }

            let object = syncobjs
                .get(SyncObjHandle(op.handle))
                .map_err(|_| PvrError::NoSuchHandle)?;

            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| Arc::ptr_eq(&entry.object, &object) && entry.point == op.point)
            {
                entry.aliases.push(op.handle);
                continue;
            }

            if op.point != 0 && op.point <= object.last_submitted_point() {
                return Err(PvrError::InvalidArgument);
            }

            // Upstream: "It's perfectly fine to get a NULL fence here, it just
            // means there's no fence attached to that point yet."
            let fence = object.fence_at(op.point);
            self.entries.push(Entry {
                handle: op.handle,
                aliases: Vec::new(),
                point: op.point,
                object,
                fence,
                producer: None,
            });
        }

        Ok(())
    }

    /// Phase 3: makes a job's fence the one the pairs it signals will get.
    ///
    /// Ports upstream `pvr_sync_signal_array_update_fences()`. `index` is the
    /// job's position in the batch, which is what [`Wait::producer`] reports to
    /// a later job.
    ///
    /// # Errors
    ///
    /// [`PvrError::InvalidArgument`] if a signal operation has no entry, which
    /// means [`Self::collect`] was not run for this job. Upstream's
    /// `WARN_ON(!sig_sync)` says the same thing about the same mistake.
    pub fn update(&mut self, ops: &Ops, fence: Arc<dyn Fence>, index: usize) -> PvrResult<()> {
        for op in ops.as_slice().iter().filter(|op| op.signal) {
            let entry = self
                .find_mut(op.handle, op.point)
                .ok_or(PvrError::InvalidArgument)?;
            entry.fence = Some(fence.clone());
            entry.producer = Some(index);
        }
        Ok(())
    }

    /// Installs every collected fence into its sync object.
    ///
    /// Ports upstream `pvr_sync_signal_array_push_fences()`. **This is the only
    /// function in the module that user space can observe**, and it must
    /// therefore be the last thing a successful submission does: everything
    /// before it is undone by dropping this array.
    ///
    /// Entries are installed in ascending point order per object, so a batch
    /// that signals points five and seven in either array order still installs
    /// them the way fence timeline-ordering contract requires.
    ///
    /// Failures are logged rather than returned, because there is nothing left
    /// to undo: the jobs are queued and their fences are the client's. The one
    /// way to get here is another thread submitting a higher point on the same
    /// object between [`Self::collect`] and this call, which upstream's chain
    /// handles no better.
    pub fn push_fences(&mut self) {
        self.entries
            .sort_by_key(|entry| (Arc::as_ptr(&entry.object) as usize, entry.point));

        for entry in &self.entries {
            let Some(fence) = entry.fence.clone() else {
                // Only reachable if a job signalled a pair and then vanished
                // without arming its fence, which `update` refuses.
                ostd::warn!(
                    "sync object {} point {} was collected but never given a fence",
                    entry.handle,
                    entry.point
                );
                continue;
            };

            if entry.point == 0 {
                // Upstream `drm_syncobj_replace_fence()`. Note that a timeline
                // handle with point zero lands here too: upstream only
                // allocates a chain when `point > 0`, so point zero is the
                // binary fence whichever handle type the client declared.
                entry.object.replace_fence(Some(fence));
            } else if let Err(err) = entry.object.add_point(entry.point, fence) {
                ostd::warn!(
                    "could not signal sync object {} point {}: {err:?}",
                    entry.handle,
                    entry.point
                );
            }
        }
    }

    /// The entry for a pair, if the batch already has one.
    fn find(&self, handle: u32, point: u64) -> Option<&Entry> {
        self.entries.iter().find(|entry| {
            (entry.handle == handle || entry.aliases.contains(&handle)) && entry.point == point
        })
    }

    /// [`Self::find`], mutably.
    fn find_mut(&mut self, handle: u32, point: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| {
            (entry.handle == handle || entry.aliases.contains(&handle)) && entry.point == point
        })
    }
}

/// Something a job must wait for, before it is attached to the job.
///
/// The submission path turns this into a `crate::sched::job::Dep`; this module
/// stops one step short so that it does not have to know how a dependency is
/// represented, and so that the resolution can be tested without building a job.
#[derive(Clone)]
pub struct Wait {
    /// The fence to wait for.
    pub fence: Arc<dyn Fence>,
    /// Index within the batch of the job that will signal it, if it is one.
    ///
    /// `Some` means the submission path can build a native dependency from the
    /// producer [`crate::sched::job::Job`] itself via
    /// [`crate::sched::job::Job::native_dep_on`]. Takes precedence over
    /// [`Self::ufo`] when both are set.
    pub producer: Option<usize>,
    /// Firmware UFO backing, when the fence is a
    /// [`crate::sched::queue::JobFence`] on the submitting device.
    ///
    /// Ports the outcome of upstream `pvr_queue_fence_is_native()`: a fence
    /// this driver made, on a queue the firmware of *this* device can see,
    /// becomes a `FENCE_PR` rather than a CPU wait — including across ioctl
    /// boundaries. See [`JobFence::native_ufo`].
    ///
    /// [`JobFence::native_ufo`]: crate::sched::queue::JobFence::native_ufo
    pub ufo: Option<(FwAddr, u32, Arc<TimelineUfo>)>,
}

/// Phase 2: resolves one job's wait operations.
///
/// Ports upstream `pvr_sync_add_deps_to_job()`, stopping before the point where
/// it attaches the result to a `drm_sched_job`.
///
/// A wait that is *already satisfied* produces no [`Wait`] at all. Two cases
/// reach that: a timeline point the object has run past (fence timeline-ordering contract), and —
/// through [`crate::sched::job::Job::add_dependency`], which drops it — a fence
/// that has signalled successfully. A fence that signalled *with an error* is
/// kept, because error-propagation contract says the job must fail rather than proceed.
///
/// # Errors
///
/// - [`PvrError::NoSuchHandle`] for a handle this file does not hold.
/// - [`PvrError::InvalidArgument`] when there is nothing at the point named:
///   an empty binary sync object, or a timeline point nothing has been
///   submitted for. Upstream reaches the same `-EINVAL` through
///   `drm_syncobj_find_fence()` with no `WAIT_FOR_SUBMIT` flag.
pub fn dependencies(
    syncobjs: &SyncObjTable,
    signals: &SignalArray,
    ops: &Ops,
    device: &Arc<PvrDevice>,
) -> PvrResult<Vec<Wait>> {
    let mut waits = Vec::new();

    for op in ops.as_slice().iter().filter(|op| !op.signal) {
        let object = syncobjs
            .get(SyncObjHandle(op.handle))
            .map_err(|_| PvrError::NoSuchHandle)?;
        // Resolve aliases by object identity. Another handle may signal this very
        // same point earlier in the batch while the shared object is still unchanged.
        if let Some(entry) = signals
            .entries
            .iter()
            .find(|entry| Arc::ptr_eq(&entry.object, &object) && entry.point == op.point)
        {
            // Upstream `WARN_ON(!sig_sync->fence)`. Reachable, and the case is
            // worth naming: a job that both signals and waits on a pair nothing
            // has ever signalled. There is no fence to wait for and the job's
            // own is not it, so the submission is refused.
            let fence = entry.fence.clone().ok_or(PvrError::InvalidArgument)?;
            waits.push(classify_wait(fence, entry.producer, device));
            continue;
        }

        match object.point_state(op.point) {
            TimelinePoint::Pending(fence) => {
                waits.push(classify_wait(fence, None, device));
            }
            // Timeline ordering: the timeline has run past this point, so the wait is
            // already satisfied and there is nothing to depend on. Upstream
            // substitutes `dma_fence_get_stub()`, an already-signalled fence,
            // which `drm_sched_job_add_dependency()` then discards; dropping it
            // here is the same outcome one step earlier.
            TimelinePoint::Passed => {}
            // Nothing that would reach this point has been submitted. This
            // driver never asks to wait for a submission — upstream passes
            // `flags = 0` to `drm_syncobj_find_fence()` — so it is an error
            // rather than a wait, and telling it apart from `Passed` is the
            // whole reason timeline-ordering contract's type is not an `Option`.
            TimelinePoint::Unsubmitted => return Err(PvrError::InvalidArgument),
        }
    }

    Ok(waits)
}

/// Tags a resolved fence with its native-UFO info when this device can wait on
/// it in firmware.
///
/// Ports the `pvr_queue_fence_is_native()` half of upstream
/// `pvr_sync_add_dep_to_job()`. A same-batch `producer` is recorded alongside
/// so the submission path can still build the dependency from the [`Job`]; the
/// UFO is filled whenever the fence itself is recognisable, covering the
/// cross-ioctl case where there is no producer index.
///
/// [`Job`]: crate::sched::job::Job
fn classify_wait(fence: Arc<dyn Fence>, producer: Option<usize>, device: &Arc<PvrDevice>) -> Wait {
    let ufo = fence
        .as_any()
        .downcast_ref::<JobFence>()
        .and_then(|job_fence| job_fence.native_ufo(device));
    Wait {
        fence,
        producer,
        ufo,
    }
}

#[cfg(ktest)]
mod test {
    use k1_musebook_drm::fence::SoftwareFence;
    use ostd::prelude::*;
    use ostd_pod::IntoBytes;

    use super::*;

    /// A sync-operation array image, as the ioctl handler would hand it over.
    fn array(ops: &[uapi::SyncOp]) -> (uapi::ObjArray, Vec<u8>) {
        let mut raw = Vec::new();
        for op in ops {
            raw.extend_from_slice(op.as_bytes());
        }
        let array = uapi::ObjArray {
            stride: size_of::<uapi::SyncOp>() as u32,
            count: ops.len() as u32,
            array: 0x4000,
        };
        (array, raw)
    }

    fn wait_op(handle: u32) -> uapi::SyncOp {
        uapi::SyncOp {
            handle,
            flags: uapi::SyncOpFlags::WAIT,
            value: 0,
        }
    }

    fn signal_op(handle: u32) -> uapi::SyncOp {
        uapi::SyncOp {
            handle,
            flags: uapi::SyncOpFlags::SIGNAL.bits(),
            value: 0,
        }
    }

    fn timeline_wait(handle: u32, point: u64) -> uapi::SyncOp {
        uapi::SyncOp {
            handle,
            flags: uapi::SyncOpFlags::HANDLE_TYPE_TIMELINE_SYNCOBJ.bits(),
            value: point,
        }
    }

    fn parse(ops: &[uapi::SyncOp]) -> PvrResult<Ops> {
        let (array, raw) = array(ops);
        Ops::parse(&array, &raw)
    }

    #[ktest]
    fn a_well_formed_array_parses_into_the_operations_it_names() {
        let ops = parse(&[wait_op(1), signal_op(2), timeline_wait(3, 7)]).expect("well formed");
        assert_eq!(
            ops.as_slice(),
            &[
                Op {
                    handle: 1,
                    point: 0,
                    kind: HandleType::Syncobj,
                    signal: false
                },
                Op {
                    handle: 2,
                    point: 0,
                    kind: HandleType::Syncobj,
                    signal: true
                },
                Op {
                    handle: 3,
                    point: 7,
                    kind: HandleType::TimelineSyncobj,
                    signal: false
                },
            ]
        );
    }

    #[ktest]
    fn every_malformed_operation_is_refused() {
        // An unknown flag bit.
        let mut unknown = wait_op(1);
        unknown.flags = 1 << 30;
        assert_eq!(parse(&[unknown]), Err(PvrError::InvalidArgument));

        // A handle type that is neither of the two defined ones. The field is
        // four bits wide, so this is inside `FLAGS_MASK` and has to be caught by
        // the type check rather than by the mask check.
        let mut bad_type = wait_op(1);
        bad_type.flags = 2;
        assert_eq!(parse(&[bad_type]), Err(PvrError::InvalidArgument));

        // A point on a plain sync object.
        let mut valued = wait_op(1);
        valued.value = 1;
        assert_eq!(parse(&[valued]), Err(PvrError::InvalidArgument));

        // A stride below the minimum, which `obj_array` refuses before it looks
        // at any element.
        let (mut array, raw) = array(&[wait_op(1)]);
        array.stride = (uapi::SyncOp::MIN_STRIDE - 1) as u32;
        assert_eq!(Ops::parse(&array, &raw), Err(PvrError::InvalidArgument));
    }

    #[ktest]
    fn a_malformed_operation_rejects_the_whole_array() {
        // The all-or-nothing property, stated as a test rather than inferred:
        // nineteen good operations and one bad one is not nineteen operations.
        let mut ops: Vec<uapi::SyncOp> = (1..20).map(wait_op).collect();
        let mut bad = wait_op(20);
        bad.flags = 1 << 30;
        ops.push(bad);
        ops.extend((21..25).map(wait_op));

        assert_eq!(parse(&ops), Err(PvrError::InvalidArgument));
    }

    /// Tests synchronization resolution without a device.
    ///
    /// These fixtures check [`SignalArray`] cache ordering. Device-dependent
    /// resolution tests live in [`crate::render::test`].
    #[ktest]
    fn the_signal_cache_orders_points_before_installing_them() {
        let object = SyncObj::new(false);
        let mut array = SignalArray::new();

        // Built by hand rather than through `collect`, which needs a file.
        let seven: Arc<dyn Fence> = SoftwareFence::new_signaled();
        let five: Arc<dyn Fence> = SoftwareFence::new_signaled();
        array.entries.push(Entry {
            handle: 1,
            aliases: Vec::new(),
            point: 7,
            object: object.clone(),
            fence: Some(seven),
            producer: Some(1),
        });
        array.entries.push(Entry {
            handle: 1,
            aliases: Vec::new(),
            point: 5,
            object: object.clone(),
            fence: Some(five),
            producer: Some(0),
        });

        array.push_fences();

        // Both points landed, which only happens if seven went in after five:
        // timeline-ordering contract refuses a point that is not strictly greater than every point
        // the object has seen.
        assert_eq!(object.last_submitted_point(), 7);
        assert!(matches!(
            object.point_state(5),
            TimelinePoint::Pending(_) | TimelinePoint::Passed
        ));
        assert!(matches!(object.point_state(7), TimelinePoint::Pending(_)));
    }
}
