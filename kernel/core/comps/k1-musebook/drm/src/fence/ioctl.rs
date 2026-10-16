// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The twelve sync-object ioctls.
//!
//! Every one of them is `DRM_RENDER_ALLOW` upstream and here,
//! because sync objects are how a render client with no display access
//! synchronizes with one that has it — the same reason PRIME is.
//!
//! They are also all gated on [`DriverFeatures::SYNCOBJ`], which is what
//! `DRM_CAP_SYNCOBJ` reports: a driver that does not claim the capability must
//! not answer the ioctls either, or user space that probed the capability and
//! believed the answer will still find them working and stop probing.
//! `SYNCOBJ_EVENTFD` additionally requires
//! [`DriverFeatures::SYNCOBJ_TIMELINE`], matching linux-6.6
//! `drm_syncobj_eventfd_ioctl`.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c>
//! and <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L690>.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use super::{
    Attachment, Blocker, Deadline, Fence, FenceWaiter, MAX_WAIT_HANDLES, SoftwareFence,
    SubmitAttachment, SyncObj, SyncObjHandle, TimelinePoint, wait_many,
};
use crate::{
    device::{DriverFeatures, DrmDevice},
    error::{DrmError, DrmResult},
    file::DrmFile,
    ioctl::{CallerContext, copy_in_array, copy_out_array, payload_read, payload_write},
    prime::{DrmExport, ExportFlags},
    uapi::sync::{
        SyncobjArray, SyncobjCreate, SyncobjDestroy, SyncobjEventfd, SyncobjHandle,
        SyncobjTimelineArray, SyncobjTimelineWait, SyncobjTransfer, SyncobjWait, create_flags,
        handle_flags, query_flags, wait_flags,
    },
};

