// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: per-context GPU virtual mappings and object ownership.

//! GPU address spaces.
//!
//! Ports `pvr_vm.c` and `pvr_vm.h` (1,268 lines).
//!
//! A [`VmContext`] is one GPU address space: a page table plus the bookkeeping
//! that tracks which ranges are mapped to what. User space creates these
//! explicitly through `DRM_IOCTL_PVR_CREATE_VM_CONTEXT` and picks its own
//! addresses, so the driver validates rather than allocates.
//!
//! # What replaces `drm_gpuvm`
//!
//! Upstream's `pvr_vm_context` embeds a `drm_gpuvm`, and every map or unmap goes
//! through `drm_gpuvm_sm_map()` / `_sm_unmap()`, which decompose the request
//! into a sequence of map, unmap and *remap* steps against the existing
//! intervals and call back into `pvr_vm_gpuva_{map,unmap,remap}()`. Asterinas has
//! no equivalent, so this is a sorted interval map with the same invariants
//! stated directly:
//!
//! - a mapping is `[device_addr, device_addr + size)`, page aligned, non-empty;
//! - it lies wholly inside one heap (for a user context — see
//!   [`VmContextKind`]);
//! - it does not overlap a live mapping;
//! - it holds an [`Arc`] to its object, so a mapped buffer cannot be freed
//!   while the GPU can still reach it.
//!
//! The one behavioural difference is what happens when a map *does* overlap.
//! `drm_gpuvm_sm_map()` splits the existing mapping and remaps around the new
//! one; this refuses with `EINVAL`. Reproducing the split faithfully means
//! reproducing `drm_gpuvm`'s three-way remap with its preallocated `prev`/`next`
//! nodes, and getting it subtly wrong yields a GPU address that resolves to the
//! wrong buffer — the worst failure this driver has. Refusing is checkable in
//! one line. Recorded in `ABI.md` compatibility limits.
//!
//! `VM_UNMAP` is unaffected: upstream's `pvr_vm_unmap()` calls
//! `drm_gpuva_find()`, which already requires the address *and* the length to
//! match a live mapping exactly, and reports `-ENOENT` otherwise.

use alloc::{collections::BTreeMap, sync::Arc};

use ostd::{mm::Daddr, sync::SpinLock};
use ostd_pod::FromZeros;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwObject},
    mm::{
        gem::PvrObject,
        mmu::{DEVICE_PAGE_SIZE, PageTable},
    },
    rogue::fwif::{FwAddr, RogueFwifFwmemcontext},
};

/// Upstream `ROGUE_FW_BIF_INVALID_PCSET`. The firmware assigns a page-catalogue
/// register set on first use; the host must not invent one.
const FW_BIF_INVALID_PCSET: u32 = 0xffff_ffff;

/// An address in a GPU address space.
///
/// A newtype, not a bare `u64`, because the driver juggles three address spaces —
/// see the table in [`crate::fw`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeviceAddr(pub u64);

/// Which of the two kinds of address space a context is.
///
/// Upstream's `pvr_vm_create_context(pvr_dev, is_userspace_context)` boolean.
/// The distinction is not cosmetic: the kernel's own context maps the firmware's
/// structures, which live at fixed addresses outside every user-visible heap, so
/// the heap containment check must not apply to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmContextKind {
    /// Created by `DRM_IOCTL_PVR_CREATE_VM_CONTEXT` on behalf of a client.
    User,
    /// The driver's own address space.
    Kernel,
}

/// One live mapping.
///
/// Ports the fields of upstream's `drm_gpuva` that this driver reads back:
/// `va.addr`, `va.range`, `gem.obj` and `gem.offset`.
#[derive(Debug)]
struct Mapping {
    /// Keeps the object alive for as long as the GPU can reach it. Upstream
    /// takes the same reference in `pvr_vm_map()`.
    object: Arc<PvrObject>,
    /// Offset into the object the mapping starts at.
    offset: u64,
    /// Length in bytes.
    size: u64,
}

