// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: firmware parameter-memory free-list ownership.

//! Parameter-buffer free lists.
//!
//! Ports `pvr_free_list.c` and `pvr_free_list.h`. A free list has three owners:
//! the client BO containing physical PM page numbers, driver-allocated backing
//! pages, and a firmware-heap structure describing the list. PM addresses the
//! backing pages physically; they need no CPU or GPU virtual mapping. Entries
//! are filled from the high end of the list.
//!
//! [`process_grow_request`] runs through FWCCB processing in the interrupt's
//! deferred half. It may neither sleep nor wait for firmware execution. In
//! particular, it does not acquire a runtime-PM reference or use `send_and_wait`.
//! The firmware must already be running to issue a grow request. KCCB headroom
//! reservation retains its bounded busy-wait.
//!
//! Backing-page allocation precedes the free-list lock. The lock disables bottom
//! halves because ioctl callers share it with deferred processing. A `Vec` push
//! may allocate under it; the kernel allocator does not sleep. Release the
//! free-list lock before sending the KCCB response. A failed grow responds with
//! `delta_pages = 0`, allowing firmware to fall back to a partial render.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::offset_of,
    sync::atomic::{AtomicU32, Ordering},
};

use aster_softirq::BottomHalfDisabled;
use ostd::{mm::VmIo, sync::SpinLock};
use ostd_pod::FromZeros;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwObject},
    mm::{gem::PvrObject, mmu::DEVICE_PAGE_SIZE, vm::DeviceAddr},
    render::{abi as render_abi, hwrt::HwrtData},
    rogue::{
        defs::{
            ROGUE_BIF_PM_FREELIST_BASE_ADDR_ALIGNSIZE, ROGUE_BIF_PM_PHYSICAL_PAGE_ALIGNSHIFT,
            ROGUE_BIF_PM_PHYSICAL_PAGE_SIZE,
        },
        fwif::{FwAddr, RogueFwifFreelist, RogueFwifKccbCmd},
    },
    sched::abi,
    uapi,
};

/// Bytes per free-list entry. Upstream `FREE_LIST_ENTRY_SIZE`.
pub const FREE_LIST_ENTRY_SIZE: u32 = size_of::<u32>() as u32;

/// Mask of the low bits a page count must not have set.
///
/// Upstream `FREE_LIST_ALIGNMENT`, which is
/// `ROGUE_BIF_PM_FREELIST_BASE_ADDR_ALIGNSIZE / FREE_LIST_ENTRY_SIZE - 1`: the
/// firmware reads the list in aligned bursts, so every boundary within it has to
/// fall on one.
pub const FREE_LIST_ALIGNMENT: u32 =
    (ROGUE_BIF_PM_FREELIST_BASE_ADDR_ALIGNSIZE as u32 / FREE_LIST_ENTRY_SIZE) - 1;

/// Smallest free list a render target may draw from. Upstream
/// `FREE_LIST_MIN_PAGES`.
const FREE_LIST_MIN_PAGES: u32 = 50;
/// The same, on a RogueXE part with BRN66011. Upstream
/// `FREE_LIST_MIN_PAGES_BRN66011`.
const FREE_LIST_MIN_PAGES_BRN66011: u32 = 40;
/// The same, on a RogueXE part without it. Upstream
/// `FREE_LIST_MIN_PAGES_ROGUEXE`.
const FREE_LIST_MIN_PAGES_ROGUEXE: u32 = 25;

/// The Parameter Manager's physical page size, which is not the MMU's.
///
/// Asserted equal to the device page size rather than assumed: the entries this
/// module writes are `daddr >> ROGUE_BIF_PM_PHYSICAL_PAGE_ALIGNSHIFT`, and
/// [`PvrObject`] hands out one bus address per device page. Upstream makes the
/// weaker claim `BUILD_BUG_ON(ROGUE_BIF_PM_PHYSICAL_PAGE_SIZE > PAGE_SIZE)` and
/// then loops over the sub-pages of each host page; with the two equal that loop
/// has one iteration, which is the case this port implements.
const _: () = assert!(ROGUE_BIF_PM_PHYSICAL_PAGE_SIZE == DEVICE_PAGE_SIZE);

