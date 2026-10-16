// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: render-resource ABI and lifetime regression tests.

//! In-kernel tests for free lists, render targets and sync operations.
//!
//! # What "no hardware" means for these
//!
//! The same as for [`crate::sched::test`]: the device is built on a simulated
//! register window and the firmware-image fixture, so everything up to and
//! including "the bytes are in the buffer and the command is in the kernel CCB"
//! really happens. What cannot happen is the firmware *answering*. Two
//! consequences shape the tests below.
//!
//! - The paths that wait for a KCCB answer — [`FreeList::destroy`] and
//!   [`HwrtDataset::destroy`] — are exercised only on a device that has been
//!   declared lost, where they return without asking. Everything else about
//!   destruction is reachable, because it is `Drop` that frees the memory.
//! - The paths that only *send* — the two firmware-request handlers — are fully
//!   reachable, and the ring's write offset is what says they answered. That is
//!   the property the grow tests turn on: growing may fail, answering may not.
//!
//! # The device-info fixture and two values it does not carry
//!
//! `crate::device_info::fixture` sets neither `isp_samples_per_pixel`, which
//! every render target's control-register maths needs, nor `num_isp_ipp_pipes`,
//! without which [`crate::sched::queue`] cannot size a fragment queue's
//! context-state object and a render context is refused outright. Rather than
//! invent values for the shared fixture — the numbers there are modelled on
//! BXS-4-64 and are meant to be re-derived from the real image — the tests here
//! patch them in locally, so each assumption is visible at the one place that
//! depends on it.

use alloc::{sync::Arc, vec::Vec};
use core::mem::offset_of;

use k1_musebook_drm::{
    DrmError,
    fence::{Fence, SoftwareFence, SyncObj, SyncObjTable, TimelinePoint},
};
use ostd::prelude::*;
use ostd_pod::IntoBytes;

use crate::{
    device::{GpuId, PvrDevice, RegisterWindow},
    driver::PvrFile,
    error::PvrError,
    fw::{self, FwInitParams},
    mm::{
        gem::PvrObject,
        mmu::DEVICE_PAGE_SIZE,
        vm::{DeviceAddr, VmContext, VmContextKind},
    },
    render::{
        abi,
        free_list::{self, FREE_LIST_ENTRY_SIZE, FreeList},
        hwrt::HwrtDataset,
    },
    rogue::{fwif::RogueFwifFreelist, heap},
    sched::{self, job, stream},
    sync::{self, Op, Ops, SignalArray},
    uapi,
};

/// The ISP samples-per-pixel value the tests assume.
///
/// One, which is the smallest legal answer and the branch of both
/// `get_cr_te_aa_val()` and `get_cr_isp_mtile_size_val()` that a single-sampled
/// target takes. See the module documentation for why this is not in the shared
/// fixture.
const ISP_SAMPLES_PER_PIXEL: u32 = 1;

/// ISP IPP pipe count for the render-context fixture.
///
/// The value comes from `rogue_36.29.52.182_v1.fw`'s device-info block. Upstream
/// `get_frag_ctx_state_size()` requires this count when `xe_memory_hierarchy`
/// is absent, as it is in the shared fixture.
const NUM_ISP_IPP_PIPES: u32 = 2;

/// A device with firmware loaded and the submission path brought up.
fn device() -> Arc<PvrDevice> {
    let image = fw::fixture::image();
    let mut validated = fw::validate_and_init_device_info(&image).expect("fixture image");
    validated.info.features.isp_samples_per_pixel = true;
    validated.info.values.isp_samples_per_pixel = ISP_SAMPLES_PER_PIXEL;
    validated.info.features.num_isp_ipp_pipes = true;
    validated.info.values.num_isp_ipp_pipes = NUM_ISP_IPP_PIPES;

    let regs = RegisterWindow::simulated();
    regs.write64(
        crate::rogue::cr::core_id_pbvnc::OFFSET,
        GpuId::K1_BXE_2_32.to_packed(),
    )
    .expect("simulated register");
    let device = PvrDevice::probe(regs, &validated).expect("probe");

    fw::init(&device, image, FwInitParams::default()).expect("firmware load");
    sched::init(&device).expect("submission path");
    device
}

/// A file with one VM context in it, that context, and its handle.
fn file_with_vm(device: &Arc<PvrDevice>) -> (Arc<PvrFile>, Arc<VmContext>, u32) {
    let file = PvrFile::new(device.clone());
    let vm = VmContext::new(device.clone(), VmContextKind::User).expect("vm context");
    let handle = file.vm_contexts.insert(vm.clone()).expect("handle");
    (file, vm, handle)
}

/// Maps a buffer big enough for `max_pages` free-list entries and returns its
/// GPU address.
///
/// `flags` is the buffer object's, so that the tests can also build the
/// combinations `CREATE_FREE_LIST` has to refuse.
fn map_list_buffer(vm: &Arc<VmContext>, at: u64, max_pages: u32, flags: u64) -> u64 {
    let bytes = u64::from(max_pages) * u64::from(FREE_LIST_ENTRY_SIZE);
    let size = bytes
        .next_multiple_of(DEVICE_PAGE_SIZE)
        .max(DEVICE_PAGE_SIZE);
    let object = PvrObject::new(size, flags).expect("free-list buffer");
    vm.map(DeviceAddr(at), &object, 0, size).expect("map");
    at
}

/// The GPU address the tests map the first free list at.
const LIST_BASE: u64 = heap::ROGUE_GENERAL_HEAP_BASE;

/// `CREATE_FREE_LIST` arguments with a threshold of zero, which turns the
/// ready-page reserve off and keeps the page arithmetic in the tests exact.
fn list_args(
    gpu_addr: u64,
    vm_handle: u32,
    initial: u32,
    max: u32,
    grow: u32,
) -> uapi::CreateFreeListArgs {
    uapi::CreateFreeListArgs {
        free_list_gpu_addr: gpu_addr,
        initial_num_pages: initial,
        max_num_pages: max,
        grow_num_pages: grow,
        grow_threshold: 0,
        vm_context_handle: vm_handle,
        handle: 0,
    }
}

/// A free list with room to grow, and the file it belongs to.
fn growable(device: &Arc<PvrDevice>) -> (Arc<PvrFile>, Arc<FreeList>) {
    let (file, vm, vm_handle) = file_with_vm(device);
    let addr = map_list_buffer(&vm, LIST_BASE, 128, uapi::bo_flags::PM_FW_PROTECT);
    let args = list_args(addr, vm_handle, 64, 128, 32);
    let free_list = FreeList::new(device, &vm, &args).expect("free list");
    (file, free_list)
}

/// How many commands the kernel CCB has taken.
fn kccb_write_offset(device: &PvrDevice) -> u32 {
    device
        .kccb
        .get()
        .expect("kccb")
        .lock()
        .ccb()
        .write_offset()
        .expect("write offset")
}

// -------------------------------------------------------------------------
// Free lists
// -------------------------------------------------------------------------

#[ktest]
fn a_free_list_refuses_page_counts_that_cannot_work() {
    let device = device();
    let (_file, vm, vm_handle) = file_with_vm(&device);
    let addr = map_list_buffer(&vm, LIST_BASE, 128, uapi::bo_flags::PM_FW_PROTECT);

    // Upstream's six conditions, one per line of its first `if`.
    let mut threshold = list_args(addr, vm_handle, 64, 128, 32);
    threshold.grow_threshold = 101;

    let mut initial_over_max = list_args(addr, vm_handle, 256, 128, 32);
    initial_over_max.grow_num_pages = 32;

    let grow_over_max = list_args(addr, vm_handle, 64, 128, 256);
    let zero_max = list_args(addr, vm_handle, 0, 0, 0);
    // Room to grow into but no grow size: nothing would ever fill it.
    let no_grow_size = list_args(addr, vm_handle, 64, 128, 0);
    // Full already but a grow size anyway: the two contradict each other.
    let pointless_grow = list_args(addr, vm_handle, 128, 128, 32);

    for args in [
        threshold,
        initial_over_max,
        grow_over_max,
        zero_max,
        no_grow_size,
        pointless_grow,
    ] {
        assert_eq!(
            FreeList::new(&device, &vm, &args).err(),
            Some(PvrError::InvalidArgument)
        );
    }

    // And the alignment rule, which applies to all three counts.
    for args in [
        list_args(addr, vm_handle, 65, 128, 32),
        list_args(addr, vm_handle, 64, 129, 32),
        list_args(addr, vm_handle, 64, 128, 33),
    ] {
        assert_eq!(
            FreeList::new(&device, &vm, &args).err(),
            Some(PvrError::InvalidArgument)
        );
    }
}