/// The firmware's view of a VM context's page tables.
///
/// Upstream `rogue_fwif_fwmemcontext`, created in `pvr_vm_create_context()` when
/// `is_userspace_context` and owned by `pvr_vm_context::fw_mem_ctx_obj`.
pub struct FwMemContext {
    /// True only after this owner successfully installed its MIPS mappings.
    mapped: bool,
    /// The allocation the firmware reads.
    object: FwObject,
    /// The device whose heap [`Self::object`] came from.
    ///
    /// Held so that [`Drop`] can return the address space. Owned by the
    /// [`VmContext`], so this is one per address space rather than one per
    /// rendering context — matching upstream, where two memory contexts for one
    /// page catalogue would make the firmware assign two page-catalogue
    /// register sets and exhaust them sooner than it should.
    device: Arc<PvrDevice>,
}

impl FwMemContext {
    /// Allocates and fills a firmware memory context naming `root_daddr`.
    ///
    /// Ports upstream `fw_mem_context_init()`. Two fields, and the second one is
    /// the one that matters: the firmware assigns a page-catalogue register set
    /// itself, so the host must say "none yet" rather than guess.
    fn new(device: &Arc<PvrDevice>, root_daddr: Daddr) -> PvrResult<Self> {
        let fw = device.fw.get().ok_or(PvrError::Unsupported)?;
        let object = fw
            .heap
            .lock()
            .alloc(size_of::<RogueFwifFwmemcontext>(), FwCache::DEVICE_UNCACHED)?;

        // Install the existing owner before the first fallible write. Its Drop
        // releases the range, and only unmaps entries after map() has succeeded.
        let mut context = Self {
            mapped: false,
            object,
            device: device.clone(),
        };
        let mut mem_context = RogueFwifFwmemcontext::new_zeroed();
        mem_context.pc_dev_paddr = root_daddr as u64;
        mem_context.page_cat_base_reg_set = FW_BIF_INVALID_PCSET;
        context.object.write_val(0, &mem_context)?;

        fw.mips.page_table.map(&context.object)?;
        context.mapped = true;
        Ok(context)
    }

    /// Firmware address of the structure.
    pub fn fw_addr(&self) -> FwAddr {
        self.object.fw_addr()
    }

    /// The allocation itself, for the CPU-cache call-site table.
    #[cfg(ktest)]
    pub(crate) fn object_for_test(&self) -> &FwObject {
        &self.object
    }
}

impl Drop for FwMemContext {
    /// Returns the allocation to the firmware heap.
    ///
    /// Runs when the owning [`VmContext`] goes, which is when the address space
    /// itself is destroyed. A device whose firmware has already been torn down
    /// has no heap to return anything to, and the whole heap goes with it.
    fn drop(&mut self) {
        if let Some(fw) = self.device.fw.get() {
            // Heap -> MIPS table matches Context::drop; this offset cannot be
            // reallocated before the corresponding entries have been removed.
            let mut heap = fw.heap.lock();
            if self.mapped
                && let Err(err) = fw.mips.page_table.unmap(&self.object)
            {
                ostd::error!("could not unmap a firmware memory context: {err:?}");
            }
            heap.free(&self.object);
        }
    }
}

/// One GPU address space.
pub struct VmContext {
    /// The GPU this address space belongs to.
    ///
    /// Upstream's `pvr_vm_context::pvr_dev`, "immutable for the life of the
    /// context". The heap layout the mapping checks consult comes from its
    /// device information, and the submission layer needs it to reach the submission
    /// path from a context.
    device: Arc<PvrDevice>,
    /// Whether the heap containment rule applies.
    kind: VmContextKind,
    /// Bus address of the page catalogue.
    ///
    /// Cached outside the lock because the submission path needs it to build a
    /// firmware memory context and must not have to take a page table lock to
    /// read a value that never changes.
    root_daddr: Daddr,
    /// Firmware memory context naming [`Self::root_daddr`].
    ///
    /// Upstream `pvr_vm_context::fw_mem_ctx_obj`. Present only for
    /// [`VmContextKind::User`], matching `pvr_vm_create_context()`'s
    /// `is_userspace_context` guard. A kernel address space has no client
    /// common context that would name it.
    fw_mem_context: Option<FwMemContext>,
    /// The page tables and the interval map of live mappings, under one lock.
    ///
    /// One lock rather than two: every operation touches both, and two locks
    /// would be two orders to get wrong. A [`SpinLock`], not a `Mutex`, because
    /// [`crate::mm::mmu::PageTable`] is also reachable from teardown paths that
    /// must not sleep; upstream can use a mutex because it has neither
    /// constraint.
    inner: SpinLock<Inner>,
}

