// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: validated submission batches and fence publication.

//! Individual units of GPU work.
//!
//! Ports `pvr_job.c` and `pvr_job.h` (944 lines).
//!
//! # Why a batch is prepared before any of it is committed
//!
//! `DRM_IOCTL_PVR_SUBMIT_JOBS` takes an array of jobs, and the whole array is
//! accepted or refused together. That is not a convenience: jobs in a batch may
//! depend on each other, and a client that learns "jobs 0 and 1 were queued, job
//! 2 was not" has no way to recover — it cannot un-queue the first two and it
//! cannot renumber the sync objects it has already been told about.
//!
//! [`prepare_batch`] validates streams and handles, creates fences, records
//! dependencies, and pairs geometry/fragment jobs before queue publication.
//! [`commit_batch`] then queues prepared jobs. If a device failure interrupts
//! commitment, every affected fence must be completed with an error; accepted
//! work must not be left pending without an owner.
//!
//! A failure in the first phase leaves nothing behind. The fences created so far
//! have never been shown to user space, so dropping them is not a fence-completion contract
//! violation — nobody can be waiting on a fence they were never given. That is
//! the same argument upstream makes in `pvr_queue_job_arm()`'s documentation.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::mem::offset_of;

use k1_musebook_drm::fence::{Fence, SyncObjTable};

use crate::{
    error::{PvrError, PvrResult},
    render::hwrt::{HwrtData, HwrtDataset},
    rogue::fwif::{
        FwAddr, RogueFwifCmdCompute, RogueFwifCmdFrag, RogueFwifCmdGeom, RogueFwifCmdTransfer,
        RogueFwifUfo,
    },
    sched::{
        abi,
        context::Context,
        stream::{self, StreamType},
    },
    sync::{self, SignalArray},
    uapi,
};

/// What kind of work a job is, from `enum drm_pvr_job_type`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobType {
    /// Geometry processing.
    Geometry = 0,
    /// Fragment shading.
    Fragment = 1,
    /// Compute.
    Compute = 2,
    /// Transfer/blit.
    Transfer = 3,
}

impl JobType {
    /// Reads a `drm_pvr_job_type` discriminant.
    pub fn from_uapi(value: u32) -> PvrResult<Self> {
        match value {
            0 => Ok(Self::Geometry),
            1 => Ok(Self::Fragment),
            2 => Ok(Self::Compute),
            3 => Ok(Self::Transfer),
            _ => Err(PvrError::InvalidArgument),
        }
    }

    /// The command stream definition table this job type's stream is checked
    /// against.
    fn stream_type(self) -> StreamType {
        match self {
            Self::Geometry => StreamType::Geometry,
            Self::Fragment => StreamType::Fragment,
            Self::Compute => StreamType::Compute,
            Self::Transfer => StreamType::Transfer,
        }
    }

    /// Whether this job type addresses a hardware render target.
    ///
    /// Ports the `args->type != GEOMETRY && args->type != FRAGMENT` test in
    /// upstream `create_job()`: only these two may name an HWRT dataset, and both
    /// of them must.
    pub fn uses_hwrt(self) -> bool {
        matches!(self, Self::Geometry | Self::Fragment)
    }
}

/// Something that must happen before a job may run.
///
/// Split by *who does the waiting*, which is the only distinction that changes
/// the code path:
///
/// - a **native** dependency is another job on one of this driver's queues, so it
///   is a value in a firmware UFO and the firmware can wait for it. The job is
///   submitted immediately with a `FENCE_PR` command in front of it.
/// - a **foreign** dependency is anything else — a sync object a client signals
///   from the CPU, a fence imported over dma-buf — and only the CPU can observe
///   it, so the job waits in the queue.
///
/// Native vs foreign is decided when the wait is resolved — either because the
/// producer is an earlier job in the same batch ([`Job::native_dep_on`]), or
/// because [`crate::sync::dependencies`] recognised a [`crate::sched::queue::JobFence`]
/// on this device via [`Fence::as_any`].
#[derive(Clone)]
pub enum Dep {
    /// Another job on one of this driver's queues.
    Native {
        /// The fence, so that a failure can be observed — error-propagation contract.
        fence: Arc<dyn Fence>,
        /// Firmware address of the queue's timeline UFO.
        ufo_addr: FwAddr,
        /// The value the UFO must reach.
        ufo_value: u32,
        /// Keeps the producer's timeline mapped until this native wait is finished.
        ufo_owner: Arc<crate::sched::queue::TimelineUfo>,
    },
    /// A fence only the CPU can observe.
    Foreign(
        /// The fence.
        Arc<dyn Fence>,
    ),
}