#[ktest]
fn a_free_list_refuses_a_buffer_it_must_not_write() {
    let device = device();
    let (_file, vm, vm_handle) = file_with_vm(&device);

    // Nothing mapped at the address at all.
    let args = list_args(LIST_BASE, vm_handle, 64, 128, 32);
    assert_eq!(
        FreeList::new(&device, &vm, &args).err(),
        Some(PvrError::InvalidArgument)
    );

    // Mapped, but user space can write it — so it is not the Parameter
    // Manager's and a client could rewrite the page numbers behind the driver.
    let cpu = map_list_buffer(
        &vm,
        LIST_BASE,
        128,
        uapi::bo_flags::ALLOW_CPU_USERSPACE_ACCESS,
    );
    assert_eq!(
        FreeList::new(&device, &vm, &list_args(cpu, vm_handle, 64, 128, 32)).err(),
        Some(PvrError::InvalidArgument)
    );

    // Mapped and not CPU accessible, but not reserved for the firmware either.
    let plain = map_list_buffer(&vm, LIST_BASE + (1 << 20), 128, 0);
    assert_eq!(
        FreeList::new(&device, &vm, &list_args(plain, vm_handle, 64, 128, 32)).err(),
        Some(PvrError::InvalidArgument)
    );

    // Reserved, but too small for `max_num_pages` entries. The count has to be
    // large enough that a whole device page of entries cannot hold it:
    // `map_list_buffer` rounds every mapping up to one, and one 4 KiB page is
    // already 1,024 entries, so a 128-page list fits in the smallest buffer
    // this helper can produce and asking for it would test nothing.
    let entries_per_page = DEVICE_PAGE_SIZE / u64::from(FREE_LIST_ENTRY_SIZE);
    let too_many = 2 * entries_per_page as u32;
    let small = map_list_buffer(&vm, LIST_BASE + (2 << 20), 4, uapi::bo_flags::PM_FW_PROTECT);
    assert_eq!(
        FreeList::new(&device, &vm, &list_args(small, vm_handle, 64, too_many, 32)).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_new_free_list_holds_its_initial_pages() {
    let device = device();
    let (_file, free_list) = growable(&device);

    assert_eq!(free_list.current_pages(), 64);
    assert_eq!(free_list.ready_pages(), 0);
    assert_eq!(free_list.node_count(), 1);
    assert_eq!(free_list.max_pages(), 128);

    // What the firmware reads. `current_dev_addr` points at the first entry the
    // firmware may pop, which is `max_pages - current_pages` entries in.
    let data = free_list.fw_data_for_test().expect("firmware structure");
    assert_eq!(data.max_pages, 128);
    assert_eq!(data.current_pages, 64);
    assert_eq!(data.grow_pages, 32);
    assert_eq!(data.ready_pages, 0);
    assert_eq!(data.current_stack_top, 63);
    assert_eq!(data.freelist_dev_addr, LIST_BASE);
    assert_eq!(
        data.current_dev_addr,
        LIST_BASE + u64::from(128u32 - 64) * u64::from(FREE_LIST_ENTRY_SIZE)
    );
    assert_ne!(data.freelist_id, 0);

    // The list itself: sixty-four consecutive physical page numbers, written at
    // the top of the buffer, because the firmware pops from the high end.
    let entries = free_list.entries_for_test(64).expect("entries");
    assert_eq!(entries.len(), 64);
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(*entry, entries[0] + index as u32);
    }
}

#[ktest]
fn growing_stops_at_the_maximum() {
    let device = device();
    let (_file, free_list) = growable(&device);

    // 64 + 32 + 32 reaches the 128-page ceiling exactly.
    free_list.grow().expect("first grow");
    assert_eq!(free_list.current_pages(), 96);
    assert_eq!(free_list.node_count(), 2);

    free_list.grow().expect("second grow");
    assert_eq!(free_list.current_pages(), 128);
    assert_eq!(free_list.node_count(), 3);

    // The third would take it past `max_pages`, where upstream's
    // `max_pages - current_pages - num_pages` wraps and the write lands outside
    // the client's buffer. Refused, and nothing about the list changed.
    assert_eq!(free_list.grow().err(), Some(PvrError::InvalidArgument));
    assert_eq!(free_list.current_pages(), 128);
    assert_eq!(free_list.node_count(), 3);

    // Every entry is still one contiguous run per node, and the whole buffer is
    // now covered.
    let entries = free_list.entries_for_test(128).expect("entries");
    assert_eq!(entries.len(), 128);
    assert!(entries.iter().all(|entry| *entry != 0));
}

#[ktest]
fn a_grow_request_is_answered_whether_or_not_it_grew() {
    let device = device();
    let (_file, free_list) = growable(&device);
    let id = free_list.fw_id();

    // A grow that succeeds: the pages arrive and the firmware is told how many.
    let before = kccb_write_offset(&device);
    free_list::process_grow_request(&device, id);
    assert_eq!(free_list.current_pages(), 96);
    assert_eq!(kccb_write_offset(&device), before + 1);

    // A full list must still answer a grow request with zero added pages. Returning
    // without a response would leave firmware waiting in the deferred interrupt path.
    free_list.grow().expect("fills the list");
    assert_eq!(free_list.current_pages(), 128);

    let before = kccb_write_offset(&device);
    free_list::process_grow_request(&device, id);
    assert_eq!(kccb_write_offset(&device), before + 1);
    assert_eq!(free_list.current_pages(), 128);
    assert_eq!(free_list.node_count(), 3);
}

#[ktest]
fn a_grow_request_for_a_list_this_driver_does_not_know_answers_nothing() {
    let device = device();
    let (_file, free_list) = growable(&device);

    // Upstream `WARN_ON(!free_list)` and returns: there is no firmware address
    // to name in a response, so there is no response to send.
    let before = kccb_write_offset(&device);
    free_list::process_grow_request(&device, free_list.fw_id().wrapping_add(1000));
    assert_eq!(kccb_write_offset(&device), before);
}

#[ktest]
fn a_reconstruction_rewrites_the_list_and_answers_the_firmware() {
    let device = device();
    let (_file, free_list) = growable(&device);
    free_list.grow().expect("second node");

    let before_pages = free_list.current_pages();
    let before_entries = free_list.entries_for_test(before_pages as usize).unwrap();

    let before = kccb_write_offset(&device);
    free_list::process_reconstruct_request(&device, &[free_list.fw_id()]);
    assert_eq!(kccb_write_offset(&device), before + 1);

    // The list is rebuilt from the nodes it already had, so both the page count
    // and the entries themselves come back identical: what a reconstruction
    // undoes is the firmware's idea of which entries it had popped.
    assert_eq!(free_list.current_pages(), before_pages);
    assert_eq!(free_list.node_count(), 2);
    assert_eq!(
        free_list.entries_for_test(before_pages as usize).unwrap(),
        before_entries
    );

    let data = free_list.fw_data_for_test().expect("firmware structure");
    assert_eq!(data.current_stack_top, before_pages - 1);
    assert_eq!(data.allocated_page_count, 0);
    assert_eq!(data.allocated_mmu_page_count, 0);
}

#[ktest]
fn destroying_a_free_list_on_a_lost_device_does_not_wait_for_it() {
    let device = device();
    let (_file, free_list) = growable(&device);

    // A lost device will never answer, so the cleanup round trip is skipped
    // rather than run into its timeout. Dropping still returns the firmware
    // heap allocation, which is what `Drop` is for.
    device.declare_lost();
    free_list.destroy().expect("nothing to tell a lost device");
}

// -------------------------------------------------------------------------
// Render target datasets
// -------------------------------------------------------------------------

/// A free list large enough to be a dataset's local one.
fn dataset_free_lists(
    device: &Arc<PvrDevice>,
    file: &Arc<PvrFile>,
    vm: &Arc<VmContext>,
    vm_handle: u32,
    local_pages: u32,
) -> (u32, u32) {
    let local_addr = map_list_buffer(vm, LIST_BASE, 128, uapi::bo_flags::PM_FW_PROTECT);
    let global_addr = map_list_buffer(
        vm,
        LIST_BASE + (1 << 20),
        128,
        uapi::bo_flags::PM_FW_PROTECT,
    );

    let local = FreeList::new(
        device,
        vm,
        &list_args(local_addr, vm_handle, local_pages, 128, 32),
    )
    .expect("local free list");
    let global = FreeList::new(device, vm, &list_args(global_addr, vm_handle, 64, 128, 32))
        .expect("global free list");

    (
        file.free_lists.insert(local).expect("local handle"),
        file.free_lists.insert(global).expect("global handle"),
    )
}

/// `CREATE_HWRT_DATASET` arguments for a plain single-layer target.
fn dataset_args(local: u32, global: u32) -> uapi::CreateHwrtDatasetArgs {
    uapi::CreateHwrtDatasetArgs {
        geom_data_args: uapi::CreateHwrtGeomDataArgs {
            tpc_dev_addr: 0x1_0000,
            tpc_size: 0x400,
            tpc_stride: 0x40,
            vheap_table_dev_addr: 0x2_0000,
            rtc_dev_addr: 0x3_0000,
        },
        rt_data_args: [
            uapi::CreateHwrtRtDataArgs {
                pm_mlist_dev_addr: 0x4_0000,
                macrotile_array_dev_addr: 0x5_0000,
                region_header_dev_addr: 0x6_0000,
            },
            uapi::CreateHwrtRtDataArgs {
                pm_mlist_dev_addr: 0x7_0000,
                macrotile_array_dev_addr: 0x8_0000,
                region_header_dev_addr: 0x9_0000,
            },
        ],
        free_list_handles: [local, global],
        width: 1920,
        height: 1080,
        samples: 1,
        layers: 1,
        isp_merge_lower_x: 1,
        isp_merge_lower_y: 2,
        isp_merge_scale_x: 3,
        isp_merge_scale_y: 4,
        isp_merge_upper_x: 5,
        isp_merge_upper_y: 6,
        region_header_size: 0x100,
        handle: 0,
    }
}

#[ktest]
fn a_dataset_describes_two_render_targets_to_the_firmware() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let args = dataset_args(local, global);

    let dataset = HwrtDataset::new(&device, &file, &args).expect("dataset");

    assert_eq!(dataset.max_rts(), 1);
    let first = dataset.data(0).expect("first target").clone();
    let second = dataset.data(1).expect("second target").clone();
    assert!(dataset.data(2).is_none());
    assert_ne!(first.fw_addr().0, 0);
    assert_ne!(first.fw_addr().0, second.fw_addr().0);

    // The cleanup block a `KICK` names sits inside the structure, at the
    // offset upstream's `fill_cmd_kick_data()` takes.
    assert_eq!(
        first.cleanup_state_fw_addr().expect("cleanup address").0,
        first.fw_addr().0 + offset_of!(crate::rogue::fwif::RogueFwifHwrtdata, cleanup_state) as u32
    );

    // Both targets share the common structure and name the same two free lists,
    // and each carries its own render-target addresses.
    let a = first.fw_data_for_test().expect("first structure");
    let b = second.fw_data_for_test().expect("second structure");
    assert_eq!(a.hwrt_data_common_fw_addr, b.hwrt_data_common_fw_addr);
    assert_ne!(a.hwrt_data_common_fw_addr.0, 0);
    assert_eq!(a.freelists_fw_addr[0], b.freelists_fw_addr[0]);
    assert_eq!(a.freelists_fw_addr[1], b.freelists_fw_addr[1]);
    assert_ne!(a.freelists_fw_addr[0], a.freelists_fw_addr[1]);
    assert_eq!(a.pm_mlist_dev_addr, 0x4_0000);
    assert_eq!(b.pm_mlist_dev_addr, 0x7_0000);
    assert_eq!(a.tail_ptrs_dev_addr, 0x1_0000);
    assert_eq!(a.rta_ctl.max_rts, 1);
    // A single-layer target has no render-target array, and a zero address is
    // how the firmware is told so.
    assert_eq!(a.rta_ctl.valid_render_targets_fw_addr.0, 0);
    assert_eq!(a.rta_ctl.rta_num_partial_renders_fw_addr.0, 0);
}