/// The smallest free list this device will render from.
///
/// Ports upstream `pvr_get_free_list_min_pages()`. Two readers, which is why it
/// takes a [`DeviceInfo`] rather than a device: [`crate::render::hwrt`] refuses
/// a dataset whose local free list is below it, and `DEV_QUERY`'s
/// `RUNTIME_INFO` reports it so that Mesa never asks for one.
///
/// [`DeviceInfo`]: crate::device_info::DeviceInfo
pub fn min_pages(info: &crate::device_info::DeviceInfo) -> u32 {
    if !info.features.roguexe {
        FREE_LIST_MIN_PAGES
    } else if info.quirks.brn66011 {
        FREE_LIST_MIN_PAGES_BRN66011
    } else {
        FREE_LIST_MIN_PAGES_ROGUEXE
    }
}

/// One run of Parameter Manager pages the driver added to a free list.
///
/// Upstream `struct pvr_free_list_node`. The memory is never mapped: the
/// Parameter Manager reads it through physical addresses that the list itself
/// carries, so this is a pure allocation held for its bus addresses.
struct Node {
    /// How many pages it contributes.
    pages: u32,
    /// The pages. Held so they are not freed while the firmware can reach them.
    memory: Arc<PvrObject>,
}

/// Everything about a free list that changes after creation.
struct Inner {
    /// Pages the firmware may allocate from. Upstream `current_pages`.
    current_pages: u32,
    /// Pages held back so the firmware can service one out-of-memory event
    /// without waiting for the host. Upstream `ready_pages`.
    ready_pages: u32,
    /// Every run of pages added so far, oldest first.
    ///
    /// Kept because a reconstruction request rebuilds the list from them; see
    /// [`FreeList::reconstruct`].
    nodes: Vec<Node>,
    /// The render targets drawing from this list, weakly.
    ///
    /// Upstream `hwrt_list`, an intrusive list of the `pvr_hwrt_data` that named
    /// this free list as their *local* one. Weak so that a dataset's lifetime is
    /// its own: a dataset holds a strong reference to its free lists, and a
    /// strong reference back would be a cycle neither could ever break.
    hwrt: Vec<Weak<HwrtData>>,
}

/// A free list of parameter-buffer pages.
///
/// Ports upstream `struct pvr_free_list`.
pub struct FreeList {
    /// Whether this owner installed the firmware object's MIPS mapping.
    mapped: bool,
    /// The device that owns it.
    device: Arc<PvrDevice>,
    /// Backing store, mapped into the owning VM context.
    ///
    /// The client's buffer object: this is the array of page numbers, not the
    /// pages themselves.
    pub object: Arc<PvrObject>,
    /// Offset within [`Self::object`] at which the mapping starts.
    ///
    /// Divergence: upstream passes `NULL` for `pvr_vm_find_gem_object()`'s
    /// `mapped_offset_out` and then indexes the *buffer object* from byte zero,
    /// while validating the length of the *mapping*. The two origins coincide
    /// only when the mapping starts at offset zero, which is what Mesa does, so
    /// upstream's arithmetic is self-consistent under its own assumption and the
    /// disagreement is unreachable in practice. This port carries the offset
    /// through so that the bytes it writes are the bytes the firmware reads at
    /// `free_list_gpu_addr` whatever the client mapped. Recorded in
    /// `ABI.md` compatibility limits.
    mapped_offset: u64,
    /// Length of that mapping, which bounds every write.
    mapped_size: u64,
    /// The structure the firmware reads.
    fw_object: FwObject,
    /// The id the firmware names this list by in a grow or reconstruct request.
    fw_id: u32,
    /// Where the list lives in the client's GPU address space.
    free_list_gpu_addr: u64,
    /// The list can never hold more than this. Upstream `max_pages`.
    max_pages: u32,
    /// How many pages one grow request adds. Upstream `grow_pages`.
    grow_pages: u32,
    /// Percentage of the list in use at which the firmware asks for a grow.
    grow_threshold: u32,
    /// Everything that changes. See the module documentation for what may take
    /// this and from where.
    inner: SpinLock<Inner, BottomHalfDisabled>,
}

