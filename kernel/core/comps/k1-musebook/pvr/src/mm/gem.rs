// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: DMA-safe GEM backing and PRIME imports.

//! GEM buffer objects and CPU mappings.
//!
//! A native object eagerly allocates contiguous DMA backing. GPU mappings and CPU
//! faults refer to the same physical frames; no GPU fault-driven allocation exists.
//! The CPU fault path runs with preemption disabled and a page-table lock held, so
//! it only locates a preallocated frame and never allocates backing memory.
//!
//! K1 is not DMA coherent. CPU user mappings use write-combining access because
//! `SUBMIT_JOBS` carries no complete BO list to flush, and clients may poll shared
//! memory without another ioctl. Cached kernel aliases still require explicit DMA
//! synchronization. A GPU device-cache bypass flag does not replace CPU cache
//! maintenance.
//!
//! Eager contiguous allocation limits sparse and large-buffer use. Imported objects
//! retain the exporter's dma-buf and map its existing backing rather than copying.

use alloc::sync::Arc;

use k1_musebook_drm::{
    error::{DrmError, DrmResult},
    gem::{BufferUse, GemObject},
    prime::DmaBuf,
};
use ostd::mm::{
    Daddr, FrameAllocOptions, HasDaddr, HasPaddr, HasSize, PAGE_SIZE, Segment, UFrame,
    dma::DmaStream,
};

use crate::{
    error::{PvrError, PvrResult},
    fw::heap::GPU_CACHE_COHERENT,
    mm::mmu::DEVICE_PAGE_SIZE,
    uapi::bo_flags,
};

/// A PowerVR buffer object.
///
/// Implements `k1-musebook-drm`'s [`GemObject`] so that the generic handle table,
/// mmap-offset allocator and PRIME export all work unchanged.
///
/// Ports upstream `struct pvr_gem_object`, which is a `drm_gem_shmem_object`
/// plus the creation flags. The flags are the interesting part: they are read
/// again on every mapping, so they are stored rather than consumed.
#[derive(Debug)]
pub struct PvrObject {
    /// Backing pages, always a stream mapping. See the module documentation in
    /// [`super`] for why never coherent.
    dma: DmaStream,
    /// Where the frames came from, and what may therefore be done with them.
    backing: Backing,
    /// Flags the object was created with, from [`crate::uapi::bo_flags`].
    flags: u64,
}

/// Where a [`PvrObject`]'s frames came from.
///
/// The distinction exists because it decides what the object may be used for,
/// not because the two are handled differently at submission time — the GPU
/// reaches both through the same [`DmaStream`], and by the time a job names an
/// address the difference is gone. That is deliberate, and it is upstream's
/// shape too: `drm/imagination` gives an imported dma-buf a real
/// `pvr_gem_object` via `gem_create_object`, so `pvr_mmu_map()` and
/// `pvr_vm_find_gem_object()` need no case analysis.
#[derive(Debug)]
enum Backing {
    /// Frames this driver allocated and owns.
    ///
    /// Holds the segment because the CPU-mapping side needs it: a clone of what
    /// was handed to [`DmaStream::map`], not a second allocation, since
    /// `Segment` is reference counted per frame.
    Owned(Segment<()>),
    /// Frames another driver allocated, reached through a dma-buf.
    ///
    /// Holds the buffer rather than the segment, for two reasons. It keeps the
    /// exporting object alive, which the frames alone would not — a dma-buf is
    /// the handle the exporter understands, and dropping the last reference to it
    /// is how the exporter learns it can release the allocation. And it keeps
    /// [`k1_musebook_drm::prime::DmaBuf::fences`] reachable, which is where the
    /// synchronisation this port does not yet do will have to hook in; see
    /// [`PvrObject::import`].
    Imported(Arc<DmaBuf>),
}