/// `DRM_IOCTL_SYNCOBJ_CREATE`: allocates a sync object in this file.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1279>
/// (`drm_syncobj_create_ioctl`).
pub fn syncobj_create(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    let mut args = payload_read::<SyncobjCreate>(payload)?;
    if args.flags & !create_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let object = SyncObj::new(args.flags & create_flags::SIGNALED != 0);
    args.handle = file.syncobjs().insert(object)?.0;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_SYNCOBJ_DESTROY`: releases one sync-object handle.
///
/// The object survives while a file descriptor or another file's handle still
/// refers to it, exactly as a GEM object survives `GEM_CLOSE`.
///
/// A handle that is not in the table is `ENOENT`, unlike GEM's `EINVAL` for
/// the same mistake. Both are what Linux returns and both are ABI, so the
/// inconsistency has to be preserved rather than tidied.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1301>
/// (`drm_syncobj_destroy_ioctl`).
pub fn syncobj_destroy(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    let args = payload_read::<SyncobjDestroy>(payload)?;
    if args.pad != 0 {
        return Err(DrmError::InvalidArgument);
    }

    file.syncobjs().remove(SyncObjHandle(args.handle))
}

/// `DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD`: shares a sync object as a descriptor.
///
/// Two different things, chosen by
/// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`:
///
///  - without it, the descriptor refers to the *object*, so signals through
///    one holder are seen by every other;
///  - with it, the descriptor is a *sync file*: a snapshot of the fence the
///    object holds right now, which later signals of the object do not
///    change.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L750>
/// (`drm_syncobj_handle_to_fd_ioctl`).
pub fn syncobj_handle_to_fd(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    let mut args = payload_read::<SyncobjHandle>(payload)?;
    if args.pad != 0 || args.flags & !handle_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let descriptors = caller.descriptors().ok_or(DrmError::NotSupported)?;
    let object = file.syncobjs().get(SyncObjHandle(args.handle))?;

    let export = if args.flags & handle_flags::SYNC_FILE != 0 {
        // A sync file needs a fence to snapshot. Upstream reports `EINVAL` for
        // an empty object rather than handing out an already-signalled fence,
        // because "nothing to wait for" and "the work is done" are different
        // answers and only the client knows which it meant.
        let fence = object.fence().ok_or(DrmError::InvalidArgument)?;
        DrmExport::SyncFile(fence)
    } else {
        DrmExport::Sync(object)
    };

    args.fd = descriptors.install(export, ExportFlags::SYNC)?;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE`: takes a shared sync object into this file.
///
/// The mirror of [`syncobj_handle_to_fd`], and asymmetric in the same way. A
/// sync-object descriptor becomes a *new handle*; a sync-file descriptor is
/// installed into an *existing* handle the caller names, because a sync file
/// is a fence rather than a container and has nowhere of its own to live.
///
/// Importing the same descriptor twice returns *two* handles onto the one
/// object, which is upstream's behaviour: `drm_syncobj_fd_to_handle` allocates
/// unconditionally. Unlike PRIME, which really does look for an existing handle
/// first, nothing here deduplicates — an importer that closes one handle keeps
/// the other.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L790>
/// (`drm_syncobj_fd_to_handle_ioctl`).
pub fn syncobj_fd_to_handle(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    let mut args = payload_read::<SyncobjHandle>(payload)?;
    if args.pad != 0 || args.flags & !handle_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let descriptors = caller.descriptors().ok_or(DrmError::NotSupported)?;
    let import = descriptors.resolve(args.fd)?;

    if args.flags & handle_flags::SYNC_FILE != 0 {
        let DrmExport::SyncFile(fence) = import else {
            return Err(DrmError::InvalidArgument);
        };

        file.syncobjs()
            .get(SyncObjHandle(args.handle))?
            .replace_fence(Some(fence));

        return payload_write(payload, &args);
    }

    let DrmExport::Sync(object) = import else {
        return Err(DrmError::InvalidArgument);
    };

    args.handle = file.syncobjs().insert(object)?.0;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_SYNCOBJ_WAIT`: blocks until the named binary sync objects signal.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1180>
/// (`drm_syncobj_wait_ioctl`).
pub fn syncobj_wait(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    let mut args = payload_read::<SyncobjWait>(payload)?;
    if args.pad != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let objects = wait_objects(file, caller, args.handles, args.count_handles, args.flags)?;
    // Point zero for every handle: a binary wait is a timeline wait on the
    // container's own fence, which is how upstream implements it too.
    let points = alloc::vec![0u64; objects.len()];

    let deadline = Deadline::from_timeout_nsec(args.timeout_nsec);
    let result = wait_syncobjs(caller, &objects, &points, args.flags, deadline);

    args.first_signaled = result.unwrap_or(0) as u32;
    payload_write(payload, &args)?;

    result.map(|_| ())
}

/// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`: blocks until the named timeline points
/// are reached.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1224>
/// (`drm_syncobj_timeline_wait_ioctl`).
pub fn syncobj_timeline_wait(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_timeline_syncobj(device)?;

    let mut args = payload_read::<SyncobjTimelineWait>(payload)?;
    if args.pad != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let objects = wait_objects(file, caller, args.handles, args.count_handles, args.flags)?;
    let points = copy_in_array::<u64>(caller, args.points, args.count_handles)?;

    let result = wait_syncobjs(
        caller,
        &objects,
        &points,
        args.flags,
        Deadline::from_timeout_nsec(args.timeout_nsec),
    );

    args.first_signaled = result.unwrap_or(0) as u32;
    payload_write(payload, &args)?;

    result.map(|_| ())
}

/// `DRM_IOCTL_SYNCOBJ_RESET`: empties the named sync objects.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1330>
/// (`drm_syncobj_reset_ioctl`).
pub fn syncobj_reset(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    for object in array_objects(file, caller, payload)? {
        object.reset();
    }

    Ok(())
}

/// `DRM_IOCTL_SYNCOBJ_SIGNAL`: signals the named binary sync objects.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1355>
/// (`drm_syncobj_signal_ioctl`).
pub fn syncobj_signal(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_syncobj(device)?;

    for object in array_objects(file, caller, payload)? {
        object.signal();
    }

    Ok(())
}

/// `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`: signals points on the named timelines.
///
/// Every point is validated before any is installed. A partially applied
/// signal would leave the caller's timelines in a state it has no way to
/// discover, since the ioctl reports one errno for the whole array.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1380>
/// (`drm_syncobj_timeline_signal_ioctl`).
pub fn syncobj_timeline_signal(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_timeline_syncobj(device)?;

    let args = payload_read::<SyncobjTimelineArray>(payload)?;
    if args.flags != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let (objects, points) = timeline_array(file, caller, &args)?;

    // Validate the whole batch, including handles that alias one object. The
    // resolved Arcs keep addresses stable throughout validation and installation.
    // This does not make the batch a transaction against other concurrent ioctls.
    let mut planned = BTreeMap::new();
    for (object, point) in objects.iter().zip(points.iter()) {
        let last = planned
            .entry(Arc::as_ptr(object) as usize)
            .or_insert_with(|| object.last_submitted_point());
        if *point == 0 || *point <= *last {
            return Err(DrmError::InvalidArgument);
        }
        *last = *point;
    }

    for (object, point) in objects.iter().zip(points.iter()) {
        object.signal_point(*point)?;
    }

    Ok(())
}

/// `DRM_IOCTL_SYNCOBJ_QUERY`: reports how far the named timelines have got.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1424>
/// (`drm_syncobj_query_ioctl`).
pub fn syncobj_query(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_timeline_syncobj(device)?;

    let args = payload_read::<SyncobjTimelineArray>(payload)?;
    if args.flags & !query_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let handles = copy_in_array::<u32>(caller, args.handles, args.count_handles)?;
    let objects = file.syncobjs().get_many(&handles)?;

    let last_submitted = args.flags & query_flags::LAST_SUBMITTED != 0;
    let values: Vec<u64> = objects
        .iter()
        .map(|object| {
            if last_submitted {
                object.last_submitted_point()
            } else {
                object.signaled_point()
            }
        })
        .collect();

    // Not the two-pass array protocol: `count_handles` is an input describing
    // the caller's own arrays, both of which it has already filled with
    // handles, so there is nothing to negotiate and nothing written back.
    copy_out_array(caller, args.points, args.count_handles, &values)?;

    Ok(())
}

/// `DRM_IOCTL_SYNCOBJ_TRANSFER`: copies a fence between sync objects.
///
/// The destination point decides which kind of transfer this is: a non-zero
/// `dst_point` adds a timeline point, and zero replaces the destination's
/// binary fence. That is upstream's dispatch, not an interpretation of it.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L890>
/// (`drm_syncobj_transfer_ioctl`).
pub fn syncobj_transfer(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_timeline_syncobj(device)?;

    let args = payload_read::<SyncobjTransfer>(payload)?;
    if args.pad != 0 || args.flags & !wait_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let syncobjs = file.syncobjs();
    let source = syncobjs.get(SyncObjHandle(args.src_handle))?;
    let destination = syncobjs.get(SyncObjHandle(args.dst_handle))?;

    let fence = transfer_fence(&source, args.src_point, args.flags, caller)?;

    if args.dst_point == 0 {
        destination.replace_fence(Some(fence));
        return Ok(());
    }

    destination.add_point(args.dst_point, fence)
}

/// Resolves a transfer source, retaining the fence once submitted.
fn transfer_fence(
    source: &Arc<SyncObj>,
    point: u64,
    flags: u32,
    caller: &CallerContext,
) -> DrmResult<Arc<dyn Fence>> {
    // Linux v6.6 drm_syncobj_find_fence bounds WAIT_FOR_SUBMIT at five seconds.
    const SUBMIT_TIMEOUT_NS: u64 = 5_000_000_000;
    let mut deadline = None;
    loop {
        match source.point_state(point) {
            TimelinePoint::Pending(fence) => return Ok(fence),
            TimelinePoint::Passed => return Ok(SoftwareFence::new_signaled()),
            TimelinePoint::Unsubmitted => {}
        }
        if flags & wait_flags::WAIT_FOR_SUBMIT == 0 {
            return Err(DrmError::InvalidArgument);
        }
        let blocker = caller.blocker().ok_or(DrmError::NotSupported)?;
        let deadline = *deadline.get_or_insert_with(|| {
            Deadline::At(blocker.now_ns().saturating_add(SUBMIT_TIMEOUT_NS))
        });
        let waiter = FenceWaiter::new(1);
        let objects = [(source.clone(), point)];
        let attachment = SubmitAttachment::new(waiter.clone(), &objects);
        let result = blocker.block(
            waiter.queue(),
            &mut || !matches!(source.point_state(point), TimelinePoint::Unsubmitted),
            deadline,
            true,
        );
        drop(attachment);
        // Prefer a source that arrived concurrently with timeout/interruption.
        if let Err(error) = result
            && matches!(source.point_state(point), TimelinePoint::Unsubmitted)
        {
            return Err(error);
        }
    }
}

/// `DRM_IOCTL_SYNCOBJ_EVENTFD`: increments an eventfd when a point is ready.
///
/// Does not block. A point already in the requested state increments the
/// eventfd before this returns; otherwise the watch stays on the object and
/// fires from the next submit or signal. That is how smithay's
/// `DrmSyncPoint::generate_blocker` turns a timeline point into a `calloop`
/// source: it creates an eventfd, hands it to this ioctl, and polls it.
///
/// Only [`wait_flags::WAIT_AVAILABLE`] is accepted. That bit is `1 << 2`,
/// not `WAIT_FOR_SUBMIT` (`1 << 1`). The ioctl always waits for submit
/// implicitly — an unsubmitted point parks on the object rather than
/// failing — so the submit flag would be redundant and is `EINVAL`.
///
/// Gated on [`crate::device::DriverFeatures::SYNCOBJ_TIMELINE`], not just `SYNCOBJ`.
/// linux-6.6's `drm_syncobj_eventfd_ioctl` checks the timeline bit, and
/// smithay's `supports_syncobj_eventfd` probe (handle 0, device fd as a
/// placeholder eventfd) relies on the *next* check: a missing handle is
/// `ENOENT` before the fd is examined. Swapping those two would turn the
/// probe's `ENOENT` into `EBADF` / `EINVAL` and look like "this kernel
/// has no eventfd ioctl".
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1417>
/// (`drm_syncobj_eventfd_ioctl`).
pub fn syncobj_eventfd(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    require_timeline_syncobj(device)?;

    let args = payload_read::<SyncobjEventfd>(payload)?;
    if args.flags & !wait_flags::WAIT_AVAILABLE != 0 {
        return Err(DrmError::InvalidArgument);
    }
    if args.pad != 0 {
        return Err(DrmError::InvalidArgument);
    }

    // Handle before the eventfd, matching upstream's order. smithay probes
    // with handle 0 and the DRM node's own fd; `ENOENT` is the success
    // criterion (`std::io::ErrorKind::NotFound`).
    let object = file.syncobjs().get(SyncObjHandle(args.handle))?;

    let descriptors = caller.descriptors().ok_or(DrmError::NotSupported)?;
    let eventfd = descriptors.eventfd(args.fd)?;

    object.add_eventfd(
        args.point,
        args.flags & wait_flags::WAIT_AVAILABLE != 0,
        eventfd,
    );

    Ok(())
}

/// Resolves the handle array shared by the two wait ioctls.
fn wait_objects(
    file: &DrmFile,
    caller: &CallerContext,
    handles: u64,
    count: u32,
    flags: u32,
) -> DrmResult<Vec<Arc<SyncObj>>> {
    if flags & !wait_flags::KNOWN != 0 {
        return Err(DrmError::InvalidArgument);
    }
    // Upstream rejects a zero count, because a wait on nothing has no
    // meaningful answer to give in `first_signaled`.
    if count == 0 || count as usize > MAX_WAIT_HANDLES {
        return Err(DrmError::InvalidArgument);
    }

    let handles = copy_in_array::<u32>(caller, handles, count)?;

    file.syncobjs().get_many(&handles)
}

/// Resolves the handle array of `SYNCOBJ_RESET` and `SYNCOBJ_SIGNAL`.
fn array_objects(
    file: &DrmFile,
    caller: &CallerContext,
    payload: &[u8],
) -> DrmResult<Vec<Arc<SyncObj>>> {
    let args = payload_read::<SyncobjArray>(payload)?;
    if args.pad != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let handles = copy_in_array::<u32>(caller, args.handles, args.count_handles)?;

    // All resolved before any is touched, so that a bad handle in the middle
    // leaves nothing half-applied.
    file.syncobjs().get_many(&handles)
}

/// Resolves the parallel handle and point arrays of the timeline ioctls.
fn timeline_array(
    file: &DrmFile,
    caller: &CallerContext,
    args: &SyncobjTimelineArray,
) -> DrmResult<(Vec<Arc<SyncObj>>, Vec<u64>)> {
    let handles = copy_in_array::<u32>(caller, args.handles, args.count_handles)?;
    let points = copy_in_array::<u64>(caller, args.points, args.count_handles)?;

    Ok((file.syncobjs().get_many(&handles)?, points))
}

/// The wait behind `SYNCOBJ_WAIT` and `SYNCOBJ_TIMELINE_WAIT`.
///
/// Returns the index of the first handle that was satisfied.
///
/// # The retry loop
///
/// A handle may have no fence for the point asked about, and
/// `WAIT_FOR_SUBMIT` says to wait for one to appear rather than fail. There is
/// nothing to attach a fence waiter to in that state, so the wait happens in
/// two alternating phases: resolve every handle to a fence, and if any is
/// still missing, sleep on the objects' submission lists and resolve again.
/// Once nothing is missing, [`wait_many`] takes over and the loop ends.
///
/// The loop cannot spin: each iteration either makes progress (a fence
/// appeared) or blocks until something wakes it, and the deadline is checked
/// by the blocker on every sleep.
fn wait_syncobjs(
    caller: &CallerContext,
    objects: &[Arc<SyncObj>],
    points: &[u64],
    flags: u32,
    deadline: Deadline,
) -> DrmResult<usize> {
    if points.len() != objects.len() {
        return Err(DrmError::InvalidArgument);
    }

    let wait_all = flags & wait_flags::WAIT_ALL != 0;
    let wait_for_submit = flags & wait_flags::WAIT_FOR_SUBMIT != 0;
    let wait_available = flags & wait_flags::WAIT_AVAILABLE != 0;

    // Every wait a process can reach is interruptible. An uninterruptible one
    // on a wedged GPU makes the process unkillable; see rule C5.
    let interruptible = true;
    let blocker = caller.blocker();

    loop {
        let resolved = resolve(objects, points, wait_available);

        if resolved.iter().all(Option::is_some) {
            let fences: Vec<Arc<dyn Fence>> = resolved.into_iter().flatten().collect();
            let Some(blocker) = blocker else {
                // No way to sleep, so the only honest answer is the current
                // state; see `UserMem::blocker`.
                return wait_many(&NoBlocker, &fences, wait_all, Deadline::Immediate, false);
            };
            return wait_many(blocker, &fences, wait_all, deadline, interruptible);
        }

        // Something is unsubmitted. Without the flag that is an error, exactly
        // as `drm_syncobj_find_fence` reports for an object with no fence.
        if !wait_for_submit {
            return Err(DrmError::InvalidArgument);
        }

        // A partial resolution can still satisfy a wait for *any* handle.
        if let Some(index) = satisfied_index(&resolved, wait_all) {
            return Ok(index);
        }

        let Some(blocker) = blocker else {
            return Err(DrmError::TimeExpired);
        };
        if deadline == Deadline::Immediate {
            return Err(DrmError::TimeExpired);
        }

        // One notification is enough: the loop re-resolves from scratch, so it
        // does not matter which object woke it.
        let waiter = FenceWaiter::new(1);
        let pending: Vec<(Arc<SyncObj>, u64)> = objects
            .iter()
            .zip(points.iter())
            .zip(resolved.iter())
            .filter(|(_, fence)| fence.is_none())
            .map(|((object, point), _)| (object.clone(), *point))
            .collect();
        // A resolved fence signalling has to wake an `any` wait too, so both
        // lists are attached. Extra wakeups only cost a re-resolve.
        let ready: Vec<Arc<dyn Fence>> = resolved.into_iter().flatten().collect();

        let submits = SubmitAttachment::new(waiter.clone(), &pending);
        let attachment = Attachment::new(waiter.clone(), &ready);

        let result = blocker.block(
            waiter.queue(),
            &mut || waiter.is_ready(false),
            deadline,
            interruptible,
        );

        drop(attachment);
        drop(submits);

        if let Err(error) = result {
            // One last look: something may have arrived in the same instant
            // the deadline did, and reporting a timeout for work that is
            // demonstrably done sends user space hunting a hang that is not
            // there.
            let resolved = resolve(objects, points, wait_available);
            if let Some(index) = satisfied_index(&resolved, wait_all) {
                return Ok(index);
            }
            return Err(error);
        }
    }
}

/// Resolves each handle and point to the fence a wait on it must observe.
///
/// `None` means nothing has been submitted that would reach the point.
///
/// A point the timeline has already passed resolves to a fresh, already
/// signalled fence rather than to `None`: the wait on it is satisfied, and
/// giving [`wait_many`] a signalled fence says exactly that without a second
/// code path. `WAIT_AVAILABLE` resolves every submitted point the same way,
/// since it asks only whether a fence exists.
fn resolve(
    objects: &[Arc<SyncObj>],
    points: &[u64],
    wait_available: bool,
) -> Vec<Option<Arc<dyn Fence>>> {
    objects
        .iter()
        .zip(points.iter())
        .map(|(object, point)| match object.point_state(*point) {
            TimelinePoint::Passed => Some(SoftwareFence::new_signaled() as Arc<dyn Fence>),
            TimelinePoint::Pending(_) if wait_available => {
                Some(SoftwareFence::new_signaled() as Arc<dyn Fence>)
            }
            TimelinePoint::Pending(fence) => Some(fence),
            TimelinePoint::Unsubmitted => None,
        })
        .collect()
}

/// Returns the index of the first satisfied entry, if the set is satisfied.
///
/// The same rule [`wait_many`] applies, one level up: an unresolved entry
/// counts as unsignalled, so a `wait_all` over a set that is still missing a
/// fence is never satisfied.
fn satisfied_index(resolved: &[Option<Arc<dyn Fence>>], wait_all: bool) -> Option<usize> {
    let signaled =
        |fence: &Option<Arc<dyn Fence>>| fence.as_ref().is_some_and(|fence| fence.is_signaled());

    if wait_all && !resolved.iter().all(signaled) {
        return None;
    }

    resolved.iter().position(signaled)
}

/// A [`Blocker`] that refuses to sleep.
///
/// Stands in when the caller supplied none, so that a wait degrades to a poll
/// instead of failing outright. Reached only through [`Deadline::Immediate`],
/// where [`wait_many`] answers from the fences' own state and never calls it.
struct NoBlocker;

impl Blocker for NoBlocker {
    fn block(
        &self,
        _queue: &ostd::sync::WaitQueue,
        _ready: &mut dyn FnMut() -> bool,
        _deadline: Deadline,
        _interruptible: bool,
    ) -> DrmResult<()> {
        Err(DrmError::TimeExpired)
    }
}

/// Refuses an ioctl on a driver that does not implement sync objects.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1284>
/// (`drm_core_check_feature(dev, DRIVER_SYNCOBJ)`).
fn require_syncobj(device: &DrmDevice) -> DrmResult<()> {
    if !device.driver().features().contains(DriverFeatures::SYNCOBJ) {
        return Err(DrmError::NotSupported);
    }
    Ok(())
}

/// Refuses an ioctl on a driver without timeline sync objects.
///
/// The timeline ioctls need both bits: a driver claiming
/// `DRIVER_SYNCOBJ_TIMELINE` without `DRIVER_SYNCOBJ` would be nonsense, and
/// upstream checks them separately for exactly the ioctls that need each.
fn require_timeline_syncobj(device: &DrmDevice) -> DrmResult<()> {
    require_syncobj(device)?;

    if !device
        .driver()
        .features()
        .contains(DriverFeatures::SYNCOBJ_TIMELINE)
    {
        return Err(DrmError::NotSupported);
    }
    Ok(())
}

#[cfg(ktest)]
mod regression {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use ostd::prelude::ktest;

    use super::*;
    struct NoMemory;
    impl crate::UserMem for NoMemory {
        fn read_bytes(&self, _: u64, _: &mut [u8]) -> DrmResult<()> {
            Err(DrmError::BadAddress)
        }
        fn write_bytes(&self, _: u64, _: &[u8]) -> DrmResult<()> {
            Err(DrmError::BadAddress)
        }
    }
    struct SubmissionBlocker {
        object: Arc<SyncObj>,
        complete: bool,
        calls: AtomicUsize,
    }
    impl Blocker for SubmissionBlocker {
        fn block(
            &self,
            _: &ostd::sync::WaitQueue,
            ready: &mut dyn FnMut() -> bool,
            deadline: Deadline,
            interruptible: bool,
        ) -> DrmResult<()> {
            assert_eq!(self.calls.fetch_add(1, Ordering::Relaxed), 0);
            assert_eq!(deadline, Deadline::At(10_000));
            assert!(interruptible);
            assert!(
                !ready(),
                "an older timeline point pre-woke a future-point wait"
            );
            if self.complete {
                self.object.signal_point(2).unwrap();
                assert!(ready());
                Ok(())
            } else {
                Err(DrmError::TimeExpired)
            }
        }
    }
    #[ktest]
    fn future_point_wait_blocks_then_completes_or_expires_once() {
        for complete in [false, true] {
            let object = SyncObj::new(false);
            object.signal_point(1).unwrap();
            let blocker = SubmissionBlocker {
                object: object.clone(),
                complete,
                calls: AtomicUsize::new(0),
            };
            let caller = CallerContext::new(&NoMemory).with_blocker(&blocker);
            let result = wait_syncobjs(
                &caller,
                &[object.clone()],
                &[2],
                wait_flags::WAIT_ALL | wait_flags::WAIT_FOR_SUBMIT,
                Deadline::At(10_000),
            );
            assert_eq!(
                result,
                if complete {
                    Ok(0)
                } else {
                    Err(DrmError::TimeExpired)
                }
            );
            assert_eq!(blocker.calls.load(Ordering::Relaxed), 1);
            assert_eq!(object.submit_waiter_count(), 0);
        }
    }
}
