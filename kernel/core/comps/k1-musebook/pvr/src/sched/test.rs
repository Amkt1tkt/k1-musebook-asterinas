// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: GPU scheduling and command-stream regression tests.

//! In-kernel tests for the submission path.
//!
//! The ring-wrap tests live next to the ring in [`crate::sched::cccb`] and the
//! command-stream ones next to the definition tables in
//! [`crate::sched::stream`], because neither needs a device. What is here needs
//! one: context lifetime, batch atomicity and the scheduler's fence behaviour.
//!
//! # What "no hardware" means for these
//!
//! The device is built on a simulated register window and the firmware image
//! fixture, so everything up to and including "the command bytes are in the CCCB
//! and the MTS register has been kicked" really happens. What cannot happen is
//! the firmware *answering*: nothing advances a timeline UFO and nothing writes a
//! KCCB return slot. So retirement is driven through
//! [`Queue::retire`], which writes the UFO the way the firmware would, and the
//! two paths that wait for a KCCB answer — a context's firmware cleanup and
//! [`crate::sched::ccb::send_and_wait`] — are exercised only in their failure
//! direction, on a device that has been declared lost.
//!
//! # One value the device-info fixture does not carry
//!
//! `crate::device_info::fixture` reports neither `xe_memory_hierarchy` nor
//! `num_isp_ipp_pipes`, and [`super::queue`] needs one of the two to size the
//! context-state object of a fragment or transfer queue — so a render or
//! transfer context on the bare fixture is refused before it is built. Rather
//! than invent a value for the shared fixture, whose numbers are meant to be
//! re-derived from the real firmware image, [`device`] patches it in here; the
//! same thing [`crate::render::test`] does, for the same reason.

use alloc::{sync::Arc, vec::Vec};
use core::mem::offset_of;

use k1_musebook_drm::{
    DrmError,
    fence::{Fence, FenceStatus, SoftwareFence},
};
use ostd::prelude::*;
use ostd_pod::IntoBytes;

use super::{
    context::{Context, ContextType, Priority},
    job::{Dep, Job, JobType},
    queue::JOB_TIMEOUT_NS,
    stream::StreamType,
    *,
};
use crate::{
    device::{GpuId, RegisterWindow},
    driver::PvrFile,
    error::PvrError,
    fw::{self, FwInitParams},
    mm::vm::{VmContext, VmContextKind},
    power::ResetKind,
    uapi,
};

/// An in-memory device with firmware loaded and command channels initialized.
///
/// The fixture uses the production probe/load/submission initialization order
/// without executing the platform's MMIO power sequence.
fn device_without_ccbs() -> Arc<PvrDevice> {
    let image = fw::fixture::image();
    let mut validated = fw::validate_and_init_device_info(&image).expect("fixture image");
    // See the module documentation: one ISP IPP pipe, the smallest a part can
    // have, so that a fragment or transfer queue has a context-state size.
    validated.info.features.num_isp_ipp_pipes = true;
    // BXE-2-32's count from the firmware device-info block.
    validated.info.values.num_isp_ipp_pipes = 2;
    let regs = RegisterWindow::simulated();
    regs.write64(
        crate::rogue::cr::core_id_pbvnc::OFFSET,
        GpuId::K1_BXE_2_32.to_packed(),
    )
    .expect("simulated register");
    let device = PvrDevice::probe(regs, &validated).expect("probe");

    fw::init(&device, image, FwInitParams::default()).expect("firmware load");
    device
}

fn device() -> Arc<PvrDevice> {
    let device = device_without_ccbs();
    init(&device).expect("submission path");
    device
}

/// A file with one VM context in it, and that context.
fn file_with_vm(device: &Arc<PvrDevice>) -> (Arc<PvrFile>, Arc<VmContext>, u32) {
    let file = PvrFile::new(device.clone());
    let vm = VmContext::new(device.clone(), VmContextKind::User).expect("vm context");
    let handle = file.vm_contexts.insert(vm.clone()).expect("handle");
    (file, vm, handle)
}

/// A static compute context state stream a client would send.
fn compute_static_state(device: &PvrDevice) -> Vec<u8> {
    stream::build_stream(
        StreamType::StaticComputeContext,
        &device.info,
        &[1, 2, 3, 4, 5, 6, 7],
    )
    .expect("seven registers")
}

/// A compute command stream a client would send.
fn compute_command(device: &PvrDevice) -> Vec<u8> {
    // Four fields are present on the fixture's BVNC; see the stream tests.
    let count = stream::main_stream_field_count(StreamType::Compute, &device.info);
    let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
    stream::build_stream(StreamType::Compute, &device.info, &values).expect("field count")
}

/// Creates a compute context on `vm`.
fn compute_context(device: &Arc<PvrDevice>, vm: &Arc<VmContext>) -> Arc<Context> {
    Context::new(
        device,
        vm.clone(),
        ContextType::Compute,
        Priority::Medium,
        &compute_static_state(device),
        0,
    )
    .expect("compute context")
}

/// Builds a `SUBMIT_JOBS` array image for `count` compute jobs on one context.
///
/// Returns the array descriptor and its bytes, so that the test goes through
/// [`uapi::obj_array`]'s stride negotiation rather than around it.
fn submit_array(context_handle: u32, streams: &[Vec<u8>]) -> (uapi::ObjArray, Vec<u8>) {
    submit_array_with_sync(context_handle, streams, 0)
}

/// [`submit_array`] with a non-empty sync-operation array count on every job.
///
/// The operation bytes themselves are handed to [`job::prepare_batch`]
/// separately; only the count has to match so the stride check accepts them.
fn submit_array_with_sync(
    context_handle: u32,
    streams: &[Vec<u8>],
    sync_op_count: u32,
) -> (uapi::ObjArray, Vec<u8>) {
    let mut raw = Vec::new();
    for stream in streams {
        let job = uapi::Job {
            r#type: JobType::Compute as u32,
            context_handle,
            flags: 0,
            cmd_stream_len: stream.len() as u32,
            // A fake user pointer: nothing dereferences it, because the ioctl
            // handler copies the stream in and hands it over separately.
            cmd_stream: 0x1000,
            sync_ops: uapi::ObjArray {
                stride: uapi::SyncOp::MIN_STRIDE as u32,
                count: sync_op_count,
                array: 0x3000,
            },
            hwrt: uapi::HwrtDataRef {
                set_handle: 0,
                data_index: 0,
            },
        };
        raw.extend_from_slice(job.as_bytes());
    }

    let array = uapi::ObjArray {
        stride: size_of::<uapi::Job>() as u32,
        count: streams.len() as u32,
        array: 0x2000,
    };
    (array, raw)
}

/// Sync-operation array image, as the ioctl handler would hand it over.
fn sync_op_bytes(ops: &[uapi::SyncOp]) -> Vec<u8> {
    let mut raw = Vec::new();
    for op in ops {
        raw.extend_from_slice(op.as_bytes());
    }
    raw
}

/// Prepares a batch without sync operations.
///
/// The sync-object table and per-job operation arrays are empty. The returned
/// `SignalArray` has no fences to publish.
fn prepare_batch(
    file: &PvrFile,
    array: &uapi::ObjArray,
    raw: &[u8],
    streams: &[Vec<u8>],
) -> Result<Vec<Job>, PvrError> {
    let syncobjs = k1_musebook_drm::fence::SyncObjTable::new();
    let sync_ops = alloc::vec![Vec::new(); array.count as usize];
    job::prepare_batch(file, &syncobjs, array, raw, streams, &sync_ops).map(|(jobs, _)| jobs)
}

/// A platform whose power sequence does nothing.
///
/// The fixture's registers are simulated, so there is nothing to program; what
/// the tests need from [`crate::power::Pm`] is its reference count.
struct NoPlatform;

impl crate::power::PlatformOps for NoPlatform {
    fn power_domain_on(&self) -> PvrResult<()> {
        Ok(())
    }

    fn clock_enable(&self) -> PvrResult<()> {
        Ok(())
    }

    fn reset_deassert(&self) -> PvrResult<()> {
        Ok(())
    }

    fn reset_assert(&self) {}

    fn clock_disable(&self) {}

    fn power_domain_off(&self) {}
}

/// The power manager the tests observe, installing it if nothing has yet.
///
/// [`crate::power::install`] is a `Once` because the driver supports one GPU, so
/// the manager outlives the test that installed it and its count carries over
/// between tests. Every assertion below is therefore a *delta* against a
/// baseline read at the start of the test.
fn power_manager() -> &'static crate::power::Pm {
    crate::power::install(crate::power::Pm::new(Arc::new(NoPlatform)));
    crate::power::manager().expect("a power manager is installed")
}

/// Runs `body` with a reference standing in for the one `submit_jobs` holds.
///
/// The real ioctl takes a [`crate::power::PowerRef`], which boots the firmware;
/// the fixture's simulated registers never answer a boot, so the tests take the
/// unattached form instead. It is the same [`crate::power::Pm::get`] and the same
/// count — only the boot is skipped.
fn in_submitting_scope<T>(body: impl FnOnce() -> T) -> T {
    crate::power::get_unattached().expect("the ioctl's own power reference");
    let out = body();
    crate::power::put_unattached();
    out
}

