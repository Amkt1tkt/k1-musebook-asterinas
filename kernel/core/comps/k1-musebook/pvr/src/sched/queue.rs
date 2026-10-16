// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: FIFO GPU submission, completion, and reset ownership.

//! Job queues and the scheduler.
//!
//! Ports `pvr_queue.c` and `pvr_queue.h` (1,681 lines).
//!
//! The scheduler supplies FIFO ordering, dependencies, completion and timeouts
//! over DRM fence primitives. It has no host priority preemption, load balancing
//! or reset-and-replay; context priority is consumed by the firmware.
//!
//! A geometry/fragment pair reaches firmware in one combined kick. Each half
//! keeps its FIFO position; geometry submits only when fragment has no older
//! unsubmitted work. Paired queues wake one another outside their locks.
//!
//! Native dependencies become firmware UFO waits; foreign dependencies stop the
//! queue until a fence callback resumes it. Failed dependencies fail the job.
//! Callback registration handles fences that signal during registration.
//!
//! A job owns its power reference until completion, timeout or explicit failure.
//! Fence signaling and destruction run outside the queue lock. The service thread
//! checks timeouts even without further client calls; a foreign dependency that
//! never signals has not reached the GPU and therefore has no execution timeout.
//!
//! # Lock order
//!
//! The order is firmware heap -> queue -> KCCB. Combined submission takes
//! geometry before fragment. Reserve a KCCB slot before taking queue locks,
//! and release one queue lock before waking another queue or signaling a fence.
//! Power-reference acquisition can sleep and precedes these locks; reference
//! release is deferred. Reset uses reset exclusion -> power transition -> heap
//! -> queue -> KCCB. MMU invalidation serializes before reset exclusion and never
//! runs while a VM or queue lock is held.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aster_softirq::BottomHalfDisabled;
use k1_musebook_drm::{
    DrmError,
    fence::{Fence, FenceCallback, FenceContext, FenceState, FenceStatus, FenceWaiter},
};
use ostd::sync::SpinLock;
use ostd_pod::{FromZeros, IntoBytes};

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwHeap, FwObject},
    mm::vm_mips::MipsPageTable,
    rogue::fwif::{
        FwAddr, RogueFwifComputeCtxState, RogueFwifFragCtxState, RogueFwifFwcommoncontext,
        RogueFwifGeomCtxState, RogueFwifKccbCmd, RogueFwifKccbCmdKickData, RogueFwifUfo,
    },
    sched::{
        abi,
        cccb::Cccb,
        context::{Context, ContextType, Priority},
        job::{Dep, HwrtRef, Job, JobType},
        now_ns,
    },
};

/// Maximum time a submitted head job may make no progress before timeout.
///
/// This port uses a longer deadline than upstream's 500 ms to accommodate long
/// rendering commands and firmware-side geometry/fragment dependencies. It is a
/// client-context deadline, separate from the KCCB progress watchdog in
/// [`crate::power::check_for_hang`]. Retirement restarts it for the next head;
/// waiting on an unsignalled producer is handled separately.
pub const JOB_TIMEOUT_NS: u64 = 4_000 * 1_000_000;

/// How long [`Queue::drain`] waits for the firmware to finish everything.
///
/// Long enough for a queue's worth of jobs to time out one after another, so that
/// a drain returns because the queue really is empty rather than because it gave
/// up first.
const DRAIN_TIMEOUT_NS: u64 = 8 * JOB_TIMEOUT_NS;

/// How often the polling loops re-check. Upstream's `usleep_range(1, 50)`.
const POLL_INTERVAL_US: u64 = 50;

/// Shared firmware timeline backing retained by native consumers after producer teardown.
pub struct TimelineUfo {
    object: Option<FwObject>,
    device: Weak<PvrDevice>,
    mapped: AtomicBool,
}

impl core::ops::Deref for TimelineUfo {
    type Target = FwObject;

    fn deref(&self) -> &Self::Target {
        self.object
            .as_ref()
            .expect("live timeline has firmware storage")
    }
}

impl Drop for TimelineUfo {
    fn drop(&mut self) {
        if let (Some(device), Some(object)) = (self.device.upgrade(), self.object.take()) {
            crate::service::retire_firmware_object(
                &device,
                object,
                self.mapped.load(Ordering::Relaxed),
            );
        }
    }
}

/// A fence backed by a queue's timeline UFO.
///
/// Ports upstream `struct pvr_queue_fence` initialised with
/// `pvr_queue_job_fence_ops`. The concrete type is what lets
/// [`crate::sync::dependencies`] recognise a fence read back out of a sync
/// object as native — upstream's `pvr_queue_fence_is_native()` compares
/// `dma_fence->ops` pointers for the same purpose.
///
/// The queue is held weakly so a sync object that keeps a completed job's
/// fence alive cannot pin the context. Upgrading fails only after the queue
/// is gone, at which point there is no UFO left to wait on and the wait
/// falls back to the CPU path.
#[derive(Debug)]
pub struct JobFence {
    state: FenceState,
    queue: Weak<Queue>,
}

impl JobFence {
    /// Signals the fence. See [`k1_musebook_drm::fence::SoftwareFence::signal`].
    pub fn signal(&self, error: Option<DrmError>) -> bool {
        self.state.signal(error)
    }

    /// Shared state, for [`FenceState::add_dependency`].
    pub fn state(&self) -> &FenceState {
        &self.state
    }

    /// Firmware UFO address and value for a native wait, if this fence still
    /// belongs to a live queue on `device`.
    ///
    /// Conditions match what [`crate::sched::job::Job::native_dep_on`] already
    /// requires of a same-batch producer, expressed for a fence that has left
    /// its job behind:
    ///
    /// 1. the concrete type is ours (upstream: `f->ops == &pvr_queue_job_fence_ops`);
    /// 2. its queue is still alive, so the timeline UFO still exists;
    /// 3. that queue belongs to `device` — the firmware can only wait on a UFO
    ///    it can see, and two [`PvrDevice`]s share this fence type.
    ///
    /// Reference: upstream `pvr_queue_fence_is_native()` and the UFO lookup in
    /// `pvr_queue_submit_job_to_cccb()` via `to_pvr_queue_job_fence()`.
    pub fn native_ufo(&self, device: &Arc<PvrDevice>) -> Option<(FwAddr, u32, Arc<TimelineUfo>)> {
        let queue = self.queue.upgrade()?;
        if !Arc::ptr_eq(&queue.device, device) {
            return None;
        }
        Some((
            queue.timeline_fw_addr(),
            self.state.seqno() as u32,
            queue.timeline.clone(),
        ))
    }
}

impl Fence for JobFence {
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

    fn add_callback(&self, callback: FenceCallback) -> k1_musebook_drm::DrmResult<()> {
        self.state.add_callback(callback)
    }

    fn is_signaled(&self) -> bool {
        self.state.is_signaled()
    }
}

/// How a job is tied to the other half of a combined geometry+fragment kick.
///
/// Ports upstream `pvr_job::paired_job`, which is one field pointing both ways.
/// Here the two directions carry different things, because the two halves are not
/// interchangeable: one queue does the submitting and the other is submitted.
enum Paired {
    /// This is a geometry job, and the named fragment job goes with it.
    Geom {
        /// The fragment queue, whose ring the fragment half is written into.
        queue: Arc<Queue>,
        /// Which job in it. A fence is a job's identity, and looking the job up
        /// by it rather than by index is what makes the pairing survive the
        /// fragment queue retiring and failing jobs in the meantime.
        fence: Arc<JobFence>,
    },
    /// This is a fragment job, and its own queue must not submit it.
    Frag {
        /// The geometry queue that will, so that it can be told when this
        /// queue's backlog has cleared. Weak, so that two queues of one context
        /// cannot keep each other alive.
        geom: Weak<Queue>,
    },
}

/// A job that has been accepted but not yet completed.
struct PendingJob {
    /// The fence this job signals. Also its identity: the seqno is the value the
    /// timeline UFO reaches when the firmware has run it.
    fence: Arc<JobFence>,
    /// Sequence number, as the firmware sees it.
    ///
    /// The fence's seqno truncated to 32 bits, because a UFO is a `u32`. The
    /// comparison in [`Queue::process`] is wrap-safe, so truncation costs
    /// nothing until a queue has run 2^32 jobs.
    ufo_value: u32,
    /// What must happen before the firmware may start it.
    deps: Vec<Dep>,
    /// The validated firmware command.
    command: Vec<u8>,
    /// Which `ROGUE_FWIF_CCB_CMD_TYPE_*` the command is.
    command_type: u32,
    /// The render target, for a geometry or fragment job.
    ///
    /// Held until the job retires because the `KICK` command names its cleanup
    /// state, and because releasing the dataset while the firmware is still
    /// rendering into it would free memory the GPU is writing.
    hwrt: Option<HwrtRef>,
    /// Job number, for the CCB header's job references and for logs.
    id: u32,
    /// When the job was written into the CCCB, or `None` if it has not been.
    submitted_at: Option<u64>,
    /// Which firmware boot the job was written under, or zero if unsubmitted.
    ///
    /// A queue outlives the firmware: the GPU powers down when the last
    /// reference goes and the autosuspend delay passes, and coming back up
    /// restarts the MIPS core. So a job can be handed to one firmware and waited
    /// on under the next. When a job never completes, this is what separates "a
    /// power cycle happened underneath it" from "the firmware now running looked
    /// at it and did not execute it" -- two failures with nothing in common but
    /// the symptom. See [`crate::fw::FwDevice::generation`].
    boot_generation: u32,
    /// The other half of a combined kick, if this job is part of one.
    paired: Option<Paired>,
    /// The runtime-PM reference that keeps the GPU up while this job may run.
    ///
    /// Upstream `pvr_job::has_pm_ref`. Held by the job rather than by the ioctl
    /// because the job is still on the GPU long after `SUBMIT_JOBS` has
    /// returned; released by dropping this structure, which is the one thing
    /// every path out of a job does. The fence-completion paths release it.
    _power: crate::power::JobPowerRef,
    /// Stop generation of the firmware to which this job was actually published.
    stop_epoch: u64,
}

impl PendingJob {
    /// Hands a validated job to the queue that now owns it.
    fn new(
        job: Job,
        power: crate::power::JobPowerRef,
        paired: Option<Paired>,
        stop_epoch: u64,
    ) -> Self {
        let ufo_value = job.fence.seqno() as u32;
        Self {
            fence: job.fence,
            ufo_value,
            deps: job.deps,
            command: job.command,
            command_type: job.command_type,
            hwrt: job.hwrt,
            id: job.id,
            submitted_at: None,
            boot_generation: 0,
            paired,
            _power: power,
            stop_epoch,
        }
    }

    fn finish(mut self, device: &PvrDevice, error: Option<DrmError>) {
        let after_stop =
            (error.is_some() && self.submitted_at.is_some()).then_some(self.stop_epoch);
        if let Some(reference) = self.hwrt.take() {
            crate::service::retire_hwrt(device, reference, after_stop);
        }
        for dep in core::mem::take(&mut self.deps) {
            if let Dep::Native { ufo_owner, .. } = dep {
                crate::service::retire_ufo(device, ufo_owner, after_stop);
            }
        }
        self.fence.signal(error);
    }
}