impl PvrObject {
    /// Allocates a buffer object.
    ///
    /// Ports upstream `pvr_gem_object_create()` together with the validation
    /// `pvr_ioctl_create_bo()` does before calling it.
    ///
    /// `size` must be a non-zero multiple of the device page size; upstream
    /// rejects anything else in the ioctl rather than rounding up
    /// (`args->size & (PVR_DEVICE_PAGE_SIZE - 1)`), because a caller that asked
    /// for a partial page and got a whole one would map memory it did not ask
    /// for. Flags outside [`bo_flags::MASK`] are rejected rather than ignored,
    /// and so is the one combination upstream calls out as invalid.
    ///
    /// The memory is zeroed, which is not optional: these frames go straight
    /// into a user address space and into the GPU's, so whatever the previous
    /// owner left in them would otherwise be readable.
    pub fn new(size: u64, flags: u64) -> PvrResult<Arc<Self>> {
        if size == 0 || !size.is_multiple_of(DEVICE_PAGE_SIZE) {
            return Err(PvrError::InvalidArgument);
        }
        if flags & !bo_flags::MASK != 0 {
            return Err(PvrError::InvalidArgument);
        }
        // Upstream `pvr_gem_object_flags_validate()`'s `invalid_combinations`
        // table has exactly one row. Memory reserved for the firmware and the
        // Parameter Manager must not also be CPU mappable, and upstream requires
        // the two flags never be given together rather than silently letting one
        // win.
        if flags & bo_flags::PM_FW_PROTECT != 0 && flags & bo_flags::ALLOW_CPU_USERSPACE_ACCESS != 0
        {
            return Err(PvrError::InvalidArgument);
        }

        let pages = usize::try_from(size >> PAGE_SIZE.ilog2()).map_err(|_| PvrError::NoMemory)?;
        // The GPU's physical bus width is 36 bits, but the platform DMA allocator's
        // address limits still apply. Imported buffers must expose the same frames to CPU
        // and GPU; a private bounce allocation would break that shared-memory contract.
        let segment = FrameAllocOptions::new()
            .zeroed(true)
            .dma_capable(true)
            .alloc_segment(pages)
            .map_err(|_| PvrError::NoMemory)?;

        // The stream and the segment name the same frames; cloning takes a
        // reference per frame rather than copying anything.
        let dma = DmaStream::map(segment.clone().into(), GPU_CACHE_COHERENT)
            .map_err(|_| PvrError::NoMemory)?;

        // The same guard `map_foreign` applies, for the same reason. This object
        // hands `segment` to the display controller (`GemObject::segment`, the
        // PRIME export path) and `dma` to the GPU; if `DmaStream::map` chose to
        // bounce, the two would name different frames and the GPU would render
        // into a copy while the panel scans out the original -- zeroes, with no
        // error anywhere.
        if dma.daddr() != segment.paddr() {
            ostd::error!(
                "GEM allocation at {:#x} was mapped for DMA at {:#x}: the platform can only \
                 reach it through a bounce buffer, which a scanout buffer cannot survive",
                segment.paddr(),
                dma.daddr()
            );
            return Err(PvrError::Unsupported);
        }

        let object = Self {
            dma,
            backing: Backing::Owned(segment),
            flags,
        };
        k1_musebook_drm::live::note_gem_alloc(pages);
        // The zeroes were written through the CPU's cached alias, so on the K1
        // the GPU cannot see them yet. Upstream reaches the same state with
        // `dma_sync_sgtable_for_device()` followed by `pvr_gem_object_zero()`.
        object.sync_to_device(0, object.size())?;

        Ok(Arc::new(object))
    }

