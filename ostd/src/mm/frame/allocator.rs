// SPDX-License-Identifier: MPL-2.0

//! The physical memory allocator.

use core::{alloc::Layout, ops::Range};

use align_ext::AlignExt;

use super::{Frame, meta::AnyFrameMeta, segment::Segment};
use crate::{
    boot::memory_region::MemoryRegionType,
    error::Error,
    impl_frame_meta_for,
    mm::{PAGE_SIZE, paddr_to_vaddr},
    prelude::*,
    util::ops::range_difference,
};

/// Options for allocating physical memory frames.
pub struct FrameAllocOptions {
    zeroed: bool,
    // K1 MUSE Book --- (32-bit DMA masters can only use the low bank)
    /// When set, the frames must be reachable by a 32-bit DMA master.
    dma_capable: bool,
    // K1 MUSE Book ---
}

impl Default for FrameAllocOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAllocOptions {
    /// Creates new options for allocating the specified number of frames.
    pub fn new() -> Self {
        // K1 MUSE Book --- (default is not DMA-capable; high bank first)
        // Self { zeroed: true }
        Self {
            zeroed: true,
            dma_capable: false,
        }
        // K1 MUSE Book ---
    }

    /// Sets whether the allocated frames should be initialized with zeros.
    ///
    /// If `zeroed` is `true`, the allocated frames are filled with zeros.
    /// If not, the allocated frames will contain sensitive data and the caller
    /// should clear them before sharing them with other components.
    ///
    /// By default, the frames are zero-initialized.
    pub fn zeroed(&mut self, zeroed: bool) -> &mut Self {
        self.zeroed = zeroed;
        self
    }

    // K1 MUSE Book --- (DMA allocations use alloc_below(dma_addr_limit))
    /// Requires the allocated frames to be reachable by a 32-bit DMA master.
    ///
    /// When `true`, the allocator uses [`GlobalFrameAllocator::alloc_below`]
    /// with [`crate::mm::dma_addr_limit`]. Off by default so ordinary
    /// allocations take high memory first.
    pub fn dma_capable(&mut self, dma_capable: bool) -> &mut Self {
        self.dma_capable = dma_capable;
        self
    }

    fn allocate(&self, layout: Layout) -> Option<Paddr> {
        let allocator = get_global_frame_allocator();
        if self.dma_capable {
            allocator.alloc_below(layout, crate::mm::dma_addr_limit())
        } else {
            allocator.alloc(layout)
        }
    }
    // K1 MUSE Book ---

    /// Allocates a single untyped frame without metadata.
    pub fn alloc_frame(&self) -> Result<Frame<()>> {
        self.alloc_frame_with(())
    }

    /// Allocates a single frame with additional metadata.
    pub fn alloc_frame_with<M: AnyFrameMeta>(&self, metadata: M) -> Result<Frame<M>> {
        let single_layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();
        // K1 MUSE Book --- (route through allocate() so dma_capable is honoured)
        // let frame = get_global_frame_allocator()
        //     .alloc(single_layout)
        //     .map(|paddr| Frame::from_unused(paddr, metadata).unwrap())
        //     .ok_or(Error::NoMemory)?;
        let Some(paddr) = self.allocate(single_layout) else {
            return Err(Error::NoMemory);
        };
        let frame =
            Frame::from_unused(paddr, metadata).expect("the allocator must return unused frames");
        // K1 MUSE Book ---

        if self.zeroed {
            let addr = paddr_to_vaddr(frame.paddr()) as *mut u8;
            // SAFETY: The newly allocated frame is guaranteed to be valid.
            unsafe { core::ptr::write_bytes(addr, 0, PAGE_SIZE) }
        }

        Ok(frame)
    }

    /// Allocates a contiguous range of untyped frames without metadata.
    pub fn alloc_segment(&self, nframes: usize) -> Result<Segment<()>> {
        self.alloc_segment_with(nframes, |_| ())
    }