#[ktest]
fn a_job_holds_its_power_reference_until_it_retires() {
    let pm = power_manager();
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let base = pm.references();

    in_submitting_scope(|| {
        let (array, raw) = submit_array(handle, &streams);
        let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
        job::commit_batch(jobs).expect("committed");
        // One reference for the ioctl and one for each job.
        assert_eq!(pm.references(), base + 3);
    });

    // Both jobs must retain their power references after the submitting ioctl
    // returns; otherwise runtime PM could power down while firmware still uses them.
    assert_eq!(queue.in_flight_count(), 2);
    assert_eq!(pm.references(), base + 2);

    // One reference per retirement, in order, and none left over.
    queue.retire(1);
    assert_eq!(pm.references(), base + 1);
    queue.retire(2);
    assert_eq!(pm.references(), base);
}

#[ktest]
fn a_job_that_never_reaches_a_queue_gives_its_power_reference_back() {
    let pm = power_manager();
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");

    // `Queue::push` takes the reference before it takes the queue lock, so it has
    // one in hand when it finds the queue killed. An unbalanced release shows up
    // here and nowhere else: `Pm::put` without a matching `get` is logged and
    // ignored, so neither direction of the mistake panics on its own.
    context.kill();

    let streams = alloc::vec![compute_command(&device)];
    let base = pm.references();
    in_submitting_scope(|| {
        let (array, raw) = submit_array(handle, &streams);
        let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
        assert_eq!(job::commit_batch(jobs).err(), Some(PvrError::DeviceLost));
    });
    assert_eq!(pm.references(), base);
}

#[ktest]
fn terminating_a_queue_releases_every_job_power_reference() {
    let pm = power_manager();
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let base = pm.references();
    in_submitting_scope(|| {
        let (array, raw) = submit_array(handle, &streams);
        let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
        job::commit_batch(jobs).expect("committed");
    });
    assert_eq!(pm.references(), base + 2);

    queue.fail_all(DrmError::Io);
    assert_eq!(pm.references(), base);

    // Nothing is released twice. `Context::destroy` fails every queue and the
    // teardown paths can reach the same queue more than once, so a second pass
    // over an empty queue must not take the count below where it started — which,
    // being unsigned, would leave it at `u32::MAX` and the GPU powered for good.
    queue.fail_all(DrmError::Io);
    assert_eq!(pm.references(), base);
}

/// The native wait pins only the producer's timeline, including across a consumer reset.
#[ktest]
fn native_ufo_outlives_the_producer_and_waits_for_the_consumers_stop() {
    use core::sync::atomic::Ordering;

    let _pm = power_manager();
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let producer = compute_context(&device, &vm);
    let consumer = compute_context(&device, &vm);
    let producer_handle = file
        .render_contexts
        .insert(producer.clone())
        .expect("producer handle");
    let consumer_handle = file
        .render_contexts
        .insert(consumer.clone())
        .expect("consumer handle");
    let producer_queue = producer
        .queue_for_job(JobType::Compute)
        .expect("producer queue")
        .clone();
    let consumer_queue = consumer
        .queue_for_job(JobType::Compute)
        .expect("consumer queue")
        .clone();
    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(producer_handle, &streams);
    let producer_jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepare producer");
    let dependency = Job::native_dep_on(&producer_jobs[0]).expect("native dependency");
    let (offset, weak) = match &dependency {
        Dep::Native { ufo_owner, .. } => (ufo_owner.fw_offset(), Arc::downgrade(ufo_owner)),
        _ => panic!("expected native dependency"),
    };
    let pfn = offset / 4096;
    let (array, raw) = submit_array(consumer_handle, &streams);
    let mut consumer_jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepare consumer");
    consumer_jobs[0]
        .add_dependency(dependency)
        .expect("add native wait");
    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    consumer_jobs[0]
        .add_dependency(Dep::Foreign(gate.clone() as Arc<dyn Fence>))
        .expect("gate");
    in_submitting_scope(|| {
        job::commit_batch(producer_jobs).expect("submit producer");
        job::commit_batch(consumer_jobs).expect("queue consumer");
    });
    producer_queue.retire(1);
    producer.kill();
    drop(
        file.render_contexts
            .remove(producer_handle)
            .expect("remove producer"),
    );
    drop(producer);
    drop(producer_queue);

    let fw = device.fw.get().expect("firmware");
    assert!(weak.upgrade().is_some());
    assert_ne!(fw.mips.page_table.entry(pfn).expect("retained UFO PTE"), 0);

    // Model a complete stop while the consumer is still waiting on its CPU gate.
    consumer_queue.stop();
    device.quiescence_epoch.fetch_add(1, Ordering::Release);
    consumer_queue.start();
    assert_eq!(
        consumer_queue
            .timeline_value()
            .expect("unsubmitted timeline"),
        0
    );
    gate.signal(None);
    consumer_queue.process();
    assert_eq!(consumer_queue.in_flight_count(), 1);

    // Failure after that new kick still retains DMA: the older stop does not cover it.
    consumer_queue.fail_all(DrmError::Io);
    crate::service::drain_retired_resources(&device);
    assert!(weak.upgrade().is_some());
    assert_ne!(
        fw.mips
            .page_table
            .entry(pfn)
            .expect("failed consumer retains UFO"),
        0
    );
    device.quiescence_epoch.fetch_add(1, Ordering::Release);
    crate::service::drain_retired_resources(&device);
    assert!(weak.upgrade().is_none());
    // TimelineUfo's destructor hands its allocation to the worker without taking heap locks.
    crate::service::drain_retired_resources(&device);
    assert_eq!(fw.mips.page_table.entry(pfn).expect("released UFO PTE"), 0);
}

#[ktest]
fn a_timed_out_job_releases_its_power_reference() {
    let pm = power_manager();
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let base = pm.references();
    in_submitting_scope(|| {
        let (array, raw) = submit_array(handle, &streams);
        let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
        job::commit_batch(jobs).expect("committed");
    });
    assert_eq!(pm.references(), base + 2);

    // A GPU that stops answering must not also leak the reference that is keeping
    // it powered, or it could never be powered down again.
    queue.check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);
    assert_eq!(queue.pending_count(), 0);
    assert_eq!(pm.references(), base);
}

#[ktest]
fn bringing_up_the_submission_path_populates_osinit() {
    let device = device();
    let fw = device.fw.get().expect("firmware");

    // Every published ring address must be nonzero and identify the owned object
    // that firmware will read during initialization.
    let kccb = device.kccb.get().expect("kccb").lock();
    let fwccb = device.fwccb.get().expect("fwccb").lock();

    use crate::rogue::fwif::{RogueFwifCompchecks, RogueFwifCompchecksBvnc, RogueFwifOsinit};
    let read = |offset: usize| -> u32 {
        fw.mem
            .osinit
            .read_val::<crate::rogue::fwif::FwAddr>(offset)
            .expect("osinit")
            .0
    };
    assert_eq!(
        read(offset_of!(RogueFwifOsinit, kernel_ccbctl_fw_addr)),
        kccb.ccb().ctrl_fw_addr().0
    );
    assert_eq!(
        read(offset_of!(RogueFwifOsinit, kernel_ccb_fw_addr)),
        kccb.ccb().ccb_fw_addr().0
    );
    assert_eq!(
        read(offset_of!(RogueFwifOsinit, kernel_ccb_rtn_slots_fw_addr)),
        kccb.rtn_fw_addr().0
    );
    assert_eq!(
        read(offset_of!(RogueFwifOsinit, firmware_ccbctl_fw_addr)),
        fwccb.ctrl_fw_addr().0
    );
    assert_eq!(
        read(offset_of!(RogueFwifOsinit, firmware_ccb_fw_addr)),
        fwccb.ccb_fw_addr().0
    );
    assert_ne!(read(offset_of!(RogueFwifOsinit, fw_os_data_fw_addr)), 0);

    // Workload estimation is not implemented, and a zero address is how the
    // firmware is told so.
    assert_eq!(
        read(offset_of!(RogueFwifOsinit, work_est_firmware_ccb_fw_addr)),
        0
    );

    // The two BVNC compatibility blocks carry the layout version the firmware
    // checks; the BVNCs themselves are the firmware's to fill.
    let base = offset_of!(RogueFwifOsinit, rogue_comp_checks);
    for field in [
        offset_of!(RogueFwifCompchecks, hw_bvnc),
        offset_of!(RogueFwifCompchecks, fw_bvnc),
    ] {
        let at = base + field;
        assert_eq!(
            fw.mem
                .osinit
                .read_val::<u32>(at + offset_of!(RogueFwifCompchecksBvnc, layout_version))
                .expect("osinit"),
            abi::COMPCHECKS_LAYOUT_VERSION
        );
        assert_eq!(
            fw.mem
                .osinit
                .read_val::<u64>(at + offset_of!(RogueFwifCompchecksBvnc, bvnc))
                .expect("osinit"),
            0
        );
    }

    // Twice is `EBUSY`: a second set of rings would leave the firmware reading
    // the first set for ever.
    drop(kccb);
    drop(fwccb);
    assert_eq!(init(&device), Err(PvrError::Busy));
}