impl core::fmt::Debug for VmContext {
    /// Deliberately does not take the lock: this is reached from diagnostics
    /// that may already hold it, and a context's identity is its root address.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmContext")
            .field("kind", &self.kind)
            .field("root_daddr", &self.root_daddr)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Inner {
    page_table: PageTable,
    /// Live mappings keyed by start address, so that the neighbour on either
    /// side of a candidate range is one lookup.
    mappings: BTreeMap<u64, Mapping>,
}

impl VmContext {
    /// Creates an empty address space.
    ///
    /// Ports upstream `pvr_vm_create_context()`, including the firmware memory
    /// context it creates for a user context — a `rogue_fwif_fwmemcontext`
    /// naming [`Self::root_daddr`].
    ///
    /// Upstream also refuses a GPU whose `virtual_address_space_bits` feature is
    /// not 40. That check needs the feature value, which the caller has and this
    /// does not; [`crate::mm::mmu::ADDR_BITS`] is the constant it compares
    /// against.
    ///
    /// # Errors
    ///
    /// [`PvrError::Unsupported`] when `kind` is [`VmContextKind::User`] and the
    /// firmware image has not been loaded yet: the memory context lives on the
    /// firmware heap. Production fills that heap in `fw::init` before the
    /// device is registered; see `platform::k1::attach`.
    pub fn new(device: Arc<PvrDevice>, kind: VmContextKind) -> PvrResult<Arc<Self>> {
        let page_table = PageTable::new(device.clone())?;
        let root_daddr = page_table.root_daddr();
        // Upstream `pvr_vm_create_context()`: only a userspace context gets a
        // `fw_mem_ctx_obj`. The kernel context maps firmware structures and has
        // no client common context that would name a memory context.
        let fw_mem_context = match kind {
            VmContextKind::User => Some(FwMemContext::new(&device, root_daddr)?),
            VmContextKind::Kernel => None,
        };

        Ok(Arc::new(Self {
            device,
            kind,
            root_daddr,
            fw_mem_context,
            inner: SpinLock::new(Inner {
                page_table,
                mappings: BTreeMap::new(),
            }),
        }))
    }

    /// Bus address of the root of the page table structure.
    ///
    /// Ports upstream `pvr_vm_get_page_table_root_addr()`. This is what goes
    /// into `rogue_fwif_fwmemcontext::pc_dev_paddr`.
    pub fn root_daddr(&self) -> Daddr {
        self.root_daddr
    }

    /// Whether this is a user-visible address space.
    pub fn kind(&self) -> VmContextKind {
        self.kind
    }

    /// The GPU this address space belongs to.
    pub fn device(&self) -> &Arc<PvrDevice> {
        &self.device
    }

    /// Firmware memory context naming this address space's page catalogue.
    ///
    /// Upstream `pvr_vm_context::fw_mem_ctx_obj`. [`None`] for
    /// [`VmContextKind::Kernel`].
    pub fn fw_mem_context(&self) -> Option<&FwMemContext> {
        self.fw_mem_context.as_ref()
    }

    /// Maps part of a buffer object at a caller-chosen GPU address.
    ///
    /// The address and length must be device-page aligned, must fall inside a
    /// heap, and must not overlap an existing mapping. Upstream checks all three;
    /// a missing check here is a way for user space to corrupt the GPU's view of
    /// memory.
    ///
    /// Ports upstream `pvr_vm_map()` and `pvr_vm_bind_op_map_init()`.
    pub fn map(
        &self,
        device_addr: DeviceAddr,
        object: &Arc<PvrObject>,
        offset: u64,
        size: u64,
    ) -> PvrResult<()> {
        self.check_range(device_addr, size)?;

        // Upstream checks the object bounds here as well as in the MMU, and so
        // does this, because the two answer different questions: this one is
        // "did user space describe a region of its buffer", the MMU's is "can
        // every page of it be encoded".
        if !offset.is_multiple_of(DEVICE_PAGE_SIZE)
            || offset >= object.size() as u64
            || offset
                .checked_add(size)
                .is_none_or(|end| end > object.size() as u64)
        {
            return Err(PvrError::InvalidArgument);
        }

        // The heap layout is user-visible through `DEV_QUERY`, and a user
        // context may only map inside one of the six heaps. The kernel's own
        // context maps firmware structures, which sit outside all of them.
        if self.kind == VmContextKind::User
            && crate::driver::find_heap_containing(&self.device.info, device_addr.0, size).is_none()
        {
            return Err(PvrError::InvalidArgument);
        }

        let mut inner = self.inner.lock();
        if inner.overlaps(device_addr.0, size) {
            return Err(PvrError::InvalidArgument);
        }

        inner.page_table.map(device_addr, object, offset, size)?;
        inner.mappings.insert(
            device_addr.0,
            Mapping {
                object: object.clone(),
                offset,
                size,
            },
        );
        crate::live::note_vm_map();
        Ok(())
    }