    /// Builds an object over a buffer another device exported.
    ///
    /// This is what lets the GPU render into memory it did not allocate — a
    /// scanout buffer belonging to the display controller, which is the only way
    /// a render-only GPU puts anything on a screen. Reached from
    /// `PvrDriver::import_buffer`, which `k1-musebook-drm` calls for
    /// `PRIME_FD_TO_HANDLE`.
    ///
    /// Ports the effect of upstream's `gem_create_object` on the import path: the
    /// result is an ordinary [`PvrObject`], so `VM_MAP`, the MMU and the
    /// submission path need no notion of imports at all.
    ///
    /// # Why the frames are re-mapped rather than borrowed
    ///
    /// The exporter's [`Segment`] names host frames; what the GPU's page tables
    /// need is bus addresses. [`DmaStream::map`] is the thing that answers that,
    /// so a stream is established over a *clone* of the exporter's segment.
    /// Cloning takes a reference per frame rather than copying, so both drivers
    /// address the same physical pages — which is the entire point, and also why
    /// the clone must not be mistaken for an allocation.
    ///
    /// This is the one place that reaches through
    /// [`k1_musebook_drm::prime::ImportedObject`]'s deliberate refusal to expose the
    /// exporter's segment. That refusal is about *CPU* mappings, and it is right:
    /// a second cached alias of frames the exporter maps uncached is how a buffer
    /// ends up written through two mappings with different attributes. A GPU page
    /// table is not a CPU mapping and carries its own attributes — this port's
    /// leaf entries always clear the coherency bit (`ABI.md`), which matches
    /// how the display controller already treats its scanout memory. So
    /// [`GemObject::segment`] still reports `None` for an import, and the CPU
    /// still cannot map it; see [`Self::allows_cpu_access`].
    ///
    /// # Not yet done: synchronisation
    ///
    /// Upstream reaches an imported buffer's fences through `dma_resv` and makes
    /// a job wait on them implicitly. Here the buffer's
    /// [`fences`](k1_musebook_drm::prime::DmaBuf::fences) are held but never consulted,
    /// so a client must sequence GPU rendering against display scanout itself,
    /// with the explicit sync objects accepted by `SUBMIT_JOBS`. Clients relying
    /// solely on implicit dma-buf synchronization are not supported.
    pub fn import(buffer: &Arc<DmaBuf>) -> PvrResult<Arc<Self>> {
        let dma = Self::map_foreign(buffer.object())?;

        Ok(Arc::new(Self {
            dma,
            backing: Backing::Imported(buffer.clone()),
            // No `bo_flags`: an import carries none, and the two that matter both
            // read correctly as absent. `PM_FW_PROTECT` clear leaves the object
            // exportable, which a re-export of a buffer that arrived this way
            // should be; `ALLOW_CPU_USERSPACE_ACCESS` clear is what makes
            // `is_mappable` false, which is upstream's answer for an import.
            flags: 0,
        }))
    }

    /// Establishes the GPU's view of frames belonging to another driver.
    ///
    /// Split out of [`Self::import`] because it is the part with judgement in it,
    /// and the part a test can reach: it needs only an exporting object, while
    /// `import` needs a whole `DmaBuf`, which in turn needs a registered device.
    fn map_foreign(exporter: &Arc<dyn GemObject>) -> PvrResult<DmaStream> {
        // The exporter must be backed by host frames. A device-private
        // allocation reports `None` here, and there is nothing this driver could
        // put in a page table for it.
        let segment = exporter.segment().ok_or_else(|| {
            ostd::error!("cannot import a buffer whose exporter has no host frames");
            PvrError::InvalidArgument
        })?;

        // The MMU encodes whole device pages, and `DEVICE_PAGE_SIZE` equals the
        // host page size (asserted below), so a segment is always a whole number
        // of them. A size that is not says the exporter measures its buffer
        // differently from its allocation, which would make every bound check
        // downstream disagree with the page tables.
        if segment.size() == 0 || !segment.size().is_multiple_of(PAGE_SIZE) {
            return Err(PvrError::InvalidArgument);
        }

        let dma = DmaStream::map(segment.clone().into(), GPU_CACHE_COHERENT).map_err(|_| {
            ostd::error!("could not establish a DMA mapping over an imported buffer");
            PvrError::NoMemory
        })?;

        // The guard that keeps a silent disaster from being possible.
        //
        // `DmaStream::map` has two implementations. When the platform can issue
        // cache-maintenance operations it maps the segment in place, and the bus
        // address is the frames' own. When it cannot, it allocates a *separate*
        // run of pages and syncs through them — a bounce buffer, whose bus
        // address belongs to the copy. For a buffer this driver allocated that is
        // merely a cost. For an imported scanout buffer it would mean the GPU
        // rendering into a copy while the display controller scans out the
        // original, with nothing on the screen and no error anywhere.
        //
        // The K1 can issue them (Zicbom; every DMA driver in this tree depends on
        // it, see `CONVENTIONS.md` DMA contract), so this is expected to hold.
        // It is checked rather than assumed because the failure is invisible.
        if dma.daddr() != segment.paddr() {
            ostd::error!(
                "refusing to import a buffer the platform can only reach through a bounce \
                 buffer: the GPU would render into a copy of it"
            );
            return Err(PvrError::Unsupported);
        }

        Ok(dma)
    }