#[ktest]
fn a_compute_context_describes_itself_to_the_firmware() {
    let device = device();
    let (_file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);

    assert_eq!(context.queues().len(), 1);
    let queue = context.queue_for_job(JobType::Compute).expect("queue");
    assert_eq!(queue.job_type(), JobType::Compute);
    assert!(context.queue_for_job(JobType::Geometry).is_none());
    assert!(!context.is_faulty());

    // The common context the firmware reads: the CCCB addresses, the data master,
    // the priority and the memory context.
    use crate::rogue::fwif::RogueFwifFwcommoncontext;
    let cctx: RogueFwifFwcommoncontext = read_fw_context(&context, 0);
    assert_ne!(cctx.ccbctl_fw_addr.0, 0);
    assert_ne!(cctx.ccb_fw_addr.0, 0);
    assert_eq!(cctx.dm, abi::FWIF_DM_CDM);
    assert_eq!(cctx.priority, Priority::Medium as u32);
    assert_eq!(cctx.max_deadline_ms, abi::MAX_DEADLINE_MS);
    assert_eq!(cctx.server_common_context_id, context.id());
    assert_ne!(cctx.fw_mem_context_fw_addr.0, 0);
    assert_ne!(cctx.context_state_addr, 0);

    // The static state stream landed in the context structure, at the offset the
    // compute layout puts it.
    let state: u64 = read_fw_value(
        &context,
        offset_of!(abi::RogueFwifFwcomputecontext, static_compute_context_state),
    );
    assert_eq!(state, 1);
}

/// Objects the firmware writes and the CPU polls must be CPU-uncached.
///
/// Built from the real constructors (`Queue::new`, `Cccb::new` inside it,
/// `Ccb::new` / `Kccb::new` inside [`init`]). The expected column is the
/// policy, not a flag this test passed in. Flipping the timeline UFO to
/// write-back, or the KCCB ring to uncacheable, turns this red.
#[ktest]
fn polled_firmware_objects_are_cpu_uncached() {
    let device = device();
    let (_file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let [cccb_ctrl, cccb_ring] = queue.cccb_cpu_uncached();
    let kccb = device.kccb.get().expect("kccb").lock();
    let fwccb = device.fwccb.get().expect("fwccb").lock();
    let fw_mem = vm.fw_mem_context().expect("user VM has a memory context");

    let rows = [
        (
            "timeline UFO",
            queue.timeline_object().is_cpu_uncached(),
            true,
        ),
        (
            "reg_state",
            queue.reg_state_object().is_cpu_uncached(),
            false,
        ),
        ("cccb ctrl", cccb_ctrl, true),
        ("cccb ring", cccb_ring, false),
        (
            "kccb ctrl",
            kccb.ccb().ctrl_object().is_cpu_uncached(),
            true,
        ),
        (
            "kccb ring",
            kccb.ccb().ccb_object().is_cpu_uncached(),
            false,
        ),
        ("kccb rtn", kccb.rtn_object().is_cpu_uncached(), true),
        ("fwccb ctrl", fwccb.ctrl_object().is_cpu_uncached(), true),
        ("fwccb ring", fwccb.ccb_object().is_cpu_uncached(), true),
        (
            "context fw object",
            context.fw_object_for_test().is_cpu_uncached(),
            false,
        ),
        (
            "fw mem context",
            fw_mem.object_for_test().is_cpu_uncached(),
            false,
        ),
    ];
    for (what, actual, expected) in rows {
        assert_eq!(actual, expected, "{what}");
    }
}

/// Every address published by a firmware context must resolve in its page table.
///
/// This includes all four queue allocations: timeline, register state, client CCB
/// control structure, and ring. Valid contents are insufficient without mappings.
#[ktest]
fn every_firmware_address_a_context_publishes_is_mapped() {
    use crate::{fw::mips::FW_HEAP_MIPS_BASE, rogue::fwif::RogueFwifFwcommoncontext};

    let device = device();
    let (_file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let fw = device.fw.get().expect("firmware");
    let table = &fw.mips.page_table;

    let mapped = |fw_addr: u32, what: &str| {
        assert_ne!(fw_addr, 0, "{what} was never filled in");
        let pfn = (fw_addr - FW_HEAP_MIPS_BASE) / 4096;
        assert_ne!(
            table.entry(pfn).expect("page is inside the heap"),
            0,
            "{what} is at {fw_addr:#x} (heap page {pfn}), which has no page-table entry: \
             the firmware will fault the moment it reads that field"
        );
    };

    let cctx: RogueFwifFwcommoncontext = read_fw_context(&context, 0);
    mapped(cctx.ccbctl_fw_addr.0, "the client CCB control structure");
    mapped(cctx.ccb_fw_addr.0, "the client CCB ring");
    mapped(cctx.fw_mem_context_fw_addr.0, "the firmware memory context");
    mapped(
        context.fw_object_for_test().fw_addr().0,
        "the context structure",
    );

    // Check the whole multi-page ring, not only its first address.
    let ring_pfn = (cctx.ccb_fw_addr.0 - FW_HEAP_MIPS_BASE) / 4096;
    let ring_pages = 1u32 << abi::CCCB_SIZE_LOG2 >> 12;
    for page in 0..ring_pages.max(1) {
        assert_ne!(
            table
                .entry(ring_pfn + page)
                .expect("page is inside the heap"),
            0,
            "page {page} of the client CCB ring has no page-table entry"
        );
    }
}

/// Reads a value out of a context's firmware structure.
fn read_fw_value(context: &Arc<Context>, offset: usize) -> u64 {
    read_fw_context::<u64>(context, offset)
}

/// Reads a POD out of a context's firmware structure.
fn read_fw_context<T: ostd_pod::Pod>(context: &Arc<Context>, offset: usize) -> T {
    context
        .fw_object_for_test()
        .read_val::<T>(offset)
        .expect("fw object")
}

#[ktest]
fn contexts_on_one_address_space_share_a_firmware_memory_context() {
    let device = device();
    let (_file, vm, _) = file_with_vm(&device);

    let first = compute_context(&device, &vm);
    let second = compute_context(&device, &vm);

    use crate::rogue::fwif::RogueFwifFwcommoncontext;
    let a: RogueFwifFwcommoncontext = read_fw_context(&first, 0);
    let b: RogueFwifFwcommoncontext = read_fw_context(&second, 0);
    // Owned by the VM context (upstream `pvr_vm_context::fw_mem_ctx_obj`): one
    // `rogue_fwif_fwmemcontext` per address space. Two of them for one page
    // catalogue would make the firmware assign two page-catalogue register sets
    // to the same tables and run out of sets earlier than it should.
    let shared = vm.fw_mem_context().expect("user vm").fw_addr();
    assert_eq!(a.fw_mem_context_fw_addr, shared);
    assert_eq!(b.fw_mem_context_fw_addr, shared);

    // A different address space gets its own.
    let other_vm = VmContext::new(device.clone(), VmContextKind::User).expect("vm context");
    let third = compute_context(&device, &other_vm);
    let c: RogueFwifFwcommoncontext = read_fw_context(&third, 0);
    assert_ne!(
        a.fw_mem_context_fw_addr,
        other_vm.fw_mem_context().expect("other user vm").fw_addr()
    );
    assert_eq!(
        c.fw_mem_context_fw_addr,
        other_vm.fw_mem_context().expect("other user vm").fw_addr()
    );
}

#[ktest]
fn destroying_a_context_returns_its_firmware_address_space() {
    let device = device();
    let (_file, vm, _) = file_with_vm(&device);

    let first = compute_context(&device, &vm);
    let addr = first.fw_addr().0;
    let queue_addr = first
        .queue_for_job(JobType::Compute)
        .expect("queue")
        .timeline_fw_addr()
        .0;
    drop(first);
    // Queue-owned UFO storage is released by the sleepable service pass.
    crate::service::drain_retired_resources(&device);

    // The firmware heap is a first-fit allocator over a sorted list of live
    // ranges, so a context created after the first one was dropped lands in
    // exactly the same place — which is the observable form of "the address space
    // came back". A leak here would be permanent for the device's lifetime.
    let second = compute_context(&device, &vm);
    assert_eq!(second.fw_addr().0, addr);
    assert_eq!(
        second
            .queue_for_job(JobType::Compute)
            .expect("queue")
            .timeline_fw_addr()
            .0,
        queue_addr
    );
}

#[ktest]
fn a_context_refuses_the_wrong_kind_of_static_state() {
    let device = device();
    let (_file, vm, _) = file_with_vm(&device);

    // A compute context with no static state: upstream's
    // `if (!args->static_context_state_len) return -EINVAL`.
    assert_eq!(
        Context::new(
            &device,
            vm.clone(),
            ContextType::Compute,
            Priority::Medium,
            &[],
            0
        )
        .err(),
        Some(PvrError::InvalidArgument)
    );

    // A transfer context *with* static state: upstream's
    // `init_transfer_fw_objs()` refuses it.
    assert_eq!(
        Context::new(
            &device,
            vm.clone(),
            ContextType::TransferFrag,
            Priority::Medium,
            &compute_static_state(&device),
            0
        )
        .err(),
        Some(PvrError::InvalidArgument)
    );

    // A call stack on anything but a render context.
    assert_eq!(
        Context::new(
            &device,
            vm.clone(),
            ContextType::Compute,
            Priority::Medium,
            &compute_static_state(&device),
            0x1000
        )
        .err(),
        Some(PvrError::InvalidArgument)
    );

    // A malformed static state stream is refused by the same validator a job
    // command goes through.
    let mut truncated = compute_static_state(&device);
    truncated.truncate(truncated.len() - 4);
    assert_eq!(
        Context::new(
            &device,
            vm,
            ContextType::Compute,
            Priority::Medium,
            &truncated,
            0
        )
        .err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_priority_the_uapi_does_not_define_is_refused_and_high_needs_privilege() {
    assert_eq!(Priority::from_uapi(0, false), Ok(Priority::Medium));
    assert_eq!(Priority::from_uapi(-512, false), Ok(Priority::Low));
    assert_eq!(Priority::from_uapi(512, true), Ok(Priority::High));
    // Upstream's `-EACCES` for an unprivileged caller; see `Priority::from_uapi`.
    assert_eq!(Priority::from_uapi(512, false), Err(PvrError::Unsupported));
    assert_eq!(
        Priority::from_uapi(1, false),
        Err(PvrError::InvalidArgument)
    );
    assert_eq!(
        Priority::from_uapi(-1, false),
        Err(PvrError::InvalidArgument)
    );

    // The firmware scale is 0, 1, 2 — not the uapi's -512, 0, 512. Writing a uapi
    // value into the common context would give the firmware a priority it has no
    // case for.
    assert_eq!(Priority::Low as u32, 0);
    assert_eq!(Priority::Medium as u32, 1);
    assert_eq!(Priority::High as u32, 2);
}

#[ktest]
fn a_batch_of_compute_jobs_reaches_the_firmware_in_order() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);

    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    assert_eq!(jobs.len(), 2);
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    // Seqnos start at one, so a zeroed timeline UFO cannot look like a completed
    // first job.
    assert_eq!(fences[0].seqno(), 1);
    assert_eq!(fences[1].seqno(), 2);

    job::commit_batch(jobs).expect("committed");

    // Both fitted in the ring, so both are on the GPU. Nothing has retired,
    // because nothing advances the timeline but the firmware.
    assert_eq!(queue.pending_count(), 2);
    assert_eq!(queue.in_flight_count(), 2);
    assert!(!fences[0].is_signaled());

    // The firmware finishes the first job.
    queue.retire(1);
    assert_eq!(fences[0].status(), FenceStatus::Signaled);
    assert!(!fences[1].is_signaled());
    assert_eq!(queue.pending_count(), 1);

    queue.retire(2);
    assert_eq!(fences[1].status(), FenceStatus::Signaled);
    assert_eq!(queue.pending_count(), 0);
}

#[ktest]
fn a_batch_that_fails_partway_leaves_nothing_behind() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    // The second job's command stream is truncated, so it is refused. The first
    // one is perfectly good, and must not reach the queue anyway.
    let mut bad = compute_command(&device);
    bad.truncate(bad.len() - 8);
    let streams = alloc::vec![compute_command(&device), bad];
    let (array, raw) = submit_array(handle, &streams);

    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::InvalidArgument)
    );
    assert_eq!(queue.pending_count(), 0);
    assert_eq!(queue.in_flight_count(), 0);

    // And the same for a bad handle in the second element: the failure is found
    // before anything is committed, which is the whole reason preparation and
    // commitment are separate phases.
    let good = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, mut raw) = submit_array(handle, &good);
    let second = size_of::<uapi::Job>() + offset_of!(uapi::Job, context_handle);
    raw[second..second + 4].copy_from_slice(&0xdeadu32.to_le_bytes());
    assert_eq!(
        prepare_batch(&file, &array, &raw, &good).err(),
        Some(PvrError::NoSuchHandle)
    );
    assert_eq!(queue.pending_count(), 0);

    // The good batch still works afterwards, so nothing was left in a state that
    // poisons the queue.
    let (array, raw) = submit_array(handle, &good);
    let jobs = prepare_batch(&file, &array, &raw, &good).expect("prepared");
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 2);
}