impl FreeList {
    /// Creates a free list.
    ///
    /// Ports upstream `pvr_free_list_create()` together with the three helpers
    /// it calls: `free_list_create_kernel_structure()`,
    /// `free_list_create_fw_structure()` and the initial
    /// `pvr_free_list_grow()`.
    ///
    /// # Errors
    ///
    /// [`PvrError::InvalidArgument`] for every case upstream returns `-EINVAL`
    /// for: an out-of-range page count or threshold, a count that is not a
    /// multiple of [`FREE_LIST_ALIGNMENT`] plus one, a GPU address with no
    /// mapping at it, a buffer object that user space can also write, one that
    /// is not reserved for the Parameter Manager, and one too small to hold
    /// `max_num_pages` entries.
    pub fn new(
        device: &Arc<PvrDevice>,
        vm: &Arc<crate::mm::vm::VmContext>,
        args: &uapi::CreateFreeListArgs,
    ) -> PvrResult<Arc<Self>> {
        device.check_alive()?;
        let fw = device.fw.get().ok_or(PvrError::Unsupported)?;

        check_page_counts(args)?;

        // Upstream `pvr_vm_find_gem_object()`. The mapping has to exist *now*:
        // the driver is about to write page numbers into it and the firmware
        // will read them through the GPU address, so a list that is mapped later
        // is a list the firmware reads as zeroes.
        let (object, mapped_offset, mapped_size) = vm
            .find_object(DeviceAddr(args.free_list_gpu_addr))
            .ok_or(PvrError::InvalidArgument)?;

        // Upstream's three checks on the object. The first two are the reason a
        // client cannot rewrite its own free list behind the driver's back: the
        // Parameter Manager owns this memory, and `PM_FW_PROTECT` is what says
        // so.
        if object.allows_cpu_access() || !object.is_fw_protected() {
            return Err(PvrError::InvalidArgument);
        }
        let needed = u64::from(args.max_num_pages) * u64::from(FREE_LIST_ENTRY_SIZE);
        if mapped_size < needed {
            return Err(PvrError::InvalidArgument);
        }

        let fw_id = device.free_lists.allocate_id()?;
        let fw_object = fw
            .heap
            .lock()
            .alloc(size_of::<RogueFwifFreelist>(), FwCache::DEVICE_UNCACHED)?;

        let mut free_list = Self {
            mapped: false,
            device: device.clone(),
            object,
            mapped_offset,
            mapped_size,
            fw_object,
            fw_id,
            free_list_gpu_addr: args.free_list_gpu_addr,
            max_pages: args.max_num_pages,
            grow_pages: args.grow_num_pages,
            grow_threshold: args.grow_threshold,
            inner: SpinLock::new(Inner {
                current_pages: 0,
                ready_pages: 0,
                nodes: Vec::new(),
                hwrt: Vec::new(),
            }),
        };

        // This owner returns the allocation and any completed MIPS mapping
        // on every early return before publication in the registry.
        free_list.write_fw_structure(args.initial_num_pages)?;
        fw.mips.page_table.map(&free_list.fw_object)?;
        free_list.mapped = true;
        free_list.grow_by(args.initial_num_pages)?;

        let free_list = Arc::new(free_list);
        device.free_lists.insert(fw_id, &free_list);
        Ok(free_list)
    }

    /// Firmware address of the structure the firmware reads.
    pub fn fw_addr(&self) -> FwAddr {
        self.fw_object.fw_addr()
    }

    /// The id the firmware names this list by.
    pub fn fw_id(&self) -> u32 {
        self.fw_id
    }

    /// Pages the firmware may currently allocate from.
    ///
    /// Excludes the reserved [`Self::ready_pages`], exactly as upstream's
    /// `current_pages` does, because that is the number
    /// [`crate::render::hwrt`] compares against [`min_pages`].
    pub fn current_pages(&self) -> u32 {
        self.inner.lock().current_pages
    }

    /// Pages held back for the firmware to use while a grow is in flight.
    pub fn ready_pages(&self) -> u32 {
        self.inner.lock().ready_pages
    }

    /// The ceiling the list was created with.
    pub fn max_pages(&self) -> u32 {
        self.max_pages
    }

    /// How many runs of pages the list has been grown with.
    ///
    /// Diagnostic, and what the tests assert against.
    pub fn node_count(&self) -> usize {
        self.inner.lock().nodes.len()
    }

    /// The structure the firmware reads, for the tests that read it back.
    ///
    /// The same seam [`crate::sched::context::Context`] has, for the same
    /// reason: these fields are the firmware's to update, so a caller looking at
    /// them is checking this module's own work rather than using the driver.
    #[cfg(ktest)]
    pub(crate) fn fw_data_for_test(&self) -> PvrResult<RogueFwifFreelist> {
        self.fw_object.read_val(0)
    }