#[ktest]
fn a_layered_dataset_allocates_its_render_target_arrays() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let mut args = dataset_args(local, global);
    args.layers = 4;

    let dataset = HwrtDataset::new(&device, &file, &args).expect("dataset");
    let data = dataset
        .data(0)
        .expect("first target")
        .fw_data_for_test()
        .expect("structure");

    assert_eq!(data.rta_ctl.max_rts, 4);
    assert_ne!(data.rta_ctl.valid_render_targets_fw_addr.0, 0);
    assert_ne!(data.rta_ctl.rta_num_partial_renders_fw_addr.0, 0);
    assert_ne!(
        data.rta_ctl.valid_render_targets_fw_addr,
        data.rta_ctl.rta_num_partial_renders_fw_addr
    );
}

#[ktest]
fn a_dataset_attaches_to_its_local_free_list_and_lets_go() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let local_list = file.free_lists.get(local).expect("local");
    let global_list = file.free_lists.get(global).expect("global");

    let dataset = HwrtDataset::new(&device, &file, &dataset_args(local, global)).expect("dataset");

    // Both render targets register with the local list. The global one is
    // shared between datasets and is not reconstructed per target, so upstream
    // registers nothing against it.
    assert_eq!(local_list.hwrt_count(), 2);
    assert_eq!(global_list.hwrt_count(), 0);

    drop(dataset);
    assert_eq!(local_list.hwrt_count(), 0);
}

#[ktest]
fn a_dataset_refuses_a_target_it_could_not_describe() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);

    let mut zero_width = dataset_args(local, global);
    zero_width.width = 0;
    let mut zero_height = dataset_args(local, global);
    zero_height.height = 0;
    let mut zero_layers = dataset_args(local, global);
    zero_layers.layers = 0;
    // `fls(samples) - 1` is negative for zero and past the end of the sample
    // position table for sixteen.
    let mut zero_samples = dataset_args(local, global);
    zero_samples.samples = 0;
    let mut too_many_samples = dataset_args(local, global);
    too_many_samples.samples = 16;

    for args in [
        zero_width,
        zero_height,
        zero_layers,
        zero_samples,
        too_many_samples,
    ] {
        assert_eq!(
            HwrtDataset::new(&device, &file, &args).err(),
            Some(PvrError::InvalidArgument)
        );
    }

    // And a free-list handle the file does not hold.
    let mut unknown = dataset_args(local, global);
    unknown.free_list_handles[0] = 4242;
    assert_eq!(
        HwrtDataset::new(&device, &file, &unknown).err(),
        Some(PvrError::NoSuchHandle)
    );
}

#[ktest]
fn a_dataset_refuses_a_local_free_list_that_is_too_small() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);

    // `min_pages` is 25 on a RogueXE part without BRN66011, which the fixture
    // is; the alignment rule makes 24 the largest legal count below it.
    assert_eq!(free_list::min_pages(&device.info), 25);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 24);

    assert_eq!(
        HwrtDataset::new(&device, &file, &dataset_args(local, global)).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn reconstructing_a_free_list_resets_the_targets_drawing_from_it() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let local_list = file.free_lists.get(local).expect("local");

    let dataset = HwrtDataset::new(&device, &file, &dataset_args(local, global)).expect("dataset");
    let first = dataset.data(0).expect("first target").clone();

    // A geometry pass has run, as the firmware would record it.
    let fresh = first.fw_data_for_test().expect("structure");
    assert_eq!(fresh.state, 0);
    assert_eq!(fresh.hwrt_data_flags & abi::HWRTDATA_HAS_LAST_GEOM, 0);

    local_list.reconstruct();

    // The parameter buffer that pass filled is gone, so the target is put into
    // the state that makes the firmware skip the work rather than re-run it.
    for index in 0..2 {
        let data = dataset
            .data(index)
            .expect("target")
            .fw_data_for_test()
            .expect("structure");
        assert_eq!(data.state, abi::RTDATA_STATE_HWR);
        assert_eq!(data.hwrt_data_flags & abi::HWRTDATA_HAS_LAST_GEOM, 0);
    }
}

#[ktest]
fn destroying_a_dataset_on_a_lost_device_does_not_wait_for_it() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let dataset = HwrtDataset::new(&device, &file, &dataset_args(local, global)).expect("dataset");

    device.declare_lost();
    dataset.destroy().expect("nothing to tell a lost device");
}

// -------------------------------------------------------------------------
// Sync operations
// -------------------------------------------------------------------------

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

fn timeline_op(handle: u32, point: u64, signal: bool) -> uapi::SyncOp {
    let mut flags = uapi::SyncOpFlags::HANDLE_TYPE_TIMELINE_SYNCOBJ.bits();
    if signal {
        flags |= uapi::SyncOpFlags::SIGNAL.bits();
    }
    uapi::SyncOp {
        handle,
        flags,
        value: point,
    }
}

/// Parses an operation list the way the ioctl handler would.
fn ops(list: &[uapi::SyncOp]) -> Ops {
    let (array, raw) = raw_ops(list);
    Ops::parse(&array, &raw).expect("well-formed operations")
}

/// The array descriptor and bytes for an operation list.
fn raw_ops(list: &[uapi::SyncOp]) -> (uapi::ObjArray, Vec<u8>) {
    let mut raw = Vec::new();
    for op in list {
        raw.extend_from_slice(op.as_bytes());
    }
    (
        uapi::ObjArray {
            stride: size_of::<uapi::SyncOp>() as u32,
            count: list.len() as u32,
            array: 0x4000,
        },
        raw,
    )
}

/// A table with one sync object in it, and its handle.
fn table_with(object: Arc<SyncObj>) -> (SyncObjTable, u32) {
    let table = SyncObjTable::new();
    let handle = table.insert(object).expect("handle");
    (table, handle.0)
}

/// [`sync::dependencies`] against a real device.
///
/// The device is only consulted when a wait names a [`sched::queue::JobFence`];
/// tests that install [`SoftwareFence`]s see `ufo: None` either way, but the
/// parameter is not optional — a missing device would mean inventing a
/// same-device check.
fn resolve(
    table: &SyncObjTable,
    signals: &SignalArray,
    ops: &Ops,
) -> Result<Vec<sync::Wait>, PvrError> {
    sync::dependencies(table, signals, ops, &device())
}

#[ktest]
fn a_wait_on_a_sync_object_with_nothing_in_it_is_refused() {
    let (table, handle) = table_with(SyncObj::new(false));
    let signals = SignalArray::new();

    // Upstream `drm_syncobj_find_fence()` with no `WAIT_FOR_SUBMIT` flag: there
    // is no fence and this driver never blocks waiting for one to appear.
    assert_eq!(
        resolve(&table, &signals, &ops(&[wait_op(handle)])).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_wait_names_the_fence_the_object_holds() {
    let object = SyncObj::new(false);
    let fence = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    object.replace_fence(Some(fence.clone()));
    let (table, handle) = table_with(object);

    let waits = resolve(&table, &SignalArray::new(), &ops(&[wait_op(handle)])).expect("wait");
    assert_eq!(waits.len(), 1);
    assert!(Arc::ptr_eq(
        &waits[0].fence,
        &(fence.clone() as Arc<dyn Fence>)
    ));
    // Nothing in this batch produces it, and a SoftwareFence is not ours, so
    // only the CPU can observe it.
    assert!(waits[0].producer.is_none());
    assert!(waits[0].ufo.is_none());
}

#[ktest]
fn a_wait_on_a_failed_fence_is_still_a_dependency() {
    // Fence error-propagation contract: a failed fence is a *signalled* fence, and a job waiting
    // on one has to fail rather than run. Dropping it here as "already
    // signalled" would run the job against work that never happened.
    let object = SyncObj::new(false);
    let fence = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    fence.signal(Some(DrmError::Io));
    object.replace_fence(Some(fence.clone()));
    let (table, handle) = table_with(object);

    let waits = resolve(&table, &SignalArray::new(), &ops(&[wait_op(handle)])).expect("wait");
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].fence.error(), Some(DrmError::Io));
}