/// A queue's mutable state.
struct QueueInner {
    /// The client command buffer this queue feeds.
    cccb: Cccb,
    /// Jobs in submission order; submission ordering.
    pending: VecDeque<PendingJob>,
    /// Set once the context is going away, so nothing new may be queued.
    killed: bool,
    /// Set while a GPU reset is running, so nothing reaches the firmware.
    ///
    /// Upstream's `drm_sched_stop()`, which parks the scheduler's worker for the
    /// same window. Jobs may still be *pushed* — they simply do not advance —
    /// exactly as they may be pushed onto a stopped `drm_sched_entity`.
    stopped: bool,
    /// Other queues this one's last operation may have unblocked.
    ///
    /// Only ever the other half of a pair. Recorded while a lock is held and
    /// acted on by [`Queue::wake_paired`] once it is not, because waking a queue
    /// means taking that queue's lock — the lock order.
    ///
    /// Weak, so that two queues of one context cannot end up holding each other
    /// through their wake lists and outliving the context that owns them.
    wake: Vec<Weak<Queue>>,
    /// Firmware address of the timeline UFO.
    ///
    /// Duplicated from [`Queue::timeline`] so that [`write_job_sequence`], which
    /// already holds this lock, can name it without a second borrow of the queue.
    timeline_fw_addr: u32,
}

/// A per-context, per-job-type queue.
pub struct Queue {
    /// The device the queue submits to.
    device: Arc<PvrDevice>,
    /// What kind of job this queue takes.
    job_type: JobType,
    /// Everything that changes, under one lock. Follow the lock order.
    inner: SpinLock<QueueInner, BottomHalfDisabled>,
    /// The timeline UFO the firmware advances as jobs complete.
    ///
    /// Upstream `pvr_queue::timeline_ufo`. One `u32`: the seqno of the last job
    /// the firmware finished.
    timeline: Arc<TimelineUfo>,
    /// The register state the firmware saves a context switch into.
    ///
    /// Upstream `pvr_queue::reg_state_obj`.
    reg_state: FwObject,
    /// The fence context every job fence on this queue belongs to.
    ///
    /// One per queue, so that `(context, seqno)` is unique and ordered — fence
    /// fence-identity contract — and so that a waiter can tell two of our fences apart from a
    /// third party's.
    fence_context: FenceContext,
    /// Next seqno to hand out. Starts at one: zero is the UFO's initial value
    /// and must not name a job, or an unstarted queue would look as though its
    /// first job had already completed.
    next_seqno: AtomicU64,
    /// Firmware address of this queue's slice of the context structure.
    ctx_fw_addr: FwAddr,
    /// Offset of that slice within the context structure, for cleanup.
    ctx_offset: u32,
    /// The owning context's fault flag, shared rather than back-referenced.
    ///
    /// A `Weak<Context>` would do, but the only thing the queue needs from its
    /// context is this bit, and sharing the bit keeps the two objects free of a
    /// reference cycle in either direction.
    faulty: Arc<AtomicBool>,
    /// The context id the firmware reports in a reset notification.
    context_id: u32,
}

/// What [`Queue::new`] needs to know about its context.
///
/// A parameter object because the context does not exist yet when its queues are
/// built — upstream has the same ordering problem and solves it by writing into a
/// scratch copy of the firmware structure.
pub struct QueueParams<'a> {
    /// The device.
    pub device: Arc<PvrDevice>,
    /// What kind of jobs the queue takes.
    pub job_type: JobType,
    /// The owning context's type, which decides whether `callstack_addr` is legal.
    pub context_type: ContextType,
    /// The owning context's priority, which the firmware scheduler reads.
    pub priority: Priority,
    /// The owning context's id.
    pub context_id: u32,
    /// The owning context's fault flag.
    pub faulty: Arc<AtomicBool>,
    /// Firmware address of the context structure plus this queue's offset.
    pub ctx_fw_addr: FwAddr,
    /// Offset of this queue's slice within the context structure.
    pub ctx_offset: u32,
    /// Firmware address of the VM context's firmware memory context.
    pub fw_mem_context: FwAddr,
    /// Initial VDM call stack pointer, from `CREATE_CONTEXT`.
    pub callstack_addr: u64,
    /// The firmware heap to allocate out of.
    pub heap: &'a mut FwHeap,
}

impl Queue {
    /// Creates a queue.
    ///
    /// Ports upstream `pvr_queue_create()`, minus the two `drm_sched` objects it
    /// has no equivalent of.
    pub fn new(params: QueueParams<'_>) -> PvrResult<Arc<Self>> {
        // Upstream's check: only a render context has a call stack, because only
        // the VDM has one to restore.
        if params.callstack_addr != 0
            && !matches!(params.job_type, JobType::Geometry | JobType::Fragment)
        {
            return Err(PvrError::InvalidArgument);
        }
        if !params.context_type.accepts(params.job_type) {
            return Err(PvrError::InvalidArgument);
        }

        let ctx_state_size = ctx_state_size(&params.device, params.job_type)?;
        let mut reg_state = params
            .heap
            .alloc_guard(ctx_state_size, FwCache::DEVICE_UNCACHED)?;
        init_reg_state(reg_state.object(), params.job_type, params.callstack_addr)?;

        let mut timeline = reg_state
            .parts()
            .1
            .alloc_guard(size_of::<u32>(), FwCache::POLLED)?;
        timeline.object().write_val(0, &0u32)?;

        let cccb = Cccb::new(timeline.parts().1, abi::CCCB_SIZE_LOG2)?;
        // All fallible work is complete. Existing Queue::release now owns the ranges.
        let timeline = Arc::new(TimelineUfo {
            object: Some(timeline.commit()),
            device: Arc::downgrade(&params.device),
            mapped: AtomicBool::new(false),
        });
        let reg_state = reg_state.commit();
        let timeline_fw_addr = timeline.fw_addr().0;

        let queue = Arc::new(Self {
            device: params.device,
            job_type: params.job_type,
            inner: SpinLock::new(QueueInner {
                cccb,
                pending: VecDeque::new(),
                killed: false,
                stopped: false,
                wake: Vec::new(),
                timeline_fw_addr,
            }),
            timeline,
            reg_state,
            fence_context: FenceContext::new(),
            next_seqno: AtomicU64::new(1),
            ctx_fw_addr: params.ctx_fw_addr,
            ctx_offset: params.ctx_offset,
            faulty: params.faulty,
            context_id: params.context_id,
        });

        Ok(queue)
    }

    /// What kind of jobs this queue takes.
    pub fn job_type(&self) -> JobType {
        self.job_type
    }

    /// Offset of this queue's slice within its context structure.
    pub fn ctx_offset(&self) -> u32 {
        self.ctx_offset
    }

    /// Firmware address of this queue's timeline UFO.
    ///
    /// What another job's `FENCE_PR` command names when it waits for a job on
    /// this queue.
    pub fn timeline_fw_addr(&self) -> FwAddr {
        self.timeline.fw_addr()
    }

    pub(crate) fn timeline_owner(&self) -> Arc<TimelineUfo> {
        self.timeline.clone()
    }

    /// The seqno the firmware has completed up to.
    pub fn timeline_value(&self) -> PvrResult<u32> {
        self.timeline.read_val::<u32>(0)
    }

    /// Maps every queue allocation into the firmware's page table.
    ///
    /// This method takes [`Self::inner`] to reach the client CCB control structure
    /// and ring as well as the timeline and register state. Every address published
    /// by `fw_common_context()` must have a corresponding firmware mapping.
    pub fn map_fw_objects(&self, table: &MipsPageTable) -> PvrResult<()> {
        table.map(&self.timeline)?;
        self.timeline.mapped.store(true, Ordering::Relaxed);
        table.map(&self.reg_state)?;
        for object in self.inner.lock().cccb.fw_objects() {
            table.map(object)?;
        }
        Ok(())
    }

    /// Removes those mappings again.
    ///
    /// Paired with [`Self::map_fw_objects`] and called before the allocations go
    /// back to the heap: a firmware that still has a mapping to a page the heap
    /// has handed to something else can write through it, which is the same
    /// class of fault as the missing mapping but harder to see.
    fn unmap_fw_objects(&self, table: &MipsPageTable) {
        // Best effort: this runs on a teardown path that cannot fail usefully,
        // and an entry that cannot be cleared is reported by `unmap` itself.
        let _ = table.unmap(&self.reg_state);
        for object in self.inner.lock().cccb.fw_objects() {
            let _ = table.unmap(object);
        }
    }

    /// Builds the firmware common context this queue is described by.
    ///
    /// Ports upstream `init_fw_context()`. The caller writes it into the context
    /// structure at [`Self::ctx_offset`]; it is produced rather than written here
    /// because the context object belongs to [`Context`], not to the queue.
    pub fn fw_common_context(
        &self,
        priority: Priority,
        fw_mem_context: FwAddr,
    ) -> PvrResult<RogueFwifFwcommoncontext> {
        let inner = self.inner.lock();
        let mut cctx = RogueFwifFwcommoncontext::new_zeroed();
        cctx.ccbctl_fw_addr = inner.cccb.ctrl_fw_addr();
        cctx.ccb_fw_addr = inner.cccb.ring_fw_addr();
        cctx.dm = dm_type(self.job_type);
        cctx.priority = priority as i32 as u32;
        cctx.priority_seq_num = 0;
        cctx.max_deadline_ms = abi::MAX_DEADLINE_MS;
        // Upstream stores `task_tgid_nr(current)`, purely for its own debug
        // reporting. There is no equivalent to read here and the firmware only
        // echoes it back in a reset notification, so zero is honest.
        cctx.pid = 0;
        cctx.server_common_context_id = self.context_id;
        cctx.fw_mem_context_fw_addr = fw_mem_context;
        cctx.context_state_addr = self.reg_state.fw_addr().0;
        Ok(cctx)
    }

    /// Whether a command sequence for `job` could ever fit in this CCCB.
    ///
    /// Ports the `pvr_cccb_cmdseq_can_fit()` check in upstream
    /// `pvr_queue_job_init()`, which is why a job too big for the ring is
    /// [`PvrError::TooBig`] at creation rather than a wait that never ends.
    pub fn job_can_fit(&self, command_len: usize, dep_count: usize) -> PvrResult<bool> {
        let size = job_cmds_size(command_len, dep_count)?;
        Ok(self.inner.lock().cccb.cmdseq_can_fit(size))
    }

    /// Allocates the fence a job on this queue will signal.
    ///
    /// Separate from [`Self::push`] because the fence has to exist while the
    /// batch is still being prepared: its dependencies are recorded on it, and a
    /// later job in the same batch may depend on it.
    ///
    /// Ports upstream `pvr_queue_job_fence_init()`.
    pub fn new_job_fence(self: &Arc<Self>) -> Arc<JobFence> {
        let seqno = self.next_seqno.fetch_add(1, Ordering::Relaxed);
        Arc::new(JobFence {
            state: FenceState::new(self.fence_context, seqno),
            queue: Arc::downgrade(self),
        })
    }