    /// Removes a mapping.
    ///
    /// Ports upstream `pvr_vm_unmap()`, including its use of `drm_gpuva_find()`:
    /// the address and the length must together name a live mapping exactly.
    /// A request that does not is [`PvrError::NotFound`] (`ENOENT`).
    pub fn unmap(&self, device_addr: DeviceAddr, size: u64) -> PvrResult<()> {
        self.check_range(device_addr, size)?;

        let mut inner = self.inner.lock();
        match inner.mappings.get(&device_addr.0) {
            Some(mapping) if mapping.size == size => {}
            _ => return Err(PvrError::NotFound),
        }

        inner.page_table.unmap(device_addr, size)?;
        // Only now: the object must outlive its page table entries, or the GPU
        // could be walking a table that names freed frames.
        if let Some(mapping) = inner.mappings.remove(&device_addr.0) {
            super::mmu::retire_buffer(&self.device, mapping.object);
        }
        crate::live::note_vm_unmap();
        Ok(())
    }

    /// Removes every mapping, in ascending address order.
    ///
    /// Ports upstream `pvr_vm_unmap_all()`, which
    /// `pvr_vm_context_release()` calls so that no mapping outlives the context.
    /// Here it is also what [`Drop`] does, so a context that goes away with
    /// mappings still in it cannot leave page table entries pointing at objects
    /// it is about to drop.
    pub fn unmap_all(&self) {
        let mut inner = self.inner.lock();
        while let Some((&addr, mapping)) = inner.mappings.iter().next() {
            let size = mapping.size;
            if let Err(err) = inner.page_table.unmap(DeviceAddr(addr), size) {
                // Upstream `WARN_ON`s the same failure. Continuing is right:
                // stopping would leave the rest of the address space mapped.
                ostd::error!(
                    "could not unmap {:#x} while tearing down a VM context: {:?}",
                    addr,
                    err
                );
            }
            if let Some(mapping) = inner.mappings.remove(&addr) {
                super::mmu::retire_buffer(&self.device, mapping.object);
            }
            crate::live::note_vm_unmap();
        }
    }

    /// Finds the object mapped at a GPU address.
    ///
    /// Ports upstream `pvr_vm_find_gem_object()`, which the submission path uses
    /// to resolve the addresses a job names. Returns the object, the offset into
    /// it that the mapping starts at, and the mapping's length.
    ///
    /// Unlike [`Self::unmap`] this matches any address inside a mapping, not
    /// only its first — upstream's `drm_gpuva_find_first(vm, device_addr, 1)`.
    pub fn find_object(&self, device_addr: DeviceAddr) -> Option<(Arc<PvrObject>, u64, u64)> {
        let inner = self.inner.lock();
        let (&start, mapping) = inner.mappings.range(..=device_addr.0).next_back()?;
        if device_addr.0 >= start + mapping.size {
            return None;
        }
        Some((mapping.object.clone(), mapping.offset, mapping.size))
    }

    /// Number of live mappings. Diagnostics and tests.
    pub fn mapping_count(&self) -> usize {
        self.inner.lock().mappings.len()
    }

    /// How many times this context's page tables have been flushed to the device.
    ///
    /// Diagnostics and tests; see [`crate::mm::mmu::PageTable::syncs_issued`].
    #[cfg(any(test, ktest))]
    pub fn syncs_issued(&self) -> u64 {
        self.inner.lock().page_table.syncs_issued()
    }