/// The render target a geometry or fragment job draws into.
///
/// Both halves are held for the job's whole life, and for different reasons:
/// the [`HwrtData`] is what the command and the kick name, while the
/// [`HwrtDataset`] owns the state the two data structures share and the free
/// lists behind them. Upstream keeps one reference — `pvr_hwrt_data_get()` takes
/// it on the *dataset* — and reaches the data through a back pointer.
#[derive(Clone)]
pub struct HwrtRef {
    /// The dataset, held so that the shared state outlives the job.
    pub dataset: Arc<HwrtDataset>,
    /// The one of its render targets this job names.
    pub data: Arc<HwrtData>,
}

/// One job, after validation and before submission.
pub struct Job {
    /// The context it runs on.
    pub context: Arc<Context>,
    /// What kind of work it is.
    pub r#type: JobType,
    /// The validated command stream, ready to copy into the CCCB.
    pub command: Vec<u8>,
    /// The render target, for a geometry or fragment job.
    ///
    /// `None` for compute and transfer, which
    /// [`JobType::uses_hwrt`] says do not address one; those two are refused at
    /// creation if they name one anyway.
    pub(crate) hwrt: Option<HwrtRef>,
    /// Which `ROGUE_FWIF_CCB_CMD_TYPE_*` the command is.
    ///
    /// Not derivable from [`Self::r#type`]: a fragment job is either `FRAG` or
    /// `FRAG_PR` depending on whether the client asked for a partial render.
    pub(crate) command_type: u32,
    /// The fence this job signals when the firmware has run it.
    pub(crate) fence: Arc<crate::sched::queue::JobFence>,
    /// The fences it waits on.
    pub(crate) deps: Vec<Dep>,
    /// Job number, for the CCB header's job references and for logs.
    pub(crate) id: u32,
    /// The fragment job this geometry job is kicked together with.
    ///
    /// Upstream `pvr_job::paired_job`, which points both ways; here only the
    /// geometry job holds the link, because the fragment job travels inside it
    /// until [`crate::sched::queue::Queue::push`] hands each half to its own
    /// queue. Set by [`link_geom_frag`] and by nothing else.
    pub(crate) paired_frag: Option<Box<Job>>,
}

impl Job {
    /// The fence a caller waits on, or attaches to a sync object.
    pub fn fence(&self) -> Arc<dyn Fence> {
        self.fence.clone()
    }

    /// Every fence this job is answerable for, including a paired fragment's.
    ///
    /// The fence-completion contract covers both halves of a pair: the fragment job's fence may already
    /// be in a later job's dependency list, and in the batch's
    /// [`SignalArray`], even though the job itself travels inside the geometry
    /// job.
    pub(crate) fn fences(&self) -> Vec<Arc<crate::sched::queue::JobFence>> {
        let mut fences = vec![self.fence.clone()];
        if let Some(frag) = &self.paired_frag {
            fences.push(frag.fence.clone());
        }
        fences
    }

    /// Takes the fragment half out, for the queue that is about to own both.
    pub(crate) fn take_paired_frag(&mut self) -> Option<Box<Job>> {
        self.paired_frag.take()
    }

    /// Records that this job must wait for `dep`.
    ///
    /// Goes through [`k1_musebook_drm::fence::FenceState::add_dependency`], so the
    /// dependency graph is walked and a cycle is refused with
    /// [`k1_musebook_drm::DrmError::WouldDeadlock`] (`EDEADLK`) — fence dependency-cycle check — rather
    /// than deadlocking at submission time.
    ///
    /// An already-signalled dependency is dropped by `add_dependency` and is also
    /// dropped here, so that the job does not carry a UFO wait the firmware would
    /// satisfy immediately. A signalled-with-*error* dependency is kept, because
    /// [`crate::sched::queue`] must observe it and fail the dependent job.
    pub fn add_dependency(&mut self, dep: Dep) -> PvrResult<()> {
        let fence = match &dep {
            Dep::Native { fence, .. } | Dep::Foreign(fence) => fence.clone(),
        };

        self.fence
            .state()
            .add_dependency(&fence)
            .map_err(map_dependency_error)?;

        if fence.is_signaled() && fence.error().is_none() {
            return Ok(());
        }
        self.deps.push(dep);
        Ok(())
    }