#[ktest]
fn the_two_ways_a_timeline_point_has_no_fence_are_told_apart() {
    let object = SyncObj::new(false);
    // Point five submitted and still running.
    let five = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    object
        .add_point(5, five.clone() as Arc<dyn Fence>)
        .expect("point five");
    let (table, handle) = table_with(object.clone());
    let signals = SignalArray::new();

    // Submitted and still pending, in the sense that a fence exists at or above
    // the point: point five resolves to its own fence, and so does three,
    // because points signal in order and five therefore implies it.
    for point in [3, 5] {
        let waits = resolve(&table, &signals, &ops(&[timeline_op(handle, point, false)]))
            .expect("a submitted point");
        assert_eq!(waits.len(), 1);
    }

    // Unsubmitted: nothing has been submitted that would reach point nine, and
    // this driver does not wait for a submission, so it is an error.
    assert_eq!(
        resolve(&table, &signals, &ops(&[timeline_op(handle, 9, false)])).err(),
        Some(PvrError::InvalidArgument)
    );

    // Passed: point five has signalled and its fence has been pruned away, so
    // nothing is left at or above three. Satisfied already, so no dependency
    // and no error — this is the case that would hang a client for ever if it
    // were folded into "unsubmitted", because nothing will signal point three
    // again.
    //
    // Reaching it takes the prune. While the fence is still there the DRM layer reports a
    // signalled point as pending on purpose, so that a wait below a point that
    // *failed* still sees the error (fence error-propagation contract); the dependency that
    // produces is dropped a layer up, by `Job::add_dependency`.
    five.signal(None);
    object.prune();
    let waits = resolve(&table, &signals, &ops(&[timeline_op(handle, 3, false)]))
        .expect("a passed point is satisfied");
    assert!(waits.is_empty());

    // Pruning does not move the timeline on, so the point past its end is still
    // unsubmitted rather than passed.
    assert_eq!(
        resolve(&table, &signals, &ops(&[timeline_op(handle, 9, false)])).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_later_job_in_a_batch_waits_on_an_earlier_one() {
    let (table, handle) = table_with(SyncObj::new(false));
    let mut signals = SignalArray::new();

    // Job 0 signals the object; job 1 waits on it.
    let producer_ops = ops(&[signal_op(handle)]);
    signals.collect(&table, &producer_ops).expect("collect");
    let job0 = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    signals
        .update(&producer_ops, job0.clone(), 0)
        .expect("update");

    let consumer_ops = ops(&[wait_op(handle)]);
    signals.collect(&table, &consumer_ops).expect("collect");
    let waits = resolve(&table, &signals, &consumer_ops).expect("wait");

    assert_eq!(waits.len(), 1);
    assert!(Arc::ptr_eq(&waits[0].fence, &(job0 as Arc<dyn Fence>)));
    // Job 0 produced it, so the firmware can do the waiting rather than the
    // driver.
    assert_eq!(waits[0].producer, Some(0));
}

#[ktest]
fn a_job_that_signals_and_waits_on_one_point_does_not_depend_on_itself() {
    // With something already there, the wait resolves against *that* — the
    // update phase has not run yet, which is the whole reason the three phases
    // are ordered the way they are.
    let object = SyncObj::new(false);
    let previous = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    object.replace_fence(Some(previous.clone()));
    let (table, handle) = table_with(object);

    let both = ops(&[signal_op(handle), wait_op(handle)]);
    let mut signals = SignalArray::new();
    signals.collect(&table, &both).expect("collect");
    let waits = resolve(&table, &signals, &both).expect("wait");
    assert_eq!(waits.len(), 1);
    assert!(Arc::ptr_eq(&waits[0].fence, &(previous as Arc<dyn Fence>)));
    assert!(waits[0].producer.is_none());

    // With nothing there, there is no fence to wait for and the job's own is not
    // it. Upstream's `WARN_ON(!sig_sync->fence)` returns `-EINVAL` here too.
    let (empty_table, empty_handle) = table_with(SyncObj::new(false));
    let both = ops(&[signal_op(empty_handle), wait_op(empty_handle)]);
    let mut signals = SignalArray::new();
    signals.collect(&empty_table, &both).expect("collect");
    assert_eq!(
        resolve(&empty_table, &signals, &both).err(),
        Some(PvrError::InvalidArgument)
    );
}

#[ktest]
fn a_timeline_point_that_has_already_been_used_is_refused_before_anything_is_committed() {
    // Fence timeline-ordering contract makes a repeated point an error at install time, and install
    // time is after the jobs are queued — where nothing may fail. So it is
    // checked while collecting, where refusing the batch is still free.
    let object = SyncObj::new(false);
    object.signal_point(5).expect("point five");
    let (table, handle) = table_with(object);

    let mut signals = SignalArray::new();
    assert_eq!(
        signals
            .collect(&table, &ops(&[timeline_op(handle, 5, true)]))
            .err(),
        Some(PvrError::InvalidArgument)
    );
    assert_eq!(
        signals
            .collect(&table, &ops(&[timeline_op(handle, 3, true)]))
            .err(),
        Some(PvrError::InvalidArgument)
    );
    // Above everything submitted, so it is fine.
    signals
        .collect(&table, &ops(&[timeline_op(handle, 6, true)]))
        .expect("point six");
}

#[ktest]
fn nothing_reaches_a_sync_object_until_the_batch_is_pushed() {
    let object = SyncObj::new(false);
    let (table, handle) = table_with(object.clone());

    let list = ops(&[signal_op(handle), timeline_op(handle, 7, true)]);
    let mut signals = SignalArray::new();
    signals.collect(&table, &list).expect("collect");
    let fence = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    signals.update(&list, fence.clone(), 0).expect("update");
    assert_eq!(signals.len(), 2);

    // Two entries collected and both given a fence, and the object still has
    // nothing: this is what makes a submission that fails later leave no trace.
    assert!(object.fence().is_none());
    assert_eq!(object.last_submitted_point(), 0);

    signals.push_fences();

    assert!(object.fence().is_some());
    assert_eq!(object.last_submitted_point(), 7);
}

#[ktest]
fn a_signal_on_a_handle_the_file_does_not_hold_is_refused() {
    let (table, handle) = table_with(SyncObj::new(false));
    let mut signals = SignalArray::new();

    assert_eq!(
        signals
            .collect(&table, &ops(&[signal_op(handle.wrapping_add(1))]))
            .err(),
        Some(PvrError::NoSuchHandle)
    );
    assert_eq!(
        resolve(&table, &signals, &ops(&[wait_op(handle.wrapping_add(1))])).err(),
        Some(PvrError::NoSuchHandle)
    );
}

// -------------------------------------------------------------------------
// The submission path, end to end
// -------------------------------------------------------------------------

/// A compute context on `vm`, and its handle.
fn compute_context(
    device: &Arc<PvrDevice>,
    file: &Arc<PvrFile>,
    vm: &Arc<VmContext>,
) -> (u32, Vec<u8>) {
    use crate::sched::{
        context::{Context, ContextType, Priority},
        stream::StreamType,
    };

    let static_state = stream::build_stream(
        StreamType::StaticComputeContext,
        &device.info,
        &[1, 2, 3, 4, 5, 6, 7],
    )
    .expect("seven registers");
    let context = Context::new(
        device,
        vm.clone(),
        ContextType::Compute,
        Priority::Medium,
        &static_state,
        0,
    )
    .expect("compute context");
    let handle = file.render_contexts.insert(context).expect("handle");

    let count = stream::main_stream_field_count(StreamType::Compute, &device.info);
    let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
    let command =
        stream::build_stream(StreamType::Compute, &device.info, &values).expect("field count");
    (handle, command)
}

/// A `SUBMIT_JOBS` array image for compute jobs, one sync-operation array each.
fn submit_array(
    context_handle: u32,
    streams: &[Vec<u8>],
    sync_ops: &[Vec<uapi::SyncOp>],
) -> (uapi::ObjArray, Vec<u8>, Vec<Vec<u8>>) {
    let mut raw = Vec::new();
    let mut op_images = Vec::new();
    for (index, stream) in streams.iter().enumerate() {
        let (array, image) = raw_ops(&sync_ops[index]);
        let job = uapi::Job {
            r#type: job::JobType::Compute as u32,
            context_handle,
            flags: 0,
            cmd_stream_len: stream.len() as u32,
            cmd_stream: 0x1000,
            sync_ops: array,
            hwrt: uapi::HwrtDataRef {
                set_handle: 0,
                data_index: 0,
            },
        };
        raw.extend_from_slice(job.as_bytes());
        op_images.push(image);
    }

    (
        uapi::ObjArray {
            stride: size_of::<uapi::Job>() as u32,
            count: streams.len() as u32,
            array: 0x2000,
        },
        raw,
        op_images,
    )
}

#[ktest]
fn a_submitted_job_carries_the_dependency_its_sync_operations_name() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let (context, command) = compute_context(&device, &file, &vm);

    let object = SyncObj::new(false);
    let blocker = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    object.replace_fence(Some(blocker.clone()));
    let (table, handle) = table_with(object.clone());

    let streams = alloc::vec![command];
    let (array, raw, op_images) = submit_array(
        context,
        &streams,
        &[alloc::vec![wait_op(handle), signal_op(handle)]],
    );

    let (jobs, mut signals) =
        job::prepare_batch(&file, &table, &array, &raw, &streams, &op_images).expect("prepared");

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].deps.len(), 1);
    // Nothing has reached the sync object yet, which is the all-or-nothing
    // property at the level of the whole ioctl.
    assert!(Arc::ptr_eq(
        &object.fence().expect("still the old fence"),
        &(blocker.clone() as Arc<dyn Fence>)
    ));

    let job_fence = jobs[0].fence();
    job::commit_batch(jobs).expect("committed");
    signals.push_fences();

    // Now the object holds the job's fence, not the one it started with.
    assert!(Arc::ptr_eq(
        &object.fence().expect("the job's fence"),
        &job_fence
    ));
}

#[ktest]
fn a_malformed_sync_operation_leaves_the_whole_batch_unsubmitted() {
    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let (context, command) = compute_context(&device, &file, &vm);

    let object = SyncObj::new(false);
    let (table, handle) = table_with(object.clone());

    // Two jobs: the first signals the object, the second carries an operation
    // with an unknown flag bit. Neither may be prepared, and in particular the
    // first must not have left a signal entry that could be pushed.
    let mut bad = wait_op(handle);
    bad.flags = 1 << 30;

    let streams = alloc::vec![command.clone(), command];
    let (array, raw, op_images) = submit_array(
        context,
        &streams,
        &[alloc::vec![signal_op(handle)], alloc::vec![bad]],
    );

    let result = job::prepare_batch(&file, &table, &array, &raw, &streams, &op_images);
    assert!(result.is_err());
    assert!(object.fence().is_none());
    assert_eq!(object.submit_waiter_count(), 0);
}