    /// Reads the page table entry describing a GPU address.
    ///
    /// For tests and for diagnosing a GPU fault against what the tables actually
    /// say, which is the only way to tell "never mapped" from "mapped to the
    /// wrong thing".
    pub fn page_entry(&self, device_addr: DeviceAddr) -> PvrResult<Option<u64>> {
        self.inner.lock().page_table.page_entry(device_addr)
    }

    /// The shared half of [`Self::map`] and [`Self::unmap`]'s validation.
    ///
    /// Ports upstream `pvr_device_addr_and_size_are_valid()`. The page table
    /// checks the same things again — deliberately, because it is also reachable
    /// from the kernel's own mapping paths.
    fn check_range(&self, device_addr: DeviceAddr, size: u64) -> PvrResult<()> {
        if size == 0
            || !device_addr.0.is_multiple_of(DEVICE_PAGE_SIZE)
            || !size.is_multiple_of(DEVICE_PAGE_SIZE)
            || device_addr
                .0
                .checked_add(size)
                .is_none_or(|end| end > crate::mm::mmu::ADDR_SPACE_SIZE)
        {
            return Err(PvrError::InvalidArgument);
        }
        Ok(())
    }
}

impl Inner {
    /// Whether `[start, start + size)` intersects a live mapping.
    ///
    /// Only two candidates can intersect a range in a set of disjoint intervals:
    /// the one starting at or before it, and the first one starting after it.
    fn overlaps(&self, start: u64, size: u64) -> bool {
        let end = start + size;
        if let Some((&before, mapping)) = self.mappings.range(..start).next_back()
            && before + mapping.size > start
        {
            return true;
        }
        self.mappings
            .range(start..)
            .next()
            .is_some_and(|(&after, _)| after < end)
    }
}

impl Drop for VmContext {
    /// Tears down every mapping before the page tables go away.
    ///
    /// Upstream reaches the same state through `pvr_vm_context_release()`. Doing
    /// it in `Drop` rather than in the ioctl means it also covers the client
    /// that closes its file with mappings still live, which is the common case.
    fn drop(&mut self) {
        self.unmap_all();
    }
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    #[cfg(ktest)]
    use super::*;
    #[cfg(ktest)]
    use crate::{
        device::{FwVersion, GpuId, RegisterWindow},
        device_info::DeviceInfo,
        fw::{self, FwInitParams},
        rogue::{cr, heap},
        uapi::bo_flags,
    };

    /// A device with a firmware image loaded, which a user [`VmContext`] needs
    /// for its firmware memory context.
    #[cfg(ktest)]
    fn device_with_firmware() -> Arc<PvrDevice> {
        let image = fw::fixture::image();
        let validated = fw::validate_and_init_device_info(&image).expect("fixture image");
        let regs = RegisterWindow::simulated();
        regs.write64(cr::core_id_pbvnc::OFFSET, GpuId::K1_BXE_2_32.to_packed())
            .expect("simulated register");
        let device = PvrDevice::probe(regs, &validated).expect("probe");
        fw::init(&device, image, FwInitParams::default()).expect("firmware load");
        device
    }

    #[cfg(ktest)]
    fn context(kind: VmContextKind) -> Arc<VmContext> {
        // A user context allocates a firmware memory context on the firmware
        // heap, so it needs a loaded image. A kernel context does not.
        let device = match kind {
            VmContextKind::User => device_with_firmware(),
            VmContextKind::Kernel => PvrDevice::new(
                RegisterWindow::simulated(),
                GpuId::K1_BXE_2_32,
                DeviceInfo::default(),
                FwVersion { major: 1, minor: 1 },
            ),
        };
        VmContext::new(device, kind).expect("context")
    }

    #[cfg(ktest)]
    fn object(pages: u64) -> Arc<PvrObject> {
        PvrObject::new(
            pages * DEVICE_PAGE_SIZE,
            bo_flags::ALLOW_CPU_USERSPACE_ACCESS,
        )
        .expect("object")
    }

    #[cfg(ktest)]
    const BASE: u64 = heap::ROGUE_GENERAL_HEAP_BASE;