    /// Builds a native dependency on a job produced by this driver.
    ///
    /// Used by resolved sync operations and paired geometry/fragment submissions.
    pub fn native_dep_on(other: &Job) -> PvrResult<Dep> {
        let queue = other
            .context
            .queue_for_job(other.r#type)
            .ok_or(PvrError::InvalidArgument)?;
        Ok(Dep::Native {
            fence: other.fence.clone(),
            ufo_addr: queue.timeline_fw_addr(),
            ufo_value: other.fence.seqno() as u32,
            ufo_owner: queue.timeline_owner(),
        })
    }
}

/// Maps a fence-layer error onto this driver's.
///
/// Divergence: a dependency cycle is rejected, but PvrError currently has no
/// WouldDeadlock variant, so the user sees EINVAL rather than EDEADLK. Keep
/// this limitation explicit until the shared error mapping gains that variant.
fn map_dependency_error(error: k1_musebook_drm::DrmError) -> PvrError {
    match error {
        k1_musebook_drm::DrmError::WouldDeadlock => {
            ostd::warn!("refusing a job batch whose dependencies form a cycle");
            PvrError::InvalidArgument
        }
        k1_musebook_drm::DrmError::NoSpace => PvrError::TooBig,
        _ => PvrError::InvalidArgument,
    }
}

/// Validates and prepares a batch of jobs.
///
/// Batches matter: `DRM_IOCTL_PVR_SUBMIT_JOBS` takes an array, jobs within it may
/// depend on each other, and the whole batch must be accepted or rejected
/// together. Preparing every job before committing any is what makes that
/// possible.
///
/// `raw` is the caller's array image, exactly `array.count * array.stride` bytes;
/// `streams` holds one command-stream image per job, already copied in, because
/// this function must not touch user space — the stride negotiation and the
/// copying are the ioctl handler's job and go through
/// [`crate::uapi::obj_array`], the only implementation of that rule.
///
/// `sync_ops` holds one already-copied sync-operation array image per job, for
/// the same reason `streams` does.
///
/// Ports upstream `pvr_job_data_init()`, the `create_job()` it calls, and
/// `prepare_job_syncs_for_each()`. Returns the batch's [`SignalArray`] alongside
/// the jobs: nothing in it has been written to a sync object yet, and
/// [`SignalArray::push_fences`] must not be called until
/// [`commit_batch`] has succeeded.
pub fn prepare_batch(
    file: &crate::driver::PvrFile,
    syncobjs: &SyncObjTable,
    array: &uapi::ObjArray,
    raw: &[u8],
    streams: &[Vec<u8>],
    sync_ops: &[Vec<u8>],
) -> PvrResult<(Vec<Job>, SignalArray)> {
    // Upstream refuses an empty batch outright, in `pvr_submit_jobs()`.
    if array.count == 0 {
        return Err(PvrError::InvalidArgument);
    }
    let elements = uapi::obj_array(array, uapi::Job::MIN_STRIDE, size_of::<uapi::Job>(), raw)?;
    if streams.len() != array.count as usize || sync_ops.len() != array.count as usize {
        return Err(PvrError::InvalidArgument);
    }

    let mut jobs: Vec<Job> = Vec::with_capacity(array.count as usize);
    let mut signals = SignalArray::new();

    for (index, chunk) in elements
        .as_chunks::<{ size_of::<uapi::Job>() }>()
        .0
        .iter()
        .enumerate()
    {
        let args = <uapi::Job as ostd_pod::Pod>::from_bytes(chunk);

        // Every operation in the array is validated before any of it is acted
        // on; see `crate::sync`'s "all or nothing".
        let ops = sync::Ops::parse(&args.sync_ops, &sync_ops[index])?;

        // The three phases, in upstream's order. Phase 1 must run before phase 2
        // for *this* job, so that a job which signals and waits on one point
        // resolves the wait against what was there before; phase 3 must run
        // before phase 1 of the next job, so that the next job's wait sees this
        // job's fence.
        signals.collect(syncobjs, &ops)?;
        let waits = sync::dependencies(syncobjs, &signals, &ops, &file.device)?;

        let mut job = create_job(file, &args, &streams[index])?;
        add_waits(&mut job, &jobs, waits)?;
        signals.update(&ops, job.fence(), index)?;

        jobs.push(job);
    }

    // Last, and still before anything is committed: upstream calls
    // `pvr_jobs_link_geom_frag()` in exactly this position — after every
    // dependency is recorded and before the point its comment marks as "anything
    // after that point must succeed because we start exposing job finished
    // fences to the outside world".
    link_geom_frag(&mut jobs)?;

    Ok((jobs, signals))
}

/// Pairs each geometry job with the fragment job that immediately follows it.
///
/// Ports upstream `pvr_jobs_link_geom_frag()`. A pair goes to the firmware in one
/// `COMBINED_GEOM_FRAG_KICK`, which is what gives the firmware a fragment job to
/// run when the Parameter Manager runs out of memory — the whole point of the
/// exercise, and the only way a partial render can happen.
///
/// The fragment job is moved *into* the geometry job rather than left beside it in
/// the batch. [`commit_batch`] pushes one job per element, and the two halves have
/// to reach [`crate::sched::queue::Queue::push`] together for it to be able to
/// place them in their two queues under one lock; see that module's FIFO ordering.
///
/// Upstream also gives the geometry job a dependency on the last fragment job
/// already queued, via `get_last_queued_job_scheduled_fence()`, so that the
/// fragment half of the kick lands in its ring behind every fragment job queued
/// before it. This port needs no fence for that: the fragment half is placed in
/// its own queue in submission order, so
/// [`crate::sched::queue::Queue::try_submit_head`] can ask the question directly
/// — see that module's FIFO ordering and the pair's own submission path.
fn link_geom_frag(jobs: &mut Vec<Job>) -> PvrResult<()> {
    let mut index = 0;
    while index + 1 < jobs.len() {
        if !can_combine_jobs(&jobs[index], &jobs[index + 1]) {
            index += 1;
            continue;
        }
        let frag = jobs.remove(index + 1);
        set_partial_render_fence(&mut jobs[index])?;
        jobs[index].paired_frag = Some(Box::new(frag));
        // Upstream's extra `i++`: the fragment job just paired must not become
        // the geometry candidate of the next pair. Taking it out of the array has
        // already brought the element after it to `index + 1`, so one step does
        // both.
        index += 1;
    }
    Ok(())
}

/// Whether `a` and `b` may be given to the firmware in one kick.
///
/// Ports upstream `can_combine_jobs()`: a geometry job immediately followed by a
/// fragment job, queued to the same context, targeting the same HWRT, with the
/// fragment job explicitly waiting for the geometry job.
///
/// That last condition is what keeps this a *client* decision rather than a
/// driver guess. Upstream asks `drm_sched_job_has_dependency()` about the geometry
/// job's `scheduled` fence; the same edge here is a [`Dep::Native`] on the
/// geometry job's fence, which [`add_waits`] recorded when it resolved the
/// fragment job's sync operations.
fn can_combine_jobs(a: &Job, b: &Job) -> bool {
    if a.r#type != JobType::Geometry || b.r#type != JobType::Fragment {
        return false;
    }
    if !Arc::ptr_eq(&a.context, &b.context) {
        return false;
    }