#[ktest]
fn a_dependency_that_would_close_a_cycle_is_refused() {
    use crate::sched::job::Dep;

    let device = device();
    let (file, vm, _) = file_with_vm(&device);
    let (context, command) = compute_context(&device, &file, &vm);

    let table = SyncObjTable::new();
    let streams = alloc::vec![command];
    let (array, raw, op_images) = submit_array(context, &streams, &[Vec::new()]);
    let (mut jobs, _) =
        job::prepare_batch(&file, &table, &array, &raw, &streams, &op_images).expect("prepared");

    // A fence that already depends on the job. Making the job depend on it in
    // turn would close a two-edge cycle, which fence dependency-cycle check refuses with
    // `EDEADLK` rather than deadlocking at submission time.
    let other = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    other
        .state()
        .add_dependency(&jobs[0].fence())
        .expect("the job does not yet depend on this fence");

    assert_eq!(
        jobs[0]
            .add_dependency(Dep::Foreign(other as Arc<dyn Fence>))
            .err(),
        Some(PvrError::InvalidArgument)
    );

    job::commit_batch(jobs).expect("committed");
}

/// A render context on `vm`, its handle, and a geometry command stream.
fn render_context(
    device: &Arc<PvrDevice>,
    file: &Arc<PvrFile>,
    vm: &Arc<VmContext>,
) -> (u32, Vec<u8>) {
    use crate::sched::{
        context::{Context, ContextType, Priority},
        stream::StreamType,
    };

    let count = stream::main_stream_field_count(StreamType::StaticRenderContext, &device.info);
    let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
    let static_state = stream::build_stream(StreamType::StaticRenderContext, &device.info, &values)
        .expect("static render state");

    let context = Context::new(
        device,
        vm.clone(),
        ContextType::Render,
        Priority::Medium,
        &static_state,
        0,
    )
    .expect("render context");
    let handle = file.render_contexts.insert(context).expect("handle");

    let count = stream::main_stream_field_count(StreamType::Geometry, &device.info);
    let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
    let command =
        stream::build_stream(StreamType::Geometry, &device.info, &values).expect("field count");
    (handle, command)
}

/// A one-job `SUBMIT_JOBS` image for a geometry job naming `hwrt`.
fn geometry_submission(
    context_handle: u32,
    command: &[u8],
    hwrt: uapi::HwrtDataRef,
) -> (uapi::ObjArray, Vec<u8>) {
    let job = uapi::Job {
        r#type: job::JobType::Geometry as u32,
        context_handle,
        flags: 0,
        cmd_stream_len: command.len() as u32,
        cmd_stream: 0x1000,
        sync_ops: raw_ops(&[]).0,
        hwrt,
    };
    (
        uapi::ObjArray {
            stride: size_of::<uapi::Job>() as u32,
            count: 1,
            array: 0x2000,
        },
        job.as_bytes().to_vec(),
    )
}

#[ktest]
fn a_geometry_job_carries_the_render_target_it_names() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let dataset = HwrtDataset::new(&device, &file, &dataset_args(local, global)).expect("dataset");
    let expected = dataset.data(1).expect("second target").fw_addr();
    let set_handle = file.hwrt_datasets.insert(dataset).expect("handle");

    let (context, command) = render_context(&device, &file, &vm);
    let (array, raw) = geometry_submission(
        context,
        &command,
        uapi::HwrtDataRef {
            set_handle,
            data_index: 1,
        },
    );
    let streams = alloc::vec![command];

    let table = SyncObjTable::new();
    let (jobs, _) =
        job::prepare_batch(&file, &table, &array, &raw, &streams, &[Vec::new()]).expect("prepared");

    // Check the firmware-visible render-target address at the offset used by
    // upstream `pvr_geom_job_fw_cmd_init()`.
    let at = offset_of!(crate::rogue::fwif::RogueFwifCmdGeom, cmd_shared)
        + offset_of!(
            crate::rogue::fwif::RogueFwifCmdGeomFragShared,
            hwrt_data_fw_addr
        );
    let written = u32::from_le_bytes(jobs[0].command[at..at + 4].try_into().expect("four bytes"));
    assert_ne!(written, 0);
    assert_eq!(written, expected.0);
}

#[ktest]
fn a_render_job_without_a_render_target_is_refused() {
    let device = device();
    let (file, vm, vm_handle) = file_with_vm(&device);
    let (context, command) = render_context(&device, &file, &vm);
    let table = SyncObjTable::new();
    let streams = alloc::vec![command.clone()];

    // Upstream `pvr_geom_job_fw_cmd_init()` returns `EINVAL` for a render job
    // without an HWRT dataset, even when its context otherwise accepts the job.
    let (array, raw) = geometry_submission(
        context,
        &command,
        uapi::HwrtDataRef {
            set_handle: 0,
            data_index: 0,
        },
    );
    assert_eq!(
        job::prepare_batch(&file, &table, &array, &raw, &streams, &[Vec::new()]).err(),
        Some(PvrError::InvalidArgument)
    );

    // A dataset handle this file does not hold.
    let (array, raw) = geometry_submission(
        context,
        &command,
        uapi::HwrtDataRef {
            set_handle: 4242,
            data_index: 0,
        },
    );
    assert_eq!(
        job::prepare_batch(&file, &table, &array, &raw, &streams, &[Vec::new()]).err(),
        Some(PvrError::NoSuchHandle)
    );

    // And a data index past the end of a dataset that does exist: upstream's
    // `pvr_hwrt_data_lookup()` bounds check, which is the same `EINVAL`.
    let (local, global) = dataset_free_lists(&device, &file, &vm, vm_handle, 64);
    let dataset = HwrtDataset::new(&device, &file, &dataset_args(local, global)).expect("dataset");
    let set_handle = file.hwrt_datasets.insert(dataset).expect("handle");
    let (array, raw) = geometry_submission(
        context,
        &command,
        uapi::HwrtDataRef {
            set_handle,
            data_index: 2,
        },
    );
    assert_eq!(
        job::prepare_batch(&file, &table, &array, &raw, &streams, &[Vec::new()]).err(),
        Some(PvrError::InvalidArgument)
    );
}

// -------------------------------------------------------------------------
// Combined geometry + fragment kicks
// -------------------------------------------------------------------------

/// A fragment command stream a client would send.
fn fragment_command(device: &Arc<PvrDevice>) -> Vec<u8> {
    use crate::sched::stream::StreamType;

    let count = stream::main_stream_field_count(StreamType::Fragment, &device.info);
    let values: Vec<u64> = (0..count).map(|i| i as u64 + 1).collect();
    stream::build_stream(StreamType::Fragment, &device.info, &values).expect("field count")
}

/// One element of a `SUBMIT_JOBS` array, as the tests below describe it.
///
/// The compute-only [`submit_array`] cannot describe a pair: pairing turns on the
/// job type, the context, the render target and the dependency, and each test
/// varies exactly one of them.
struct JobSpec {
    /// What kind of work.
    r#type: job::JobType,
    /// Which context.
    context: u32,
    /// The command stream.
    stream: Vec<u8>,
    /// The render target it names.
    hwrt: uapi::HwrtDataRef,
    /// Its sync operations.
    sync_ops: Vec<uapi::SyncOp>,
    /// `drm_pvr_job::flags`.
    flags: u32,
}

/// A geometry job on `context` naming `hwrt`.
fn geometry_spec(
    context: u32,
    stream: &[u8],
    hwrt: uapi::HwrtDataRef,
    sync_ops: Vec<uapi::SyncOp>,
) -> JobSpec {
    JobSpec {
        r#type: job::JobType::Geometry,
        context,
        stream: stream.to_vec(),
        hwrt,
        sync_ops,
        flags: 0,
    }
}

/// A fragment job on `context` naming `hwrt`.
fn fragment_spec(
    context: u32,
    stream: &[u8],
    hwrt: uapi::HwrtDataRef,
    sync_ops: Vec<uapi::SyncOp>,
) -> JobSpec {
    JobSpec {
        r#type: job::JobType::Fragment,
        context,
        stream: stream.to_vec(),
        hwrt,
        sync_ops,
        flags: 0,
    }
}

/// Prepares a batch from `specs`, the way the ioctl handler would.
fn prepare_specs(
    file: &Arc<PvrFile>,
    table: &SyncObjTable,
    specs: &[JobSpec],
) -> Result<Vec<job::Job>, PvrError> {
    let mut raw = Vec::new();
    let mut streams = Vec::new();
    let mut op_images = Vec::new();
    for spec in specs {
        let (ops_array, ops_image) = raw_ops(&spec.sync_ops);
        let element = uapi::Job {
            r#type: spec.r#type as u32,
            context_handle: spec.context,
            flags: spec.flags,
            cmd_stream_len: spec.stream.len() as u32,
            cmd_stream: 0x1000,
            sync_ops: ops_array,
            hwrt: spec.hwrt,
        };
        raw.extend_from_slice(element.as_bytes());
        streams.push(spec.stream.clone());
        op_images.push(ops_image);
    }

    let array = uapi::ObjArray {
        stride: size_of::<uapi::Job>() as u32,
        count: specs.len() as u32,
        array: 0x2000,
    };
    job::prepare_batch(file, table, &array, &raw, &streams, &op_images).map(|(jobs, _)| jobs)
}

/// Everything a pairing test needs: a render context and a render target.
///
/// Returns the context, its handle, its two queues' owner, a geometry stream and
/// the dataset handle.
struct RenderFixture {
    /// The device.
    device: Arc<PvrDevice>,
    /// The file that owns the handles.
    file: Arc<PvrFile>,
    /// The address space.
    vm: Arc<VmContext>,
    /// The VM context's handle, for further free lists.
    vm_handle: u32,
    /// The render context's handle.
    context: u32,
    /// A geometry command stream for it.
    geometry: Vec<u8>,
    /// A fragment command stream for it.
    fragment: Vec<u8>,
    /// A render target both halves can name.
    hwrt: uapi::HwrtDataRef,
}

