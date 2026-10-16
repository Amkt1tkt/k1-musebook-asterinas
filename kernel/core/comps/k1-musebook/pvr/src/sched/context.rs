// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: GPU context resources and safe teardown.

//! Rendering contexts.
//!
//! Ports `pvr_context.c` and `pvr_context.h` (715 lines).
//!
//! A context is an address space plus the firmware state for one to two queues.
//! Its lifetime is the interesting part: the handle a client holds is only one
//! reference, and the firmware may still be running work out of the context's
//! client CCBs after that handle is closed. So closing the handle *kills* the
//! queues — nothing new may be queued — and the teardown that frees firmware
//! memory happens when the last job has gone, not when the handle does.

use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::offset_of,
    sync::atomic::{AtomicBool, Ordering},
};

use k1_musebook_drm::DrmError;
use ostd_pod::FromZeros;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwObject},
    mm::vm::{FwMemContext, VmContext},
    rogue::fwif::{FwAddr, RogueFwifKccbCmd},
    sched::{
        abi::{
            self, RogueFwifFwcomputecontext, RogueFwifFwrendercontext, RogueFwifFwtransfercontext,
        },
        job::JobType,
        queue::{Queue, QueueParams},
        stream::{self, StreamType},
    },
};

/// What kind of work a context accepts.
///
/// Discriminants are ABI, from `enum drm_pvr_ctx_type`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextType {
    /// Geometry and fragment rendering.
    Render = 0,
    /// Compute.
    Compute = 1,
    /// Transfer/blit.
    TransferFrag = 2,
}

impl ContextType {
    /// Reads a `drm_pvr_ctx_type` discriminant.
    pub fn from_uapi(value: u32) -> PvrResult<Self> {
        match value {
            0 => Ok(Self::Render),
            1 => Ok(Self::Compute),
            2 => Ok(Self::TransferFrag),
            _ => Err(PvrError::InvalidArgument),
        }
    }

    /// The job types this context runs, in the order upstream creates them.
    ///
    /// Ports upstream `pvr_context_create_queues()`. A render context gets
    /// geometry first and fragment second, which matters only in that a failure
    /// creating the second must tear the first down.
    pub fn job_types(self) -> &'static [JobType] {
        match self {
            Self::Render => &[JobType::Geometry, JobType::Fragment],
            Self::Compute => &[JobType::Compute],
            Self::TransferFrag => &[JobType::Transfer],
        }
    }

    /// Whether a job of this type may be submitted to this kind of context.
    ///
    /// Ports the `switch (ctx->type)` in upstream `pvr_queue_create()` and the
    /// matching check in each `pvr_*_job_fw_cmd_init()`.
    pub fn accepts(self, job_type: JobType) -> bool {
        self.job_types().contains(&job_type)
    }

    /// Size of the firmware structure a context of this type needs.
    ///
    /// Ports upstream `get_fw_obj_size()`.
    fn fw_object_size(self) -> usize {
        match self {
            Self::Render => size_of::<RogueFwifFwrendercontext>(),
            Self::Compute => size_of::<RogueFwifFwcomputecontext>(),
            Self::TransferFrag => size_of::<RogueFwifFwtransfercontext>(),
        }
    }

    /// Offset of a job type's common context within the firmware structure.
    ///
    /// Ports upstream `get_ctx_offset()`.
    fn ctx_offset(self, job_type: JobType) -> PvrResult<u32> {
        let offset = match (self, job_type) {
            (Self::Render, JobType::Geometry) => {
                offset_of!(RogueFwifFwrendercontext, geom_context)
            }
            (Self::Render, JobType::Fragment) => {
                offset_of!(RogueFwifFwrendercontext, frag_context)
            }
            (Self::Compute, JobType::Compute) => {
                offset_of!(RogueFwifFwcomputecontext, cdm_context)
            }
            (Self::TransferFrag, JobType::Transfer) => {
                offset_of!(RogueFwifFwtransfercontext, tq_context)
            }
            _ => return Err(PvrError::InvalidArgument),
        };
        u32::try_from(offset).map_err(|_| PvrError::InvalidArgument)
    }

    /// Offset and stream type of the static context state, if this kind has any.
    ///
    /// Ports upstream `init_render_fw_objs()`, `init_compute_fw_objs()` and
    /// `init_transfer_fw_objs()`. The transfer case is `None`, and upstream turns
    /// that into "a non-zero `static_context_state_len` is `EINVAL`" — which is
    /// what [`Context::new`] does with it.
    fn static_state(self) -> Option<(usize, StreamType)> {
        match self {
            Self::Render => Some((
                offset_of!(RogueFwifFwrendercontext, static_render_context_state),
                StreamType::StaticRenderContext,
            )),
            Self::Compute => Some((
                offset_of!(RogueFwifFwcomputecontext, static_compute_context_state),
                StreamType::StaticComputeContext,
            )),
            Self::TransferFrag => None,
        }
    }
}