    // Upstream's `a->hwrt != b->hwrt`, a pointer comparison on `pvr_hwrt_data`.
    // Both sides are always `Some` here, because `finish_command` refuses a
    // render job without a target, so `None` on either side is treated as a
    // mismatch: refusing to pair is always safe, and pairing on a guess is not.
    let same_hwrt = match (&a.hwrt, &b.hwrt) {
        (Some(x), Some(y)) => Arc::ptr_eq(&x.data, &y.data),
        _ => false,
    };
    if !same_hwrt {
        return false;
    }

    let geometry: Arc<dyn Fence> = a.fence.clone();
    b.deps.iter().any(|dep| match dep {
        Dep::Native { fence, .. } => Arc::ptr_eq(fence, &geometry),
        Dep::Foreign(_) => false,
    })
}

/// Writes the UFO the firmware tests when it decides to start a partial render.
///
/// Ports the `job->type == GEOMETRY && job->paired_job` branch of upstream
/// `pvr_queue_submit_job_to_cccb()`: the geometry queue's timeline UFO, and this
/// job's own sequence number minus one — which is the value the timeline holds for
/// as long as this job is the one running.
///
/// Divergence in *where*, not in *what*. Upstream writes it at submission time
/// because that is where it first has the queue to hand; both inputs are known as
/// soon as the pair is made, and writing it here keeps every field this module
/// places into a command image in this module.
fn set_partial_render_fence(geom: &mut Job) -> PvrResult<()> {
    let (addr, value) = {
        let queue = geom
            .context
            .queue_for_job(JobType::Geometry)
            .ok_or(PvrError::InvalidArgument)?;
        (
            queue.timeline_fw_addr(),
            (geom.fence.seqno() as u32).wrapping_sub(1),
        )
    };

    let at = offset_of!(RogueFwifCmdGeom, partial_render_geom_frag_fence);
    write_u32(
        &mut geom.command,
        at + offset_of!(RogueFwifUfo, addr),
        addr.0,
    )?;
    write_u32(
        &mut geom.command,
        at + offset_of!(RogueFwifUfo, value),
        value,
    )
}