    /// Queues a job, blocking it until its dependencies signal.
    ///
    /// Ports upstream `pvr_queue_job_push()`. Takes the job by value: after this
    /// returns the queue owns it, and the caller keeps only the fence.
    ///
    /// # Errors
    ///
    /// [`PvrError::DeviceLost`] if the GPU has been declared lost, if the context
    /// has been marked faulty by a previous timeout, or if the queue has been
    /// killed. All three are upstream's `-EIO` — from `drm_dev_enter()`, from
    /// `atomic_read(&job->ctx->faulty)` and from a destroyed
    /// `drm_sched_entity` — and all three mean the same thing to the client: this
    /// context is finished, make a new one.
    pub fn push(self: &Arc<Self>, mut job: Job) -> PvrResult<()> {
        self.device.check_alive()?;
        if self.is_faulty() {
            return Err(PvrError::DeviceLost);
        }

        // Both halves of a combined kick take their runtime-PM reference here,
        // because this is the latest point on the submission path that may sleep
        // — power-reference ownership — and before any queue lock, because power acquisition may sleep. Upstream
        // makes the same two `pvr_job_get_pm_ref()` calls, one per half, in
        // `pvr_queue_run_job()`. Both are dropped again by the early returns
        // below if the jobs never reach a deque.
        let stop_epoch = self.device.quiescence_epoch.load(Ordering::Acquire);
        let frag = job.take_paired_frag();
        let power = crate::power::JobPowerRef::take(&self.device)?;
        let frag = match frag {
            Some(frag) => {
                let queue = frag
                    .context
                    .queue_for_job(JobType::Fragment)
                    .cloned()
                    .ok_or(PvrError::InvalidArgument)?;
                let power = crate::power::JobPowerRef::take(&self.device)?;
                Some((queue, frag, power))
            }
            None => None,
        };

        {
            let mut inner = self.inner.lock();
            if inner.killed {
                return Err(PvrError::DeviceLost);
            }

            let paired = match frag {
                Some((frag_queue, frag_job, frag_power)) => {
                    // The fragment half takes its place in its own queue now, so
                    // that the queue's order is still the order user space asked
                    // for; it is marked so that the queue does not submit it on
                    // its own. Under this queue's lock, which is the documented lock order,
                    // and after the only step that can still fail, so a refused
                    // push cannot leave half a pair behind.
                    let fence = frag_job.fence.clone();
                    let mut frag_inner = frag_queue.inner.lock();
                    if frag_inner.killed {
                        return Err(PvrError::DeviceLost);
                    }
                    frag_inner.pending.push_back(PendingJob::new(
                        *frag_job,
                        frag_power,
                        Some(Paired::Frag {
                            geom: Arc::downgrade(self),
                        }),
                        stop_epoch,
                    ));
                    drop(frag_inner);
                    Some(Paired::Geom {
                        queue: frag_queue,
                        fence,
                    })
                }
                None => None,
            };

            inner
                .pending
                .push_back(PendingJob::new(job, power, paired, stop_epoch));
        }

        // A foreign dependency signalling has to be able to restart this queue,
        // and `add_callback` runs the callback inline if the fence has already
        // signalled — fence callback-registration contract. Registered after the job is in the deque so
        // that an inline run finds it.
        self.arm_foreign_dep_callbacks();

        self.advance()
    }

    /// Registers an `advance` callback on every foreign dependency of the head.
    ///
    /// Only the head matters: FIFO ordering prevents a later job from running first, so nothing
    /// behind it needs to be woken.
    fn arm_foreign_dep_callbacks(self: &Arc<Self>) {
        let foreign: Vec<Arc<dyn Fence>> = {
            let inner = self.inner.lock();
            match inner.pending.iter().find(|job| job.submitted_at.is_none()) {
                Some(job) => {
                    let mut foreign = pending_foreign_deps(&job.deps);
                    // Ports upstream `pvr_queue_get_paired_frag_job_dep()`: a
                    // geometry job cannot be kicked until the fragment job it
                    // carries is ready either, so this queue has to watch the
                    // fragment job's dependencies as well as its own. Nothing
                    // else would wake it — the fragment queue's own callbacks
                    // only advance the fragment queue, which cannot submit a
                    // paired job.
                    if let Some(Paired::Geom { queue, fence }) = &job.paired {
                        let frag_inner = queue.inner.lock();
                        if let Some(frag) = frag_inner
                            .pending
                            .iter()
                            .find(|frag| Arc::ptr_eq(&frag.fence, fence))
                        {
                            foreign.extend(pending_foreign_deps(&frag.deps));
                        }
                    }
                    foreign
                }
                None => Vec::new(),
            }
        };

        for fence in foreign {
            let queue = Arc::downgrade(self);
            // A foreign fence may signal in a bottom half. Notify the existing
            // service worker instead of reserving KCCB space synchronously here.
            let callback = Arc::new(move |_status| {
                if let Some(queue) = queue.upgrade() {
                    crate::service::request_wake(&queue.device);
                }
            });
            if let Err(err) = fence.add_callback(callback) {
                ostd::warn!("could not watch a job dependency: {err:?}");
            }
        }
    }

    /// Submits as many jobs to the firmware as are ready.
    ///
    /// The scheduler's whole body. Called from [`Self::push`], from
    /// [`Self::process`] and from the service worker after a foreign dependency signals; idempotent, so
    /// calling it when there is nothing to do costs one lock acquisition.
    pub fn advance(self: &Arc<Self>) -> PvrResult<()> {
        self.advance_within(ADVANCE_WAKE_BUDGET)
    }

    /// [`Self::advance`] with a bound on how many further queues it may wake.
    fn advance_within(self: &Arc<Self>, budget: u32) -> PvrResult<()> {
        self.check_timeout();
        let result = self.submit_ready();
        // Last, and with no queue lock held: waking another queue takes that
        // queue's lock. Follow the lock order.
        self.wake_paired(budget);
        result
    }

    /// Advances the other half of a pair, if this queue's work has unblocked it.
    ///
    /// A combined kick makes two queues each other's business in both directions,
    /// and neither queue can see the other's state:
    ///
    /// - a fragment queue that has just submitted something may have cleared the
    ///   backlog a pair on some geometry queue was waiting behind;
    /// - a geometry queue that has just kicked or dropped a pair has changed what
    ///   the fragment jobs behind it can do.
    ///
    /// `budget` is spent one unit per wake, so the chain terminates by
    /// construction rather than by an argument about which queue can wake which.
    /// A wake that runs out of budget is left in place for the next
    /// [`Self::advance`], which arrives with a full one, rather than dropped.
    fn wake_paired(self: &Arc<Self>, budget: u32) {
        if budget == 0 {
            return;
        }
        let wake = {
            let mut inner = self.inner.lock();
            core::mem::take(&mut inner.wake)
        };
        for queue in wake.iter().filter_map(Weak::upgrade) {
            if let Err(err) = queue.advance_within(budget - 1) {
                ostd::warn!("could not advance the other half of a pair: {err:?}");
            }
        }
    }

    /// The submission loop of [`Self::advance`].
    fn submit_ready(self: &Arc<Self>) -> PvrResult<()> {
        loop {
            if !self.head_looks_ready()? {
                return Ok(());
            }

            // Reserved before the queue lock is taken: reserving busy-waits and
            // the queue lock must not be held across that. Follow the lock order.
            crate::sched::ccb::reserve_slot(&self.device)?;
            match self.try_submit_head() {
                Ok(Progress::Submitted) => {}
                Ok(Progress::Blocked) => {
                    crate::sched::ccb::release_slot(&self.device);
                    return Ok(());
                }
                Ok(Progress::Failed(jobs, error)) => {
                    crate::sched::ccb::release_slot(&self.device);
                    // Signalled outside the queue lock, which `try_submit_head`
                    // has already released. Follow the lock order. Dropping each job afterwards
                    // is what releases its power reference, in that order: the
                    // GPU stays up until the fence says the job is over.
                    for job in jobs {
                        job.finish(&self.device, Some(error));
                    }
                }
                Err(err) => {
                    crate::sched::ccb::release_slot(&self.device);
                    return Err(err);
                }
            }
        }
    }

    /// Whether the first unsubmitted job might be submittable.
    ///
    /// Advisory: [`Self::try_submit_head`] decides for real, under the lock. This
    /// exists so that the common case — nothing to do — does not reserve a KCCB
    /// slot and give it straight back.
    ///
    /// The size it computes counts the same dependencies
    /// [`Self::try_submit_head`] will turn into UFO waits, so the two agree: an
    /// estimate that counted signalled dependencies too would refuse a job that
    /// does fit, and a queue whose head can never be submitted has no timeout to
    /// rescue it.
    fn head_looks_ready(&self) -> PvrResult<bool> {
        let inner = self.inner.lock();
        if inner.stopped {
            return Ok(false);
        }
        let Some(job) = inner.pending.iter().find(|job| job.submitted_at.is_none()) else {
            return Ok(false);
        };

        let mut waits = 0;
        for dep in &job.deps {
            match dep {
                Dep::Foreign(fence) if !fence.is_signaled() => return Ok(false),
                Dep::Native { fence, .. } if !fence.is_signaled() => waits += 1,
                _ => {}
            }
        }

        // A fragment job that is half of a pair is submitted by its geometry
        // partner, so this queue has nothing to reserve a KCCB slot for. The one
        // exception is a dependency that *failed*, which
        // [`Self::try_submit_head`] has to see so that it can fail the job: rule
        // A failed fence is still signalled, so nothing else would.
        if matches!(job.paired, Some(Paired::Frag { .. })) && !job.deps.iter().any(dep_failed) {
            return Ok(false);
        }

        let size = job_cmds_size(job.command.len(), waits)?;
        inner.cccb.cmdseq_fits(size)
    }

    /// Writes the first unsubmitted job into the CCCB and kicks the firmware.
    ///
    /// Ports upstream `pvr_queue_submit_job_to_cccb()` and the `KICK` half of
    /// `pvr_queue_run_job()`.
    ///
    /// All-or-nothing: the CCCB write offset is captured first and restored if
    /// any part of the sequence fails, so a refused job leaves the ring exactly
    /// as it was. Nothing has to undo the bytes themselves — the firmware never
    /// looks past the offset a kick published, and no kick has been sent yet.
    fn try_submit_head(&self) -> PvrResult<Progress> {
        let mut inner = self.inner.lock();

        // Re-checked under the lock, not only in `head_looks_ready`: a reset may
        // have stopped the queue in between, and the whole point of stopping it
        // is that no command reaches a firmware that is being restarted.
        if inner.stopped {
            return Ok(Progress::Blocked);
        }

        let Some(index) = inner
            .pending
            .iter()
            .position(|job| job.submitted_at.is_none())
        else {
            return Ok(Progress::Blocked);
        };

        // Re-examine the dependencies now that the lock is held; the pre-submission dependency check.
        // Cloned first: deciding may mean removing the job, which cannot happen
        // while its dependency list is borrowed.
        let deps = inner.pending[index].deps.clone();
        let ufos = match examine_deps(&deps) {
            Deps::Ready(ufos) => ufos,
            Deps::Blocked => return Ok(Progress::Blocked),
            Deps::Failed(error) => {
                let mut job = inner.pending.remove(index).expect("index is in range");
                drop(inner);
                ostd::warn!(
                    "job {} fails because something it waits for did: {error:?}",
                    job.id
                );
                // A pair fails together: half a combined kick is not a thing the
                // firmware can be given, and leaving the fragment job in its
                // queue would leave a fence nothing will ever signal.
                let partner = self.detach_pair(job.paired.take());
                let mut jobs = alloc::vec![job];
                jobs.extend(partner);
                return Ok(Progress::Failed(jobs, error));
            }
        };

        // A fragment job that is half of a pair is written to the ring by the
        // geometry job it goes with, not by this queue. Its dependencies were
        // still examined above, so a failure among them is not lost.
        let pair = match &inner.pending[index].paired {
            Some(Paired::Frag { .. }) => return Ok(Progress::Blocked),
            Some(Paired::Geom { queue, fence }) => Some((queue.clone(), fence.clone())),
            None => None,
        };

        if let Some((frag_queue, frag_fence)) = pair {
            match self.submit_pair(&mut inner, index, &ufos, &frag_queue, &frag_fence)? {
                PairProgress::Submitted => {
                    let id = inner.pending[index].id;
                    drop(inner);
                    ostd::debug!("job {id} and the fragment job it carries kicked together");
                    return Ok(Progress::Submitted);
                }
                PairProgress::Blocked => return Ok(Progress::Blocked),
                PairProgress::FragmentFailed(job, error) => {
                    // The geometry job is still runnable on its own; the next
                    // pass through `submit_ready` takes it.
                    inner.pending[index].paired = None;
                    drop(inner);
                    return Ok(Progress::Failed(alloc::vec![job], error));
                }
                // The fragment half has gone, so this is an ordinary job again.
                PairProgress::Unpaired => inner.pending[index].paired = None,
            }
        }

        let size = job_cmds_size(inner.pending[index].command.len(), ufos.len())?;
        if !inner.cccb.cmdseq_fits(size)? {
            return Ok(Progress::Blocked);
        }

        let mark = inner.cccb.write_offset();
        match write_job_sequence(&mut inner, index, &ufos) {
            Ok(()) => {}
            Err(err) => {
                inner.cccb.rewind_to(mark);
                return Err(err);
            }
        }

        // The kick publishes the write offset, so it has to happen under the same
        // lock that decided it: two kicks racing could otherwise publish the
        // older, smaller offset last and take the firmware backwards.
        let hwrt = inner.pending[index].hwrt.clone();
        let kick = match kick_command(&KickHalf {
            ctx_fw_addr: self.ctx_fw_addr,
            write_offset: inner.cccb.write_offset(),
            wrap_mask: inner.cccb.wrap_mask(),
            hwrt: hwrt.as_ref(),
        }) {
            Ok(kick) => kick,
            Err(err) => {
                inner.cccb.rewind_to(mark);
                return Err(err);
            }
        };
        let id = inner.pending[index].id;
        match crate::sched::ccb::send_reserved(&self.device, &kick) {
            Ok(()) => {
                inner.pending[index].submitted_at = Some(now_ns());
                inner.pending[index].boot_generation = self.boot_generation();
                inner.pending[index].stop_epoch =
                    self.device.quiescence_epoch.load(Ordering::Acquire);
                self.note_paired_geometry_queue(&mut inner);
                drop(inner);
                ostd::debug!("job {id} written to the client CCB and kicked");
                Ok(Progress::Submitted)
            }
            Err(err) => {
                inner.cccb.rewind_to(mark);
                Err(err)
            }
        }
    }