impl RenderFixture {
    fn new() -> Self {
        let device = device();
        let (file, vm, vm_handle) = file_with_vm(&device);
        let hwrt = uapi::HwrtDataRef {
            set_handle: new_dataset(&device, &file, &vm, vm_handle),
            data_index: 0,
        };
        let (context, geometry) = render_context(&device, &file, &vm);
        let fragment = fragment_command(&device);
        Self {
            device,
            file,
            vm,
            vm_handle,
            context,
            geometry,
            fragment,
            hwrt,
        }
    }

    /// The render context itself, for the queues behind it.
    fn context(&self) -> Arc<sched::context::Context> {
        self.file
            .render_contexts
            .get(self.context)
            .expect("context")
    }

    fn geometry_queue(&self) -> Arc<sched::queue::Queue> {
        self.context()
            .queue_for_job(job::JobType::Geometry)
            .expect("geometry queue")
            .clone()
    }

    fn fragment_queue(&self) -> Arc<sched::queue::Queue> {
        self.context()
            .queue_for_job(job::JobType::Fragment)
            .expect("fragment queue")
            .clone()
    }
}

/// A render target dataset on `vm`, and its handle.
///
/// Each one gets its own pair of free lists at a fresh address, so that a test can
/// have two datasets that really are different objects.
fn new_dataset(
    device: &Arc<PvrDevice>,
    file: &Arc<PvrFile>,
    vm: &Arc<VmContext>,
    vm_handle: u32,
) -> u32 {
    // Free lists are mapped one after another, so the base moves along with the
    // number already created rather than colliding with them.
    let existing = file.free_lists.len() as u64;
    let base = LIST_BASE + existing * (1 << 20);
    let local_addr = map_list_buffer(vm, base, 128, uapi::bo_flags::PM_FW_PROTECT);
    let global_addr = map_list_buffer(vm, base + (1 << 20), 128, uapi::bo_flags::PM_FW_PROTECT);

    let local = FreeList::new(device, vm, &list_args(local_addr, vm_handle, 64, 128, 32))
        .expect("local free list");
    let global = FreeList::new(device, vm, &list_args(global_addr, vm_handle, 64, 128, 32))
        .expect("global free list");
    let local = file.free_lists.insert(local).expect("local handle");
    let global = file.free_lists.insert(global).expect("global handle");

    let dataset =
        HwrtDataset::new(device, file, &dataset_args(local, global)).expect("hwrt dataset");
    file.hwrt_datasets.insert(dataset).expect("dataset handle")
}

/// The last command the driver put in the kernel CCB.
fn last_kccb_command(device: &PvrDevice) -> crate::rogue::fwif::RogueFwifKccbCmd {
    device
        .kccb
        .get()
        .expect("kccb")
        .lock()
        .last_command()
        .expect("kccb read")
        .expect("a command was sent")
}

/// The payload of the first command of `cmd_type` in a queue's ring.
fn first_command(queue: &Arc<sched::queue::Queue>, cmd_type: u32) -> Vec<u8> {
    queue
        .cccb_commands()
        .expect("ring read")
        .into_iter()
        .find(|(kind, _)| *kind == cmd_type)
        .map(|(_, payload)| payload)
        .unwrap_or_else(|| panic!("no command of type {cmd_type:#x} in the ring"))
}

/// The UFOs a `FENCE_PR` payload carries.
fn ufos(payload: &[u8]) -> Vec<crate::rogue::fwif::RogueFwifUfo> {
    payload
        .chunks_exact(size_of::<crate::rogue::fwif::RogueFwifUfo>())
        .map(<crate::rogue::fwif::RogueFwifUfo as ostd_pod::Pod>::from_bytes)
        .collect()
}

#[ktest]
fn a_geometry_job_and_the_fragment_after_it_reach_the_firmware_in_one_kick() {
    let f = RenderFixture::new();
    let geom_queue = f.geometry_queue();
    let frag_queue = f.fragment_queue();
    let geom_timeline = geom_queue.timeline_fw_addr();

    // The client's own geometry-then-fragment dependency, expressed the way Mesa
    // expresses it: the geometry job signals a sync object and the fragment job
    // waits on it. That edge is what `can_combine_jobs()` looks for.
    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]),
    ];

    let before = kccb_write_offset(&f.device);
    let jobs = prepare_specs(&f.file, &table, &specs).expect("prepared");

    // One element, not two: the fragment job travels inside the geometry job so
    // that `commit_batch` hands both halves to one `Queue::push`.
    assert_eq!(jobs.len(), 1);
    let fences = jobs[0].fences();
    assert_eq!(fences.len(), 2);
    let geom_seqno = fences[0].seqno() as u32;
    let frag_seqno = fences[1].seqno() as u32;

    job::commit_batch(jobs).expect("committed");

    // Both halves went to the firmware, each in its own ring.
    assert_eq!(geom_queue.in_flight_count(), 1);
    assert_eq!(frag_queue.in_flight_count(), 1);

    // One kernel CCB slot for the pair. Upstream drops the fragment job's KCCB
    // fence when it makes the pair for exactly this reason: the geometry job's
    // reservation covers both.
    assert_eq!(kccb_write_offset(&f.device), before + 1);

    // And it really is the combined command, naming both contexts and both rings.
    let kick = last_kccb_command(&f.device);
    assert_eq!(kick.cmd_type, sched::abi::KCCB_CMD_COMBINED_GEOM_FRAG_KICK);
    let combined = kick.cmd_data.combined_geom_frag_cmd_kick_data();
    assert_eq!(
        combined.geom_cmd_kick_data.context_fw_addr,
        geom_queue.ctx_fw_addr()
    );
    assert_eq!(
        combined.frag_cmd_kick_data.context_fw_addr,
        frag_queue.ctx_fw_addr()
    );
    assert_eq!(
        combined.geom_cmd_kick_data.client_woff_update,
        geom_queue.cccb_write_offset()
    );
    assert_eq!(
        combined.frag_cmd_kick_data.client_woff_update,
        frag_queue.cccb_write_offset()
    );
    // Neither half is a partial render, so both retain the render target and the
    // firmware reports cleanup on it for each.
    assert_eq!(combined.geom_cmd_kick_data.num_cleanup_ctl, 1);
    assert_eq!(combined.frag_cmd_kick_data.num_cleanup_ctl, 1);

    // The fragment half's wait on the geometry half is in its ring, as the
    // partial-render fence upstream emits: the geometry queue's timeline UFO at
    // the geometry job's own sequence number.
    let frag_waits = ufos(&first_command(
        &frag_queue,
        sched::abi::CCB_CMD_TYPE_FENCE_PR,
    ));
    assert!(
        frag_waits
            .iter()
            .any(|ufo| ufo.addr == geom_timeline.0 && ufo.value == geom_seqno),
        "the fragment half does not wait for the geometry half"
    );

    // And the geometry command carries the UFO the firmware tests when it decides
    // to start a partial render: the same timeline, one value *behind* the
    // geometry job, which is what it reads while that job is the one running.
    let geom_cmd = first_command(&geom_queue, sched::abi::CCB_CMD_TYPE_GEOM);
    let at = offset_of!(
        crate::rogue::fwif::RogueFwifCmdGeom,
        partial_render_geom_frag_fence
    );
    let addr = u32::from_le_bytes(geom_cmd[at..at + 4].try_into().expect("four bytes"));
    let value = u32::from_le_bytes(geom_cmd[at + 4..at + 8].try_into().expect("four bytes"));
    assert_eq!(addr, geom_timeline.0);
    assert_eq!(value, geom_seqno.wrapping_sub(1));

    // Each half still retires on its own queue's timeline, which is what keeps
    // fence-completion contract an ordinary property of the queues rather than something the pair
    // has to arrange.
    geom_queue.retire(geom_seqno);
    assert!(fences[0].is_signaled());
    assert!(!fences[1].is_signaled());
    frag_queue.retire(frag_seqno);
    assert!(fences[1].is_signaled());
}

#[ktest]
fn a_partial_render_fragment_job_does_not_retain_the_target_twice() {
    let f = RenderFixture::new();

    // Upstream: "if this is a partial-render job, we don't attach resources to the
    // cleanup-ctl array, because the resources are already retained by the
    // geometry job."
    let (table, sync) = table_with(SyncObj::new(false));
    let mut frag = fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]);
    frag.flags = uapi::FragCmdFlags::PARTIAL_RENDER.bits() as u32;
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        frag,
    ];

    let jobs = prepare_specs(&f.file, &table, &specs).expect("prepared");
    assert_eq!(jobs.len(), 1);
    job::commit_batch(jobs).expect("committed");

    let kick = last_kccb_command(&f.device);
    assert_eq!(kick.cmd_type, sched::abi::KCCB_CMD_COMBINED_GEOM_FRAG_KICK);
    let combined = kick.cmd_data.combined_geom_frag_cmd_kick_data();
    assert_eq!(combined.geom_cmd_kick_data.num_cleanup_ctl, 1);
    assert_eq!(combined.frag_cmd_kick_data.num_cleanup_ctl, 0);

    // The command really is the partial-render one, so the branch above was taken
    // for the reason the test claims.
    assert!(
        f.fragment_queue()
            .cccb_commands()
            .expect("ring read")
            .iter()
            .any(|(kind, _)| *kind == sched::abi::CCB_CMD_TYPE_FRAG_PR)
    );
}