/// Records a job's resolved waits as dependencies.
///
/// Same-batch producers still go through [`Job::native_dep_on`]. Everything
/// else uses the UFO [`crate::sync::Wait::ufo`] already resolved — including
/// a fence from an earlier ioctl that [`crate::sync::dependencies`] recognised
/// as a [`crate::sched::queue::JobFence`] on this device.
fn add_waits(job: &mut Job, earlier: &[Job], waits: Vec<sync::Wait>) -> PvrResult<()> {
    for wait in waits {
        let dep = match wait.producer.and_then(|index| earlier.get(index)) {
            Some(producer) => Job::native_dep_on(producer)?,
            None => match wait.ufo {
                Some((ufo_addr, ufo_value, ufo_owner)) => Dep::Native {
                    fence: wait.fence,
                    ufo_addr,
                    ufo_value,
                    ufo_owner,
                },
                None => Dep::Foreign(wait.fence),
            },
        };
        job.add_dependency(dep)?;
    }
    Ok(())
}

/// Builds one job. Ports upstream `create_job()` and `pvr_job_fw_cmd_init()`.
fn create_job(
    file: &crate::driver::PvrFile,
    args: &uapi::Job,
    raw_stream: &[u8],
) -> PvrResult<Job> {
    let device = &file.device;
    device.check_alive()?;

    // Upstream `if (!args->cmd_stream || !args->cmd_stream_len)`.
    if args.cmd_stream == 0 || args.cmd_stream_len == 0 {
        return Err(PvrError::InvalidArgument);
    }
    if raw_stream.len() != args.cmd_stream_len as usize {
        return Err(PvrError::BadAddress);
    }

    let job_type = JobType::from_uapi(args.r#type)?;

    // Only geometry and fragment jobs address an HWRT dataset. A non-zero
    // reference on anything else is a client that has confused its job types.
    if !job_type.uses_hwrt() && (args.hwrt.set_handle != 0 || args.hwrt.data_index != 0) {
        return Err(PvrError::InvalidArgument);
    }

    let context = file
        .render_contexts
        .get(args.context_handle)
        .map_err(|_| PvrError::NoSuchHandle)?;
    if !context.r#type.accepts(job_type) {
        return Err(PvrError::InvalidArgument);
    }
    // Upstream's `atomic_read(&job->ctx->faulty)` check in
    // `pvr_queue_job_init()`.
    if context.is_faulty() {
        return Err(PvrError::DeviceLost);
    }

    // Upstream `pvr_hwrt_data_lookup()`, whose two failure modes — an unknown
    // set handle and an index past the end of the set — are one `-EINVAL`. The
    // "must have one" half is in `finish_command`, where upstream also puts it.
    let hwrt = if args.hwrt.set_handle != 0 {
        let dataset = file
            .hwrt_datasets
            .get(args.hwrt.set_handle)
            .map_err(|_| PvrError::NoSuchHandle)?;
        let data = dataset
            .data(args.hwrt.data_index)
            .ok_or(PvrError::InvalidArgument)?
            .clone();
        Some(HwrtRef { dataset, data })
    } else {
        None
    };

    let (command_type, fw_flags) = command_type_and_flags(job_type, args.flags as u64)?;

    let mut command = stream::validate(job_type.stream_type(), raw_stream, &device.info)?;
    finish_command(&mut command, job_type, fw_flags, hwrt.as_ref())?;

    let queue = context
        .queue_for_job(job_type)
        .ok_or(PvrError::InvalidArgument)?;

    // Upstream's `pvr_cccb_cmdseq_can_fit()` check, with the same "a fragment
    // job always waits on at least the geometry job" floor. A job that cannot
    // fit is `E2BIG`: no amount of waiting will make room.
    let min_deps = usize::from(job_type == JobType::Fragment);
    if !queue.job_can_fit(command.len(), min_deps)? {
        return Err(PvrError::TooBig);
    }

    let fence = queue.new_job_fence();
    Ok(Job {
        context,
        r#type: job_type,
        command,
        hwrt,
        command_type,
        fence,
        deps: Vec::new(),
        id: device.sched.next_job_id(),
        paired_frag: None,
    })
}