#[ktest]
fn an_empty_batch_and_a_bad_stride_are_refused() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file.render_contexts.insert(context).expect("handle");

    let (mut array, raw) = submit_array(handle, &alloc::vec![compute_command(&device)]);
    let streams = alloc::vec![compute_command(&device)];

    let empty = uapi::ObjArray { count: 0, ..array };
    assert_eq!(
        prepare_batch(&file, &empty, &[], &[]).err(),
        Some(PvrError::InvalidArgument)
    );

    // A stride below the mandatory minimum: upstream's `pvr_get_uobj_array()`
    // gate, which lives in `uapi::obj_array` and nowhere else.
    array.stride = (uapi::Job::MIN_STRIDE - 1) as u32;
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_sync_operation_array_that_does_not_match_its_descriptor_is_refused() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");

    let streams = alloc::vec![compute_command(&device)];
    let (array, mut raw) = submit_array(handle, &streams);
    let at = offset_of!(uapi::Job, sync_ops) + offset_of!(uapi::ObjArray, count);
    raw[at..at + 4].copy_from_slice(&1u32.to_le_bytes());

    // A descriptor claiming an operation must include its bytes. Reject the whole
    // batch before resolving anything or queueing jobs when those counts disagree.
    // Operation-resolution tests live in `crate::render::test`.
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::InvalidArgument)
    );
    assert_eq!(
        context
            .queue_for_job(JobType::Compute)
            .expect("queue")
            .pending_count(),
        0
    );
}

#[ktest]
fn a_job_waits_for_a_foreign_fence_and_the_signal_releases_it() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let mut jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");

    // A foreign fence whose completion is observable only on the CPU.
    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    let done = jobs[0].fence();
    jobs[0]
        .add_dependency(Dep::Foreign(gate.clone() as Arc<dyn Fence>))
        .expect("dependency recorded");

    job::commit_batch(jobs).expect("committed");

    // Queued but not on the GPU: a foreign dependency is one the driver has to
    // wait for, so the job sits at the head of the queue.
    assert_eq!(queue.pending_count(), 1);
    assert_eq!(queue.in_flight_count(), 0);

    // The callback only records work, even before a service thread has started.
    // This fixture runs the production queue-processing step of its next pass.
    gate.signal(None);
    assert_eq!(queue.in_flight_count(), 0);
    assert!(crate::service::wake_requested(&device));
    queue::process_all(&device);
    assert_eq!(queue.in_flight_count(), 1);
    assert!(!done.is_signaled());

    queue.retire(1);
    assert_eq!(done.status(), FenceStatus::Signaled);
}

#[ktest]
fn a_dependency_that_has_already_signalled_does_not_block() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let mut jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");

    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    gate.signal(None);
    jobs[0]
        .add_dependency(Dep::Foreign(gate as Arc<dyn Fence>))
        .expect("dependency recorded");

    job::commit_batch(jobs).expect("committed");
    // Fence dependency-cycle check drops an already-signalled dependency, so the job goes
    // straight to the GPU rather than waiting for something that has happened.
    assert_eq!(queue.in_flight_count(), 1);
}

#[ktest]
fn a_failed_dependency_fails_the_job_instead_of_hanging_it() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let mut jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");

    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    let done = jobs[0].fence();
    jobs[0]
        .add_dependency(Dep::Foreign(gate.clone() as Arc<dyn Fence>))
        .expect("dependency recorded");
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 0);

    // Fence error-propagation contract: a failed fence is *signalled*, so the consumer must make
    // progress and observe the error rather than keep waiting. Making progress
    // here means failing, because the work this job reads was never produced.
    gate.signal(Some(DrmError::TimedOut));
    assert_eq!(done.status(), FenceStatus::Pending);
    assert!(crate::service::wake_requested(&device));
    queue::process_all(&device);
    assert_eq!(done.status(), FenceStatus::Failed(DrmError::TimedOut));
    assert_eq!(queue.pending_count(), 0);
    assert_eq!(queue.in_flight_count(), 0);
}