#[ktest]
fn jobs_that_differ_in_context_target_order_or_dependency_are_not_paired() {
    let f = RenderFixture::new();

    // A second render context on the same address space, and a second render
    // target, so that each condition can be varied on its own.
    let (other_context, other_geometry) = render_context(&f.device, &f.file, &f.vm);
    let other_hwrt = uapi::HwrtDataRef {
        set_handle: new_dataset(&f.device, &f.file, &f.vm, f.vm_handle),
        data_index: 0,
    };

    // A different context: the two halves would be kicked on two firmware
    // contexts, which is not what `COMBINED_GEOM_FRAG_KICK` describes.
    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        geometry_spec(
            other_context,
            &other_geometry,
            f.hwrt,
            alloc::vec![signal_op(sync)],
        ),
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]),
    ];
    assert_eq!(
        prepare_specs(&f.file, &table, &specs)
            .expect("prepared")
            .len(),
        2,
        "jobs on different contexts must not pair"
    );

    // A different render target: there would be no single HWRT for the kick's
    // cleanup-control entry to name, and the partial render would have nowhere to
    // go.
    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        fragment_spec(
            f.context,
            &f.fragment,
            other_hwrt,
            alloc::vec![wait_op(sync)],
        ),
    ];
    assert_eq!(
        prepare_specs(&f.file, &table, &specs)
            .expect("prepared")
            .len(),
        2,
        "jobs on different render targets must not pair"
    );

    // The other way round: a fragment job followed by a geometry job that waits
    // for it. A pair is geometry *then* fragment, because the point of it is to
    // have a fragment job in hand while the geometry job is running.
    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![signal_op(sync)]),
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![wait_op(sync)]),
    ];
    assert_eq!(
        prepare_specs(&f.file, &table, &specs)
            .expect("prepared")
            .len(),
        2,
        "a fragment job followed by a geometry job must not pair"
    );

    // Adjacent, same context, same target — but no dependency at all. Pairing is
    // the client asking for a partial render, not the driver guessing that two
    // adjacent jobs belong together.
    let table = SyncObjTable::new();
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, Vec::new()),
        fragment_spec(f.context, &f.fragment, f.hwrt, Vec::new()),
    ];
    assert_eq!(
        prepare_specs(&f.file, &table, &specs)
            .expect("prepared")
            .len(),
        2,
        "jobs with no dependency between them must not pair"
    );

    // Adjacent to the *wrong* geometry job: the fragment job waits for the first
    // of two, so the geometry job immediately in front of it is not the one it
    // depends on. Upstream compares against `job_data[i]`'s scheduled fence and
    // nothing else, so this must not pair either.
    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        geometry_spec(f.context, &f.geometry, f.hwrt, Vec::new()),
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]),
    ];
    assert_eq!(
        prepare_specs(&f.file, &table, &specs)
            .expect("prepared")
            .len(),
        3,
        "a fragment job must pair only with the geometry job in front of it"
    );
}

#[ktest]
fn a_pair_waits_for_the_fragment_jobs_queued_before_it() {
    let f = RenderFixture::new();
    let geom_queue = f.geometry_queue();
    let frag_queue = f.fragment_queue();

    // An earlier fragment job, then the pair. Upstream keeps the fragment half of
    // the kick behind it with a dependency on `get_last_queued_job_scheduled_fence()`;
    // here the fragment half is in its own queue in order, so the pair simply does
    // not go until nothing unsubmitted is ahead of it.
    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        fragment_spec(f.context, &f.fragment, f.hwrt, Vec::new()),
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]),
    ];
    let mut jobs = prepare_specs(&f.file, &table, &specs).expect("prepared");
    // Two elements: the earlier fragment job, and the geometry job carrying the
    // later one.
    assert_eq!(jobs.len(), 2);

    // Hold the earlier fragment job up on something only the CPU can observe.
    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    jobs[0]
        .add_dependency(job::Dep::Foreign(gate.clone() as Arc<dyn Fence>))
        .expect("dependency recorded");
    let seqnos: Vec<u32> = jobs
        .iter()
        .flat_map(|job| job.fences())
        .map(|fence| fence.seqno() as u32)
        .collect();

    let before = kccb_write_offset(&f.device);
    job::commit_batch(jobs).expect("committed");

    // Nothing has gone. The pair is ready in itself, but sending it now would put
    // its fragment half in the ring ahead of a fragment job the client asked for
    // first, and then the fragment timeline would no longer say which jobs are
    // done in the order they were queued.
    assert_eq!(geom_queue.in_flight_count(), 0);
    assert_eq!(frag_queue.in_flight_count(), 0);
    assert_eq!(kccb_write_offset(&f.device), before);

    // Releasing the earlier fragment job lets it go, and the fragment queue then
    // tells the geometry queue its backlog has cleared — neither queue can see the
    // other's state, so without that the pair would sit until the next ioctl.
    gate.signal(None);
    assert_eq!(frag_queue.in_flight_count(), 0);
    assert_eq!(kccb_write_offset(&f.device), before);
    sched::queue::process_all(&f.device);
    assert_eq!(frag_queue.in_flight_count(), 2);
    assert_eq!(geom_queue.in_flight_count(), 1);
    // Two kicks: the earlier fragment job's own, and the pair's combined one.
    assert_eq!(kccb_write_offset(&f.device), before + 2);
    assert_eq!(
        last_kccb_command(&f.device).cmd_type,
        sched::abi::KCCB_CMD_COMBINED_GEOM_FRAG_KICK
    );

    // The rings agree with the order the client asked for: the earlier fragment
    // job's command comes before the paired one's.
    let commands = frag_queue.cccb_commands().expect("ring read");
    let frag_indices: Vec<usize> = commands
        .iter()
        .enumerate()
        .filter(|(_, (kind, _))| *kind == sched::abi::CCB_CMD_TYPE_FRAG)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(frag_indices.len(), 2);
    assert!(frag_indices[0] < frag_indices[1]);
    // Sequence numbers ascend with ring position, which is the property
    // `Queue::process` relies on to decide what the timeline has retired.
    assert!(seqnos[0] < seqnos[2]);
}

/// Closing a file transfers firmware resources to sleepable cleanup.
///
/// The service must retain them until firmware releases them. Context cleanup
/// precedes free-list cleanup because pending context work may name those pages.
#[ktest]
fn closing_a_file_hands_its_firmware_structures_to_the_service_thread() {
    let f = RenderFixture::new();
    // No threads in a ktest, and `enqueue` drops what nothing can drain.

    // The fixture's file owns one render context, the two free lists its dataset
    // needs, one dataset, and one address space.
    assert_eq!(f.file.render_contexts.len(), 1);
    assert_eq!(f.file.free_lists.len(), 2);
    assert_eq!(f.file.hwrt_datasets.len(), 1);

    f.file.teardown();

    // Nothing is reachable from user space any more...
    assert_eq!(f.file.render_contexts.len(), 0);
    assert_eq!(f.file.free_lists.len(), 0);
    assert_eq!(f.file.hwrt_datasets.len(), 0);
    // ...and nothing has been freed behind the firmware's back either.
    assert_eq!(
        crate::service::doomed_kinds(&f.device),
        alloc::vec!["context", "free list", "free list", "render target dataset"]
    );
}

/// IRQ-side completion transfers the final render-target owner to the service thread.
#[ktest]
fn completed_hwrt_reference_is_released_only_by_service() {
    let f = RenderFixture::new();
    let dataset = f
        .file
        .hwrt_datasets
        .get(f.hwrt.set_handle)
        .expect("dataset");
    let weak = Arc::downgrade(&dataset);
    let specs = [geometry_spec(f.context, &f.geometry, f.hwrt, Vec::new())];
    let jobs = prepare_specs(&f.file, &SyncObjTable::new(), &specs).expect("prepare");
    job::commit_batch(jobs).expect("queue");
    drop(
        f.file
            .hwrt_datasets
            .remove(f.hwrt.set_handle)
            .expect("remove handle"),
    );
    drop(dataset);

    f.geometry_queue().retire(1);
    assert!(weak.upgrade().is_some());
    crate::service::drain_retired_resources(&f.device);
    assert!(weak.upgrade().is_none());
}

/// Failing a submitted fence must not release render-target memory still used by DMA.
#[ktest]
fn failed_hwrt_reference_waits_for_confirmed_stop_epoch() {
    let f = RenderFixture::new();
    let dataset = f
        .file
        .hwrt_datasets
        .get(f.hwrt.set_handle)
        .expect("dataset");
    let weak = Arc::downgrade(&dataset);
    let specs = [geometry_spec(f.context, &f.geometry, f.hwrt, Vec::new())];
    let jobs = prepare_specs(&f.file, &SyncObjTable::new(), &specs).expect("prepare");
    job::commit_batch(jobs).expect("queue");
    drop(
        f.file
            .hwrt_datasets
            .remove(f.hwrt.set_handle)
            .expect("remove handle"),
    );
    drop(dataset);

    f.geometry_queue().fail_all(DrmError::Io);
    crate::service::drain_retired_resources(&f.device);
    assert!(weak.upgrade().is_some());
    f.device.declare_lost();
    crate::service::drain_retired_resources(&f.device);
    assert!(weak.upgrade().is_some());

    // The fixture executes no DMA. Model a successful complete stop followed by restart.
    f.device
        .quiescence_epoch
        .fetch_add(1, core::sync::atomic::Ordering::Release);
    crate::service::drain_retired_resources(&f.device);
    assert!(weak.upgrade().is_none());
}

/// Error publication alone must not release allocations still reachable by DMA.
#[ktest]
fn lost_cleanup_waits_for_confirmed_quiescence() {
    let f = RenderFixture::new();
    f.file.teardown();
    assert_eq!(crate::service::doomed_kinds(&f.device).len(), 4);

    f.device.declare_lost();
    crate::service::drain_doomed(&f.device);
    assert_eq!(crate::service::doomed_kinds(&f.device).len(), 4);

    // The fixture never executes GPU DMA. Model a complete successful hardware stop.
    f.device
        .quiesced
        .store(true, core::sync::atomic::Ordering::Release);
    crate::service::drain_doomed(&f.device);
    assert!(crate::service::doomed_kinds(&f.device).is_empty());
}