    /// The page numbers the list currently holds, for the tests.
    #[cfg(ktest)]
    pub(crate) fn entries_for_test(&self, count: usize) -> PvrResult<Vec<u32>> {
        let start = self.mapped_offset as usize
            + (self.max_pages as usize - count) * FREE_LIST_ENTRY_SIZE as usize;
        let mut bytes = alloc::vec![0u8; count * FREE_LIST_ENTRY_SIZE as usize];
        self.object.sync_from_device(start, bytes.len())?;
        self.object
            .dma()
            .read_bytes(start, &mut bytes)
            .map_err(|_| PvrError::InvalidArgument)?;
        Ok(bytes
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().expect("four bytes")))
            .collect())
    }

    /// Adds pages in response to a firmware request.
    ///
    /// Failing to grow is not fatal: report it to the firmware, which will fall
    /// back to a partial render. Blocking here, on the other hand, stalls
    /// interrupt processing for the whole device. See the module documentation
    /// for the full set of constraints this runs under — they are not obvious
    /// from the body.
    ///
    /// Ports upstream `pvr_free_list_grow()` with `free_list->grow_pages`.
    pub fn grow(&self) -> PvrResult<()> {
        self.grow_by(self.grow_pages)
    }

    /// Adds `num_pages` pages to the list.
    ///
    /// The body of upstream `pvr_free_list_grow()`. The pages are allocated
    /// before the lock is taken and the write goes out of a stack buffer, so the
    /// lock is held for a fixed amount of work whatever `num_pages` is; see the
    /// module documentation for the rest of the constraints.
    fn grow_by(&self, num_pages: u32) -> PvrResult<()> {
        if num_pages == 0 || num_pages & FREE_LIST_ALIGNMENT != 0 {
            return Err(PvrError::InvalidArgument);
        }

        // Upstream `pvr_gem_object_create(..., PVR_BO_FW_FLAGS_DEVICE_CACHED)`,
        // which is zero flags: this memory is neither CPU mappable nor GPU
        // mapped, so neither flag applies to it.
        let bytes = u64::from(num_pages) << ROGUE_BIF_PM_PHYSICAL_PAGE_ALIGNSHIFT;
        let memory = PvrObject::new(bytes, 0)?;

        let mut inner = self.inner.lock();

        // Divergence: upstream has no bound here. Its
        // `pvr_free_list_insert_node_locked()` computes
        // `max_pages - current_pages - num_pages` in `u32` and trusts the
        // firmware never to ask for more than the list was created for; if it
        // did, the subtraction would wrap and the write would land outside the
        // client's buffer. Checking on the host costs a comparison and turns a
        // misbehaving firmware into a refused grow. Recorded in `ABI.md`
        // compatibility limits.
        let total = inner
            .current_pages
            .checked_add(inner.ready_pages)
            .and_then(|held| held.checked_add(num_pages))
            .ok_or(PvrError::InvalidArgument)?;
        if total > self.max_pages {
            ostd::warn!(
                "refusing to grow free list {} past its {} page maximum",
                self.fw_id,
                self.max_pages
            );
            return Err(PvrError::InvalidArgument);
        }

        self.insert_node(&mut inner, &memory, num_pages)?;
        inner.nodes.push(Node {
            pages: num_pages,
            memory,
        });

        // Upstream reserves the ready pages *after* the insert, out of the new
        // total, so that the reservation grows with the list.
        inner.ready_pages = self.ready_pages_for(inner.current_pages);
        inner.current_pages -= inner.ready_pages;
        Ok(())
    }

    /// Writes one node's page numbers into the client's list.
    ///
    /// Ports upstream `pvr_free_list_insert_node_locked()` and the
    /// `pvr_free_list_insert_pages_locked()` it calls. The list fills from the
    /// top down, so this computes where the run starts rather than appending.
    fn insert_node(&self, inner: &mut Inner, memory: &PvrObject, num_pages: u32) -> PvrResult<()> {
        let start_page = self
            .max_pages
            .checked_sub(inner.current_pages)
            .and_then(|free| free.checked_sub(num_pages))
            .ok_or(PvrError::InvalidArgument)?;

        // Upstream masks the byte offset down to the burst alignment. With
        // `start_page` a multiple of `FREE_LIST_ALIGNMENT + 1` — which the
        // creation checks and the grow check both enforce — the mask is already
        // satisfied; it is applied anyway so that a future relaxation of those
        // checks cannot silently produce an unaligned base.
        let offset = (u64::from(start_page) * u64::from(FREE_LIST_ENTRY_SIZE))
            & !(ROGUE_BIF_PM_FREELIST_BASE_ADDR_ALIGNSIZE - 1);

        let len = u64::from(num_pages) * u64::from(FREE_LIST_ENTRY_SIZE);
        let end = offset.checked_add(len).ok_or(PvrError::InvalidArgument)?;
        if end > self.mapped_size {
            return Err(PvrError::InvalidArgument);
        }
        let base = usize::try_from(self.mapped_offset + offset).map_err(|_| PvrError::NoMemory)?;

        write_page_numbers(&self.object, base, memory, num_pages)?;
        inner.current_pages += num_pages;
        Ok(())
    }

    /// How many pages to hold back at a given list size.
    ///
    /// Ports upstream `calculate_free_list_ready_pages_locked()`. The reserve
    /// exists so the firmware can service an out-of-memory event immediately and
    /// ask for the grow asynchronously; below one alignment unit there is no
    /// point and the feature turns itself off.
    fn ready_pages_for(&self, pages: u32) -> u32 {
        let ready = pages.saturating_mul(self.grow_threshold) / 100;
        core::cmp::min(ready, self.grow_pages) & !FREE_LIST_ALIGNMENT
    }

    /// Fills in the structure the firmware reads.
    ///
    /// Ports upstream `free_list_fw_init()`, which runs as the initialiser
    /// callback of `pvr_fw_object_create_and_map()` and is therefore the state
    /// the firmware sees before the first grow. The host's counters reach the
    /// same values a moment later, when [`Self::grow_by`] adds the initial
    /// pages.
    fn write_fw_structure(&self, initial_num_pages: u32) -> PvrResult<()> {
        let ready_pages = self.ready_pages_for(initial_num_pages);
        let current_pages = initial_num_pages
            .checked_sub(ready_pages)
            .ok_or(PvrError::InvalidArgument)?;

        let mut data = RogueFwifFreelist::new_zeroed();
        data.max_pages = self.max_pages;
        data.current_pages = current_pages;
        data.grow_pages = self.grow_pages;
        data.ready_pages = ready_pages;
        data.freelist_id = self.fw_id;
        data.grow_pending = 0;
        // Upstream writes `current_pages - 1` unconditionally. At zero current
        // pages that is `0xffffffff`, which is what an empty stack looks like to
        // the firmware, so the wrap is intended rather than an oversight.
        data.current_stack_top = current_pages.wrapping_sub(1);
        data.freelist_dev_addr = self.free_list_gpu_addr;
        data.current_dev_addr = (data.freelist_dev_addr
            + u64::from(data.max_pages.saturating_sub(data.current_pages))
                * u64::from(FREE_LIST_ENTRY_SIZE))
            & !(ROGUE_BIF_PM_FREELIST_BASE_ADDR_ALIGNSIZE - 1);

        self.fw_object.write_val(0, &data)
    }

    /// Records that a render target draws from this list.
    ///
    /// Ports upstream `pvr_free_list_add_hwrt()`. Only the *local* free list of
    /// a dataset gets these, because only it is reconstructed.
    pub(super) fn attach_hwrt(&self, data: &Arc<HwrtData>) {
        let mut inner = self.inner.lock();
        inner.hwrt.retain(|weak| weak.strong_count() > 0);
        inner.hwrt.push(Arc::downgrade(data));
    }

    /// Forgets a render target. Ports upstream `pvr_free_list_remove_hwrt()`.
    pub(super) fn detach_hwrt(&self, data: &Arc<HwrtData>) {
        let target = Arc::as_ptr(data);
        let mut inner = self.inner.lock();
        inner
            .hwrt
            .retain(|weak| weak.strong_count() > 0 && !core::ptr::eq(weak.as_ptr(), target));
    }

    /// How many render targets are attached. Diagnostic, and what tests assert.
    pub fn hwrt_count(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.hwrt.retain(|weak| weak.strong_count() > 0);
        inner.hwrt.len()
    }

    /// Rewrites the list after the firmware lost track of it.
    ///
    /// Ports upstream `pvr_free_list_reconstruct()`. The nodes are still there
    /// and their pages are still ours, so the list is rebuilt from them rather
    /// than reallocated: what was lost is the firmware's idea of which entries
    /// it had already popped.
    ///
    /// Runs on the same deferred path [`Self::grow`] does and under the same
    /// rules, which is why it writes through the same allocation-free helper.
    pub fn reconstruct(&self) {
        let mut inner = self.inner.lock();

        inner.current_pages = 0;
        // The nodes are re-inserted in the order they were added, so each lands
        // back where it was: the offset depends only on how many pages precede
        // it. Taken out of `inner` for the walk because `insert_node` needs the
        // rest of it mutably; nothing else can touch the list meanwhile, since
        // this holds the lock throughout.
        let nodes = core::mem::take(&mut inner.nodes);
        for node in &nodes {
            if let Err(err) = self.insert_node(&mut inner, &node.memory, node.pages) {
                ostd::error!("could not rebuild free list {}: {err:?}", self.fw_id);
            }
        }
        inner.nodes = nodes;
        inner.current_pages = inner.current_pages.saturating_sub(inner.ready_pages);

        let current_pages = inner.current_pages;
        if let Err(err) = self.rewrite_reconstructed_fw_state(current_pages) {
            ostd::error!(
                "could not rewrite free list {}'s firmware state: {err:?}",
                self.fw_id
            );
        }

        // Upstream resets every render target that draws from this list: the
        // parameter buffer they were writing into is gone, so the geometry pass
        // that filled it has to be treated as never having happened.
        inner.hwrt.retain(|weak| weak.strong_count() > 0);
        for data in inner.hwrt.iter().filter_map(Weak::upgrade) {
            if let Err(err) = data.mark_reconstructed() {
                ostd::error!("could not reset a render target after reconstruction: {err:?}");
            }
            crate::service::retire_hwrt_data(&self.device, data);
        }
    }

    /// The three firmware fields a reconstruction resets.
    fn rewrite_reconstructed_fw_state(&self, current_pages: u32) -> PvrResult<()> {
        self.fw_object.write_val(
            offset_of!(RogueFwifFreelist, current_stack_top),
            &current_pages.wrapping_sub(1),
        )?;
        self.fw_object
            .write_val(offset_of!(RogueFwifFreelist, allocated_page_count), &0u32)?;
        self.fw_object.write_val(
            offset_of!(RogueFwifFreelist, allocated_mmu_page_count),
            &0u32,
        )
    }

    /// Asks the firmware to stop referring to this free list.
    ///
    /// Ports the `pvr_fw_structure_cleanup()` half of upstream
    /// `pvr_free_list_release()`, including its one retry: `CLEANUP_BUSY` means
    /// the firmware still has a render or a reconstruction outstanding against
    /// the list, and draining the FWCCB is what clears it.
    ///
    /// Called from `DESTROY_FREE_LIST`, which is a process context — unlike
    /// [`Self::grow`], this one *may* wait for the firmware, and must, because
    /// freeing the pages while the Parameter Manager can still reach them is
    /// exactly the write-after-free this command exists to prevent.
    pub fn destroy(&self) -> PvrResult<()> {
        if self.device.check_alive().is_err() {
            // No firmware to tell, and nothing of ours it can still be reading.
            return Ok(());
        }

        match self.cleanup() {
            Err(PvrError::Busy) => {
                crate::sched::ccb::process_fwccb(&self.device);
                self.cleanup()
            }
            other => other,
        }
    }

    /// One `ROGUE_FWIF_CLEANUP_FREELIST` round trip.
    fn cleanup(&self) -> PvrResult<()> {
        let mut cmd = RogueFwifKccbCmd::new_zeroed();
        cmd.cmd_type = abi::KCCB_CMD_CLEANUP;
        {
            let cleanup = cmd.cmd_data.cleanup_data_mut();
            cleanup.cleanup_type = render_abi::CLEANUP_FREELIST;
            *cleanup.cleanup_data.freelist_fw_addr_mut() = self.fw_addr();
        }

        let rtn = crate::sched::ccb::send_and_wait(&self.device, &cmd)?;
        if rtn & abi::KCCB_RTN_SLOT_CLEANUP_BUSY != 0 {
            return Err(PvrError::Busy);
        }
        Ok(())
    }
}