#[ktest]
fn a_dependency_cycle_within_a_batch_is_refused() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let mut jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");

    // B waits for A, which is fine.
    let on_first = Job::native_dep_on(&jobs[0]).expect("native dep");
    jobs[1].add_dependency(on_first).expect("B waits for A");

    // A waits for B, which would close a cycle. Fence dependency-cycle check walks the graph
    // under one lock and refuses the edge rather than letting the two jobs wait
    // for each other for ever.
    let on_second = Job::native_dep_on(&jobs[1]).expect("native dep");
    assert_eq!(
        jobs[0].add_dependency(on_second).err(),
        Some(PvrError::InvalidArgument)
    );

    // A job may not depend on itself either.
    let on_self = Job::native_dep_on(&jobs[0]).expect("native dep");
    assert_eq!(
        jobs[0].add_dependency(on_self).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_native_dependency_becomes_a_firmware_wait_rather_than_a_driver_wait() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let mut jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");

    let on_first = Job::native_dep_on(&jobs[0]).expect("native dep");
    jobs[1].add_dependency(on_first).expect("B waits for A");
    job::commit_batch(jobs).expect("committed");

    // Both are on the GPU even though the second waits for the first: a native
    // dependency is a `FENCE_PR` command in the ring, so the *firmware* blocks
    // and the driver does not. That is the whole point of the distinction, and a
    // driver that waited here would serialise every dependent submission.
    assert_eq!(queue.in_flight_count(), 2);
}

#[ktest]
fn a_fence_from_a_previous_submission_is_recognised_as_native() {
    // Recognize a native fence across separate submissions, matching upstream
    // `pvr_queue_fence_is_native()` rather than relying on same-batch identity.
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let ctx_handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let syncobjs = k1_musebook_drm::fence::SyncObjTable::new();
    let sync_handle = syncobjs
        .insert(k1_musebook_drm::fence::SyncObj::new(false))
        .expect("syncobj")
        .0;

    let signal_ops = sync_op_bytes(&[uapi::SyncOp {
        handle: sync_handle,
        flags: uapi::SyncOpFlags::SIGNAL.bits(),
        value: 0,
    }]);
    let wait_ops = sync_op_bytes(&[uapi::SyncOp {
        handle: sync_handle,
        flags: uapi::SyncOpFlags::WAIT,
        value: 0,
    }]);

    // First ioctl: A signals the sync object.
    let stream_a = alloc::vec![compute_command(&device)];
    let (array_a, raw_a) = submit_array_with_sync(ctx_handle, &stream_a, 1);
    let (jobs_a, mut signals_a) =
        job::prepare_batch(&file, &syncobjs, &array_a, &raw_a, &stream_a, &[signal_ops])
            .expect("prepare A");
    let fence_a = jobs_a[0].fence();
    let ufo_a = fence_a
        .as_any()
        .downcast_ref::<queue::JobFence>()
        .expect("job fence")
        .native_ufo(&device)
        .expect("A's UFO");
    job::commit_batch(jobs_a).expect("commit A");
    signals_a.push_fences();

    // Second ioctl: B waits on the same sync object. The fence comes out of
    // the object, not the batch cache, so `producer` is `None` — recognition
    // has to go through the downcast.
    let stream_b = alloc::vec![compute_command(&device)];
    let (array_b, raw_b) = submit_array_with_sync(ctx_handle, &stream_b, 1);
    let (jobs_b, _) =
        job::prepare_batch(&file, &syncobjs, &array_b, &raw_b, &stream_b, &[wait_ops])
            .expect("prepare B");
    assert_eq!(jobs_b.len(), 1);
    match &jobs_b[0].deps[..] {
        [
            Dep::Native {
                fence,
                ufo_addr,
                ufo_value,
                ..
            },
        ] => {
            assert!(Arc::ptr_eq(fence, &fence_a));
            assert_eq!((*ufo_addr, *ufo_value), (ufo_a.0, ufo_a.1));
        }
        other => panic!("expected one native dep, got {}", other.len()),
    }

    job::commit_batch(jobs_b).expect("commit B");
    // Both in flight: B's wait is a `FENCE_PR`, not a CPU block on the queue.
    assert_eq!(queue.in_flight_count(), 2);
}

#[ktest]
fn a_job_that_never_completes_is_timed_out_and_its_fence_fails() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 2);

    // Nothing has overrun yet.
    queue.check_timeout_at(now_ns());
    assert!(!fences[0].is_signaled());
    assert!(
        !device.reset_request.is_pending(),
        "a queue with nothing overrun must not ask for a reset"
    );

    // Half a second later the firmware still has not written the first job's UFO
    // update, so it is hung. Fence fence-completion contract says the driver must end the fence
    // itself, and with `Io` rather than by abandoning it.
    queue.check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);
    assert_eq!(fences[0].status(), FenceStatus::Failed(DrmError::Io));

    // And the job behind it, because the firmware is stuck on the same ring with
    // that job's update unwritten: it can never run, so leaving its fence pending
    // would hang every consumer of it for ever.
    assert_eq!(fences[1].status(), FenceStatus::Failed(DrmError::Io));
    assert_eq!(queue.pending_count(), 0);

    // The context is unusable afterwards, which is what stops a client feeding
    // more work to a firmware that has given up on it.
    assert!(context.is_faulty());
    let (array, raw) = submit_array(handle, &streams);
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::DeviceLost)
    );

    // Failing the fences is not enough on its own. The timed-out jobs took the
    // runtime-PM reference with them, and the firmware still reports `POW_ON`
    // for the kick it never finished, so the suspend gate will refuse for ever
    // and the next context this client makes meets the same firmware. Only a
    // reset gets out of that, and nothing else on this path asks for one: the
    // watchdog samples KCCB progress, and an abandoned client CCB makes none of
    // its own. Asserting the kind matters as much as the pending bit -- a soft
    // reset restarts the processor without rebuilding the rings the abandoned
    // kick is sitting in.
    assert_eq!(device.reset_request.take(), Some(ResetKind::Hard));
}

/// Timeout processing must retire a completed timeline before failing jobs.
///
/// The fixture advances the timeline without calling `process`, so only the
/// retirement inside the timeout path can observe completion.
#[ktest]
fn the_timeout_retires_a_job_the_timeline_has_already_passed_instead_of_failing_it() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 2);

    // The firmware finished both and wrote the second one's seqno, exactly as
    // the board's `UFO Update` trace showed. Nothing has read it yet.
    let last = fences[1].seqno() as u32;
    queue.set_timeline_for_test(last);

    // Long past the deadline, and the only thing that runs is `advance`.
    queue.check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);

    assert_eq!(
        fences[0].status(),
        FenceStatus::Signaled,
        "a job the timeline has passed must retire, not fail"
    );
    assert_eq!(fences[1].status(), FenceStatus::Signaled);
    assert!(
        !context.is_faulty(),
        "a queue that was merely un-retired is not a faulty context"
    );
    assert!(
        !device.reset_request.is_pending(),
        "and it must not drag the GPU through a reset"
    );
}

/// A second timeline read reporting completion takes precedence over timeout.
///
/// Advance the timeline at the production reread seam without retiring elsewhere,
/// so the timeout check itself must preserve the successful result.
#[ktest]
fn a_second_reading_that_says_the_job_finished_beats_the_timeout() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");

    queue.set_timeline_for_test(fences[1].seqno() as u32);
    queue.check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);

    assert_eq!(fences[0].status(), FenceStatus::Signaled);
    assert_eq!(fences[1].status(), FenceStatus::Signaled);
    assert_eq!(queue.pending_count(), 0);
    assert!(!context.is_faulty());
    assert!(
        !device.reset_request.is_pending(),
        "a queue whose work finished must not drag the GPU through a reset"
    );
}

/// A submitted job waiting behind a backlog receives a fresh head deadline.
///
/// Backdated submission times model queued work. Retiring the preceding job must
/// refresh the next head's timestamp, matching `drm_sched`'s progress-based timer.
#[ktest]
fn a_job_waiting_behind_a_backlog_gets_a_fresh_deadline_when_the_one_ahead_retires() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams: Vec<Vec<u8>> = (0..3).map(|_| compute_command(&device)).collect();
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 3);

    // Backdate the ring as far as uptime permits. On a fast boot this can
    // saturate at zero; the timestamp assertion below still proves renewal.
    queue.backdate_submitted_for_test(JOB_TIMEOUT_NS + 1);

    // The firmware finishes the first one and starts on the second.
    queue.set_timeline_for_test(fences[0].seqno() as u32);
    let before_retirement = now_ns();
    queue.check_timeout_at(before_retirement);
    assert!(
        queue
            .head_submitted_at_for_test()
            .expect("new head submitted")
            >= before_retirement,
        "retirement must stamp the next head with a fresh deadline"
    );

    assert_eq!(fences[0].status(), FenceStatus::Signaled);
    assert_eq!(
        fences[1].status(),
        FenceStatus::Pending,
        "the new head had just been handed the GPU; its deadline restarts there"
    );
    assert_eq!(fences[2].status(), FenceStatus::Pending);
    assert_eq!(queue.pending_count(), 2);
    assert!(!context.is_faulty());
    assert!(
        !device.reset_request.is_pending(),
        "a queue making progress must not drag the GPU through a reset"
    );

    // The restart is not a reprieve for a job that really is stuck: once the
    // fresh deadline has passed with the timeline standing still, it fails.
    // An explicit future instant works even before uptime reaches the timeout.
    queue.check_timeout_at(now_ns().saturating_add(JOB_TIMEOUT_NS + 1));
    assert_eq!(fences[1].status(), FenceStatus::Failed(DrmError::Io));
}