/// Checks a job's flags and picks the CCB command type they imply.
///
/// Ports the four `convert_*_flags()` functions and the `args->flags &
/// ~*_FLAGS_MASK` test each of their callers makes. An unknown flag bit is
/// refused: it is a newer client asking for behaviour this driver does not
/// implement, and guessing would be the `CONVENTIONS.md` command-stream contract mistake applied
/// to the argument structure instead of the command stream.
///
/// The converted firmware flags are returned alongside so that
/// [`finish_command`] can write them; the translation is not the identity, the
/// two sets of bits are unrelated numbering.
fn command_type_and_flags(job_type: JobType, flags: u64) -> PvrResult<(u32, u32)> {
    use crate::uapi::{ComputeCmdFlags, FragCmdFlags, GeomCmdFlags, TransferCmdFlags};

    match job_type {
        JobType::Geometry => {
            if flags & !GeomCmdFlags::FLAGS_MASK != 0 {
                return Err(PvrError::InvalidArgument);
            }
            let mut out = 0;
            if flags & GeomCmdFlags::FIRST.bits() != 0 {
                out |= fw_flags::GEOM_FIRSTKICK;
            }
            if flags & GeomCmdFlags::LAST.bits() != 0 {
                out |= fw_flags::GEOM_LASTKICK;
            }
            if flags & GeomCmdFlags::SINGLE_CORE.bits() != 0 {
                out |= fw_flags::GEOM_SINGLE_CORE;
            }
            Ok((abi::CCB_CMD_TYPE_GEOM, out))
        }
        JobType::Fragment => {
            if flags & !FragCmdFlags::FLAGS_MASK != 0 {
                return Err(PvrError::InvalidArgument);
            }
            let mut out = 0;
            if flags & FragCmdFlags::SINGLE_CORE.bits() != 0 {
                out |= fw_flags::FRAG_SINGLE_CORE;
            }
            if flags & FragCmdFlags::DEPTHBUFFER.bits() != 0 {
                out |= fw_flags::FRAG_DEPTHBUFFER;
            }
            if flags & FragCmdFlags::STENCILBUFFER.bits() != 0 {
                out |= fw_flags::FRAG_STENCILBUFFER;
            }
            if flags & FragCmdFlags::PREVENT_CDM_OVERLAP.bits() != 0 {
                out |= fw_flags::FRAG_PREVENT_CDM_OVERLAP;
            }
            if flags & FragCmdFlags::SCRATCHBUFFER.bits() != 0 {
                out |= fw_flags::FRAG_SCRATCHBUFFER;
            }
            if flags & FragCmdFlags::GET_VIS_RESULTS.bits() != 0 {
                out |= fw_flags::FRAG_GET_VIS_RESULTS;
            }
            if flags & FragCmdFlags::DISABLE_PIXELMERGE.bits() != 0 {
                out |= fw_flags::FRAG_DISABLE_PIXELMERGE;
            }
            // The one flag that selects a different command rather than a flag
            // bit, because a partial render is a different thing for the
            // firmware's scheduler to run.
            let command = if flags & FragCmdFlags::PARTIAL_RENDER.bits() != 0 {
                abi::CCB_CMD_TYPE_FRAG_PR
            } else {
                abi::CCB_CMD_TYPE_FRAG
            };
            Ok((command, out))
        }
        JobType::Compute => {
            if flags & !ComputeCmdFlags::FLAGS_MASK != 0 {
                return Err(PvrError::InvalidArgument);
            }
            let mut out = 0;
            if flags & ComputeCmdFlags::PREVENT_ALL_OVERLAP.bits() != 0 {
                out |= fw_flags::COMPUTE_PREVENT_ALL_OVERLAP;
            }
            if flags & ComputeCmdFlags::SINGLE_CORE.bits() != 0 {
                out |= fw_flags::COMPUTE_SINGLE_CORE;
            }
            Ok((abi::CCB_CMD_TYPE_CDM, out))
        }
        JobType::Transfer => {
            if flags & !TransferCmdFlags::FLAGS_MASK != 0 {
                return Err(PvrError::InvalidArgument);
            }
            let mut out = 0;
            if flags & TransferCmdFlags::SINGLE_CORE.bits() != 0 {
                out |= fw_flags::TRANSFER_SINGLE_CORE;
            }
            Ok((abi::CCB_CMD_TYPE_TQ_3D, out))
        }
    }
}

