// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Buffer objects backed by ordinary system memory.

use alloc::sync::Arc;

use ostd::mm::{FrameAllocOptions, HasSize, PAGE_SIZE, Segment, UFrame, dma::DmaStream};

use super::{BufferUse, GemObject};
use crate::error::{DrmError, DrmResult};

/// A GEM object backed by ordinary system memory.
///
/// Covers dumb buffers and anything else that does not need device-private
/// memory.
///
/// # Why the pages are allocated up front and contiguously
///
/// Linux's `drm_gem_shmem_helper` populates a shmem object page by page on
/// first touch, and its pages are scattered. That is right for a GPU behind an
/// IOMMU, and wrong here for two reasons:
///
///  - the K1 display engine fetches scanout lines by physical address with no
///    IOMMU in front of it, so a buffer it will ever scan out has to be one
///    physically contiguous run. Simple KMS drivers upstream take the same
///    route through `drm_gem_dma_helper`;
///  - [`fault`](GemObject::fault) runs with preemption disabled and the page
///    table lock held. Having every page already allocated makes it a pure
///    lookup: no allocation, no lock, and no way for it to fail late and leave
///    a half-populated buffer. Demand paging is still what the *mapping* does;
///    it is only the physical memory that is reserved eagerly.
#[derive(Debug)]
pub struct ShmemObject {
    segment: Segment<()>,
    buffer_use: BufferUse,
    /// Streaming-DMA view of `segment`, for cache maintenance against the
    /// kernel's cached alias. `None` for [`BufferUse::CpuOnly`], which no
    /// device ever reads.
    dma: Option<DmaStream>,
}

impl ShmemObject {
    /// Allocates a new object of `size` bytes, rounded up to a page.
    ///
    /// The memory is zeroed, which is not optional: these frames go straight
    /// into a user address space, so whatever the previous owner left in them
    /// would be readable by an unprivileged client.
    ///
    /// # Errors
    ///
    /// [`DrmError::InvalidArgument`] for a zero or unrepresentable size, and
    /// [`DrmError::OutOfMemory`] if no contiguous run that large is free.
    pub fn new(size: usize, buffer_use: BufferUse) -> DrmResult<Arc<Self>> {
        let size = size
            .checked_next_multiple_of(PAGE_SIZE)
            .filter(|size| *size > 0)
            .ok_or(DrmError::InvalidArgument)?;

        // Scanout is programmed into the DPU as a physical address with
        // no IOMMU, so scanout buffers must fit its 32-bit address range.
        // Render / CPU-only objects use the GPU MMU, whose wider address
        // fields can reach high memory.
        //
        // Foreign GPU import must retain the DMA owner and reject a bounced
        // mapping when the GPU requires the object's original physical pages.
        let mut options = FrameAllocOptions::new();
        options.zeroed(true);
        if buffer_use == BufferUse::Scanout {
            options.dma_capable(true);
        }
        let segment = options
            .alloc_segment(size / PAGE_SIZE)
            .map_err(|_| DrmError::OutOfMemory)?;

        // The zeroes above went through the kernel's *cached* alias of the
        // frames. Every later writer of a Scanout/Render object bypasses the
        // cache -- user space through a write-combining mapping
        // (`BufferUse::cache_policy`), the DPU straight from DRAM -- so the
        // dirty zero lines in L1/L2 can overwrite newer client pixels when
        // evicted, because client writes bypass that cached alias.
        //
        // Write the zeroes back and drop the lines (`cbo.flush`: the stream
        // is bidirectional) before anyone else can touch the memory. This is
        // what `pvr::mm::gem::PvrObject::new` already does for render
        // objects. A CPU-only object keeps its cached mapping and needs
        // nothing.
        //
        let dma = if buffer_use == BufferUse::CpuOnly {
            None
        } else {
            let dma =
                DmaStream::map(segment.clone().into(), false).map_err(|_| DrmError::OutOfMemory)?;
            dma.sync_to_device(0..size)
                .map_err(|_| DrmError::OutOfMemory)?;
            Some(dma)
        };

        crate::live::note_gem_alloc(size / PAGE_SIZE);
        Ok(Arc::new(Self {
            segment,
            buffer_use,
            dma,
        }))
    }
}

impl ShmemObject {
    /// Makes writes the kernel made through its cached alias visible to the
    /// device (`cbo.flush` over `range`). A no-op for CPU-only objects.
    pub fn sync_to_device(&self, range: core::ops::Range<usize>) -> DrmResult<()> {
        match &self.dma {
            Some(dma) => dma
                .sync_to_device(range)
                .map_err(|_| DrmError::InvalidArgument),
            None => Ok(()),
        }
    }

    /// Drops stale cache lines so the kernel's next read through its cached
    /// alias sees what the device (or an uncached user mapping) wrote. A
    /// no-op for CPU-only objects.
    pub fn sync_from_device(&self, range: core::ops::Range<usize>) -> DrmResult<()> {
        match &self.dma {
            Some(dma) => dma
                .sync_from_device(range)
                .map_err(|_| DrmError::InvalidArgument),
            None => Ok(()),
        }
    }
}

impl Drop for ShmemObject {
    fn drop(&mut self) {
        crate::live::note_gem_free(self.segment.size() / PAGE_SIZE);
    }
}

impl GemObject for ShmemObject {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn size(&self) -> usize {
        self.segment.size()
    }

    fn buffer_use(&self) -> BufferUse {
        self.buffer_use
    }

    fn segment(&self) -> Option<&Segment<()>> {
        Some(&self.segment)
    }

    fn prepare_cpu_read(&self, pgoff: usize) -> DrmResult<()> {
        let offset = pgoff
            .checked_mul(PAGE_SIZE)
            .filter(|offset| *offset < self.segment.size())
            .ok_or(DrmError::BadAddress)?;
        self.sync_from_device(offset..offset + PAGE_SIZE)
    }

    fn fault(&self, pgoff: usize) -> DrmResult<UFrame> {
        let offset = pgoff
            .checked_mul(PAGE_SIZE)
            .filter(|offset| *offset < self.segment.size())
            .ok_or(DrmError::BadAddress)?;

        // Slicing takes a reference on the page and handing the frame out
        // transfers it to the caller, so the object's own reference — the one
        // the segment holds — is untouched. That is what lets the page outlive
        // every mapping of it.
        let frame = self
            .segment
            .slice(&(offset..offset + PAGE_SIZE))
            .into_iter()
            .next()
            .expect("a one-page slice of a segment yields exactly one frame");

        Ok(frame.into())
    }
}