    /// Writes a paired geometry and fragment job into their two rings and sends
    /// one `COMBINED_GEOM_FRAG_KICK`.
    ///
    /// Ports the `job->paired_job` half of upstream `pvr_queue_run_job()`,
    /// together with `pvr_cccb_send_kccb_combined_kick()`. Atomic in both rings:
    /// either both sequences and the kick go, or both write offsets are put back
    /// where they were and nothing was published.
    ///
    /// Called with this queue's lock held and takes the fragment queue's inside
    /// it. That is the only place two queue locks are held at once, and the order
    /// — geometry before fragment — is geometry before fragment.
    fn submit_pair(
        &self,
        inner: &mut QueueInner,
        index: usize,
        ufos: &[RogueFwifUfo],
        frag_queue: &Arc<Queue>,
        frag_fence: &Arc<JobFence>,
    ) -> PvrResult<PairProgress> {
        let mut frag_inner = frag_queue.inner.lock();

        // Never write into a ring whose queue is stopped. The check is here
        // because this is the only place one queue writes into *another* one's
        // ring: `submit_ready` checks `self.stopped` for this queue, and nothing
        // was checking it for the fragment queue.
        //
        // **Defensive, not a live bug**, and worth saying which: a reset holds
        // `PvrDevice::reset_lock` for writing across the whole sequence, so no
        // `SUBMIT_JOBS` can push while queues are stopped; `Queue::start` drains
        // its own queue before its trailing `advance`, so the restart order
        // cannot leave this queue with a pairable job while the fragment queue is
        // still stopped either. That is five facts in three files holding the
        // invariant up. One local check is cheaper to keep true.
        //
        // `Blocked` rather than `Unpaired`: the pair is still legal, it just
        // cannot go yet — the same situation as a fragment queue with earlier
        // work outstanding, and the same recovery, since the fragment queue's
        // `start` ends in an `advance` that wakes this one.
        //
        // `killed` needs no such check: it resolves itself, because the fragment
        // half is removed by `fail_all` and the next attempt finds no partner and
        // returns `Unpaired`.
        if frag_inner.stopped {
            return Ok(PairProgress::Blocked);
        }

        // The fragment half may have gone: failed by a dependency of its own, or
        // swept up by its queue timing out. Upstream's `pvr_queue_free_job()`
        // unpairs for the same reason, and a geometry job with no partner is an
        // ordinary geometry job.
        let Some(frag_index) = frag_inner
            .pending
            .iter()
            .position(|job| Arc::ptr_eq(&job.fence, frag_fence))
        else {
            return Ok(PairProgress::Unpaired);
        };

        // Upstream's `WARN_ON` in `pvr_queue_run_job()`, which re-checks the three
        // properties `can_combine_jobs()` established. Reaching this with any of
        // them false means the pairing itself was built wrong, and a combined kick
        // over mismatched halves is a firmware-visible error rather than something
        // to paper over.
        if self.job_type != JobType::Geometry
            || frag_queue.job_type != JobType::Fragment
            || self.context_id != frag_queue.context_id
            || !same_hwrt(
                inner.pending[index].hwrt.as_ref(),
                frag_inner.pending[frag_index].hwrt.as_ref(),
            )
        {
            ostd::error!(
                "refusing a combined kick whose two halves do not agree on type, \
                 context or render target"
            );
            return Err(PvrError::InvalidArgument);
        }

        // Everything the fragment queue was asked to run before this job has to be
        // in its ring already, or the ring would stop being in the order the
        // timeline UFO advances in — and that correspondence is the queue's whole
        // FIFO completion judgement. Upstream buys the same property with a
        // dependency on `get_last_queued_job_scheduled_fence()`; here the fragment
        // job is in its own queue, in order, so the question can be asked
        // directly.
        if frag_inner
            .pending
            .iter()
            .take(frag_index)
            .any(|job| job.submitted_at.is_none())
        {
            return Ok(PairProgress::Blocked);
        }

        // The pre-submission dependency check for the fragment half. Its wait on the geometry job is an
        // ordinary native dependency and becomes a `FENCE_PR` like any other:
        // upstream has to reach past its dependency list for that one UFO only
        // because its done fence is not UFO-backed until submission, while
        // `Job::native_dep_on` resolved it when the batch was prepared.
        let frag_deps = frag_inner.pending[frag_index].deps.clone();
        let frag_ufos = match examine_deps(&frag_deps) {
            Deps::Ready(ufos) => ufos,
            Deps::Blocked => return Ok(PairProgress::Blocked),
            Deps::Failed(error) => {
                let job = frag_inner
                    .pending
                    .remove(frag_index)
                    .expect("index is in range");
                ostd::warn!(
                    "fragment job {} fails because something it waits for did: {error:?}",
                    job.id
                );
                return Ok(PairProgress::FragmentFailed(job, error));
            }
        };

        let geom_size = job_cmds_size(inner.pending[index].command.len(), ufos.len())?;
        let frag_size = job_cmds_size(
            frag_inner.pending[frag_index].command.len(),
            frag_ufos.len(),
        )?;
        if !inner.cccb.cmdseq_fits(geom_size)? || !frag_inner.cccb.cmdseq_fits(frag_size)? {
            return Ok(PairProgress::Blocked);
        }

        let geom_mark = inner.cccb.write_offset();
        let frag_mark = frag_inner.cccb.write_offset();

        if let Err(err) = write_job_sequence(inner, index, ufos) {
            inner.cccb.rewind_to(geom_mark);
            return Err(err);
        }
        if let Err(err) = write_job_sequence(&mut frag_inner, frag_index, &frag_ufos) {
            inner.cccb.rewind_to(geom_mark);
            frag_inner.cccb.rewind_to(frag_mark);
            return Err(err);
        }

        let hwrt = inner.pending[index].hwrt.clone();
        // Upstream: "if this is a partial-render job, we don't attach resources to
        // the cleanup-ctl array, because the resources are already retained by the
        // geometry job".
        let frag_is_partial =
            frag_inner.pending[frag_index].command_type == abi::CCB_CMD_TYPE_FRAG_PR;
        let kick = combined_kick_command(
            &KickHalf {
                ctx_fw_addr: self.ctx_fw_addr,
                write_offset: inner.cccb.write_offset(),
                wrap_mask: inner.cccb.wrap_mask(),
                hwrt: hwrt.as_ref(),
            },
            &KickHalf {
                ctx_fw_addr: frag_queue.ctx_fw_addr,
                write_offset: frag_inner.cccb.write_offset(),
                wrap_mask: frag_inner.cccb.wrap_mask(),
                hwrt: if frag_is_partial { None } else { hwrt.as_ref() },
            },
        );
        let kick = match kick {
            Ok(kick) => kick,
            Err(err) => {
                inner.cccb.rewind_to(geom_mark);
                frag_inner.cccb.rewind_to(frag_mark);
                return Err(err);
            }
        };

        // One slot for the pair. `advance` reserved exactly one before it took any
        // lock, and this is the one command that consumes it — the fragment job
        // never reserves one of its own, which is upstream dropping the fragment
        // job's KCCB fence when it makes the pair.
        match crate::sched::ccb::send_reserved(&self.device, &kick) {
            Ok(()) => {
                let now = now_ns();
                let generation = self.boot_generation();
                let stop_epoch = self.device.quiescence_epoch.load(Ordering::Acquire);
                inner.pending[index].submitted_at = Some(now);
                inner.pending[index].boot_generation = generation;
                inner.pending[index].stop_epoch = stop_epoch;
                frag_inner.pending[frag_index].submitted_at = Some(now);
                frag_inner.pending[frag_index].boot_generation = generation;
                frag_inner.pending[frag_index].stop_epoch = stop_epoch;
                drop(frag_inner);
                // Whatever is behind the fragment half may be able to go now.
                note_wake(inner, frag_queue);
                Ok(PairProgress::Submitted)
            }
            Err(err) => {
                inner.cccb.rewind_to(geom_mark);
                frag_inner.cccb.rewind_to(frag_mark);
                Err(err)
            }
        }
    }

    /// Records the geometry queue of a paired fragment job still waiting here.
    ///
    /// Called after this queue submits a job on its own: if the job that could
    /// not go is half of a pair, this queue has just moved its backlog along and
    /// the geometry queue may now be able to kick. A geometry queue never holds a
    /// [`Paired::Frag`] entry, so on one this finds nothing.
    fn note_paired_geometry_queue(&self, inner: &mut QueueInner) {
        let geom = inner
            .pending
            .iter()
            .find(|job| job.submitted_at.is_none())
            .and_then(|job| match &job.paired {
                Some(Paired::Frag { geom }) => geom.upgrade(),
                _ => None,
            });
        if let Some(geom) = geom {
            note_wake(inner, &geom);
        }
    }

    /// Removes the fragment job a geometry job was going to be kicked with.
    ///
    /// Ports the `pvr_job_put(job->paired_job)` in upstream
    /// `pvr_queue_free_job()`: a pair that never reaches the firmware must not
    /// leave half of itself in a queue with a fence nothing will ever signal.
    /// Returns the job so that the caller can signal it outside every lock.
    ///
    /// Must be called with no queue lock held. This queue's lock is taken first
    /// and released before the fragment queue's, so the documented lock order is not inverted.
    fn detach_pair(&self, paired: Option<Paired>) -> Option<PendingJob> {
        let Some(Paired::Geom { queue, fence }) = paired else {
            return None;
        };

        {
            let mut inner = self.inner.lock();
            note_wake(&mut inner, &queue);
        }

        let mut frag_inner = queue.inner.lock();
        let index = frag_inner
            .pending
            .iter()
            .position(|job| Arc::ptr_eq(&job.fence, &fence))?;
        frag_inner.pending.remove(index)
    }