    /// Allocates a contiguous range of frames with additional metadata.
    ///
    /// The returned [`Segment`] contains at least one frame. The method returns
    /// an error if the number of frames is zero.
    pub fn alloc_segment_with<M: AnyFrameMeta, F>(
        &self,
        nframes: usize,
        metadata_fn: F,
    ) -> Result<Segment<M>>
    where
        F: FnMut(Paddr) -> M,
    {
        if nframes == 0 {
            return Err(Error::InvalidArgs);
        }
        // K1 MUSE Book --- (reject sizes that cannot describe a valid allocation)
        // let layout = Layout::from_size_align(nframes * PAGE_SIZE, PAGE_SIZE).unwrap();
        let size = nframes.checked_mul(PAGE_SIZE).ok_or(Error::InvalidArgs)?;
        let layout = Layout::from_size_align(size, PAGE_SIZE).map_err(|_| Error::InvalidArgs)?;
        // K1 MUSE Book ---
        // K1 MUSE Book --- (route segment alloc through allocate() so dma_capable is honoured)
        // let segment = get_global_frame_allocator()
        //     .alloc(layout)
        //     .map(|start| {
        //         Segment::from_unused(start..start + nframes * PAGE_SIZE, metadata_fn).unwrap()
        //     })
        //     .ok_or(Error::NoMemory)?;
        let Some(start) = self.allocate(layout) else {
            return Err(Error::NoMemory);
        };
        let segment = Segment::from_unused(start..start + nframes * PAGE_SIZE, metadata_fn)
            .expect("the allocator must return unused frames");
        // K1 MUSE Book ---

        if self.zeroed {
            let addr = paddr_to_vaddr(segment.paddr()) as *mut u8;
            // SAFETY: The newly allocated segment is guaranteed to be valid.
            unsafe { core::ptr::write_bytes(addr, 0, nframes * PAGE_SIZE) }
        }

        Ok(segment)
    }
}

#[cfg(ktest)]
#[ktest(expect_redundant_test_prefix)]
fn test_alloc_dealloc() {
    // Here we allocate and deallocate frames in random orders to test the allocator.
    // We expect the test to fail if the underlying implementation panics.
    let single_options = FrameAllocOptions::new();
    let mut contiguous_options = FrameAllocOptions::new();
    contiguous_options.zeroed(false);
    let mut remember_vec = Vec::new();
    for _ in 0..10 {
        for i in 0..10 {
            let single_frame = single_options.alloc_frame().unwrap();
            if i % 3 == 0 {
                remember_vec.push(single_frame);
            }
        }
        let contiguous_segment = contiguous_options.alloc_segment(10).unwrap();
        drop(contiguous_segment);
        remember_vec.pop();
    }
}

// K1 MUSE Book --- (ktest: dma_capable frames stay below dma_addr_limit)
#[cfg(ktest)]
#[ktest(expect_redundant_test_prefix)]
fn test_dma_capable_stays_below_limit() {
    let limit = crate::mm::dma_addr_limit();
    let frame = FrameAllocOptions::new()
        .dma_capable(true)
        .alloc_frame()
        .expect("every platform this kernel boots has some memory below the DMA limit");
    assert!(
        frame.paddr() < limit,
        "dma_capable frame {:#x} is at or above limit {:#x}",
        frame.paddr(),
        limit
    );

    let segment = FrameAllocOptions::new()
        .dma_capable(true)
        .alloc_segment(4)
        .expect("4-page DMA segment");
    assert!(
        segment.paddr() + segment.size() <= limit,
        "dma_capable segment {:#x}..{:#x} crosses limit {:#x}",
        segment.paddr(),
        segment.paddr() + segment.size(),
        limit
    );
}
// K1 MUSE Book ---

/// The trait for the global frame allocator.
///
/// OSTD allows a customized frame allocator by the [`global_frame_allocator`]
/// attribute, which marks a static variable of this type.
///
/// The API mimics the standard Rust allocator API ([`GlobalAlloc`] and
/// [`global_allocator`]). However, this trait is much safer. Double free
/// or freeing in-use memory through this trait only messes up the allocator's
/// state rather than causing undefined behavior.
///
/// Whenever OSTD or other modules need to allocate or deallocate frames via
/// [`FrameAllocOptions`], they are forwarded to the global frame allocator.
/// It is not encouraged to call the global allocator directly.
///
/// [`global_frame_allocator`]: crate::global_frame_allocator
/// [`GlobalAlloc`]: core::alloc::GlobalAlloc
pub trait GlobalFrameAllocator: Sync {
    /// Allocates a contiguous range of frames.
    ///
    /// The caller guarantees that `layout.size()` is aligned to [`PAGE_SIZE`].
    ///
    /// When any of the allocated memory is not in use, OSTD returns them by
    /// calling [`GlobalFrameAllocator::dealloc`]. If multiple frames are
    /// allocated, they may be returned in any order with any number of calls.
    fn alloc(&self, layout: Layout) -> Option<Paddr>;