/// An unsignalled producing dependency exempts the head from execution timeout.
///
/// Firmware-side `FENCE_PR` waiting is not evidence that this queue has stalled.
#[ktest]
fn a_head_job_blocked_on_an_unsignalled_dependency_is_not_timed_out() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    // A dependency nothing in this batch produces, so only the CPU can observe
    // it -- the shape a cross-queue `FENCE_PR` wait has.
    let blocker = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    let syncobjs = k1_musebook_drm::fence::SyncObjTable::new();
    let object = k1_musebook_drm::fence::SyncObj::new(false);
    object.replace_fence(Some(blocker.clone() as Arc<dyn Fence>));
    let sync_handle = syncobjs.insert(object).expect("handle").0;

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array_with_sync(handle, &streams, 1);
    let wait_ops = sync_op_bytes(&[uapi::SyncOp {
        handle: sync_handle,
        flags: uapi::SyncOpFlags::WAIT,
        value: 0,
    }]);
    let (jobs, _) = job::prepare_batch(&file, &syncobjs, &array, &raw, &streams, &[wait_ops])
        .expect("prepared");
    let fence = jobs[0].fence();
    job::commit_batch(jobs).expect("committed");

    // Long past the deadline, but the work it is waiting for has not happened.
    queue.check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);
    assert_eq!(
        fence.status(),
        FenceStatus::Pending,
        "a job parked on a dependency must not be failed for taking too long"
    );
    assert!(!context.is_faulty());
    assert!(!device.reset_request.is_pending());

    // Once the dependency lands, the job is the firmware's to run and the
    // deadline means something again.
    blocker.signal(None);
    queue::process_all(&device);
    queue.check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);
    assert_eq!(fence.status(), FenceStatus::Failed(DrmError::Io));
}

/// One read of the timeline retires the whole span behind it.
///
/// The board saw a timeline at `0x710` with a head job at `0x703`: thirteen
/// jobs between them, all finished, none retired. A test that retires one job
/// at a time cannot tell that apart from a loop that stops after the first.
#[ktest]
fn one_retire_signals_every_job_the_timeline_has_passed() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams: Vec<Vec<u8>> = (0..5).map(|_| compute_command(&device)).collect();
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 5);

    // Seqnos are handed out in order, so the last one implies the four before
    // it -- that is the property `process`'s wrapping comparison encodes.
    let last = fences[4].seqno() as u32;
    for (index, pair) in fences.windows(2).enumerate() {
        assert!(
            pair[1].seqno() > pair[0].seqno(),
            "seqno {} is not after {}",
            index + 1,
            index
        );
    }

    queue.retire(last);

    for (index, fence) in fences.iter().enumerate() {
        assert_eq!(
            fence.status(),
            FenceStatus::Signaled,
            "job {index} was left pending behind a timeline that had passed it"
        );
    }
    assert_eq!(queue.pending_count(), 0);
}

/// A stopped queue writes nothing to the firmware, and a started one may again.
///
/// The queue half of upstream `pvr_power_reset()`:
/// `pvr_queue_device_pre_reset()` stops every queue before the firmware is
/// touched and `pvr_queue_device_post_reset()` starts them afterwards. A job
/// pushed while the queue is stopped stays where it is — upstream's
/// `drm_sched_stop()` parks the worker rather than refusing submissions — and
/// goes to the GPU when the queue restarts.
#[ktest]
fn a_queue_stopped_for_a_reset_takes_no_work_until_it_is_started() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    queue::stop_all(&device);
    assert!(queue.is_stopped());

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let done = jobs[0].fence();
    job::commit_batch(jobs).expect("committed");

    // Queued but not on the GPU: the firmware is being restarted, so nothing may
    // reach the client CCB or the KCCB.
    assert_eq!(queue.pending_count(), 1);
    assert_eq!(queue.in_flight_count(), 0);

    queue::start_all(&device);
    assert!(!queue.is_stopped());

    // It was never in flight, so the reset did not have to terminate it, and the
    // trailing `advance` in `Queue::start` submits it.
    assert!(!done.is_signaled());
    assert_eq!(queue.in_flight_count(), 1);
    assert!(!context.is_faulty());
}

/// In-flight work is terminated by a reset, never replayed.
///
/// Upstream flags the entity guilty when it finds unfinished jobs after a reset
/// ("so no new job can be submitted"); this port also signals their fences,
/// because fence-completion contract makes that an obligation and there is no scheduler to hold
/// them. The `pending_count` assertion is the "no reset-and-replay" half: the
/// job is gone from the queue rather than waiting to be submitted again.
#[ktest]
fn a_reset_terminates_in_flight_jobs_rather_than_replaying_them() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.in_flight_count(), 2);

    queue::stop_all(&device);
    queue::start_all(&device);

    for fence in &fences {
        assert_eq!(fence.status(), FenceStatus::Failed(DrmError::Io));
    }
    assert_eq!(queue.pending_count(), 0);
    assert_eq!(queue.in_flight_count(), 0);

    // Guilty, as upstream's `atomic_set(&queue->ctx->faulty, 1)`.
    assert!(context.is_faulty());
    let (array, raw) = submit_array(handle, &streams);
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::DeviceLost)
    );
}

/// A job the firmware finished while the queue was stopped still succeeds.
///
/// Upstream `pvr_queue_start()` walks the pending list and re-attaches the done
/// fence of any job that completed after `drm_sched_stop()` was called, rather
/// than failing it. Here that is the [`Queue::process`] call at the top of
/// `start`, and it has to run before the timeline UFO is overwritten.
#[ktest]
fn a_job_the_firmware_finished_before_the_reset_is_not_failed_by_it() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let fences: Vec<Arc<dyn Fence>> = jobs.iter().map(|job| job.fence()).collect();
    job::commit_batch(jobs).expect("committed");

    queue::stop_all(&device);
    // The firmware wrote the first job's update and then stopped answering. This
    // is the write only the firmware makes in production, hence the test-only
    // setter; `retire` would also notice it, which is what makes it the wrong
    // tool for reproducing "nobody has looked yet".
    queue.set_timeline_for_test(fences[0].seqno() as u32);
    queue::start_all(&device);

    assert_eq!(fences[0].status(), FenceStatus::Signaled);
    assert_eq!(fences[1].status(), FenceStatus::Failed(DrmError::Io));
}

/// The timeline UFO is CPU-signalled, so a native waiter is not stranded.
///
/// Upstream `pvr_queue_start()` line 829: `*queue->timeline_ufo.value =
/// atomic_read(&queue->job_fence_ctx.seqno)`. Another queue may have a
/// `FENCE_PR` command in its client CCB naming this UFO, and the firmware that
/// would have advanced it has been restarted — so the host advances it instead.
#[ktest]
fn a_reset_cpu_signals_the_timeline_so_other_queues_are_not_stranded() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device), compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let last = jobs[1].fence().seqno() as u32;
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.timeline_value(), Ok(0));

    queue::stop_all(&device);
    queue::start_all(&device);

    // The last seqno handed out, not the last one that ran: a waiter is asking
    // "has everything up to N finished", and after a reset the answer has to be
    // yes for every N this queue could have issued, or the waiter waits for ever.
    assert_eq!(queue.timeline_value(), Ok(last));
}

#[ktest]
fn a_timeout_on_one_context_does_not_touch_another() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let victim = compute_context(&device, &vm);
    let bystander = compute_context(&device, &vm);
    let victim_handle = file.render_contexts.insert(victim.clone()).expect("handle");
    let bystander_handle = file
        .render_contexts
        .insert(bystander.clone())
        .expect("handle");

    let streams = alloc::vec![compute_command(&device)];
    let mut fences = Vec::new();
    for handle in [victim_handle, bystander_handle] {
        let (array, raw) = submit_array(handle, &streams);
        let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
        fences.push(jobs[0].fence());
        job::commit_batch(jobs).expect("committed");
    }

    victim
        .queue_for_job(JobType::Compute)
        .expect("queue")
        .check_timeout_at(now_ns() + JOB_TIMEOUT_NS + 1);

    assert_eq!(fences[0].status(), FenceStatus::Failed(DrmError::Io));
    assert!(victim.is_faulty());

    // Separate CCCB, separate timeline: nothing about the victim's hang says
    // anything about this context's work.
    assert!(!fences[1].is_signaled());
    assert!(!bystander.is_faulty());
    assert_eq!(
        bystander
            .queue_for_job(JobType::Compute)
            .expect("queue")
            .in_flight_count(),
        1
    );
}