    /// Adds the fragment half of every pair in `jobs` to the list.
    ///
    /// So that a terminated geometry job never leaves one behind, whichever of
    /// completion, failure or teardown terminated it.
    fn with_pairs(&self, jobs: Vec<PendingJob>) -> Vec<PendingJob> {
        let mut out = Vec::with_capacity(jobs.len());
        for mut job in jobs {
            let partner = self.detach_pair(job.paired.take());
            out.push(job);
            out.extend(partner);
        }
        out
    }

    /// Signals completed jobs, then advances submissions waiting for CCCB space.
    ///
    /// Ports upstream `pvr_queue_signal_done_fences()` and
    /// `pvr_queue_check_job_waiting_for_cccb_space()`. [`process_all`] applies this
    /// to every live queue from deferred IRQ handling and the service worker.
    pub fn process(self: &Arc<Self>) {
        self.retire_finished();
        // `advance` reaches `check_timeout`, which retires again. That second
        // read costs one cache-maintenance operation on four bytes and finds
        // nothing; paying it is how the ordering guarantee holds for every
        // caller rather than only for the ones that came through here.
        if let Err(err) = self.advance() {
            ostd::warn!("could not advance a queue after retirement: {err:?}");
        }
    }

    /// Signals every job the timeline UFO says the firmware has finished.
    ///
    /// The retirement half of [`Self::process`], split out so that every path
    /// that can declare a job hung can first ask whether it is actually done.
    /// Does **not** advance: [`Self::check_timeout_at`] calls this and is
    /// itself reached from [`Self::advance_within`], so advancing here would
    /// recurse.
    /// Returns the timeline value it acted on, so that a caller about to
    /// condemn a job can say which reading it was condemned against.
    fn retire_finished(&self) -> Option<u32> {
        let current = match self.timeline_value() {
            Ok(value) => value,
            Err(err) => {
                ostd::error!("could not read a queue's timeline: {err:?}");
                return None;
            }
        };

        let done = {
            let mut inner = self.inner.lock();
            let mut done = Vec::new();
            while let Some(job) = inner.pending.front() {
                // Wrap-safe: upstream's `(int)(cur_seqno - seqno) < 0`. A queue
                // that has issued 2^31 jobs since the firmware last made
                // progress is not a case either side handles.
                if job.submitted_at.is_none() || (current.wrapping_sub(job.ufo_value) as i32) < 0 {
                    break;
                }
                done.push(inner.pending.pop_front().expect("front exists"));
            }

            // Restart the next head's deadline when its predecessor completes. Ring
            // submission time includes time spent behind earlier jobs, so it cannot measure
            // that head's lack of progress. Upstream `drm_sched_start_timeout` likewise
            // restarts the timer when a job retires.
            if !done.is_empty()
                && let Some(head) = inner.pending.front_mut()
                && head.submitted_at.is_some()
            {
                head.submitted_at = Some(now_ns());
            }

            done
        };

        // Release outside the queue lock. Each job is dropped as soon as its fence has
        // signalled, which is what releases its power reference — upstream does
        // the same two steps in the same order in
        // `pvr_queue_signal_done_fences()`.
        for job in done {
            job.finish(&self.device, None);
        }

        Some(current)
    }

    /// Signals fences up to `seqno` as though the firmware had reported them.
    ///
    /// The seam the tests drive the retirement path through, and the shape the
    /// original stub promised. Writes the timeline UFO and then runs
    /// [`Self::process`], so the ordering between the UFO and the fences is the
    /// same one the firmware produces.
    pub fn retire(self: &Arc<Self>, seqno: u32) {
        if let Err(err) = self.timeline.write_val(0, &seqno) {
            ostd::error!("could not write a queue's timeline: {err:?}");
            return;
        }
        self.process();
    }

    /// Fails the oldest submitted job if it has overrun, and everything after it.
    ///
    /// Timeout handling Discharges fence fence-completion contract for a hung GPU: the fence is
    /// signalled with [`DrmError::Io`], never left pending.
    pub fn check_timeout(&self) {
        self.check_timeout_at(now_ns());
    }

    /// [`Self::check_timeout`] against an explicit clock reading.
    ///
    /// The clock is a parameter so that a test can reach the timeout path without
    /// waiting half a second of wall time, and so that it exercises the same code
    /// the real path does rather than a `#[cfg(ktest)]` copy of it.
    pub(crate) fn check_timeout_at(&self, now: u64) {
        // Retire completed work before every timeout decision, including calls from
        // pushes and dependency callbacks that did not pass through `process`. A completed
        // timeline value must take precedence over the elapsed deadline.
        let retired_against = self.retire_finished();

        // Read the timeline again before failing an overdue head. Firmware can advance
        // it between the retirement pass and this decision; a newly completed job must
        // retire successfully. This extra read is limited to the suspected-timeout path.
        let head = {
            let inner = self.inner.lock();
            inner
                .pending
                .front()
                .filter(|job| {
                    job.submitted_at
                        .is_some_and(|at| now.saturating_sub(at) > JOB_TIMEOUT_NS)
                })
                .map(|job| (job.ufo_value, unsatisfied_dep(&job.deps)))
        };

        // A head blocked on an unsignalled producing fence is waiting for work on
        // another queue, so its own execution deadline must not fail it. Failed fences
        // are signalled and do not qualify for this exemption.
        if let Some((head_ufo, Some(blocked_on))) = &head {
            ostd::debug!(
                "[k1-gpu] job timeout declined on a {:?} queue: the head job needs {:#x} but is \
                 still waiting on {} -- it is blocked, not hung, and failing it would abandon \
                 the work everything behind it needs",
                self.job_type,
                head_ufo,
                blocked_on
            );
            return;
        }

        if let Some((head_ufo, _)) = head {
            let second = self.timeline_value().ok();
            if let Some(second) = second
                && (second.wrapping_sub(head_ufo) as i32) >= 0
            {
                ostd::debug!(
                    "[k1-gpu] job timeout averted on a {:?} queue: the head job needs {:#x}, \
                     the retirement just before this read the timeline as {:?}, and reading it \
                     again gives {:#x}. Two readings of the same four bytes disagreed, so the \
                     job is being retired rather than failed. If this line appears at all, the \
                     fault is in what the CPU can see of firmware writes",
                    self.job_type,
                    head_ufo,
                    retired_against,
                    second
                );
                self.retire_finished();
                return;
            }
        }

        let expired = {
            let mut inner = self.inner.lock();
            let overrun = inner
                .pending
                .front()
                .and_then(|job| job.submitted_at)
                .is_some_and(|at| now.saturating_sub(at) > JOB_TIMEOUT_NS);
            if !overrun {
                Vec::new()
            } else {
                // Snapshot the client ring before abandoning it, while the queue lock keeps
                // the command bytes and offsets stable.
                inner.cccb.report("job timeout");
                // Firmware trace messages identify contexts, CCBs, and UFOs by firmware
                // address. Record this queue's addresses to make that correlation possible.
                ostd::debug!(
                    "[k1-gpu] job timeout addrs: FWCtx={:#010x} ccb={:#010x} ccbctl={:#010x} \
                     timeline={:#010x} dm={:?}",
                    self.ctx_fw_addr.0,
                    inner.cccb.ring_fw_addr().0,
                    inner.cccb.ctrl_fw_addr().0,
                    self.timeline_fw_addr().0,
                    self.job_type
                );
                // KCCB offsets distinguish an unconsumed kick from work that
                // firmware accepted but has not completed in the client CCB.
                if let Some(kccb) = crate::sched::ccb::snapshot(&self.device) {
                    kccb.report("job timeout");
                }
                // The firmware is stuck on this CCCB with the head job's update
                // unwritten, so nothing behind it will run either.
                let expired: Vec<PendingJob> = inner.pending.drain(..).collect();
                self.faulty.store(true, Ordering::Release);
                expired
            }
        };

        if expired.is_empty() {
            return;
        }

        // Report the full failed seqno span in the units used for timeline retirement.
        ostd::debug!(
            "[k1-gpu] job timeout: failing {} job(s), timeline seqnos {:#x}..={:#x} \
             (ids {}..={})",
            expired.len(),
            expired[0].ufo_value,
            expired[expired.len() - 1].ufo_value,
            expired[0].id,
            expired[expired.len() - 1].id
        );

        // Report submission and firmware generations to identify work spanning a reset.
        let current = self.boot_generation();
        ostd::debug!(
            "[k1-gpu] job timeout: job {} submitted under firmware boot generation {}, \
             current generation {} — {}",
            expired[0].id,
            expired[0].boot_generation,
            current,
            if expired[0].boot_generation != current {
                "a power cycle happened underneath it, so no job should have been \
                 outstanding when the GPU was powered down"
            } else {
                "the firmware now running was given this job and did not execute it"
            }
        );
        // Report the head's dependency addresses and values for correlation with
        // firmware UFO updates. Later jobs are blocked behind this head.
        for (index, dep) in expired[0].deps.iter().enumerate() {
            match dep {
                Dep::Native {
                    fence,
                    ufo_addr,
                    ufo_value,
                    ..
                } => ostd::debug!(
                    "[k1-gpu] job timeout dep[{}]: native UFO [{:#010x}] must reach {:#x}; \
                     producing fence signalled={}",
                    index,
                    ufo_addr.0,
                    ufo_value,
                    fence.is_signaled()
                ),
                Dep::Foreign(fence) => ostd::debug!(
                    "[k1-gpu] job timeout dep[{}]: foreign fence, signalled={} — the CPU has \
                     to observe this one, the firmware cannot",
                    index,
                    fence.is_signaled()
                ),
            }
        }
        if expired[0].deps.is_empty() {
            ostd::debug!(
                "[k1-gpu] job timeout: the head job has no dependencies, so nothing was \
                 keeping the firmware from running it"
            );
        }
        // Capture both the CPU-observed timeline value and the head's target. These
        // are the same values retirement compares; either one alone is insufficient.
        match self.timeline_value() {
            Ok(current) => ostd::debug!(
                "[k1-gpu] job timeout: timeline UFO [{:#010x}] currently reads {:#x}, the \
                 retirement before this read {:?}; the head job would write {:#x} — {}",
                self.timeline_fw_addr().0,
                current,
                retired_against,
                expired[0].ufo_value,
                if (current.wrapping_sub(expired[0].ufo_value) as i32) >= 0 {
                    "the firmware passed it while this block was being printed -- the two \
                     reads above bracket a hundred-odd milliseconds of serial output, so the \
                     GPU was working the whole time and this deadline was premature"
                } else {
                    "the firmware has not reached it yet, and the reading the deadline was \
                     measured against is the one on the left"
                }
            ),
            Err(err) => ostd::debug!(
                "[k1-gpu] job timeout: could not read the timeline UFO [{:#010x}] at all \
                 ({:?}); the head job would write {:#x}. `Queue::process` reads the same way, \
                 so it has been failing silently too",
                self.timeline_fw_addr().0,
                err,
                expired[0].ufo_value
            ),
        }

        // The firmware's own account, which is the only witness to what it did
        // with a command it took and did not run. Printed after both rings so the
        // driver's view and the firmware's view sit next to each other, and only
        // on this path: sixty-four decoded entries per thread is not something to
        // pay for anywhere else.
        crate::fw::log_trace(&self.device, "job timeout");

        ostd::error!(
            "job {} on a {:?} queue did not complete in {} ms; failing it and the {} \
             job(s) behind it, and marking the context faulty",
            expired[0].id,
            self.job_type,
            JOB_TIMEOUT_NS / 1_000_000,
            expired.len() - 1
        );
        for job in self.with_pairs(expired) {
            job.finish(&self.device, Some(DrmError::Io));
        }

        // Unlike upstream's retained pending job, this timeout releases the job's
        // runtime-PM reference. Request recovery so firmware still executing an abandoned
        // kick does not prevent every subsequent idle transition.
        //
        // Defer the reset until the queue teardown is complete. The service worker or
        // the next `PowerRef::take` runs it with sleepable reset/transition locks.
        // Do not request another reset while one is already restarting queues.
        if crate::power::manager().is_some_and(crate::power::Pm::is_resetting) {
            return;
        }
        crate::power::request_reset(&self.device, crate::power::ResetKind::Hard);
    }