/// The one place a queue writes into another queue's ring, so the one place that
/// has to ask whether the other queue is stopped.
///
/// Pins a local invariant rather than a reachable failure: a reset holds the
/// device's reset lock for writing throughout, so nothing can be pushed while
/// queues are stopped. The check exists so that stops keep meaning "nothing of
/// mine reaches the firmware" without depending on that argument holding in three
/// other files. The releasing half is covered by
/// [`a_pair_waits_for_the_fragment_jobs_queued_before_it`], which reaches the same
/// `Blocked` state through a backlog instead.
#[ktest]
fn a_pair_does_not_go_into_a_fragment_queue_that_is_stopped() {
    let f = RenderFixture::new();
    let geom_queue = f.geometry_queue();
    let frag_queue = f.fragment_queue();

    frag_queue.stop();
    assert!(frag_queue.is_stopped());

    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]),
    ];
    let jobs = prepare_specs(&f.file, &table, &specs).expect("prepared");
    // One element: the geometry job carries the fragment half.
    assert_eq!(jobs.len(), 1);

    let before = kccb_write_offset(&f.device);
    job::commit_batch(jobs).expect("committed");

    // Neither half went, and no kick was sent. Submitting the fragment half here
    // would put a job in a ring whose owner is about to drain its pending list and
    // fail that very fence, leaving the firmware running work the host has already
    // called failed.
    assert_eq!(kccb_write_offset(&f.device), before);
    assert_eq!(geom_queue.in_flight_count(), 0);
    assert_eq!(frag_queue.in_flight_count(), 0);
}

#[ktest]
fn a_pair_whose_geometry_half_fails_does_not_leave_the_fragment_half_behind() {
    let f = RenderFixture::new();
    let geom_queue = f.geometry_queue();
    let frag_queue = f.fragment_queue();

    let (table, sync) = table_with(SyncObj::new(false));
    let specs = [
        geometry_spec(f.context, &f.geometry, f.hwrt, alloc::vec![signal_op(sync)]),
        fragment_spec(f.context, &f.fragment, f.hwrt, alloc::vec![wait_op(sync)]),
    ];
    let mut jobs = prepare_specs(&f.file, &table, &specs).expect("prepared");
    assert_eq!(jobs.len(), 1);
    let fences = jobs[0].fences();

    // Something the geometry half waits for fails. Half a combined kick is not
    // something the firmware can be given, so the fragment half has to end too —
    // and it is sitting in a different queue, which nothing else walks.
    let gate = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    jobs[0]
        .add_dependency(job::Dep::Foreign(gate.clone() as Arc<dyn Fence>))
        .expect("dependency recorded");
    job::commit_batch(jobs).expect("committed");
    assert_eq!(frag_queue.pending_count(), 1);

    gate.signal(Some(DrmError::TimedOut));
    sched::queue::process_all(&f.device);

    // The fence-completion contract for both halves, and nothing left in either queue.
    assert_eq!(
        fences[0].status(),
        k1_musebook_drm::fence::FenceStatus::Failed(DrmError::TimedOut)
    );
    assert_eq!(
        fences[1].status(),
        k1_musebook_drm::fence::FenceStatus::Failed(DrmError::TimedOut)
    );
    assert_eq!(geom_queue.pending_count(), 0);
    assert_eq!(frag_queue.pending_count(), 0);
}

/// Sanity: the operation type the tests build by hand is the one the parser
/// produces, so a change to either is caught here rather than in a resolution
/// test that then fails for the wrong reason.
#[ktest]
fn the_test_operation_builders_agree_with_the_parser() {
    assert_eq!(
        ops(&[wait_op(3), signal_op(4), timeline_op(5, 9, true)]).as_slice(),
        &[
            Op {
                handle: 3,
                point: 0,
                kind: sync::HandleType::Syncobj,
                signal: false
            },
            Op {
                handle: 4,
                point: 0,
                kind: sync::HandleType::Syncobj,
                signal: true
            },
            Op {
                handle: 5,
                point: 9,
                kind: sync::HandleType::TimelineSyncobj,
                signal: true
            },
        ]
    );
}

/// The free-list constants, cross-checked against upstream's arithmetic.
#[ktest]
fn the_free_list_constants_match_upstream() {
    // `FREE_LIST_ALIGNMENT` is
    // `ROGUE_BIF_PM_FREELIST_BASE_ADDR_ALIGNSIZE / FREE_LIST_ENTRY_SIZE - 1`,
    // which for a 16-byte burst and 4-byte entries is three.
    assert_eq!(FREE_LIST_ENTRY_SIZE, 4);
    assert_eq!(free_list::FREE_LIST_ALIGNMENT, 3);
    // Two `aligned_u64` and twelve 32-bit fields, the last of which is
    // upstream's explicit `padding`, so the structure needs none of its own.
    assert_eq!(size_of::<RogueFwifFreelist>(), 16 + 12 * 4);
}

#[ktest]
fn render_allocations_and_mappings_roll_back_without_touching_live_owners() {
    let device = device();
    let (file, vm, handle) = file_with_vm(&device);
    let (local, global) = dataset_free_lists(&device, &file, &vm, handle, 64);
    let mut args = dataset_args(local, global);
    args.layers = 3;
    let _bystander = HwrtDataset::new(&device, &file, &args).expect("bystander");
    let fw = device.fw.get().expect("firmware");
    let baseline = fw.heap.lock().ranges_for_test();
    let ptes = fw.mips.page_table.entries_for_test();
    // Common + two (SRTC, RAA, firmware) triples, each occupying one page.
    for allowed in 0..7 {
        fw.heap.lock().fail_alloc_after_for_test(Some(allowed));
        let result = HwrtDataset::new(&device, &file, &args);
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
    for allowed in 0..7 {
        fw.mips.page_table.fail_write_after_for_test(Some(allowed));
        let result = HwrtDataset::new(&device, &file, &args);
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
    for layers in [1, 3] {
        args.layers = layers;
        let dataset = HwrtDataset::new(&device, &file, &args).expect("dataset");
        assert_ne!(fw.mips.page_table.entries_for_test(), ptes);
        drop(dataset);
        assert_eq!(fw.heap.lock().ranges_for_test(), baseline);
        assert_eq!(fw.mips.page_table.entries_for_test(), ptes);
    }
}

#[ktest]
fn free_list_mapping_failure_and_drop_preserve_other_mappings() {
    let device = device();
    let (file, vm, handle) = file_with_vm(&device);
    let _ = dataset_free_lists(&device, &file, &vm, handle, 64);
    let at = map_list_buffer(
        &vm,
        LIST_BASE + (2 << 20),
        128,
        uapi::bo_flags::PM_FW_PROTECT,
    );
    let args = list_args(at, handle, 64, 128, 32);
    let fw = device.fw.get().expect("firmware");
    let baseline = fw.heap.lock().ranges_for_test();
    let ptes = fw.mips.page_table.entries_for_test();
    fw.mips.page_table.fail_write_after_for_test(Some(0));
    let result = FreeList::new(&device, &vm, &args);
    fw.mips.page_table.fail_write_after_for_test(None);
    assert_eq!(result.err(), Some(PvrError::InvalidArgument));
    assert_eq!(fw.heap.lock().ranges_for_test(), baseline);
    assert_eq!(fw.mips.page_table.entries_for_test(), ptes);
    let free_list = FreeList::new(&device, &vm, &args).expect("free list");
    assert_ne!(fw.mips.page_table.entries_for_test(), ptes);
    drop(free_list);
    assert_eq!(fw.heap.lock().ranges_for_test(), baseline);
    assert_eq!(fw.mips.page_table.entries_for_test(), ptes);
}

/// Distinct handles for one object share preparation cache and ascending point publication.
#[ktest]
fn aliased_syncobj_handles_share_signal_points_and_batch_dependencies() {
    let object = SyncObj::new(false);
    let table = SyncObjTable::new();
    let first = table.insert(object.clone()).expect("first handle").0;
    let second = table.insert(object.clone()).expect("alias handle").0;
    let mut signals = SignalArray::new();
    let high = ops(&[timeline_op(first, 7, true)]);
    let low = ops(&[timeline_op(second, 5, true)]);
    let five = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    let seven = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    signals.collect(&table, &high).expect("collect high");
    signals
        .update(&high, seven.clone(), 0)
        .expect("update high");
    signals.collect(&table, &low).expect("collect low");
    signals.update(&low, five.clone(), 1).expect("update low");
    // This alias was never collected for point five, but must resolve the cached producer.
    let waits =
        resolve(&table, &signals, &ops(&[timeline_op(first, 5, false)])).expect("wait alias");
    assert_eq!(waits[0].producer, Some(1));
    assert!(Arc::ptr_eq(&waits[0].fence, &(five as Arc<dyn Fence>)));
    signals.push_fences();
    assert_eq!(object.last_submitted_point(), 7);
    assert!(matches!(object.point_state(5), TimelinePoint::Pending(_)));
}

#[ktest]
fn aliased_handles_for_one_signal_point_install_only_the_last_job_fence() {
    let object = SyncObj::new(false);
    let table = SyncObjTable::new();
    let first = table.insert(object.clone()).expect("first handle").0;
    let second = table.insert(object.clone()).expect("alias handle").0;
    let mut signals = SignalArray::new();
    let initial = ops(&[timeline_op(first, 5, true)]);
    let later = ops(&[timeline_op(second, 5, true)]);
    let a = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    let b = SoftwareFence::new(k1_musebook_drm::fence::FenceContext::new(), 1);
    signals.collect(&table, &initial).expect("collect initial");
    signals.update(&initial, a, 0).expect("update initial");
    signals.collect(&table, &later).expect("collect alias");
    signals.update(&later, b.clone(), 1).expect("update alias");
    assert_eq!(signals.len(), 1);
    signals.push_fences();
    assert!(Arc::ptr_eq(
        &object.fence_at(5).expect("installed fence"),
        &(b as Arc<dyn Fence>)
    ));
}