/// Scheduling priority as the *firmware* understands it.
///
/// Upstream `enum pvr_context_priority`, whose values are 0, 1 and 2 — not the
/// uapi's -512, 0 and 512, which are [`crate::uapi::CtxPriority`]. The two are
/// deliberately different types with one conversion between them
/// ([`Self::from_uapi`]), because writing a uapi value into
/// [`crate::rogue::fwif::RogueFwifFwcommoncontext::priority`] would give the
/// firmware a priority it has no case for and the symptom would be a scheduling
/// decision nobody could explain.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Below normal. Upstream `PVR_CTX_PRIORITY_LOW`.
    Low = 0,
    /// The default. Upstream `PVR_CTX_PRIORITY_MEDIUM`.
    Medium = 1,
    /// Above normal. Upstream `PVR_CTX_PRIORITY_HIGH`.
    High = 2,
}

impl Priority {
    /// Maps a `drm_pvr_ctx_priority` onto the firmware's scale.
    ///
    /// Ports upstream `remap_priority()`.
    ///
    /// Divergence: upstream gates `DRM_PVR_CTX_PRIORITY_HIGH` on `CAP_SYS_NICE`
    /// or being the DRM master. `k1-musebook-drm` hands a driver-private ioctl handler
    /// no capability set and this is a render-only driver with no master, so the
    /// check cannot be made here. `high_allowed` carries the decision in from
    /// wherever it *can* be made; the only current caller passes `false`, so a
    /// client asking for high priority gets EOPNOTSUPP; this differs from
    /// upstream's EACCES until the shared error mapping gains that variant. Recorded in `ABI.md` compatibility limits.
    pub fn from_uapi(value: i32, high_allowed: bool) -> PvrResult<Self> {
        match value {
            v if v == crate::uapi::CtxPriority::Low as i32 => Ok(Self::Low),
            v if v == crate::uapi::CtxPriority::Normal as i32 => Ok(Self::Medium),
            v if v == crate::uapi::CtxPriority::High as i32 => {
                if high_allowed {
                    Ok(Self::High)
                } else {
                    // Upstream's `-EACCES`. `PvrError` has no variant for it and
                    // adding one to reach a path no caller can take yet would be
                    // worse than the nearest honest answer.
                    Err(PvrError::Unsupported)
                }
            }
            _ => Err(PvrError::InvalidArgument),
        }
    }
}

/// What [`Context::build`] produces.
///
/// The error carries the queues that *were* built, because a queue owns three
/// firmware-heap allocations and dropping its `Arc` frees none of them: the
/// caller has to release them explicitly or the address space is gone for the
/// device's lifetime.
type BuildResult = Result<Vec<Arc<Queue>>, (PvrError, Vec<Arc<Queue>>)>;

/// What [`Context::build`] needs, gathered so that the signature stays readable.
///
/// A parameter object rather than ten arguments: the list is long because a
/// context's firmware structure is described by its type, its priority, its
/// address space and its client's static state all at once, and nothing here is
/// derivable from the rest.
struct BuildParams<'a> {
    /// The device.
    device: &'a Arc<PvrDevice>,
    /// Its firmware state, for the heap and the page table.
    fw: &'a crate::fw::FwDevice,
    /// The context structure being filled in.
    fw_object: &'a FwObject,
    /// What kind of context it is.
    r#type: ContextType,
    /// Its priority, on the firmware's scale.
    priority: Priority,
    /// The client's static context state stream, possibly empty.
    static_context_state: &'a [u8],
    /// The initial VDM call stack pointer.
    callstack_addr: u64,
    /// The id the firmware will know the context by.
    id: u32,
    /// The fault flag shared with the queues.
    faulty: &'a Arc<AtomicBool>,
    /// The firmware's view of the address space.
    fw_mem_context: &'a FwMemContext,
}

/// A rendering context.
pub struct Context {
    /// The address space this context's work runs in.
    pub vm: Arc<VmContext>,
    /// What kind of work it takes.
    pub r#type: ContextType,
    /// Its priority.
    pub priority: Priority,
    /// The device it belongs to.
    device: Arc<PvrDevice>,
    /// The firmware structure describing the context and its queues.
    fw_object: FwObject,
    /// One queue per job type this context accepts.
    queues: Vec<Arc<Queue>>,
    /// The id the firmware reports in a reset notification.
    id: u32,
    /// Set when the firmware has given up on this context, or a job of its has
    /// timed out. Shared with the queues, which is where it is set.
    faulty: Arc<AtomicBool>,
}