    /// Whether the owning context has been marked unusable.
    ///
    /// Shared with the context rather than reached through it; see the field's
    /// own comment for why the bit rather than a `Weak<Context>`.
    pub fn is_faulty(&self) -> bool {
        self.faulty.load(Ordering::Acquire)
    }

    /// Which firmware boot is currently running, or zero if none has.
    ///
    /// Zero is also what an unsubmitted job carries, which is harmless: a job
    /// with no `submitted_at` is never reported against a generation.
    fn boot_generation(&self) -> u32 {
        self.device
            .fw
            .get()
            .map_or(0, |fw| fw.generation.load(Ordering::Acquire))
    }

    /// Makes the queue refuse new jobs, without waiting for the old ones.
    ///
    /// Ports upstream `pvr_queue_kill()`. Called when the context handle is
    /// destroyed; the queue itself may outlive that while jobs are in flight.
    pub fn kill(&self) {
        self.inner.lock().killed = true;
    }

    /// Stops the queue for the duration of a GPU reset.
    ///
    /// Ports upstream `pvr_queue_stop()`, which is `drm_sched_stop()`: from here
    /// until [`Self::start`] nothing this queue holds is written to the client
    /// CCB and no kick is sent, because the firmware that would read them is
    /// about to be restarted.
    pub fn stop(&self) {
        self.inner.lock().stopped = true;
    }

    /// Restarts the queue after a GPU reset, terminating what did not survive.
    ///
    /// Ports upstream `pvr_queue_start()`, in the same order:
    ///
    /// 1. **Retire whatever really finished.** Upstream's loop re-attaches the
    ///    done fence of any job that completed after `drm_sched_stop()`; here
    ///    that is [`Self::process`], which signals exactly the jobs the timeline
    ///    UFO says the firmware got to. It runs first, while the UFO still holds
    ///    the value the dead firmware left.
    /// 2. **CPU-signal the timeline UFO** through the last submitted seqno (upstream: `*queue->timeline_ufo.value = seqno`). Another queue may
    ///    have a `FENCE_PR` command waiting on this UFO, and the firmware that
    ///    would have advanced it is gone.
    /// 3. **Terminate what is left and mark the context faulty**, which is
    ///    upstream's "if we had unfinished jobs, flag the entity as guilty so no
    ///    new job can be submitted".
    ///
    /// Step 3 is where this port and upstream part company, and deliberately:
    /// upstream leaves the unfinished jobs on the scheduler's pending list, this
    /// port fails their fences with [`DrmError::Io`]. **There is no
    /// reset-and-replay**, and the fence contract says a fence must be signalled rather than abandoned. A job whose CCCB
    /// contents the firmware may or may not have consumed cannot be re-run
    /// safely, and the client gets the same `EIO` it would from a timeout.
    ///
    /// A queue with nothing outstanding is simply restarted: its context stays
    /// usable, which is the whole difference between a reset and
    /// [`crate::device::PvrDevice::declare_lost`].
    pub fn start(self: &Arc<Self>) {
        // Step 1. Still stopped, so the `advance` inside cannot submit anything.
        self.process();

        // Preparing a job allocates its fence value before it reaches firmware.
        // Advance only through submitted jobs: retained unsubmitted jobs must still
        // wait for the new firmware to complete their own timeline value.
        let last_submitted = self
            .inner
            .lock()
            .pending
            .iter()
            .rfind(|job| job.submitted_at.is_some())
            .map(|job| job.ufo_value);
        if let Some(value) = last_submitted
            && let Err(err) = self.timeline.write_val(0, &value)
        {
            ostd::error!("could not CPU-signal a queue's timeline after a reset: {err:?}");
        }

        let expired: Vec<_> = {
            let mut inner = self.inner.lock();
            // Only terminate jobs published to firmware. A job still waiting its turn
            // has not reached the client CCB and remains queued for the trailing `advance`.
            // Upstream likewise walks the submitted pending list, not the entity's queue.
            let mut kept = VecDeque::with_capacity(inner.pending.len());
            let mut expired = Vec::new();
            while let Some(job) = inner.pending.pop_front() {
                if job.submitted_at.is_some() {
                    expired.push(job);
                } else {
                    kept.push_back(job);
                }
            }
            inner.pending = kept;
            if !expired.is_empty() {
                self.faulty.store(true, Ordering::Release);
            }
            inner.stopped = false;
            expired
        };

        if !expired.is_empty() {
            ostd::error!(
                "GPU reset terminated {} job(s) on a {:?} queue; its context is now faulty",
                expired.len(),
                self.job_type
            );
        }

        // Release outside the queue lock.
        for job in expired {
            job.finish(&self.device, Some(DrmError::Io));
        }

        // The queue is running again, so anything that arrived while it was
        // stopped may now go. Upstream's trailing `drm_sched_start()`.
        if let Err(err) = self.advance() {
            ostd::warn!("could not advance a queue after a reset: {err:?}");
        }
    }

    /// Whether the queue is stopped for a reset. Diagnostic, and for the tests.
    pub fn is_stopped(&self) -> bool {
        self.inner.lock().stopped
    }

    /// Writes the timeline UFO the way the firmware would, without retiring.
    ///
    /// [`Self::retire`] is the same write followed by [`Self::process`], which is
    /// what a test wants nearly everywhere. The exception is a job the firmware
    /// finished while the queue was stopped for a reset: nothing has noticed it
    /// yet, and whether [`Self::start`] does is the property under test.
    /// Deliberately not available outside tests — in production the firmware is
    /// the only writer of this word.
    #[cfg(ktest)]
    pub(crate) fn set_timeline_for_test(&self, seqno: u32) {
        self.timeline.write_val(0, &seqno).expect("timeline write");
    }

    /// Reads the current head timestamp without changing scheduler behavior.
    #[cfg(ktest)]
    pub(crate) fn head_submitted_at_for_test(&self) -> Option<u64> {
        self.inner
            .lock()
            .pending
            .front()
            .and_then(|job| job.submitted_at)
    }

    /// Moves every submitted job's deadline `delta_ns` further into the past.
    ///
    /// What a deep ring looks like on hardware: a command written several
    /// frames ago that the firmware has not reached yet. A test cannot get
    /// there by waiting, and stamping the clock itself would not exercise the
    /// same field the real path sets.
    #[cfg(ktest)]
    pub(crate) fn backdate_submitted_for_test(&self, delta_ns: u64) {
        let mut inner = self.inner.lock();
        for job in inner.pending.iter_mut() {
            if let Some(at) = job.submitted_at {
                job.submitted_at = Some(at.saturating_sub(delta_ns));
            }
        }
    }

    /// Timeline UFO allocation, for the CPU-cache call-site table.
    #[cfg(ktest)]
    pub(crate) fn timeline_object(&self) -> &FwObject {
        &self.timeline
    }

    /// Register-state allocation, for the CPU-cache call-site table.
    #[cfg(ktest)]
    pub(crate) fn reg_state_object(&self) -> &FwObject {
        &self.reg_state
    }

    /// CCCB control and ring CPU-cache flags, for the call-site table.
    ///
    /// Returns `[ctrl, ring]`. The objects live behind the queue lock, so the
    /// flags are copied out rather than borrowed.
    #[cfg(ktest)]
    pub(crate) fn cccb_cpu_uncached(&self) -> [bool; 2] {
        let inner = self.inner.lock();
        let objs = inner.cccb.fw_objects();
        [objs[0].is_cpu_uncached(), objs[1].is_cpu_uncached()]
    }

    /// Waits for the queue to drain.
    ///
    /// Returns [`crate::PvrError::DeviceLost`] rather than blocking forever if the
    /// GPU is declared lost while waiting.
    ///
    /// Polls rather than sleeping, to process completions and deadlines without a firmware wait queue. Each pass retires
    /// what the firmware has finished and checks for a timeout, so a hung job
    /// makes this return through [`Self::check_timeout`] rather than through the
    /// outer deadline.
    pub fn drain(self: &Arc<Self>) -> PvrResult<()> {
        let deadline = now_ns().saturating_add(DRAIN_TIMEOUT_NS);
        loop {
            if self.device.check_alive().is_err() {
                // A lost device will never advance the timeline, so the jobs must
                // be failed here rather than waited for: fence-completion contract.
                self.fail_all(DrmError::Io);
                return Err(PvrError::DeviceLost);
            }

            self.process();
            if self.inner.lock().pending.is_empty() {
                return Ok(());
            }
            if now_ns() >= deadline {
                ostd::error!("a queue did not drain; failing what is left");
                self.fail_all(DrmError::Io);
                return Err(PvrError::Timeout);
            }
            crate::fw::delay_us(POLL_INTERVAL_US);
        }
    }

    /// Signals everything still queued with `error`.
    ///
    /// Used when the context is torn down
    /// or the device is lost, both of which mean no firmware will ever advance
    /// this timeline again.
    pub fn fail_all(&self, error: DrmError) {
        let expired: Vec<PendingJob> = {
            let mut inner = self.inner.lock();
            inner.killed = true;
            inner.pending.drain(..).collect()
        };
        for job in self.with_pairs(expired) {
            job.finish(&self.device, Some(error));
        }
    }