    /// The buffer this object was imported from, if it was.
    ///
    /// Diagnostic, and what distinguishes the two backings for callers that need
    /// to — the submission path deliberately does not.
    pub fn imported_from(&self) -> Option<&Arc<DmaBuf>> {
        match &self.backing {
            Backing::Owned(_) => None,
            Backing::Imported(buffer) => Some(buffer),
        }
    }

    /// Whether the frames belong to another driver.
    pub fn is_imported(&self) -> bool {
        self.imported_from().is_some()
    }

    /// Flags the object was created with.
    pub fn flags(&self) -> u64 {
        self.flags
    }

    /// Size in bytes. Always a whole number of device pages.
    pub fn size(&self) -> usize {
        self.dma.size()
    }

    /// Whether the object is reserved for firmware and Parameter Manager use and
    /// is therefore not CPU mappable.
    pub fn is_fw_protected(&self) -> bool {
        self.flags & bo_flags::PM_FW_PROTECT != 0
    }

    /// Whether user space asked to be able to map this object.
    ///
    /// Upstream `pvr_gem_mmap()` refuses with `-EINVAL` when this is false.
    pub fn allows_cpu_access(&self) -> bool {
        self.flags & bo_flags::ALLOW_CPU_USERSPACE_ACCESS != 0
    }

    /// Bus address of byte `offset`.
    ///
    /// Ports upstream `pvr_gem_get_dma_addr()`, which walks a scatter-gather
    /// table to answer the same question. The allocation is one contiguous run
    /// here (see the module documentation), so this is addition — and one bound
    /// check, which upstream also performs by falling off the end of its walk.
    pub fn daddr_at(&self, offset: usize) -> PvrResult<Daddr> {
        if offset >= self.size() {
            return Err(PvrError::InvalidArgument);
        }
        Ok(self.dma.daddr() + offset)
    }

    /// The underlying DMA allocation.
    ///
    /// For callers that own the sync discipline themselves. Prefer the accessors
    /// below, which sync.
    pub fn dma(&self) -> &DmaStream {
        &self.dma
    }