impl Context {
    /// Creates a context and registers it with the firmware.
    ///
    /// Ports upstream `pvr_context_create()`. The order differs in one place and
    /// deliberately: upstream builds its queues against a scratch copy of the
    /// firmware structure and creates the real allocation afterwards, because
    /// `pvr_fw_object_create()` takes an initialiser callback. Here the allocation
    /// comes first and the queues write into it, which is the same bytes in the
    /// same places and means a queue can be told its own firmware address instead
    /// of having to be patched later.
    ///
    /// # Errors
    ///
    /// - [`PvrError::InvalidArgument`] for an unknown type or priority, a
    ///   `callstack_addr` on a context that has no call stack, or a static state
    ///   stream that fails [`crate::sched::stream::validate`].
    /// - [`PvrError::InvalidArgument`] when a render or compute context supplies
    ///   no static state at all, which upstream also refuses: the firmware would
    ///   context-switch into zeroed registers.
    /// - [`PvrError::Unsupported`] if the firmware is not loaded, or for a
    ///   high-priority request; see [`Priority::from_uapi`].
    pub fn new(
        device: &Arc<PvrDevice>,
        vm: Arc<VmContext>,
        r#type: ContextType,
        priority: Priority,
        static_context_state: &[u8],
        callstack_addr: u64,
    ) -> PvrResult<Arc<Self>> {
        device.check_alive()?;
        let fw = device.fw.get().ok_or(PvrError::Unsupported)?;

        // Upstream refuses a call stack on anything but a render context, in
        // `pvr_queue_create()`. Checked here as well so that the error does not
        // depend on which queue happens to be built first.
        if callstack_addr != 0 && r#type != ContextType::Render {
            return Err(PvrError::InvalidArgument);
        }

        // The static state has to be all-or-nothing per upstream: a render or
        // compute context without it is refused, a transfer context with it is.
        let static_state = r#type.static_state();
        match (static_state, static_context_state.is_empty()) {
            (Some(_), true) | (None, false) => return Err(PvrError::InvalidArgument),
            _ => {}
        }

        let id = device.sched.next_context_id()?;
        let faulty = Arc::new(AtomicBool::new(false));
        // Owned by the VM context (upstream `pvr_vm_context::fw_mem_ctx_obj`).
        // Every rendering context on one address space names the same one.
        let fw_mem_context = vm.fw_mem_context().ok_or(PvrError::InvalidArgument)?;

        let fw_object = {
            let mut heap = fw.heap.lock();
            let allocation = heap.alloc_guard(r#type.fw_object_size(), FwCache::DEVICE_UNCACHED)?;
            allocation.object().zero()?;
            allocation.commit()
        };

        // From here on every early return has to undo what came before it: the
        // firmware heap is not garbage collected and a leaked allocation is
        // permanent for the device's lifetime. `build` does the fallible work and
        // this function does the unwinding, which keeps the two apart.
        match Self::build(BuildParams {
            device,
            fw,
            fw_object: &fw_object,
            r#type,
            priority,
            static_context_state,
            callstack_addr,
            id,
            faulty: &faulty,
            fw_mem_context,
        }) {
            Ok(queues) => {
                let context = Arc::new(Self {
                    vm,
                    r#type,
                    priority,
                    device: device.clone(),
                    fw_object,
                    queues,
                    id,
                    faulty,
                });
                for queue in &context.queues {
                    device.sched.register_queue(queue);
                }
                device.sched.register_context(id, &context);
                Ok(context)
            }
            Err((err, queues)) => {
                let mut heap = fw.heap.lock();
                for queue in queues {
                    queue.rollback(&mut heap, &fw.mips.page_table);
                }
                heap.free(&fw_object);
                Err(err)
            }
        }
    }

    /// The fallible half of [`Self::new`].
    ///
    /// Returns the queues it managed to build alongside the error, so that the
    /// caller can release them: a queue owns three firmware-heap allocations and
    /// dropping the `Arc` frees none of them.
    fn build(params: BuildParams<'_>) -> BuildResult {
        let mut queues: Vec<Arc<Queue>> = Vec::new();
        match Self::build_step(&params, &mut queues) {
            Ok(()) => Ok(queues),
            Err(err) => Err((err, queues)),
        }
    }