    #[ktest]
    fn a_new_context_is_empty_but_has_a_root() {
        let vm = context(VmContextKind::User);
        assert_eq!(vm.mapping_count(), 0);
        assert_ne!(vm.root_daddr(), 0);
        assert_eq!(vm.page_entry(DeviceAddr(BASE)).unwrap(), None);
    }

    /// The round trip user space actually performs: map, look the address back
    /// up, unmap.
    #[ktest]
    fn map_records_the_mapping_and_unmap_removes_it() {
        let vm = context(VmContextKind::User);
        let bo = object(4);

        vm.map(DeviceAddr(BASE), &bo, 0, 4 * DEVICE_PAGE_SIZE)
            .expect("map");
        assert_eq!(vm.mapping_count(), 1);
        assert!(vm.page_entry(DeviceAddr(BASE)).unwrap().is_some());

        // Any address inside the mapping resolves to the object.
        let (found, offset, size) = vm
            .find_object(DeviceAddr(BASE + 2 * DEVICE_PAGE_SIZE))
            .expect("inside");
        assert!(Arc::ptr_eq(&found, &bo));
        assert_eq!((offset, size), (0, 4 * DEVICE_PAGE_SIZE));
        // One byte past the end does not.
        assert!(
            vm.find_object(DeviceAddr(BASE + 4 * DEVICE_PAGE_SIZE))
                .is_none()
        );

        vm.unmap(DeviceAddr(BASE), 4 * DEVICE_PAGE_SIZE)
            .expect("unmap");
        assert_eq!(vm.mapping_count(), 0);
        assert_eq!(vm.page_entry(DeviceAddr(BASE)).unwrap(), None);
    }

    /// PTE removal is not proof that a cached GPU translation stopped reaching the BO.
    #[ktest]
    fn unmapped_backing_waits_for_confirmed_gpu_stop() {
        let vm = context(VmContextKind::User);
        let bo = object(1);
        let weak = Arc::downgrade(&bo);
        vm.map(DeviceAddr(BASE), &bo, 0, DEVICE_PAGE_SIZE)
            .expect("map");
        drop(bo);
        vm.unmap(DeviceAddr(BASE), DEVICE_PAGE_SIZE).expect("unmap");
        assert_eq!(vm.mapping_count(), 0);
        assert!(weak.upgrade().is_some());

        super::super::mmu::release_after_stop(vm.device());
        assert!(weak.upgrade().is_some());
        vm.device().declare_lost();
        super::super::mmu::release_after_stop(vm.device());
        assert!(weak.upgrade().is_some());

        // Simulated registers never execute DMA. Model acknowledged idle and reset.
        vm.device()
            .quiesced
            .store(true, core::sync::atomic::Ordering::Release);
        super::super::mmu::release_after_stop(vm.device());
        assert!(weak.upgrade().is_none());
    }

    /// Overlap is refused from every direction, and a refusal changes nothing.
    #[ktest]
    fn overlapping_mappings_are_refused() {
        let vm = context(VmContextKind::User);
        let bo = object(8);
        let page = DEVICE_PAGE_SIZE;

        vm.map(DeviceAddr(BASE + 4 * page), &bo, 0, 4 * page)
            .expect("map");
        let syncs = vm.syncs_issued();

        for (addr, size) in [
            // Identical.
            (BASE + 4 * page, 4 * page),
            // Starts inside.
            (BASE + 5 * page, 4 * page),
            // Ends inside.
            (BASE + 2 * page, 4 * page),
            // Strictly contains.
            (BASE + 2 * page, 8 * page),
            // Strictly contained.
            (BASE + 5 * page, page),
        ] {
            assert_eq!(
                vm.map(DeviceAddr(addr), &bo, 0, size),
                Err(PvrError::InvalidArgument),
                "{addr:#x}+{size:#x} overlaps and must be refused"
            );
        }

        // Nothing was written for any of them.
        assert_eq!(vm.syncs_issued(), syncs);
        assert_eq!(vm.mapping_count(), 1);

        // Abutting on either side is not overlapping.
        vm.map(DeviceAddr(BASE), &bo, 0, 4 * page).expect("before");
        vm.map(DeviceAddr(BASE + 8 * page), &bo, 0, 4 * page)
            .expect("after");
        assert_eq!(vm.mapping_count(), 3);
    }