    /// Makes CPU writes visible to the GPU.
    ///
    /// Must be called before any GPU access to a region the CPU has written
    /// *through the kernel's mapping*. A user-space mapping is write-combining
    /// and needs no equivalent; see the module documentation.
    pub fn sync_to_device(&self, offset: usize, len: usize) -> PvrResult<()> {
        let range = self.check_range(offset, len)?;
        self.dma
            .sync_to_device(range)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Makes GPU writes visible to the CPU.
    pub fn sync_from_device(&self, offset: usize, len: usize) -> PvrResult<()> {
        let range = self.check_range(offset, len)?;
        self.dma
            .sync_from_device(range)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Bounds a `[offset, offset + len)` access against the allocation.
    fn check_range(&self, offset: usize, len: usize) -> PvrResult<core::ops::Range<usize>> {
        let end = offset.checked_add(len).ok_or(PvrError::InvalidArgument)?;
        if end > self.size() {
            return Err(PvrError::InvalidArgument);
        }
        Ok(offset..end)
    }
}

impl Drop for PvrObject {
    fn drop(&mut self) {
        if let Backing::Owned(segment) = &self.backing {
            k1_musebook_drm::live::note_gem_free(segment.size() / PAGE_SIZE);
        }
    }
}

impl GemObject for PvrObject {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn size(&self) -> usize {
        Self::size(self)
    }

    /// Write-combining on the CPU side. See the module documentation for why
    /// the "there is an ioctl at submission time" argument does not rescue a
    /// writeback mapping here.
    fn buffer_use(&self) -> BufferUse {
        BufferUse::Render
    }

    /// The frames, for the CPU-mapping side — and `None` for an import.
    ///
    /// Withholding them for an import is the same judgement
    /// [`k1_musebook_drm::prime::ImportedObject`] makes, and this type is subject to it
    /// even though it can reach them: a CPU alias of frames the exporter maps
    /// with a different cache policy is a correctness bug on a SoC with no DMA
    /// coherency. The GPU side is unaffected, because it goes through
    /// [`Self::daddr_at`] rather than here. See [`Self::import`].
    fn segment(&self) -> Option<&Segment<()>> {
        match &self.backing {
            Backing::Owned(segment) => Some(segment),
            Backing::Imported(_) => None,
        }
    }

    /// Synchronizes the cached kernel alias before a private CPU copy.
    ///
    /// The existing DMA mapping owns these pages. Cache maintenance is bounded
    /// to one page and does not allocate, sleep, or enter the VM layer.
    fn prepare_cpu_read(&self, pgoff: usize) -> DrmResult<()> {
        let segment = GemObject::segment(self).ok_or(DrmError::BadAddress)?;
        let offset = pgoff
            .checked_mul(PAGE_SIZE)
            .filter(|offset| *offset < segment.size())
            .ok_or(DrmError::BadAddress)?;
        self.sync_from_device(offset, PAGE_SIZE)
            .map_err(|_| DrmError::BadAddress)
    }

    /// Resolves one page of the object into the frame that backs it.
    ///
    /// Runs with preemption disabled and a page table lock held, so it allocates
    /// nothing, takes no lock and cannot fail except by being asked for a page
    /// that does not exist. Two address spaces faulting on the same `pgoff`
    /// concurrently both get the same frame, because the answer is a function of
    /// the segment alone.
    ///
    /// An imported object has no segment to answer from and reports
    /// [`DrmError::BadAddress`]. Unreachable in practice —
    /// [`Self::is_mappable`] is false for one, so no `mmap` offset is ever handed
    /// out — and a backstop rather than a path, exactly as it is for
    /// `ImportedObject`.
    fn fault(&self, pgoff: usize) -> DrmResult<UFrame> {
        let segment = GemObject::segment(self).ok_or(DrmError::BadAddress)?;

        let offset = pgoff
            .checked_mul(PAGE_SIZE)
            .filter(|offset| *offset < segment.size())
            .ok_or(DrmError::BadAddress)?;

        // Slicing takes a reference on the frame and handing it out transfers
        // that reference to the caller, so the object keeps its own. That is
        // what lets a page outlive the mapping that faulted it in.
        let frame = segment
            .slice(&(offset..offset + PAGE_SIZE))
            .into_iter()
            .next()
            .expect("a one-page slice of a segment yields exactly one frame");

        Ok(frame.into())
    }

    /// Firmware- and Parameter-Manager-protected memory may not leave the
    /// driver.
    ///
    /// Upstream `pvr_gem_export()` refuses the same objects with `-EPERM`.
    fn is_exportable(&self) -> bool {
        !self.is_fw_protected()
    }

    /// Only objects created with `DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS` may be
    /// mapped into a process.
    ///
    /// There is no per-object DRM `mmap` hook here. Enforce this both in the mapping
    /// predicate and before `GET_BO_MMAP_OFFSET` hands out an offset.
    fn is_mappable(&self) -> bool {
        self.allows_cpu_access()
    }
}

/// A buffer object is one contiguous run of host pages, and the MMU's page size
/// is the host's, so a device page never straddles two frames.
const _: () = assert!(DEVICE_PAGE_SIZE == PAGE_SIZE as u64);

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    #[cfg(ktest)]
    use super::*;

    /// An exporting object that is not this driver's, for the import tests.
    ///
    /// Not `k1_musebook_drm::gem::ShmemObject`, even though it would do for the happy
    /// path: the interesting cases are an exporter with no host frames and one
    /// whose size is not a whole number of pages, and a real object cannot be
    /// made to be either.
    #[cfg(ktest)]
    #[derive(Debug)]
    struct FakeExporter {
        segment: Option<Segment<()>>,
        size: usize,
    }

    #[cfg(ktest)]
    impl FakeExporter {
        /// An exporter backed by `pages` real frames, as a display driver's
        /// scanout buffer would be.
        fn with_frames(pages: usize) -> Arc<dyn GemObject> {
            let segment = FrameAllocOptions::new()
                .zeroed(true)
                .alloc_segment(pages)
                .expect("frames");
            Arc::new(Self {
                size: segment.size(),
                segment: Some(segment),
            })
        }

        /// An exporter whose memory is device-private, so it has no frames to
        /// hand over.
        fn without_frames() -> Arc<dyn GemObject> {
            Arc::new(Self {
                segment: None,
                size: PAGE_SIZE,
            })
        }
    }

    #[cfg(ktest)]
    impl GemObject for FakeExporter {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
            self
        }

        fn size(&self) -> usize {
            self.size
        }

        fn buffer_use(&self) -> BufferUse {
            BufferUse::Scanout
        }

        fn segment(&self) -> Option<&Segment<()>> {
            self.segment.as_ref()
        }

        fn fault(&self, _pgoff: usize) -> DrmResult<UFrame> {
            Err(DrmError::BadAddress)
        }
    }