    /// How many jobs are queued but not signalled.
    ///
    /// Diagnostic, and what the tests assert against.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().pending.len()
    }

    /// How many jobs are on the GPU.
    ///
    /// Upstream `pvr_queue::in_flight_job_count`, which it keeps as an atomic
    /// because its interrupt path reads it without the list lock. Nothing here
    /// needs that, so it is derived rather than stored.
    pub fn in_flight_count(&self) -> usize {
        self.inner
            .lock()
            .pending
            .iter()
            .filter(|job| job.submitted_at.is_some())
            .count()
    }

    /// The commands in this queue's ring, for the tests that read them back.
    ///
    /// Not part of the driver's interface: the ring is the firmware's to read, and
    /// a caller with a reason to look at it is checking this module's own work.
    #[cfg(ktest)]
    pub(crate) fn cccb_commands(&self) -> PvrResult<Vec<(u32, Vec<u8>)>> {
        self.inner.lock().cccb.commands()
    }

    /// This queue's ring's current write offset, for the tests.
    #[cfg(ktest)]
    pub(crate) fn cccb_write_offset(&self) -> u32 {
        self.inner.lock().cccb.write_offset()
    }

    /// Firmware address of this queue's slice of its context structure.
    ///
    /// What a kick names, and therefore what a test asserting on a combined kick
    /// has to compare against.
    #[cfg(ktest)]
    pub(crate) fn ctx_fw_addr(&self) -> FwAddr {
        self.ctx_fw_addr
    }

    /// Reclaims a constructor failure before the context or queue is published.
    ///
    /// No job can hold this queue or its UFO before Context::new registers it.
    /// Consume its unique owners while the caller already holds the heap lock,
    /// so partial construction returns every range without deferred retirement.
    pub(crate) fn rollback(self: Arc<Self>, heap: &mut FwHeap, table: &MipsPageTable) {
        let Ok(mut queue) = Arc::try_unwrap(self) else {
            unreachable!("Context construction never publishes a partial queue");
        };
        queue.release(heap, table);
        let timeline = Arc::get_mut(&mut queue.timeline)
            .expect("an unpublished timeline has no native consumers");
        if let Some(object) = timeline.object.take() {
            if timeline.mapped.swap(false, Ordering::Relaxed) {
                let _ = table.unmap(&object);
            }
            heap.free(&object);
        }
    }

    /// Frees the queue's firmware-heap allocations.
    ///
    /// The timeline has a separate shared owner: native consumers may still name
    /// it after this queue's context is cleaned up. Its final Drop returns that range.
    pub fn release(&self, heap: &mut FwHeap, table: &MipsPageTable) {
        // Unmapped before freed, so that the firmware cannot reach a page the
        // heap has already given to something else.
        self.unmap_fw_objects(table);
        heap.free(&self.reg_state);
        let inner = self.inner.lock();
        inner.cccb.release(heap);
    }
}

/// The outcome of one attempt to submit the head of a queue.
enum Progress {
    /// A job was written to the CCCB and the firmware kicked.
    Submitted,
    /// Nothing could be submitted: a dependency is pending or the ring is full.
    Blocked,
    /// Jobs were removed and their fences must be failed, outside the queue lock.
    ///
    /// More than one when a pair fails, which the two halves always do together.
    /// The jobs travel with their fences rather than being dropped inside
    /// [`Queue::try_submit_head`], so that their power references outlive the
    /// signal: power-reference ownership.
    Failed(Vec<PendingJob>, DrmError),
}

/// The outcome of one attempt to kick a pair.
enum PairProgress {
    /// Both halves are in their rings and one combined kick has been sent.
    Submitted,
    /// The pair cannot go yet: a dependency of either half is pending, the
    /// fragment queue still has something ahead of its half, or a ring is full.
    Blocked,
    /// The fragment half is gone; the geometry job is an ordinary job now.
    Unpaired,
    /// The fragment half must be failed, and the geometry job kicked on its own.
    FragmentFailed(PendingJob, DrmError),
}

/// How many further queues one [`Queue::advance`] may wake.
///
/// See [`Queue::wake_paired`]. Two is what the two directions need: a fragment
/// queue tells the geometry queue its backlog has cleared, and the geometry queue
/// tells the fragment queue that the pair has gone. A third hop would only be a
/// failure path re-entering the first, and stopping there costs at most a delay
/// until the next submission on a context that is already faulty.
const ADVANCE_WAKE_BUDGET: u32 = 2;

/// Records that `queue` may have work to do, without duplicating an entry.
fn note_wake(inner: &mut QueueInner, queue: &Arc<Queue>) {
    if !inner
        .wake
        .iter()
        .any(|other| Weak::upgrade(other).is_some_and(|other| Arc::ptr_eq(&other, queue)))
    {
        inner.wake.push(Arc::downgrade(queue));
    }
}

/// Whether two jobs draw into the same render target.
///
/// Upstream compares `job->hwrt` pointers. A job with no target never reaches a
/// combined kick — `finish_command` refuses a render job without one — so two
/// `None`s are reported as a mismatch rather than a match: refusing to pair is
/// always safe.
fn same_hwrt(a: Option<&HwrtRef>, b: Option<&HwrtRef>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(&a.data, &b.data),
        _ => false,
    }
}

/// Whether a dependency has signalled with an error. The failed-fence contract.
fn dep_failed(dep: &Dep) -> bool {
    match dep {
        Dep::Native { fence, .. } | Dep::Foreign(fence) => fence.error().is_some(),
    }
}

/// The foreign dependencies of a job that have not signalled yet.
fn pending_foreign_deps(deps: &[Dep]) -> Vec<Arc<dyn Fence>> {
    deps.iter()
        .filter_map(|dep| match dep {
            Dep::Foreign(fence) if !fence.is_signaled() => Some(fence.clone()),
            _ => None,
        })
        .collect()
}

/// What examining one job's dependencies concluded. The pre-submission dependency check.
enum Deps {
    /// Every dependency is met, or has become one of these UFO waits for the
    /// firmware to do.
    Ready(Vec<RogueFwifUfo>),
    /// Something only the CPU can observe has not signalled.
    Blocked,
    /// Something this job waits for failed, so this job fails too.
    Failed(DrmError),
}

/// Describes the first unsignalled dependency of a job.
///
/// Ports the dependency loop of upstream `pvr_queue_submit_job_to_cccb()`.
/// `None` means every dependency has signalled, so waiting for a producer no
/// longer exempts this job from its execution deadline. The description retains
/// the UFO identity for diagnostics.
fn unsatisfied_dep(deps: &[Dep]) -> Option<alloc::string::String> {
    for (index, dep) in deps.iter().enumerate() {
        match dep {
            Dep::Native {
                fence,
                ufo_addr,
                ufo_value,
                ..
            } if !fence.is_signaled() => {
                return Some(alloc::format!(
                    "dep[{}] native UFO [{:#010x}] reaching {:#x}",
                    index,
                    ufo_addr.0,
                    ufo_value
                ));
            }
            Dep::Foreign(fence) if !fence.is_signaled() => {
                return Some(alloc::format!("dep[{}] a foreign fence", index));
            }
            _ => {}
        }
    }
    None
}

/// Takes the list by reference rather than reading it out of the queue, because
/// the answer may be "remove this job" and that cannot happen while its own
/// dependency list is borrowed.
fn examine_deps(deps: &[Dep]) -> Deps {
    let mut ufos = Vec::new();
    for dep in deps {
        let (fence, ufo): (&Arc<dyn Fence>, Option<RogueFwifUfo>) = match dep {
            Dep::Native {
                fence,
                ufo_addr,
                ufo_value,
                ..
            } => (
                fence,
                Some(RogueFwifUfo {
                    addr: ufo_addr.0,
                    value: *ufo_value,
                }),
            ),
            Dep::Foreign(fence) => (fence, None),
        };

        if let Some(error) = fence.error() {
            // The failed-fence contract: a failed fence is signalled, so this job makes progress —
            // by failing. The dependent job fails too.
            return Deps::Failed(error);
        }
        if fence.is_signaled() {
            continue;
        }
        match ufo {
            // Still pending and native: the firmware waits, not the driver.
            Some(ufo) => ufos.push(ufo),
            // Still pending and foreign: nothing behind it may go either.
            None => return Deps::Blocked,
        }
    }
    Deps::Ready(ufos)
}

/// One half of a kick: whose ring, how far it now goes, and which render target
/// the firmware should report cleanup on.
struct KickHalf<'a> {
    /// Firmware address of the queue's slice of its context structure.
    ctx_fw_addr: FwAddr,
    /// The ring's new write offset, which this kick publishes.
    write_offset: u32,
    /// The ring's wrap mask.
    wrap_mask: u32,
    /// The render target, for a geometry or fragment job.
    hwrt: Option<&'a HwrtRef>,
}

/// Fills in one kick's worth of data. Ports upstream `fill_cmd_kick_data()`.
///
/// The single cleanup-control entry upstream can add is the render target's
/// cleanup state, which is how the firmware reports that it has finished with a
/// dataset; without it, `DESTROY_HWRT_DATASET` could free memory the firmware is
/// still rendering into.
fn fill_kick_data(kick: &mut RogueFwifKccbCmdKickData, half: &KickHalf<'_>) -> PvrResult<()> {
    kick.context_fw_addr = half.ctx_fw_addr;
    kick.client_woff_update = half.write_offset;
    kick.client_wrap_mask_update = half.wrap_mask;
    kick.num_cleanup_ctl = 0;
    if let Some(hwrt) = half.hwrt {
        kick.cleanup_ctl_fw_addr[0] = hwrt.data.cleanup_state_fw_addr()?;
        kick.num_cleanup_ctl = 1;
    }
    Ok(())
}

/// Builds the `KICK` command that tells the firmware where one CCCB now ends.
///
/// Ports upstream `pvr_cccb_send_kccb_kick()`.
fn kick_command(half: &KickHalf<'_>) -> PvrResult<RogueFwifKccbCmd> {
    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = abi::KCCB_CMD_KICK;
    fill_kick_data(cmd.cmd_data.cmd_kick_data_mut(), half)?;
    Ok(cmd)
}

/// Builds the one command that starts a geometry job and a fragment job together.
///
/// Ports upstream `pvr_cccb_send_kccb_combined_kick()`. One KCCB command means the
/// firmware learns about both rings at once, which is what lets its partial-render
/// logic find a fragment job to run when the Parameter Manager runs out of memory
/// part-way through the geometry job — the reason pairing exists at all.
fn combined_kick_command(geom: &KickHalf<'_>, frag: &KickHalf<'_>) -> PvrResult<RogueFwifKccbCmd> {
    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = abi::KCCB_CMD_COMBINED_GEOM_FRAG_KICK;
    {
        let combined = cmd.cmd_data.combined_geom_frag_cmd_kick_data_mut();
        fill_kick_data(&mut combined.geom_cmd_kick_data, geom)?;
        fill_kick_data(&mut combined.frag_cmd_kick_data, frag)?;
    }
    Ok(cmd)
}

/// Writes one job's whole command sequence into the CCCB.
///
/// Ports upstream `pvr_queue_submit_job_to_cccb()`. The sequence is, in order:
/// the UFO waits split into `FENCE_PR` commands of at most
/// [`abi::CCB_CMD_MAX_UFOS`] each, the job command itself, and one `UPDATE`
/// command that writes this job's seqno to the timeline UFO. The `UPDATE` is what
/// makes the job's completion observable, so it must come last and must be part
/// of the same all-or-nothing write.
fn write_job_sequence(
    inner: &mut QueueInner,
    index: usize,
    ufos: &[RogueFwifUfo],
) -> PvrResult<()> {
    let (command, command_type, id, ufo_value) = {
        let job = &inner.pending[index];
        (job.command.clone(), job.command_type, job.id, job.ufo_value)
    };

    for chunk in ufos.chunks(abi::CCB_CMD_MAX_UFOS) {
        let mut bytes = Vec::with_capacity(size_of_val(chunk));
        for ufo in chunk {
            bytes.extend_from_slice(ufo.as_bytes());
        }
        inner
            .cccb
            .write_command(abi::CCB_CMD_TYPE_FENCE_PR, &bytes, 0, 0)?;
    }

    inner.cccb.write_command(command_type, &command, id, id)?;

    let update = RogueFwifUfo {
        addr: timeline_addr(inner),
        value: ufo_value,
    };
    inner
        .cccb
        .write_command(abi::CCB_CMD_TYPE_UPDATE, update.as_bytes(), 0, 0)
}

/// Firmware address of the timeline UFO the `UPDATE` command writes.
///
/// Threaded through [`QueueInner`] rather than read off the [`Queue`] because
/// [`write_job_sequence`] already holds the inner lock and taking the outer
/// reference as well would be a second borrow of the same object.
fn timeline_addr(inner: &QueueInner) -> u32 {
    inner.timeline_fw_addr
}