#[ktest]
fn killing_a_context_refuses_new_work_and_fails_what_is_queued() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let mut jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    // Blocked on a foreign fence, so it is still in the queue when the context is
    // killed rather than already on the GPU.
    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    let done = jobs[0].fence();
    jobs[0]
        .add_dependency(Dep::Foreign(gate as Arc<dyn Fence>))
        .expect("dependency");
    job::commit_batch(jobs).expect("committed");
    assert_eq!(queue.pending_count(), 1);

    context.kill();

    // Nothing new may be queued: the third of the three paths out of a job.
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    assert_eq!(job::commit_batch(jobs).err(), Some(PvrError::DeviceLost));

    // And fence-completion contract for the one already there.
    queue.fail_all(DrmError::Io);
    assert_eq!(done.status(), FenceStatus::Failed(DrmError::Io));
    assert_eq!(queue.pending_count(), 0);
}

#[ktest]
fn a_lost_device_fails_a_teardown_rather_than_waiting_for_it() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file
        .render_contexts
        .insert(context.clone())
        .expect("handle");
    let queue = context.queue_for_job(JobType::Compute).expect("queue");

    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    let jobs = prepare_batch(&file, &array, &raw, &streams).expect("prepared");
    let done = jobs[0].fence();
    job::commit_batch(jobs).expect("committed");

    // A lost device will never advance a timeline again, so a teardown that waited
    // would hang the caller and one that walked away would leave the fence pending
    // for ever. It does neither: it fails everything and says so.
    device.declare_lost();
    assert_eq!(context.destroy(), Ok(()));
    assert_eq!(done.status(), FenceStatus::Failed(DrmError::Io));
    assert_eq!(queue.pending_count(), 0);
}

#[ktest]
fn a_command_sequence_larger_than_half_the_ring_is_refused_at_creation() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let queue = context.queue_for_job(JobType::Compute).expect("queue");
    let _ = file;

    // A compute command is 128 bytes and the ring is 32 KiB, so a real job is
    // nowhere near the limit — and one that was would be `E2BIG` rather than a
    // wait that never ends, because no amount of the firmware draining the ring
    // makes room for a sequence bigger than half of it.
    assert!(queue.job_can_fit(size_of::<crate::rogue::fwif::RogueFwifCmdCompute>(), 0) == Ok(true));
    assert_eq!(queue.job_can_fit(1 << 20, 0), Ok(false));
}

#[ktest]
fn the_job_command_size_accounts_for_every_command_in_the_sequence() {
    use crate::{rogue::fwif::RogueFwifUfo, sched::cccb::Cccb};

    // One `UPDATE` command carrying one UFO, and nothing else.
    let bare = queue::job_cmds_size(8, 0).expect("size");
    let one_ufo = Cccb::size_of_cmd_with_hdr(size_of::<RogueFwifUfo>() as u32).expect("size");
    let one_cmd = Cccb::size_of_cmd_with_hdr(8).expect("size");
    assert_eq!(bare, one_cmd + one_ufo);

    // A dependency adds a `FENCE_PR` command carrying one UFO.
    assert_eq!(
        queue::job_cmds_size(8, 1).expect("size"),
        one_cmd + one_ufo + one_ufo
    );

    // Exactly the maximum number of UFOs is one full command, and one more is two.
    let full =
        Cccb::size_of_cmd_with_hdr((abi::CCB_CMD_MAX_UFOS * size_of::<RogueFwifUfo>()) as u32)
            .expect("size");
    assert_eq!(
        queue::job_cmds_size(8, abi::CCB_CMD_MAX_UFOS).expect("size"),
        one_cmd + one_ufo + full
    );
    assert_eq!(
        queue::job_cmds_size(8, abi::CCB_CMD_MAX_UFOS + 1).expect("size"),
        one_cmd + one_ufo + full + one_ufo
    );
}

#[ktest]
fn a_context_only_accepts_the_job_types_it_has_queues_for() {
    assert!(ContextType::Render.accepts(JobType::Geometry));
    assert!(ContextType::Render.accepts(JobType::Fragment));
    assert!(!ContextType::Render.accepts(JobType::Compute));
    assert!(ContextType::Compute.accepts(JobType::Compute));
    assert!(!ContextType::Compute.accepts(JobType::Transfer));
    assert!(ContextType::TransferFrag.accepts(JobType::Transfer));
    assert!(!ContextType::TransferFrag.accepts(JobType::Fragment));

    assert_eq!(ContextType::from_uapi(0), Ok(ContextType::Render));
    assert_eq!(
        ContextType::from_uapi(3).err(),
        Some(PvrError::InvalidArgument)
    );
    assert_eq!(JobType::from_uapi(3), Ok(JobType::Transfer));
    assert_eq!(JobType::from_uapi(4).err(), Some(PvrError::InvalidArgument));
}