    /// The happy path, and the property the whole feature rests on: the GPU's
    /// bus address must be the exporter's *own* frames.
    ///
    /// If this ever failed, the GPU would render into a copy of the display
    /// controller's scanout buffer and the screen would stay blank with no error
    /// reported anywhere — see the guard in `PvrObject::map_foreign`.
    #[ktest]
    fn importing_addresses_the_exporters_own_frames() {
        let exporter = FakeExporter::with_frames(4);
        let expected = exporter.segment().expect("frames").paddr();

        let dma = PvrObject::map_foreign(&exporter).expect("import");

        assert_eq!(dma.daddr(), expected);
        assert_eq!(dma.size(), 4 * PAGE_SIZE);
    }

    /// An exporter with no host frames cannot be imported.
    ///
    /// Device-private memory is the real case: there is nothing this driver could
    /// write into a page table for it, and inventing an address would be worse
    /// than refusing.
    #[ktest]
    fn an_exporter_without_host_frames_is_refused() {
        assert_eq!(
            PvrObject::map_foreign(&FakeExporter::without_frames()).err(),
            Some(PvrError::InvalidArgument)
        );
    }

    /// The two backings differ in exactly what they are allowed to be used for.
    ///
    /// Asserted together rather than one test each, because the point is the
    /// contrast: an owned object may be CPU-mapped when its flags say so, and an
    /// imported one may never be, however the request is phrased.
    #[ktest]
    fn an_imported_object_is_never_cpu_mappable_and_an_owned_one_may_be() {
        let owned = PvrObject::new(PAGE_SIZE as u64, bo_flags::ALLOW_CPU_USERSPACE_ACCESS)
            .expect("owned object");
        assert!(!owned.is_imported());
        assert!(GemObject::is_mappable(&*owned));
        assert!(GemObject::segment(&*owned).is_some());
        assert!(GemObject::fault(&*owned, 0).is_ok());

        // The real path, through the same `import` the driver hook calls.
        let exporter = FakeExporter::with_frames(1);
        let expected = exporter.segment().expect("frames").paddr();
        let buffer = k1_musebook_drm::prime::foreign_buffer_for_test(exporter);
        let imported = PvrObject::import(&buffer).expect("import");

        assert!(imported.is_imported());
        assert!(Arc::ptr_eq(
            imported.imported_from().expect("buffer"),
            &buffer
        ));
        // Never mappable: the pages belong to the exporter, which maps them with
        // its own cache policy.
        assert!(!GemObject::is_mappable(&*imported));
        assert!(GemObject::segment(&*imported).is_none());
        assert!(GemObject::fault(&*imported, 0).is_err());
        // The GPU side still works, because it goes through the stream.
        assert_eq!(imported.daddr_at(0).unwrap(), expected);
    }

