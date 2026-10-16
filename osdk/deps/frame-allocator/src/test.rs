// SPDX-License-Identifier: MPL-2.0

//! Providing test utilities and high-level test cases for the frame allocator.

use core::alloc::Layout;

use ostd::{
    mm::{
        FrameAllocOptions, HasPaddr, PAGE_SIZE, Paddr, Segment, UniqueFrame,
        frame::GlobalFrameAllocator,
    },
    prelude::ktest,
};

use super::FrameAllocator;

#[ktest]
fn frame_allocator_alloc_layout_match() {
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap());
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 2, PAGE_SIZE).unwrap());
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 3, PAGE_SIZE).unwrap());
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 4, PAGE_SIZE).unwrap());

    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 2, PAGE_SIZE * 2).unwrap());
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 4, PAGE_SIZE * 4).unwrap());
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 8, PAGE_SIZE * 8).unwrap());
    assert_allocation_well_formed(Layout::from_size_align(PAGE_SIZE * 16, PAGE_SIZE * 16).unwrap());
}

#[track_caller]
fn assert_allocation_well_formed(layout: Layout) {
    let instance = FrameAllocator;

    let allocated = instance.alloc(layout).unwrap();
    assert_eq!(
        allocated % layout.align(),
        0,
        "Allocation alignment mismatch"
    );

    for offset in (0..layout.size()).step_by(PAGE_SIZE) {
        let frame = allocated + offset;
        let frame = UniqueFrame::from_unused(frame, ()).unwrap_or_else(|e| {
            panic!(
                "Metadata not well-formed after allocation at offset {:#x}: {:#?}",
                offset, e
            )
        });
        frame.reset_as_unused();
    }

    instance.add_free_memory(allocated, layout.size());
}

// K1 MUSE Book --- (DMA/normal zone policy)
#[ktest]
fn split_at_dma_limit_partitions_a_range() {
    use crate::pools::split_at_dma_limit;

    let limit = 16 * PAGE_SIZE;
    assert_eq!(split_at_dma_limit(0, 0, limit), (None, None));
    assert_eq!(
        split_at_dma_limit(0, 8 * PAGE_SIZE, limit),
        (Some((0, 8 * PAGE_SIZE)), None)
    );
    assert_eq!(
        split_at_dma_limit(limit, 8 * PAGE_SIZE, limit),
        (None, Some((limit, 8 * PAGE_SIZE)))
    );
    assert_eq!(
        split_at_dma_limit(8 * PAGE_SIZE, 16 * PAGE_SIZE, limit),
        (
            Some((8 * PAGE_SIZE, 8 * PAGE_SIZE)),
            Some((limit, 8 * PAGE_SIZE))
        )
    );
}

#[ktest]
fn alloc_below_never_returns_address_at_or_above_limit() {
    let limit = ostd::mm::dma_addr_limit();
    let instance = FrameAllocator;
    let layout = Layout::from_size_align(PAGE_SIZE, PAGE_SIZE).unwrap();

    let mut addrs = alloc::vec::Vec::new();
    for _ in 0..8 {
        let Some(paddr) = instance.alloc_below(layout, limit) else {
            break;
        };
        assert!(
            paddr < limit,
            "alloc_below returned {:#x} >= limit {:#x}",
            paddr,
            limit
        );
        assert!(
            paddr.saturating_add(layout.size()) <= limit,
            "alloc_below range {:#x}..{:#x} crosses limit {:#x}",
            paddr,
            paddr + layout.size(),
            limit
        );
        addrs.push(paddr);
    }
    assert!(
        !addrs.is_empty(),
        "alloc_below must succeed at least once: every platform this kernel \
         boots has some memory below the DMA limit"
    );
    for paddr in addrs {
        instance.add_free_memory(paddr, PAGE_SIZE);
    }
}
// K1 MUSE Book ---

/// A mocked memory region for testing.
///
/// All the frames in the returned memory region will be marked as used.
/// When the region is dropped, all the frames will be returned to the global
/// frame allocator. If any frame is not unused by that time, the drop will panic.
pub(crate) struct MockMemoryRegion {
    addr: Paddr,
    size: usize,
}

impl MockMemoryRegion {
    /// Gets a memory region for testing.
    pub(crate) fn alloc(size: usize) -> Self {
        let seg = FrameAllocOptions::new()
            .alloc_segment(size / PAGE_SIZE)
            .unwrap();
        let addr = seg.paddr();
        for frame in seg {
            UniqueFrame::try_from(frame).unwrap().reset_as_unused();
        }
        Self { addr, size }
    }

    /// Gets the start address of the memory region.
    pub(crate) fn paddr(&self) -> Paddr {
        self.addr
    }
}

impl Drop for MockMemoryRegion {
    fn drop(&mut self) {
        let seg = Segment::from_unused(self.addr..self.addr + self.size, |_| ()).unwrap();
        drop(seg);
    }
}