    /// The body of [`Self::build`], so that every `?` unwinds to one place.
    fn build_step(params: &BuildParams<'_>, queues: &mut Vec<Arc<Queue>>) -> PvrResult<()> {
        let BuildParams {
            device,
            fw,
            fw_object,
            r#type,
            priority,
            static_context_state,
            callstack_addr,
            id,
            faulty,
            fw_mem_context,
        } = *params;

        for job_type in r#type.job_types() {
            let ctx_offset = r#type.ctx_offset(*job_type)?;
            let ctx_fw_addr = fw_object.fw_addr_at(ctx_offset)?;
            let queue = Queue::new(QueueParams {
                device: device.clone(),
                job_type: *job_type,
                context_type: r#type,
                priority,
                context_id: id,
                faulty: faulty.clone(),
                ctx_fw_addr,
                ctx_offset,
                fw_mem_context: fw_mem_context.fw_addr(),
                callstack_addr,
                heap: &mut fw.heap.lock(),
            })?;

            // The queue's slice of the firmware structure.
            // Cover the current queue before fallible initialization or mapping.
            queues.push(queue);
            let queue = queues.last().expect("just added the current queue");
            let cctx = queue.fw_common_context(priority, fw_mem_context.fw_addr())?;
            fw_object.write_val(ctx_offset as usize, &cctx)?;

            // The queue maps all its firmware-visible allocations, including the client
            // CCB control structure and ring protected by its internal lock.
            queue.map_fw_objects(&fw.mips.page_table)?;
        }

        // Upstream's `init_fw_objs()`: the static context state, validated through
        // the same table-driven path as a job command stream.
        if let Some((offset, stream_type)) = r#type.static_state() {
            let state = stream::validate(stream_type, static_context_state, &device.info)?;
            fw_object.write_bytes(offset, &state)?;
        }

        // The context structure itself is firmware-visible.
        fw.mips.page_table.map(fw_object)?;
        Ok(())
    }

    /// The id the firmware knows this context by.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Firmware address of the context structure.
    pub fn fw_addr(&self) -> FwAddr {
        self.fw_object.fw_addr()
    }

    /// The firmware structure, for the tests that read it back.
    ///
    /// Not part of the driver's interface: the structure is the firmware's to
    /// read, and a caller with a reason to look at it is checking this module's
    /// own work.
    #[cfg(ktest)]
    pub(crate) fn fw_object_for_test(&self) -> &FwObject {
        &self.fw_object
    }

    /// The queue a job of this type goes to.
    ///
    /// Ports upstream `pvr_context_get_queue_for_job()`. `None` when the context
    /// does not accept that kind of work, which is the caller's `EINVAL`.
    pub fn queue_for_job(&self, job_type: JobType) -> Option<&Arc<Queue>> {
        self.queues
            .iter()
            .find(|queue| queue.job_type() == job_type)
    }

    /// Every queue this context owns.
    pub fn queues(&self) -> &[Arc<Queue>] {
        &self.queues
    }

    /// Whether the context has become unusable.
    ///
    /// Set by a job timeout or by a firmware reset notification. Once set, every
    /// submission to the context fails: its firmware state describes work that
    /// was abandoned partway and no client can reason about it any more.
    pub fn is_faulty(&self) -> bool {
        self.faulty.load(Ordering::Acquire)
    }

    /// Marks the context unusable. Ports upstream's `atomic_set(&ctx->faulty, 1)`.
    pub fn mark_faulty(&self) {
        self.faulty.store(true, Ordering::Release);
    }

    /// Makes the queues refuse new work without waiting for the old.
    ///
    /// Ports upstream `pvr_context_kill_queues()`, which `pvr_context_destroy()`
    /// calls before dropping the handle's reference. Separate from
    /// [`Self::destroy`] because closing a handle must not block on the GPU.
    pub fn kill(&self) {
        for queue in &self.queues {
            queue.kill();
        }
    }

    /// Tears the context down.
    ///
    /// Must wait for in-flight work: destroying a context whose jobs the firmware
    /// is still running leaves the firmware writing into freed memory. On a lost
    /// device, abandon the wait rather than hanging the caller.
    ///
    /// Ports upstream `pvr_context_destroy_queues()` and
    /// `pvr_queue_cleanup_fw_context()`. Best effort by construction: every step
    /// is reported and none of them stops the next, because the alternative to
    /// finishing the teardown is leaking firmware address space.
    pub fn destroy(&self) -> PvrResult<()> {
        self.kill();

        let mut result = Ok(());
        let lost = self.device.check_alive().is_err();

        for queue in &self.queues {
            if lost {
                // Nothing will advance the timeline again, so waiting would be a
                // hang and leaving the fences pending would violate fence completion.
                queue.fail_all(DrmError::Io);
                continue;
            }
            if let Err(err) = queue.drain() {
                ostd::error!("a queue did not drain during context teardown: {err:?}");
                result = Err(err);
            }
        }

        // Only once the queues are empty: the cleanup request tells the firmware
        // to stop referring to the common context, and it refuses while the
        // context is busy.
        if !lost {
            for queue in &self.queues {
                if let Err(err) = self.cleanup_fw_context(queue.ctx_offset()) {
                    ostd::error!("firmware refused to release a common context: {err:?}");
                    result = Err(err);
                }
            }
        }

        result
    }

    /// Asks the firmware to stop referring to one common context.
    ///
    /// Ports upstream `pvr_fw_structure_cleanup()` for
    /// `ROGUE_FWIF_CLEANUP_FWCOMMONCONTEXT`. `CLEANUP_BUSY` in the answer means
    /// the firmware still has work referring to it, which after a successful
    /// drain should not happen and is therefore reported rather than retried.
    fn cleanup_fw_context(&self, ctx_offset: u32) -> PvrResult<()> {
        let mut cmd = RogueFwifKccbCmd::new_zeroed();
        cmd.cmd_type = abi::KCCB_CMD_CLEANUP;
        {
            let cleanup = cmd.cmd_data.cleanup_data_mut();
            cleanup.cleanup_type = abi::CLEANUP_FWCOMMONCONTEXT;
            *cleanup.cleanup_data.context_fw_addr_mut() = self.fw_object.fw_addr_at(ctx_offset)?;
        }

        let rtn = crate::sched::ccb::send_and_wait(&self.device, &cmd)?;
        if rtn & abi::KCCB_RTN_SLOT_CLEANUP_BUSY != 0 {
            return Err(PvrError::Busy);
        }
        Ok(())
    }
}