    /// An imported object keeps the buffer alive, and therefore the exporter's
    /// allocation.
    ///
    /// The frames would survive on the segment clone alone, but the exporting
    /// *object* would not, and a display driver learns that its scanout buffer is
    /// free when the last reference to the dma-buf goes. Dropping it early is how
    /// a compositor's frame gets recycled while the GPU is still drawing into it.
    #[ktest]
    fn an_imported_object_holds_the_buffer_it_came_from() {
        let buffer = k1_musebook_drm::prime::foreign_buffer_for_test(FakeExporter::with_frames(2));
        let before = Arc::strong_count(&buffer);

        let imported = PvrObject::import(&buffer).expect("import");
        assert_eq!(Arc::strong_count(&buffer), before + 1);

        drop(imported);
        assert_eq!(Arc::strong_count(&buffer), before);
    }

    /// Every rejection `CREATE_BO` owes user space, checked at the layer that
    /// actually enforces it.
    #[ktest]
    fn creation_rejects_what_upstream_rejects() {
        // Zero, and anything that is not a whole number of device pages.
        assert_eq!(PvrObject::new(0, 0).err(), Some(PvrError::InvalidArgument));
        assert_eq!(
            PvrObject::new(PAGE_SIZE as u64 - 1, 0).err(),
            Some(PvrError::InvalidArgument)
        );
        assert_eq!(
            PvrObject::new(PAGE_SIZE as u64 + 1, 0).err(),
            Some(PvrError::InvalidArgument)
        );

        // Flags outside the mask, including the kernel-only bits upstream
        // reserves at the top of the word.
        assert_eq!(
            PvrObject::new(PAGE_SIZE as u64, 1 << 3).err(),
            Some(PvrError::InvalidArgument)
        );
        assert_eq!(
            PvrObject::new(PAGE_SIZE as u64, 1 << 63).err(),
            Some(PvrError::InvalidArgument)
        );

        // The one invalid combination: firmware-protected and CPU mappable.
        assert_eq!(
            PvrObject::new(
                PAGE_SIZE as u64,
                bo_flags::PM_FW_PROTECT | bo_flags::ALLOW_CPU_USERSPACE_ACCESS
            )
            .err(),
            Some(PvrError::InvalidArgument)
        );

        // Each flag on its own is fine.
        for flags in [
            0,
            bo_flags::BYPASS_DEVICE_CACHE,
            bo_flags::PM_FW_PROTECT,
            bo_flags::ALLOW_CPU_USERSPACE_ACCESS,
            bo_flags::BYPASS_DEVICE_CACHE | bo_flags::ALLOW_CPU_USERSPACE_ACCESS,
        ] {
            assert!(
                PvrObject::new(PAGE_SIZE as u64, flags).is_ok(),
                "{flags:#x}"
            );
        }
    }

    /// A fresh object reads back as zeroes through the kernel's alias, and its
    /// pages are contiguous on the bus.
    #[ktest]
    fn a_new_object_is_zeroed_and_contiguous() {
        use ostd::mm::VmIo;

        let object = PvrObject::new(3 * PAGE_SIZE as u64, 0).expect("three pages");
        assert_eq!(object.size(), 3 * PAGE_SIZE);

        assert_eq!(object.dma().read_val::<u64>(0).unwrap(), 0);
        assert_eq!(object.dma().read_val::<u64>(3 * PAGE_SIZE - 8).unwrap(), 0);

        let base = object.daddr_at(0).unwrap();
        assert_eq!(object.daddr_at(PAGE_SIZE).unwrap(), base + PAGE_SIZE);
        assert_eq!(
            object.daddr_at(3 * PAGE_SIZE - 1).unwrap(),
            base + 3 * PAGE_SIZE - 1
        );
        assert!(object.daddr_at(3 * PAGE_SIZE).is_err());
    }

    /// The GPU view and the CPU view must be the same physical memory, or a
    /// buffer written through the mapping renders as whatever the GPU's copy
    /// happens to hold. Comparing the frame `fault` hands out with the bus
    /// address the page tables would encode is the check that they are.
    #[ktest]
    fn the_cpu_and_gpu_views_are_the_same_pages() {
        use ostd::mm::HasPaddr;

        let object = PvrObject::new(2 * PAGE_SIZE as u64, bo_flags::ALLOW_CPU_USERSPACE_ACCESS)
            .expect("two pages");

        for page in 0..2usize {
            let frame = GemObject::fault(&*object, page).expect("in range");
            assert_eq!(
                frame.paddr(),
                object.daddr_at(page * PAGE_SIZE).unwrap(),
                "page {page}"
            );
        }
        assert!(GemObject::fault(&*object, 2).is_err());
    }