    // K1 MUSE Book --- (an unconstrained fallback would violate the device address limit)
    /// Allocates a contiguous range whose last byte is strictly below `limit`.
    ///
    /// The default is `None`, not a fallback to [`Self::alloc`]. An
    /// implementor that has not been taught about address constraints
    /// must not silently hand out high memory.
    fn alloc_below(&self, layout: Layout, limit: Paddr) -> Option<Paddr> {
        let _ = (layout, limit);
        None
    }
    // K1 MUSE Book ---

    /// Deallocates a contiguous range of frames.
    ///
    /// The caller guarantees that `addr` and `size` are both aligned to
    /// [`PAGE_SIZE`]. The deallocated memory should always be allocated by
    /// [`GlobalFrameAllocator::alloc`]. However, if
    /// [`GlobalFrameAllocator::alloc`] returns multiple frames, it is possible
    /// that some of them are deallocated before others. The deallocated memory
    /// must never overlap with any memory that is already deallocated or
    /// added, without being allocated in between.
    ///
    /// The deallocated memory can be uninitialized.
    fn dealloc(&self, addr: Paddr, size: usize);

    /// Adds a contiguous range of frames to the allocator.
    ///
    /// The memory being added must never overlap with any memory that was
    /// added before.
    ///
    /// The added memory can be uninitialized.
    fn add_free_memory(&self, addr: Paddr, size: usize);
}

unsafe extern "Rust" {
    /// The global frame allocator's reference exported by
    /// [`crate::global_frame_allocator`].
    static __GLOBAL_FRAME_ALLOCATOR_REF: &'static dyn GlobalFrameAllocator;
}

pub(super) fn get_global_frame_allocator() -> &'static dyn GlobalFrameAllocator {
    // SAFETY: The global frame allocator is set up correctly with the
    // `global_frame_allocator` attribute. If they use safe code only, the
    // up-call is safe.
    unsafe { __GLOBAL_FRAME_ALLOCATOR_REF }
}

/// Initializes the global frame allocator.
///
/// It just does adds the frames to the global frame allocator. Calling it
/// multiple times would be not safe.
///
/// # Safety
///
/// This function should be called only once.
pub(crate) unsafe fn init() {
    // Retire the early allocator.
    let early_allocator = EARLY_ALLOCATOR.lock().take().unwrap();
    let early_allocated_ranges = early_allocator.allocated_regions();

    #[cfg(all(target_arch = "x86_64", feature = "cvm_guest"))]
    let unaccepted_range = crate::if_tdx_enabled!({
        super::unaccepted::init_unaccepted_memory_state(&early_allocated_ranges)
    } else {
        0..0
    });
    #[cfg(not(all(target_arch = "x86_64", feature = "cvm_guest")))]
    let unaccepted_range: Range<Paddr> = 0..0;

    let regions = &crate::boot::EARLY_INFO.get().unwrap().memory_regions;
    let [range_1, range_2] = &early_allocated_ranges;

    for region in regions
        .iter()
        .filter(|region| region.typ() == MemoryRegionType::Usable)
    {
        debug_assert!(region.base().is_multiple_of(PAGE_SIZE));
        debug_assert!(region.len().is_multiple_of(PAGE_SIZE));

        let region_range = region.base()..region.end();
        for free_range in range_difference(&region_range, range_1)
            .flat_map(|range| range_difference(&range, range_2))
            .flat_map(|range| range_difference(&range, &unaccepted_range))
        {
            crate::info!("Adding free frames to the allocator: {:x?}", free_range);
            get_global_frame_allocator().add_free_memory(free_range.start, free_range.len());
        }
    }
}

/// An allocator in the early boot phase when frame metadata is not available.
pub(super) struct EarlyFrameAllocator {
    // We need to allocate from under 4G first since the linear mapping for
    // the higher region is not constructed yet.
    under_4g_range: Range<Paddr>,
    under_4g_end: Paddr,

    // And also sometimes 4G is not enough for early phase. This, if not `0..0`,
    // is the largest region above 4G.
    max_range: Range<Paddr>,
    max_end: Paddr,
}