impl Drop for Context {
    /// Releases this context's firmware-heap allocations without waiting for GPU work.
    ///
    /// [`Self::destroy`] must run first. Explicit destruction performs that cleanup
    /// inline; file closure transfers ownership to [`crate::service`] until it returns.
    /// A fence holds only a weak queue reference, so fence lifetime alone cannot prove
    /// that firmware has stopped using these allocations.
    ///
    /// Release queues before the context structure. The firmware memory context belongs
    /// to the [`VmContext`] and is released with that address space.
    fn drop(&mut self) {
        // A device whose firmware has gone away has no heap to return anything
        // to, and the whole heap goes with it.
        let Some(fw) = self.device.fw.get() else {
            return;
        };
        let mut heap = fw.heap.lock();
        for queue in &self.queues {
            queue.release(&mut heap, &fw.mips.page_table);
        }
        // The context structure was mapped alongside the queues, so it comes out
        // of the page table before it goes back to the heap too.
        let _ = fw.mips.page_table.unmap(&self.fw_object);
        heap.free(&self.fw_object);
    }
}

impl core::fmt::Debug for Context {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Context")
            .field("id", &self.id)
            .field("type", &self.r#type)
            .field("priority", &self.priority)
            .field("queues", &self.queues.len())
            .field("faulty", &self.is_faulty())
            .finish_non_exhaustive()
    }
}

/// The bytes a validated static render context state occupies.
///
/// A `const` assertion rather than a comment because
/// [`ContextType::static_state`] writes [`stream::validate`]'s output at a fixed
/// offset, and a mismatch would silently overwrite the field after it.
const _: () = {
    use crate::rogue::fwif::{
        RogueFwifCdmRegistersCswitch, RogueFwifGeomRegistersCaswitch,
        RogueFwifStaticComputecontextState, RogueFwifStaticRendercontextState,
    };

    // Upstream writes the render stream at `&static_render_context_state
    // ->ctxswitch_regs[0]`, which is the start of the structure, and the stream
    // fills one `caswitch` block of the two the structure holds.
    assert!(offset_of!(RogueFwifStaticRendercontextState, ctxswitch_regs) == 0);
    assert!(
        size_of::<RogueFwifGeomRegistersCaswitch>() * 2
            == size_of::<RogueFwifStaticRendercontextState>()
    );

    assert!(offset_of!(RogueFwifStaticComputecontextState, ctxswitch_regs) == 0);
    assert!(
        size_of::<RogueFwifCdmRegistersCswitch>()
            == size_of::<RogueFwifStaticComputecontextState>()
    );
};