/// Writes the fields the command stream does not carry.
///
/// Ports the tail of each `pvr_*_job_fw_cmd_init()`: the frame number, the
/// converted flags and, for a render job, the HWRT data's firmware address. The
/// command is a byte image, so each field is placed with `offset_of!` on the
/// generated structure — the same mechanism [`stream::validate`] uses, and the
/// reason a layout change is a compile error rather than a firmware that renders
/// nothing.
fn finish_command(
    command: &mut [u8],
    job_type: JobType,
    flags: u32,
    hwrt: Option<&HwrtRef>,
) -> PvrResult<()> {
    let (frame_num_at, flags_at) = match job_type {
        JobType::Geometry => (
            offset_of!(RogueFwifCmdGeom, cmd_shared.cmn.frame_num),
            offset_of!(RogueFwifCmdGeom, flags),
        ),
        JobType::Fragment => (
            offset_of!(RogueFwifCmdFrag, cmd_shared.cmn.frame_num),
            offset_of!(RogueFwifCmdFrag, flags),
        ),
        JobType::Compute => (
            offset_of!(RogueFwifCmdCompute, common.frame_num),
            offset_of!(RogueFwifCmdCompute, flags),
        ),
        JobType::Transfer => (
            offset_of!(RogueFwifCmdTransfer, common.frame_num),
            offset_of!(RogueFwifCmdTransfer, flags),
        ),
    };

    // Upstream sets this to zero explicitly, having zeroed the whole structure
    // first. Written out for the same reason it does: a frame number is a thing
    // the firmware reports back, and "we never set it" is worth saying in code.
    write_u32(command, frame_num_at, 0)?;
    write_u32(command, flags_at, flags)?;

    if job_type.uses_hwrt() {
        // Upstream `pvr_geom_job_fw_cmd_init()` and `pvr_frag_job_fw_cmd_init()`
        // both open with `if (!job->hwrt) return -EINVAL`, and both then write
        // the render target's firmware address into the shared part of the
        // command. A render job without a target has nowhere to put its
        // geometry, so this is a client error rather than something to default.
        let hwrt = hwrt.ok_or(PvrError::InvalidArgument)?;
        let at = match job_type {
            JobType::Geometry => {
                offset_of!(RogueFwifCmdGeom, cmd_shared.hwrt_data_fw_addr)
            }
            JobType::Fragment => {
                offset_of!(RogueFwifCmdFrag, cmd_shared.hwrt_data_fw_addr)
            }
            // `uses_hwrt` is the only other reader of this distinction, so the
            // two lists cannot drift apart without this arm becoming reachable.
            JobType::Compute | JobType::Transfer => return Err(PvrError::InvalidArgument),
        };
        write_u32(command, at, hwrt.data.fw_addr().0)?;
    }

    Ok(())
}