impl Drop for FreeList {
    /// Returns the firmware-heap allocation and forgets the id.
    ///
    /// The pages themselves go with [`Inner::nodes`], and the client's buffer
    /// object with its own handle. [`Self::destroy`] is what tells the firmware;
    /// by the time this runs the handle is gone.
    fn drop(&mut self) {
        self.device.free_lists.remove(self.fw_id);
        if let Some(fw) = self.device.fw.get() {
            let mut heap = fw.heap.lock();
            if self.mapped
                && let Err(err) = fw.mips.page_table.unmap(&self.fw_object)
            {
                ostd::error!("could not unmap a free list: {err:?}");
            }
            heap.free(&self.fw_object);
        }
    }
}

impl core::fmt::Debug for FreeList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("FreeList")
            .field("fw_id", &self.fw_id)
            .field("current_pages", &inner.current_pages)
            .field("ready_pages", &inner.ready_pages)
            .field("max_pages", &self.max_pages)
            .finish_non_exhaustive()
    }
}

/// Validates the page counts a client asked for.
///
/// Ports the first two `if` statements of upstream
/// `free_list_create_kernel_structure()`, in the same order and with the same
/// errno. The pair at the end is the interesting one: a list that can grow must
/// say by how much, and a list that cannot must not.
fn check_page_counts(args: &uapi::CreateFreeListArgs) -> PvrResult<()> {
    if args.grow_threshold > 100
        || args.initial_num_pages > args.max_num_pages
        || args.grow_num_pages > args.max_num_pages
        || args.max_num_pages == 0
        || (args.initial_num_pages < args.max_num_pages && args.grow_num_pages == 0)
        || (args.initial_num_pages == args.max_num_pages && args.grow_num_pages != 0)
    {
        return Err(PvrError::InvalidArgument);
    }
    if (args.initial_num_pages | args.max_num_pages | args.grow_num_pages) & FREE_LIST_ALIGNMENT
        != 0
    {
        return Err(PvrError::InvalidArgument);
    }
    Ok(())
}