/// Bytes one job's command sequence occupies in a CCCB.
///
/// Ports upstream `job_cmds_size()`: one `FENCE_PR` command per full batch of
/// UFO waits, the job command, and one `UPDATE`.
pub fn job_cmds_size(command_len: usize, dep_count: usize) -> PvrResult<u32> {
    let command_len = u32::try_from(command_len).map_err(|_| PvrError::InvalidArgument)?;
    let waits = ufo_cmds_size(dep_count)?;
    let update = ufo_cmds_size(1)?;
    let job = Cccb::size_of_cmd_with_hdr(command_len)?;
    waits
        .checked_add(job)
        .and_then(|sum| sum.checked_add(update))
        .ok_or(PvrError::InvalidArgument)
}

/// Bytes `count` UFOs occupy once split into commands.
///
/// Ports upstream `ufo_cmds_size()`.
fn ufo_cmds_size(count: usize) -> PvrResult<u32> {
    let ufo = size_of::<RogueFwifUfo>();
    let full = count / abi::CCB_CMD_MAX_UFOS;
    let remainder = count % abi::CCB_CMD_MAX_UFOS;

    let per_full = Cccb::size_of_cmd_with_hdr((abi::CCB_CMD_MAX_UFOS * ufo) as u32)?;
    let mut size = u32::try_from(full)
        .ok()
        .and_then(|full| full.checked_mul(per_full))
        .ok_or(PvrError::InvalidArgument)?;
    if remainder != 0 {
        size = size
            .checked_add(Cccb::size_of_cmd_with_hdr((remainder * ufo) as u32)?)
            .ok_or(PvrError::InvalidArgument)?;
    }
    Ok(size)
}

/// The data master a job type runs on.
///
/// Ports upstream `get_dm_type()`. Transfer jobs share the fragment data master,
/// which is why `DRM_PVR_CTX_TYPE_TRANSFER_FRAG` is spelled the way it is.
fn dm_type(job_type: JobType) -> u32 {
    match job_type {
        JobType::Geometry => abi::FWIF_DM_GEOM,
        JobType::Fragment | JobType::Transfer => abi::FWIF_DM_FRAG,
        JobType::Compute => abi::FWIF_DM_CDM,
    }
}

/// Size of the context state the firmware saves a switch into.
///
/// Ports upstream `get_ctx_state_size()` and the two helpers behind it. The
/// fragment and transfer sizes depend on how many ISP store registers the GPU
/// has, which is a valued feature; a device whose firmware does not report it is
/// [`PvrError::Unsupported`], matching upstream's `WARN_ON` plus error return.
fn ctx_state_size(device: &PvrDevice, job_type: JobType) -> PvrResult<usize> {
    let features = &device.info.features;
    let values = &device.info.values;

    let isp_store_registers = |for_fragment: bool| -> PvrResult<u32> {
        if features.xe_memory_hierarchy {
            if !for_fragment {
                // Upstream's transfer path takes one register on an XE part
                // without consulting any feature.
                return Ok(1);
            }
            if !features.num_raster_pipes {
                ostd::error!("firmware does not report the raster pipe count");
                return Err(PvrError::Unsupported);
            }
            let mut count = values.num_raster_pipes;
            if features.gpu_multicore_support {
                if !features.xpu_max_slaves {
                    ostd::error!("multicore GPU does not report its slave count");
                    return Err(PvrError::Unsupported);
                }
                count = count
                    .checked_mul(1 + values.xpu_max_slaves)
                    .ok_or(PvrError::InvalidArgument)?;
            }
            Ok(count)
        } else {
            if !features.num_isp_ipp_pipes {
                ostd::error!("firmware does not report the ISP IPP pipe count");
                return Err(PvrError::Unsupported);
            }
            Ok(values.num_isp_ipp_pipes)
        }
    };

    match job_type {
        JobType::Geometry => Ok(size_of::<RogueFwifGeomCtxState>()),
        JobType::Compute => Ok(size_of::<RogueFwifComputeCtxState>()),
        JobType::Fragment | JobType::Transfer => {
            let registers = isp_store_registers(job_type == JobType::Fragment)?;
            // `frag_reg_isp_store` is a zero-length trailing array, so the
            // structure's size is the fixed part and the registers follow it.
            size_of::<RogueFwifFragCtxState>()
                .checked_add(
                    (registers as usize)
                        .checked_mul(size_of::<u32>())
                        .ok_or(PvrError::InvalidArgument)?,
                )
                .ok_or(PvrError::InvalidArgument)
        }
    }
}

/// Writes the initial contents of a queue's register-state object.
///
/// Ports upstream `reg_state_init()`. Only a geometry queue has anything to put
/// there: the VDM call stack pointer the client asked for.
fn init_reg_state(reg_state: &FwObject, job_type: JobType, callstack_addr: u64) -> PvrResult<()> {
    reg_state.zero()?;
    if job_type == JobType::Geometry {
        let offset = core::mem::offset_of!(RogueFwifGeomCtxState, geom_core)
            + core::mem::offset_of!(
                crate::rogue::fwif::RogueFwifGeomCtxStatePerGeom,
                geom_reg_vdm_call_stack_pointer_init
            );
        reg_state.write_val(offset, &callstack_addr)?;
    }
    Ok(())
}

/// Runs [`Queue::process`] for every live queue on a device.
///
/// A GPU interrupt covers the device rather than an individual queue, so the
/// deferred handler must inspect each live queue. Dead registry entries are pruned.
pub fn process_all(device: &PvrDevice) {
    let queues = device.sched.live_queues();
    for queue in queues {
        queue.process();
    }
}

/// Runs [`Queue::check_timeout`] for every live queue on a device.
///
/// [`crate::service`] calls this periodically from a sleepable context, so a
/// submitted job can time out even if its client makes no further ioctl calls.
pub fn check_all_timeouts(device: &PvrDevice) {
    let queues = device.sched.live_queues();
    for queue in queues {
        queue.check_timeout();
    }
}

/// Stops every live queue on a device, before a GPU reset.
///
/// Ports upstream `pvr_queue_device_pre_reset()`. Upstream walks its idle list
/// and then its active list; this port keeps one list (see [`SchedState`]), so
/// the two loops are one.
pub fn stop_all(device: &PvrDevice) {
    for queue in device.sched.live_queues() {
        queue.stop();
    }
}

/// Restarts every live queue after a GPU reset.
///
/// Ports upstream `pvr_queue_device_post_reset()`. Called on the failure path
/// too, exactly as upstream calls it before returning through `err_device_lost`:
/// the jobs on these queues still hold fences, and fence-completion contract says every one of
/// them must be signalled rather than left pending.
pub fn start_all(device: &PvrDevice) {
    for queue in device.sched.live_queues() {
        queue.start();
    }
}

/// Marks the context the firmware reset as faulty.
///
/// Called from [`crate::sched::ccb::process_fwccb`] when the firmware reports a
/// context reset. The jobs on it are not failed here: the firmware may still
/// advance the timeline for the ones it did finish, and the next
/// [`Queue::check_timeout`] terminates the rest. What must happen immediately is
/// that no *new* job goes to a context the firmware has given up on.
pub fn mark_context_faulty(device: &PvrDevice, context_id: u32) {
    match device.sched.context_by_id(context_id) {
        Some(context) => {
            context.mark_faulty();
            crate::service::retire_context(device, context);
        }
        None => ostd::warn!("firmware reset context {context_id}, which this driver does not know"),
    }
}

/// Device-level submission state.
///
/// Holds the command channels, queue registry, and context/job id allocators.
/// Unlike upstream's separate active and idle lists, this registry contains all
/// live queues; processing an empty queue still incurs its state checks.
pub struct SchedState {
    /// Every queue that exists, weakly.
    ///
    /// Weak so that the device does not keep a context alive: a queue outlives
    /// its handle only while jobs are in flight, and that lifetime belongs to the
    /// [`Context`], not here.
    queues: SpinLock<Vec<Weak<Queue>>, BottomHalfDisabled>,
    /// Contexts by the id the firmware reports, weakly, for reset notifications.
    contexts: SpinLock<Vec<(u32, Weak<Context>)>, BottomHalfDisabled>,
    /// Next context id. Upstream's `pvr_dev->ctx_ids` xarray, which allocates
    /// from one; zero is left unused so that a zeroed firmware structure cannot
    /// look like a valid context.
    next_context_id: AtomicU32,
    /// Nonzero firmware trace reference. Fence identity has its own 64-bit sequence.
    next_job_id: AtomicU32,
}

impl Default for SchedState {
    /// The state of a device with nothing submitted to it.
    ///
    /// Hand-written because `ostd`'s [`SpinLock`] has no `Default`, which is
    /// deliberate on its part: a lock's default would have to invent an initial
    /// value for what it guards.
    fn default() -> Self {
        Self {
            queues: SpinLock::new(Vec::new()),
            contexts: SpinLock::new(Vec::new()),
            next_context_id: AtomicU32::new(0),
            next_job_id: AtomicU32::new(0),
        }
    }
}

impl SchedState {
    /// Records a new queue.
    pub fn register_queue(&self, queue: &Arc<Queue>) {
        let mut queues = self.queues.lock();
        queues.retain(|weak| weak.strong_count() > 0);
        queues.push(Arc::downgrade(queue));
    }

    /// Every queue that is still alive, pruning the ones that are not.
    pub fn live_queues(&self) -> Vec<Arc<Queue>> {
        let mut queues = self.queues.lock();
        queues.retain(|weak| weak.strong_count() > 0);
        queues.iter().filter_map(Weak::upgrade).collect()
    }

    /// Records a new context and returns the id the firmware will know it by.
    pub fn register_context(&self, id: u32, context: &Arc<Context>) {
        let mut contexts = self.contexts.lock();
        contexts.retain(|(_, weak)| weak.strong_count() > 0);
        contexts.push((id, Arc::downgrade(context)));
    }

    /// Looks a context up by the id the firmware reports.
    pub fn context_by_id(&self, id: u32) -> Option<Arc<Context>> {
        let mut contexts = self.contexts.lock();
        contexts.retain(|(_, weak)| weak.strong_count() > 0);
        contexts
            .iter()
            .find(|(candidate, _)| *candidate == id)
            .and_then(|(_, weak)| weak.upgrade())
    }

    /// Allocates a context id. Never zero, and never reused.
    pub fn next_context_id(&self) -> PvrResult<u32> {
        self.next_context_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map(|value| value + 1)
            .map_err(|_| PvrError::NoMemory)
    }

    /// Allocates a nonzero diagnostic job reference, wrapping after the 32-bit range.
    /// This is not a registry key or a fence identity.
    pub fn next_job_id(&self) -> u32 {
        let previous = self
            .next_job_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.wrapping_add(1).max(1))
            })
            .expect("job reference always advances");
        previous.wrapping_add(1).max(1)
    }
}

#[cfg(ktest)]
mod id_tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn context_ids_exhaust_without_zero_reuse_or_counter_wrap() {
        let state = SchedState::default();
        state.next_context_id.store(u32::MAX - 1, Ordering::Relaxed);
        assert_eq!(state.next_context_id(), Ok(u32::MAX));
        assert_eq!(state.next_context_id(), Err(PvrError::NoMemory));
        assert_eq!(state.next_context_id(), Err(PvrError::NoMemory));
        assert_eq!(state.next_context_id.load(Ordering::Relaxed), u32::MAX);
        state.next_job_id.store(u32::MAX - 1, Ordering::Relaxed);
        assert_eq!(state.next_job_id(), u32::MAX);
        assert_eq!(state.next_job_id(), 1);
    }
}