    #[ktest]
    fn private_copy_preparation_is_bounded_and_nonblocking() {
        let object = PvrObject::new(2 * PAGE_SIZE as u64, bo_flags::ALLOW_CPU_USERSPACE_ACCESS)
            .expect("two pages");
        let preempt_guard = ostd::task::disable_preempt();
        assert!(GemObject::prepare_cpu_read(&*object, 0).is_ok());
        assert!(GemObject::prepare_cpu_read(&*object, 1).is_ok());
        assert_eq!(
            GemObject::prepare_cpu_read(&*object, 2),
            Err(DrmError::BadAddress)
        );
        assert_eq!(
            GemObject::prepare_cpu_read(&*object, usize::MAX),
            Err(DrmError::BadAddress)
        );
        drop(preempt_guard);

        let buffer = k1_musebook_drm::prime::foreign_buffer_for_test(FakeExporter::with_frames(1));
        let imported = PvrObject::import(&buffer).expect("import");
        assert_eq!(
            GemObject::prepare_cpu_read(&*imported, 0),
            Err(DrmError::BadAddress)
        );
    }

    /// The fault handler must be idempotent: two address spaces mapping one
    /// object fault on the same page independently and must share the frame.
    #[ktest]
    fn faulting_the_same_page_twice_gives_the_same_frame() {
        use ostd::mm::HasPaddr;

        let object = PvrObject::new(PAGE_SIZE as u64, bo_flags::ALLOW_CPU_USERSPACE_ACCESS)
            .expect("one page");
        let first = GemObject::fault(&*object, 0).expect("in range");
        let second = GemObject::fault(&*object, 0).expect("in range");
        assert_eq!(first.paddr(), second.paddr());
    }

    /// The two predicates `k1-musebook-drm` consults before letting an object out of
    /// the driver.
    #[ktest]
    fn firmware_protected_objects_are_neither_mappable_nor_exportable() {
        let protected = PvrObject::new(PAGE_SIZE as u64, bo_flags::PM_FW_PROTECT).expect("object");
        assert!(!protected.is_mappable());
        assert!(!protected.is_exportable());

        let mappable =
            PvrObject::new(PAGE_SIZE as u64, bo_flags::ALLOW_CPU_USERSPACE_ACCESS).expect("object");
        assert!(mappable.is_mappable());
        assert!(mappable.is_exportable());

        // A plain object is exportable but not mappable: user space has to ask
        // for CPU access at creation time, as upstream requires.
        let plain = PvrObject::new(PAGE_SIZE as u64, 0).expect("object");
        assert!(!plain.is_mappable());
        assert!(plain.is_exportable());
    }

    /// Every buffer object is write-combining on the CPU. See the module
    /// documentation; this pins the decision so that changing it is a visible
    /// edit rather than a side effect.
    #[ktest]
    fn buffer_objects_are_write_combining() {
        use ostd::mm::CachePolicy;

        let object = PvrObject::new(PAGE_SIZE as u64, 0).expect("object");
        assert_eq!(object.buffer_use(), BufferUse::Render);
        assert_eq!(
            object.buffer_use().cache_policy(),
            CachePolicy::WriteCombining
        );
    }

    /// The kernel-side accessors are bounded, so a driver bug is an error rather
    /// than cache maintenance on somebody else's memory.
    #[ktest]
    fn sync_ranges_are_bounded() {
        let object = PvrObject::new(PAGE_SIZE as u64, 0).expect("object");

        assert!(object.sync_to_device(0, PAGE_SIZE).is_ok());
        assert!(object.sync_from_device(PAGE_SIZE, 0).is_ok());
        assert!(object.sync_to_device(0, PAGE_SIZE + 1).is_err());
        assert!(object.sync_to_device(PAGE_SIZE, 1).is_err());
        assert!(object.sync_to_device(usize::MAX, 1).is_err());
    }
}