    /// A user context may only map inside one of the six heaps `DEV_QUERY`
    /// advertises, and a mapping that runs off the end of one is not "inside" it.
    #[ktest]
    fn a_user_context_may_only_map_inside_a_heap() {
        let vm = context(VmContextKind::User);
        // Two pages, so that the "runs off the end of the heap" case below is
        // refused by the heap check rather than by the object bounds.
        let bo = object(2);

        // Between the general heap and the next one is a hole.
        let hole = heap::ROGUE_GENERAL_HEAP_BASE + heap::ROGUE_GENERAL_HEAP_SIZE;
        assert_eq!(
            vm.map(DeviceAddr(hole), &bo, 0, DEVICE_PAGE_SIZE),
            Err(PvrError::InvalidArgument)
        );
        // Address zero is in no heap.
        assert_eq!(
            vm.map(DeviceAddr(0), &bo, 0, DEVICE_PAGE_SIZE),
            Err(PvrError::InvalidArgument)
        );
        // The last page of a heap is inside it; one past its end is not.
        let last = hole - DEVICE_PAGE_SIZE;
        vm.map(DeviceAddr(last), &bo, 0, DEVICE_PAGE_SIZE)
            .expect("last page of the general heap");
        assert_eq!(
            vm.map(DeviceAddr(last), &bo, 0, 2 * DEVICE_PAGE_SIZE).err(),
            Some(PvrError::InvalidArgument)
        );

        // The kernel's own context is not bound by the heap layout: the
        // firmware structures it maps live outside every user-visible heap.
        let kernel = context(VmContextKind::Kernel);
        kernel
            .map(DeviceAddr(hole), &bo, 0, DEVICE_PAGE_SIZE)
            .expect("the kernel context maps where it likes");
    }

    /// Unmap names a mapping exactly, as upstream's `drm_gpuva_find()` does.
    #[ktest]
    fn unmap_must_name_a_whole_mapping() {
        let vm = context(VmContextKind::User);
        let bo = object(4);
        let page = DEVICE_PAGE_SIZE;

        vm.map(DeviceAddr(BASE), &bo, 0, 4 * page).expect("map");

        // Right address, wrong length.
        assert_eq!(
            vm.unmap(DeviceAddr(BASE), 2 * page),
            Err(PvrError::NotFound)
        );
        // Inside the mapping but not its start.
        assert_eq!(
            vm.unmap(DeviceAddr(BASE + page), 3 * page),
            Err(PvrError::NotFound)
        );
        // Nothing there at all.
        assert_eq!(
            vm.unmap(DeviceAddr(BASE + 8 * page), page),
            Err(PvrError::NotFound)
        );
        assert_eq!(vm.mapping_count(), 1);

        vm.unmap(DeviceAddr(BASE), 4 * page).expect("exact");
        assert_eq!(vm.mapping_count(), 0);
    }

    /// Malformed ranges are refused before the page tables are touched.
    #[ktest]
    fn misaligned_and_out_of_range_requests_are_refused() {
        let vm = context(VmContextKind::User);
        let bo = object(2);
        let page = DEVICE_PAGE_SIZE;
        let syncs = vm.syncs_issued();

        for (addr, offset, size) in [
            // Zero length.
            (BASE, 0, 0),
            // Misaligned address, length and object offset.
            (BASE + 8, 0, page),
            (BASE, 0, page + 8),
            (BASE, 8, page),
            // Past the end of the object.
            (BASE, 0, 3 * page),
            (BASE, 2 * page, page),
            // Past the top of the address space, and wrapping it.
            (crate::mm::mmu::ADDR_SPACE_SIZE, 0, page),
            (crate::mm::mmu::ADDR_SPACE_SIZE - page, 0, 2 * page),
            (!(page - 1), 0, page),
        ] {
            assert_eq!(
                vm.map(DeviceAddr(addr), &bo, offset, size),
                Err(PvrError::InvalidArgument),
                "{addr:#x} offset {offset:#x} size {size:#x}"
            );
        }
        assert_eq!(vm.syncs_issued(), syncs);
        assert_eq!(vm.mapping_count(), 0);
    }