/// Writes a `u32` into a command image, bounds-checked.
fn write_u32(command: &mut [u8], offset: usize, value: u32) -> PvrResult<()> {
    let end = offset.checked_add(4).ok_or(PvrError::InvalidArgument)?;
    if end > command.len() {
        return Err(PvrError::InvalidArgument);
    }
    command[offset..end].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// Commits a prepared batch.
///
/// Must not fail partway: by this point space is reserved and fences are created,
/// so anything that can fail has to have failed in [`prepare_batch`].
///
/// One element may be two jobs: a geometry job that [`link_geom_frag`] paired
/// carries its fragment job inside it, and [`crate::sched::queue::Queue::push`]
/// places both in their own queues under one lock.
///
/// "Must not" is not "cannot": a firmware that stops draining its KCCB can make
/// the push of job *n* fail after job *n-1* has gone. That is not partial
/// success, it is a lost device — the jobs already pushed will time out and their
/// fences will be failed by [`crate::sched::queue::Queue::check_timeout`], and the
/// ones not yet pushed are failed here. Either way every fence signals, which is
/// fence-completion contract, and the client sees one error.
pub fn commit_batch(jobs: Vec<Job>) -> PvrResult<()> {
    let mut result = Ok(());
    let mut remaining = Vec::new();

    for job in jobs {
        if result.is_err() {
            remaining.push(job);
            continue;
        }
        let Some(queue) = job.context.queue_for_job(job.r#type).cloned() else {
            result = Err(PvrError::InvalidArgument);
            remaining.push(job);
            continue;
        };
        // Both halves of a pair, because `push` takes the geometry job by value
        // and the fragment job travels inside it.
        let fences = job.fences();
        if let Err(err) = queue.push(job) {
            ostd::error!("a prepared job could not be queued: {err:?}");
            for fence in fences {
                fence.signal(Some(k1_musebook_drm::DrmError::Io));
            }
            result = Err(err);
        }
    }

    // The fence-completion contract for the jobs that never reached a queue: nobody outside the driver
    // has these fences yet — `SignalArray::push_fences` runs only after this
    // function has succeeded — but a later job in the batch may already depend
    // on one, so they have to be signalled rather than dropped.
    for job in remaining {
        for fence in job.fences() {
            fence.signal(Some(k1_musebook_drm::DrmError::Io));
        }
    }

    result
}

/// Every job command is a whole number of firmware allocation units long.
///
/// `Cccb::write_command` refuses a length that is not a multiple of eight, which
/// upstream's `pvr_cccb_get_size_of_cmd_with_hdr()` only `WARN_ON`s. A command
/// structure that grew an odd tail would therefore turn every submission of that
/// job type into `EINVAL` at runtime; asserting it here makes that a build
/// failure in the one place a regeneration could cause it.
const _: () = {
    assert!(size_of::<RogueFwifCmdGeom>().is_multiple_of(8));
    assert!(size_of::<RogueFwifCmdFrag>().is_multiple_of(8));
    assert!(size_of::<RogueFwifCmdCompute>().is_multiple_of(8));
    assert!(size_of::<RogueFwifCmdTransfer>().is_multiple_of(8));
};

/// Firmware command flags, upstream `pvr_rogue_fwif_client.h`.
///
/// The output side of the four `convert_*_flags()` functions. Transcribed here
/// rather than in [`crate::sched::abi`] because they belong to the client
/// interface rather than to the kernel/firmware one, which is the same split
/// upstream makes between `pvr_rogue_fwif_client.h` and `pvr_rogue_fwif.h`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif_client.h>.
mod fw_flags {
    /// Upstream `ROGUE_GEOM_FLAGS_FIRSTKICK`.
    pub(super) const GEOM_FIRSTKICK: u32 = 1 << 0;
    /// Upstream `ROGUE_GEOM_FLAGS_LASTKICK`.
    pub(super) const GEOM_LASTKICK: u32 = 1 << 1;
    /// Upstream `ROGUE_GEOM_FLAGS_SINGLE_CORE`.
    pub(super) const GEOM_SINGLE_CORE: u32 = 1 << 3;

    /// Upstream `ROGUE_FRAG_FLAGS_SINGLE_CORE`.
    pub(super) const FRAG_SINGLE_CORE: u32 = 1 << 3;
    /// Upstream `ROGUE_FRAG_FLAGS_DEPTHBUFFER`.
    pub(super) const FRAG_DEPTHBUFFER: u32 = 1 << 7;
    /// Upstream `ROGUE_FRAG_FLAGS_STENCILBUFFER`.
    pub(super) const FRAG_STENCILBUFFER: u32 = 1 << 8;
    /// Upstream `ROGUE_FRAG_FLAGS_PREVENT_CDM_OVERLAP`.
    pub(super) const FRAG_PREVENT_CDM_OVERLAP: u32 = 1 << 26;
    /// Upstream `ROGUE_FRAG_FLAGS_SCRATCHBUFFER`.
    pub(super) const FRAG_SCRATCHBUFFER: u32 = 1 << 19;
    /// Upstream `ROGUE_FRAG_FLAGS_GET_VIS_RESULTS`.
    pub(super) const FRAG_GET_VIS_RESULTS: u32 = 1 << 5;
    /// Upstream `ROGUE_FRAG_FLAGS_DISABLE_PIXELMERGE`.
    pub(super) const FRAG_DISABLE_PIXELMERGE: u32 = 1 << 15;

    /// Upstream `ROGUE_COMPUTE_FLAG_PREVENT_ALL_OVERLAP`.
    pub(super) const COMPUTE_PREVENT_ALL_OVERLAP: u32 = 1 << 2;
    /// Upstream `ROGUE_COMPUTE_FLAG_SINGLE_CORE`.
    pub(super) const COMPUTE_SINGLE_CORE: u32 = 1 << 5;

    /// Upstream `ROGUE_TRANSFER_FLAGS_SINGLE_CORE`.
    pub(super) const TRANSFER_SINGLE_CORE: u32 = 1 << 1;
}