/// Writes `num_pages` physical page numbers into a free list.
///
/// Ports upstream `pvr_free_list_insert_pages_locked()`. Allocates nothing: the
/// entries go out in fixed-size chunks from the stack. Stack usage is bounded;
/// time under the lock scales with the list's page count. This also runs on the
/// deferred interrupt path.
fn write_page_numbers(
    list: &PvrObject,
    base: usize,
    memory: &PvrObject,
    num_pages: u32,
) -> PvrResult<()> {
    /// Entries per pass. Small enough to sit on an interrupt stack, large
    /// enough that a full-sized free list costs tens of writes rather than
    /// thousands.
    const CHUNK: usize = 128;

    let mut chunk = [0u8; CHUNK * 4];
    let mut written = 0usize;
    while written < num_pages as usize {
        let count = core::cmp::min(CHUNK, num_pages as usize - written);
        for (index, entry) in chunk[..count * 4]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let offset = (written + index) * DEVICE_PAGE_SIZE as usize;
            let pfn = (memory.daddr_at(offset)? as u64) >> ROGUE_BIF_PM_PHYSICAL_PAGE_ALIGNSHIFT;
            // Upstream's `WARN_ON_ONCE(dma_pfn >> 32)`. A page number that does
            // not fit is not something to truncate: the entry would name a
            // different page and the Parameter Manager would write over it.
            let pfn = u32::try_from(pfn).map_err(|_| PvrError::InvalidArgument)?;
            entry.copy_from_slice(&pfn.to_le_bytes());
        }

        let at = base + written * 4;
        list.dma()
            .write_bytes(at, &chunk[..count * 4])
            .map_err(|_| PvrError::InvalidArgument)?;
        // Upstream's `wmb()` after the loop. On the K1 a barrier is not enough:
        // these bytes were written through the CPU's cached alias and the
        // firmware reads them through the bus. See `CONVENTIONS.md` DMA contract.
        list.sync_to_device(at, count * 4)?;
        written += count;
    }
    Ok(())
}