#[ktest]
fn a_job_for_the_wrong_context_type_is_refused() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = Context::new(
        &device,
        vm,
        ContextType::TransferFrag,
        Priority::Medium,
        &[],
        0,
    )
    .expect("transfer context");
    let handle = file.render_contexts.insert(context).expect("handle");

    // The job says compute and the context is a transfer one, so there is no
    // queue for it. Upstream refuses this in `pvr_compute_job_fw_cmd_init()`.
    let streams = alloc::vec![compute_command(&device)];
    let (array, raw) = submit_array(handle, &streams);
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn an_unknown_flag_bit_is_refused() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file.render_contexts.insert(context).expect("handle");

    let streams = alloc::vec![compute_command(&device)];
    let (array, mut raw) = submit_array(handle, &streams);
    let at = offset_of!(uapi::Job, flags);
    // Bit 2 is not a compute flag. A newer client asking for behaviour this
    // driver does not implement must be told so, not have it quietly dropped.
    raw[at..at + 4].copy_from_slice(&0b100u32.to_le_bytes());
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_non_render_job_may_not_name_an_hwrt_dataset() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let context = compute_context(&device, &vm);
    let handle = file.render_contexts.insert(context).expect("handle");

    let streams = alloc::vec![compute_command(&device)];
    let (array, mut raw) = submit_array(handle, &streams);
    let at = offset_of!(uapi::Job, hwrt) + offset_of!(uapi::HwrtDataRef, set_handle);
    raw[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(
        prepare_batch(&file, &array, &raw, &streams).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn the_kernel_command_buffer_reports_a_full_ring_rather_than_overwriting_it() {
    let device = device();
    let kccb = device.kccb.get().expect("kccb");

    // Read into a local first: a lock guard in a `for` loop's iterator expression
    // would be held for the whole body, and `reserve_slot` takes the same lock.
    let capacity = kccb.lock().ccb().capacity();

    // The ring holds 128 slots and one is always left unused, so 127 reservations
    // succeed and the next one has nothing to give. The firmware never runs, so
    // nothing is ever released.
    for _ in 0..capacity {
        ccb::reserve_slot(&device).expect("a slot is free");
    }
    assert_eq!(ccb::reserve_slot(&device).err(), Some(PvrError::Busy));

    // Giving one back makes room again.
    ccb::release_slot(&device);
    ccb::reserve_slot(&device).expect("the released slot");
}

/// An initialized submission path with no outstanding commands is idle.
///
/// The watchdog must not equate zero execution progress with a hang when there
/// was no command for firmware to execute.
#[ktest]
fn an_untouched_kernel_command_buffer_reads_as_idle() {
    let device = device();

    // No command has been submitted; allocated rings alone do not imply work.
    assert!(crate::power::kccb_is_idle(&device));

    // ...and it is not answering "idle" unconditionally: one command written into
    // the ring moves the write offset past the read offset, and the firmware in
    // this fixture never runs to catch up.
    let mut cmd = <crate::rogue::fwif::RogueFwifKccbCmd as ostd_pod::FromZeros>::new_zeroed();
    // Which command it is does not matter -- nothing consumes the ring in this
    // fixture -- so this is the one with no payload the driver has to fill in.
    cmd.cmd_type = abi::KCCB_CMD_LOGTYPE_UPDATE;
    ccb::send(&device, &cmd).expect("an empty ring has room");
    assert!(!crate::power::kccb_is_idle(&device));
}

#[ktest]
fn a_firmware_ccb_command_without_the_magic_value_is_ignored() {
    let device = device();
    let fwccb = device.fwccb.get().expect("fwccb");

    // Corruption of shared memory rather than an unknown command: the driver must
    // not act on it, and must not mistake it for forward compatibility.
    write_fwccb_command(fwccb, 0);
    ccb::process_fwccb(&device);
    assert!(device.check_alive().is_ok());

    // A well-formed command the driver does not know is different, and also
    // harmless.
    write_fwccb_command(fwccb, abi::CMD_MAGIC_DWORD_SHIFTED | 199);
    ccb::process_fwccb(&device);
    assert!(device.check_alive().is_ok());

    // A firmware restart notification requests a soft reset from the service
    // worker without declaring the device permanently lost.
    write_fwccb_command(fwccb, abi::FWCCB_CMD_REQUEST_GPU_RESTART);
    ccb::process_fwccb(&device);
    assert!(device.check_alive().is_ok());
    assert!(device.reset_request.is_pending());
}

/// Writes one command into the FWCCB as the firmware would, and advances its
/// write offset.
fn write_fwccb_command(
    fwccb: &ostd::sync::SpinLock<ccb::Ccb, aster_softirq::BottomHalfDisabled>,
    cmd_type: u32,
) {
    use crate::rogue::fwif::RogueFwifFwccbCmd;

    let ccb = fwccb.lock();
    let mut command = <RogueFwifFwccbCmd as ostd_pod::FromZeros>::new_zeroed();
    command.cmd_type = cmd_type;
    ccb.write_for_test(&command).expect("write");
}

// The injection is per heap/table and compiled only for ktest; tests use the
// production Context -> Queue -> CCCB constructors, not a rollback model.
#[ktest]
fn allocation_failures_restore_every_context_range() {
    let device = device();
    let vm = VmContext::new(device.clone(), VmContextKind::User).expect("VM");
    let fw = device.fw.get().expect("firmware");
    let baseline = fw.heap.lock().ranges_for_test();
    let ptes = fw.mips.page_table.entries_for_test();
    let count = stream::main_stream_field_count(StreamType::StaticRenderContext, &device.info);
    let state = stream::build_stream(
        StreamType::StaticRenderContext,
        &device.info,
        &alloc::vec![0; count],
    )
    .expect("static render state");
    // Context + four objects in each of the geometry/fragment queues.
    for allowed in 0..9 {
        fw.heap.lock().fail_alloc_after_for_test(Some(allowed));
        let result = Context::new(
            &device,
            vm.clone(),
            ContextType::Render,
            Priority::Medium,
            &state,
            0,
        );
        fw.heap.lock().fail_alloc_after_for_test(None);
        assert_eq!(
            result.err(),
            Some(PvrError::NoMemory),
            "allocation {}",
            allowed
        );
        assert_eq!(
            fw.heap.lock().ranges_for_test(),
            baseline,
            "allocation {}",
            allowed
        );
        assert_eq!(
            fw.mips.page_table.entries_for_test(),
            ptes,
            "allocation {}",
            allowed
        );
    }
    let context = Context::new(
        &device,
        vm,
        ContextType::Render,
        Priority::Medium,
        &state,
        0,
    )
    .expect("success after rollback");
    // vm is owned by context; keep a reference while comparing the baseline.
    let vm = context.vm.clone();
    drop(context);
    crate::service::drain_retired_resources(&device);
    assert_eq!(fw.heap.lock().ranges_for_test(), baseline);
    assert_eq!(fw.mips.page_table.entries_for_test(), ptes);
    drop(vm);
}

#[ktest]
fn mapping_failures_restore_partial_queues_and_preserve_other_contexts() {
    let device = device();
    let vm = VmContext::new(device.clone(), VmContextKind::User).expect("VM");
    let _bystander = compute_context(&device, &vm);
    let fw = device.fw.get().expect("firmware");
    let baseline = fw.heap.lock().ranges_for_test();
    let ptes = fw.mips.page_table.entries_for_test();
    let count = stream::main_stream_field_count(StreamType::StaticRenderContext, &device.info);
    let state = stream::build_stream(
        StreamType::StaticRenderContext,
        &device.info,
        &alloc::vec![0; count],
    )
    .expect("static render state");
    // Exercise first object, CCCB ctrl/ring, partial ring, second queue, and
    // the final context mapping. Each queue maps eleven pages with this fixture.
    for allowed in [0, 1, 2, 3, 4, 10, 11, 12, 21, 22] {
        fw.mips.page_table.fail_write_after_for_test(Some(allowed));
        let result = Context::new(
            &device,
            vm.clone(),
            ContextType::Render,
            Priority::Medium,
            &state,
            0,
        );
        fw.mips.page_table.fail_write_after_for_test(None);
        assert_eq!(
            result.err(),
            Some(PvrError::InvalidArgument),
            "PTE {}",
            allowed
        );
        assert_eq!(
            fw.heap.lock().ranges_for_test(),
            baseline,
            "PTE {}",
            allowed
        );
        assert_eq!(
            fw.mips.page_table.entries_for_test(),
            ptes,
            "PTE {}",
            allowed
        );
    }
}

// Unlike FwDevice construction, CCB initialization retries on a live firmware
// heap after failure. Exercise the real constructors and every mapped page.
#[ktest]
fn failed_ccb_initialization_restores_ranges_and_ptes_before_retry() {
    use crate::rogue::fwif::{RogueFwifFwccbCmd, RogueFwifKccbCmd};
    let device = device_without_ccbs();
    let fw = device.fw.get().expect("firmware");
    let baseline = fw.heap.lock().ranges_for_test();
    let ptes = fw.mips.page_table.entries_for_test();

    // KCCB control/ring/return slots, then FWCCB control/ring.
    for allowed in 0..5 {
        fw.heap.lock().fail_alloc_after_for_test(Some(allowed));
        let result = ccb::init(&device);
        fw.heap.lock().fail_alloc_after_for_test(None);
        assert_eq!(result, Err(PvrError::NoMemory), "allocation {}", allowed);
        assert_eq!(
            fw.heap.lock().ranges_for_test(),
            baseline,
            "allocation {}",
            allowed
        );
        assert_eq!(
            fw.mips.page_table.entries_for_test(),
            ptes,
            "allocation {}",
            allowed
        );
        assert!(!device.kccb.is_completed() && !device.fwccb.is_completed());
    }

    let page = ostd::mm::PAGE_SIZE;
    let kslots = 1usize << abi::KCCB_NUMCMDS_LOG2;
    let fslots = 1usize << abi::FWCCB_NUMCMDS_LOG2;
    let map_pages = 2
        + (kslots * size_of::<RogueFwifKccbCmd>()).div_ceil(page)
        + (kslots * size_of::<u32>()).div_ceil(page)
        + (fslots * size_of::<RogueFwifFwccbCmd>()).div_ceil(page);
    for allowed in 0..map_pages {
        fw.mips
            .page_table
            .fail_write_after_for_test(Some(allowed as u64));
        let result = ccb::init(&device);
        fw.mips.page_table.fail_write_after_for_test(None);
        assert_eq!(result, Err(PvrError::InvalidArgument), "PTE {}", allowed);
        assert_eq!(
            fw.heap.lock().ranges_for_test(),
            baseline,
            "PTE {}",
            allowed
        );
        assert_eq!(
            fw.mips.page_table.entries_for_test(),
            ptes,
            "PTE {}",
            allowed
        );
        assert!(!device.kccb.is_completed() && !device.fwccb.is_completed());
    }

    // The real public initializer also publishes osinit. No failed attempt may
    // leave a Once cell occupied, otherwise retry would be mistaken for success.
    init(&device).expect("successful retry");
    assert!(device.kccb.is_completed() && device.fwccb.is_completed());
    assert_eq!(init(&device), Err(PvrError::Busy));
    let kccb = device.kccb.get().expect("KCCB").lock();
    assert_eq!(kccb.ccb().used().expect("empty ring"), 0);
}

/// IRQ bottom halves must not re-enter a command ring held by the interrupted task.
#[ktest]
fn command_rings_defer_bottom_half_until_unlocked() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use aster_softirq::{BottomHalfDisabled, Taskless};
    use ostd::sync::SpinLock;

    static SOFTIRQ: spin::Once<()> = spin::Once::new();
    SOFTIRQ.call_once(aster_softirq::init_for_ktest);

    let device = device();
    for firmware_ring in [false, true] {
        let observed = Arc::new(AtomicUsize::new(0));
        let callback_device = device.clone();
        let callback_observed = observed.clone();
        let taskless = Taskless::new(move || {
            let available = if firmware_ring {
                callback_device.fwccb.get().unwrap().try_lock().is_some()
            } else {
                callback_device.kccb.get().unwrap().try_lock().is_some()
            };
            // A failed try_lock records the deadlock that a blocking callback
            // would cause, without hanging the regression test itself.
            callback_observed.store(if available { 1 } else { 2 }, Ordering::Relaxed);
        });
        let dispatch = SpinLock::<(), BottomHalfDisabled>::new(());
        if firmware_ring {
            let ring = device.fwccb.get().unwrap().lock();
            let dispatch_guard = dispatch.lock();
            taskless.schedule();
            drop(dispatch_guard);
            assert_eq!(observed.load(Ordering::Relaxed), 0);
            drop(ring);
        } else {
            let ring = device.kccb.get().unwrap().lock();
            let dispatch_guard = dispatch.lock();
            taskless.schedule();
            drop(dispatch_guard);
            assert_eq!(observed.load(Ordering::Relaxed), 0);
            drop(ring);
        }
        assert_eq!(observed.load(Ordering::Relaxed), 1);
    }
}