/// The global frame allocator in the early boot phase.
///
/// It is used to allocate frames before the frame metadata is initialized.
/// The allocated frames are not tracked by the frame metadata. After the
/// metadata is initialized with [`super::meta::init`], the frames are tracked
/// with metadata and the early allocator is no longer used.
///
/// This is protected by the [`spin::Mutex`] rather than [`crate::sync::SpinLock`]
/// since the latter uses CPU-local storage, which isn't available in the early
/// boot phase. So we must make sure that no interrupts are enabled when using
/// this allocator.
pub(super) static EARLY_ALLOCATOR: spin::Mutex<Option<EarlyFrameAllocator>> =
    spin::Mutex::new(None);

impl EarlyFrameAllocator {
    /// Creates a new early frame allocator.
    ///
    /// It uses at most 2 regions, the first is the maximum usable region below
    /// 4 GiB. The other is the maximum usable region above 4 GiB and is only
    /// usable when linear mapping is constructed.
    pub(crate) fn new() -> Self {
        let regions = &crate::boot::EARLY_INFO.get().unwrap().memory_regions;

        let mut under_4g_range = 0..0;
        let mut max_range = 0..0;
        for region in regions.iter() {
            if region.typ() != MemoryRegionType::Usable {
                continue;
            }
            const PADDR4G: Paddr = 0x1_0000_0000;
            if region.base() < PADDR4G {
                let range = region.base()..region.end().min(PADDR4G);
                if range.len() > under_4g_range.len() {
                    under_4g_range = range;
                }
            }
            if region.end() >= PADDR4G {
                let range = region.base().max(PADDR4G)..region.end();
                if range.len() > max_range.len() {
                    max_range = range;
                }
            }
        }

        crate::debug!(
            "Early frame allocator (below 4G) at: {:#x?}",
            under_4g_range
        );
        if !max_range.is_empty() {
            crate::debug!("Early frame allocator (above 4G) at: {:#x?}", max_range);
        }

        Self {
            under_4g_range: under_4g_range.clone(),
            under_4g_end: under_4g_range.start,
            max_range: max_range.clone(),
            max_end: max_range.start,
        }
    }

    /// Allocates a contiguous range of frames.
    pub(super) fn alloc(&mut self, layout: Layout) -> Option<Paddr> {
        let size = layout.size().align_up(PAGE_SIZE);
        let align = layout.align().max(PAGE_SIZE);

        for (tail, end) in [
            (&mut self.under_4g_end, self.under_4g_range.end),
            (&mut self.max_end, self.max_range.end),
        ] {
            let allocated = tail.align_up(align);
            if let Some(allocated_end) = allocated.checked_add(size)
                && allocated_end <= end
            {
                *tail = allocated_end;
                return Some(allocated);
            }
        }

        None
    }

    pub(super) fn allocated_regions(&self) -> [Range<Paddr>; 2] {
        [
            self.under_4g_range.start..self.under_4g_end,
            self.max_range.start..self.max_end,
        ]
    }
}

/// Metadata for frames allocated in the early boot phase.
///
/// Frames allocated with [`early_alloc`] are not immediately tracked with
/// frame metadata. But [`super::meta::init`] will track them later.
#[derive(Debug)]
pub(crate) struct EarlyAllocatedFrameMeta;

impl_frame_meta_for!(EarlyAllocatedFrameMeta);

/// Allocates a contiguous range of frames in the early boot phase.
///
/// The early allocated frames will not be reclaimable, until the metadata is
/// initialized by [`super::meta::init`]. Then we can use [`Frame::from_raw`]
/// to free the frames.
///
/// # Panics
///
/// This function panics if:
///  - it is called before [`init_early_allocator`],
///  - or if is called after [`init`].
pub(crate) fn early_alloc(layout: Layout) -> Option<Paddr> {
    let mut early_allocator = EARLY_ALLOCATOR.lock();
    let paddr = early_allocator.as_mut().unwrap().alloc(layout)?;

    #[cfg(all(target_arch = "x86_64", feature = "cvm_guest"))]
    crate::if_tdx_enabled!({
        // SAFETY: The early allocator only exists in the boot context of the BSP before the APs
        // boot up. Therefore, there are no other concurrent operations that accept pages.
        unsafe { super::unaccepted::accept_early_allocated_range(paddr, layout.size()) };
    });

    Some(paddr)
}

/// Initializes the early frame allocator.
///
/// [`early_alloc`] should be used after this initialization. After [`init`], the
/// early allocator.
///
/// # Safety
///
/// This function should be called only once after the memory regions are ready.
pub(crate) unsafe fn init_early_allocator() {
    let mut early_allocator = EARLY_ALLOCATOR.lock();
    *early_allocator = Some(EarlyFrameAllocator::new());
}