    /// A mapping holds its object alive, which is what stops a client from
    /// closing a handle and pulling the pages out from under the GPU.
    #[ktest]
    fn a_mapping_holds_its_object() {
        let vm = context(VmContextKind::User);
        let before = k1_musebook_drm::live::gem_objects();
        let bo = object(1);
        assert_eq!(Arc::strong_count(&bo), 1);
        assert_eq!(k1_musebook_drm::live::gem_objects(), before + 1);

        vm.map(DeviceAddr(BASE), &bo, 0, DEVICE_PAGE_SIZE)
            .expect("map");
        assert_eq!(Arc::strong_count(&bo), 2);

        drop(bo);
        assert_eq!(
            k1_musebook_drm::live::gem_objects(),
            before + 1,
            "the GPU VM mapping must keep the object after the caller's Arc is gone"
        );

        vm.unmap(DeviceAddr(BASE), DEVICE_PAGE_SIZE).expect("unmap");
        assert_eq!(
            k1_musebook_drm::live::gem_objects(),
            before + 1,
            "removed GPU translations retain backing until invalidated or stopped"
        );
        // The fixture has no DMA engine; model acknowledged idle and reset.
        vm.device()
            .quiesced
            .store(true, core::sync::atomic::Ordering::Release);
        super::super::mmu::release_after_stop(vm.device());
        assert_eq!(
            k1_musebook_drm::live::gem_objects(),
            before,
            "a confirmed stop releases the final mapping owner"
        );
    }

    /// Dropping a context with live mappings unmaps them, so no page table entry
    /// outlives the object it names.
    #[ktest]
    fn dropping_a_context_releases_its_mappings() {
        let bo = object(2);
        {
            let vm = context(VmContextKind::User);
            vm.map(DeviceAddr(BASE), &bo, 0, DEVICE_PAGE_SIZE)
                .expect("first");
            vm.map(
                DeviceAddr(BASE + 4 * DEVICE_PAGE_SIZE),
                &bo,
                DEVICE_PAGE_SIZE,
                DEVICE_PAGE_SIZE,
            )
            .expect("second");
            assert_eq!(Arc::strong_count(&bo), 3);
        }
        assert_eq!(
            Arc::strong_count(&bo),
            1,
            "a dropped context must release every object it mapped"
        );
    }

    /// `unmap_all` is what teardown relies on, and it must leave the page tables
    /// as empty as it found them so that a leaked directory cannot make an
    /// unmapped span look present.
    #[ktest]
    fn unmap_all_empties_the_hierarchy() {
        let vm = context(VmContextKind::User);
        let bo = object(1);

        for span in 0..3u64 {
            vm.map(
                DeviceAddr(BASE + span * (1 << 21)),
                &bo,
                0,
                DEVICE_PAGE_SIZE,
            )
            .expect("map");
        }
        assert_eq!(vm.mapping_count(), 3);

        vm.unmap_all();
        assert_eq!(vm.mapping_count(), 0);
        for span in 0..3u64 {
            assert_eq!(
                vm.page_entry(DeviceAddr(BASE + span * (1 << 21))).unwrap(),
                None,
                "span {span} still has a page table"
            );
        }
    }

    #[ktest]
    fn firmware_memory_context_failure_and_drop_restore_its_range_and_ptes() {
        let device = device_with_firmware();
        // An unrelated live VM must survive both rollback and successful teardown.
        let _bystander = VmContext::new(device.clone(), VmContextKind::User).expect("bystander");
        let fw = device.fw.get().expect("firmware");
        let baseline = fw.heap.lock().ranges_for_test();
        let ptes = fw.mips.page_table.entries_for_test();

        fw.mips.page_table.fail_write_after_for_test(Some(0));
        let result = VmContext::new(device.clone(), VmContextKind::User);
        fw.mips.page_table.fail_write_after_for_test(None);
        assert_eq!(result.err(), Some(PvrError::InvalidArgument));
        assert_eq!(fw.heap.lock().ranges_for_test(), baseline);
        assert_eq!(fw.mips.page_table.entries_for_test(), ptes);

        let context = VmContext::new(device.clone(), VmContextKind::User).expect("VM");
        assert_ne!(fw.mips.page_table.entries_for_test(), ptes);
        drop(context);
        assert_eq!(fw.heap.lock().ranges_for_test(), baseline);
        assert_eq!(fw.mips.page_table.entries_for_test(), ptes);
    }
}