/// Firmware-visible free lists on one device, keyed by the id the firmware uses
/// in FWCCB messages.
///
/// Upstream `pvr_device::free_list_ids`. Held by [`PvrDevice`]; entries are
/// [`Weak`] so a list's owning `Arc` (the client's handle table) is what keeps
/// it alive — the device must not keep a free list alive past the client's
/// drop, or teardown would cycle through [`FreeList`]'s `device` field.
pub struct FreeListRegistry {
    entries: SpinLock<Vec<(u32, Weak<FreeList>)>, BottomHalfDisabled>,
    /// Ids are never reused, so a stale id from a confused firmware resolves to
    /// nothing rather than to somebody else's list. Starts at one, because a
    /// zeroed firmware structure must not look like a valid free list.
    next_id: AtomicU32,
}

impl FreeListRegistry {
    /// An empty table.
    pub const fn new() -> Self {
        Self {
            entries: SpinLock::new(Vec::new()),
            next_id: AtomicU32::new(0),
        }
    }

    /// Allocates an id. Never zero, never reused.
    fn allocate_id(&self) -> PvrResult<u32> {
        self.next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map(|value| value + 1)
            .map_err(|_| PvrError::NoMemory)
    }

    /// Records a list under an id [`Self::allocate_id`] produced.
    fn insert(&self, id: u32, free_list: &Arc<FreeList>) {
        let mut entries = self.entries.lock();
        entries.retain(|(_, weak)| weak.strong_count() > 0);
        entries.push((id, Arc::downgrade(free_list)));
    }

    /// Forgets an id.
    fn remove(&self, id: u32) {
        self.entries
            .lock()
            .retain(|(other, weak)| *other != id && weak.strong_count() > 0);
    }

    /// Resolves an id the firmware reported.
    ///
    /// Ports upstream `pvr_free_list_lookup_id()`, including its
    /// `kref_get_unless_zero()`: a list whose last reference has gone but whose
    /// entry has not yet been reaped must not be resurrected, which is what
    /// upgrading a `Weak` gives for free.
    pub fn lookup(&self, id: u32) -> Option<Arc<FreeList>> {
        let mut entries = self.entries.lock();
        entries.retain(|(_, weak)| weak.strong_count() > 0);
        entries
            .iter()
            .find(|(other, _)| *other == id)
            .and_then(|(_, weak)| weak.upgrade())
    }
}

impl Default for FreeListRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Answers a firmware request to grow a free list.
///
/// Ports upstream `pvr_free_list_process_grow_req()`. Read the module
/// documentation before changing anything here: this is the deferred interrupt
/// path, and the shape of the function — grow, then answer, whether or not the
/// grow worked — is what keeps a failure to grow from becoming a stalled device.
pub fn process_grow_request(device: &PvrDevice, freelist_id: u32) {
    let Some(free_list) = device.free_lists.lookup(freelist_id) else {
        // Upstream `WARN_ON(!free_list)`. Nothing to answer: the address the
        // response would name is the one we could not find.
        ostd::warn!(
            "firmware asked to grow free list {freelist_id}, which this driver does not know"
        );
        return;
    };

    // The firmware only asks once it has consumed the reserve, so the host's
    // idea of how many pages are in play has to catch up before anything else.
    {
        let mut inner = free_list.inner.lock();
        inner.current_pages += inner.ready_pages;
        inner.ready_pages = 0;
    }

    let delta_pages = match free_list.grow() {
        Ok(()) => free_list.grow_pages,
        Err(err) => {
            // Not fatal, and deliberately not retried: `delta_pages = 0` is how
            // the firmware is told to fall back to a partial render.
            ostd::warn!(
                "could not grow free list {freelist_id} by {} pages ({err:?}); \
                 the firmware will render partially",
                free_list.grow_pages
            );
            0
        }
    };

    let (current_pages, ready_pages) = {
        let inner = free_list.inner.lock();
        (inner.current_pages, inner.ready_pages)
    };

    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = render_abi::KCCB_CMD_FREELIST_GROW_UPDATE;
    {
        let resp = cmd.cmd_data.free_list_gs_data_mut();
        resp.freelist_fw_addr = free_list.fw_addr();
        resp.delta_pages = delta_pages;
        resp.new_pages = current_pages + ready_pages;
        resp.ready_pages = ready_pages;
    }

    // `send`, never `send_and_wait`: see the module documentation.
    if let Err(err) = crate::sched::ccb::send(device, &cmd) {
        ostd::error!("could not answer a free-list grow request: {err:?}");
    }
    crate::service::retire_free_list(device, free_list);
}

/// Answers a firmware request to reconstruct free lists.
///
/// Ports upstream `pvr_free_list_process_reconstruct_req()`. The answer echoes
/// the ids back, so the firmware learns which of them the host actually knows —
/// which is why an unknown id is skipped rather than dropped from the reply.
pub fn process_reconstruct_request(device: &PvrDevice, ids: &[u32]) {
    for &id in ids {
        match device.free_lists.lookup(id) {
            Some(free_list) => {
                free_list.reconstruct();
                crate::service::retire_free_list(device, free_list);
            }
            None => ostd::warn!(
                "firmware asked to reconstruct free list {id}, which this driver does not know"
            ),
        }
    }

    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = render_abi::KCCB_CMD_FREELISTS_RECONSTRUCTION_UPDATE;
    {
        let resp = cmd.cmd_data.free_lists_reconstruction_data_mut();
        let count = core::cmp::min(ids.len(), resp.freelist_ids.len());
        resp.freelist_count = count as u32;
        resp.freelist_ids[..count].copy_from_slice(&ids[..count]);
    }

    if let Err(err) = crate::sched::ccb::send(device, &cmd) {
        ostd::error!("could not answer a free-list reconstruction request: {err:?}");
    }
}

#[cfg(ktest)]
mod id_tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn free_list_ids_exhaust_without_reusing_firmware_identity() {
        let registry = FreeListRegistry::new();
        assert_eq!(registry.allocate_id(), Ok(1));
        registry.next_id.store(u32::MAX - 1, Ordering::Relaxed);
        assert_eq!(registry.allocate_id(), Ok(u32::MAX));
        assert_eq!(registry.allocate_id(), Err(PvrError::NoMemory));
        assert_eq!(registry.allocate_id(), Err(PvrError::NoMemory));
    }
}
