// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book --- (two buddy pools split at dma_addr_limit: ZONE_DMA32 / ZONE_NORMAL)
//! Buddy pools, split at the platform DMA address limit.
//!
//! Ordinary allocations prefer the normal pool, preserving low-address RAM
//! for devices with 32-bit DMA addresses. Only registered free extents enter
//! either pool; splitting does not create RAM in the K1's MMIO hole.

mod balancing;

use core::{
    alloc::Layout,
    cell::RefCell,
    ops::DerefMut,
    sync::atomic::{AtomicUsize, Ordering},
};

use ostd::{
    cpu_local,
    irq::DisabledLocalIrqGuard,
    // mm::Paddr,
    mm::{Paddr, dma_addr_limit},
    sync::{LocalIrqDisabled, SpinLock, SpinLockGuard},
};

use crate::chunk::{BuddyOrder, greater_order_of, lesser_order_of, size_of_order, split_to_chunks};

use super::set::BuddySet;

// static GLOBAL_POOL: SpinLock<BuddySet<MAX_BUDDY_ORDER>, LocalIrqDisabled> =
//     SpinLock::new(BuddySet::new_empty());
// static GLOBAL_POOL_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Free frames whose start address is below [`dma_addr_limit`].
static GLOBAL_DMA_POOL: SpinLock<BuddySet<MAX_BUDDY_ORDER>, LocalIrqDisabled> =
    SpinLock::new(BuddySet::new_empty());
static GLOBAL_DMA_POOL_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Free frames whose start address is at or above [`dma_addr_limit`].
static GLOBAL_NORMAL_POOL: SpinLock<BuddySet<MAX_BUDDY_ORDER>, LocalIrqDisabled> =
    SpinLock::new(BuddySet::new_empty());
static GLOBAL_NORMAL_POOL_SIZE: AtomicUsize = AtomicUsize::new(0);

// CPU-local free buddies for the normal zone only.
cpu_local! {
    static LOCAL_POOL: RefCell<BuddySet<MAX_LOCAL_BUDDY_ORDER>> = RefCell::new(BuddySet::new_empty());
}

/// Maximum supported order of the buddy system.
///
/// i.e., it is the number of classes of free blocks. It determines the
/// maximum size of each allocation.
///
/// A maximum buddy order of 32 supports up to 4KiB*2^31 = 8 TiB of chunks.
const MAX_BUDDY_ORDER: BuddyOrder = 32;

/// Maximum supported order of the buddy system for CPU-local buddy system.
///
/// Since large blocks are rarely allocated, caching such blocks will lead
/// to much fragmentation.
///
/// Lock guards are also allocated on stack. We can limit the stack usage
/// for common paths in this way.
///
/// A maximum local buddy order of 18 supports up to 4KiB*2^17 = 512 MiB of
/// chunks.
const MAX_LOCAL_BUDDY_ORDER: BuddyOrder = 18;

/// A physical-memory extent represented by its start address and byte length.
type MemoryExtent = (Paddr, usize);

/// Splits `[addr, addr+size)` at `limit` so each piece lies on one side.
///
/// `limit` is exclusive: the first `Option` is the DMA piece (`addr < limit`),
/// the second is the normal piece (`addr >= limit`).
pub(crate) fn split_at_dma_limit(
    addr: Paddr,
    size: usize,
    limit: Paddr,
) -> (Option<MemoryExtent>, Option<MemoryExtent>) {
    if size == 0 {
        return (None, None);
    }
    let end = addr.saturating_add(size);
    if end <= limit {
        (Some((addr, size)), None)
    } else if addr >= limit {
        (None, Some((addr, size)))
    } else {
        (Some((addr, limit - addr)), Some((limit, end - limit)))
    }
}

// Original upstream implementation.
// pub(super) fn alloc(guard: &DisabledLocalIrqGuard, layout: Layout) -> Option<Paddr> {
//     let local_pool_cell = LOCAL_POOL.get_with(guard);
//     let mut local_pool = local_pool_cell.borrow_mut();
//     let mut global_pool = OnDemandGlobalLock::new();
//
//     let size_order = greater_order_of(layout.size());
//     let align_order = greater_order_of(layout.align());
//     let order = size_order.max(align_order);
//
//     let mut chunk_addr = None;
//
//     if order < MAX_LOCAL_BUDDY_ORDER {
//         chunk_addr = local_pool.alloc_chunk(order);
//     }
//
//     // Fall back to the global free lists if the local free lists are empty.
//     if chunk_addr.is_none() {
//         chunk_addr = global_pool.get().alloc_chunk(order);
//     }
//     // TODO: On memory pressure the global pool may be not enough. We may need
//     // to merge all buddy chunks from the local pools to the global pool and
//     // try again.
//
//     // If the alignment order is larger than the size order, we need to split
//     // the chunk and return the rest part back to the free lists.
//     let allocated_size = size_of_order(order);
//     if allocated_size > layout.size()
//         && let Some(chunk_addr) = chunk_addr
//     {
//         do_dealloc(
//             &mut local_pool,
//             &mut global_pool,
//             [(chunk_addr + layout.size(), allocated_size - layout.size())].into_iter(),
//         );
//     }
//
//     balancing::balance(local_pool.deref_mut(), &mut global_pool);
//
//     chunk_addr
// }
pub(super) fn alloc(guard: &DisabledLocalIrqGuard, layout: Layout) -> Option<Paddr> {
    let local_pool_cell = LOCAL_POOL.get_with(guard);
    let mut local_pool = local_pool_cell.borrow_mut();
    {
        let mut global_normal = OnDemandGlobalLock::normal();
        if let Some(addr) = try_alloc(Some(local_pool.deref_mut()), &mut global_normal, layout) {
            balancing::balance(local_pool.deref_mut(), &mut global_normal);
            return Some(addr);
        }
    }
    // High zone empty: fall back to the DMA bank so a machine with no
    // high memory (typical QEMU virt) still allocates.
    let mut global_dma = OnDemandGlobalLock::dma();
    try_alloc(None, &mut global_dma, layout)
}

pub(super) fn alloc_below(
    _guard: &DisabledLocalIrqGuard,
    layout: Layout,
    limit: Paddr,
) -> Option<Paddr> {
    let mut global_dma = OnDemandGlobalLock::dma();
    let addr = try_alloc(None, &mut global_dma, layout)?;
    if addr.saturating_add(layout.size()) > limit {
        split_to_chunks(addr, layout.size()).for_each(|(addr, order)| {
            global_dma.get().insert_chunk(addr, order);
        });
        return None;
    }
    Some(addr)
}

// Original upstream implementation.
// pub(super) fn dealloc(
//     guard: &DisabledLocalIrqGuard,
//     segments: impl Iterator<Item = (Paddr, usize)>,
// ) {
//     let local_pool_cell = LOCAL_POOL.get_with(guard);
//     let mut local_pool = local_pool_cell.borrow_mut();
//     let mut global_pool = OnDemandGlobalLock::new();
//
//     do_dealloc(&mut local_pool, &mut global_pool, segments);
//
//     balancing::balance(local_pool.deref_mut(), &mut global_pool);
// }
pub(super) fn dealloc(
    guard: &DisabledLocalIrqGuard,
    segments: impl Iterator<Item = (Paddr, usize)>,
) {
    let local_pool_cell = LOCAL_POOL.get_with(guard);
    let mut local_pool = local_pool_cell.borrow_mut();
    let mut global_dma = OnDemandGlobalLock::dma();
    let _ = global_dma.get();
    let mut global_normal = OnDemandGlobalLock::normal();
    let limit = dma_addr_limit();
    let mut touched_normal = false;

    segments.for_each(|(addr, size)| {
        split_to_chunks(addr, size).for_each(|(addr, order)| {
            if addr < limit {
                global_dma.get().insert_chunk(addr, order);
            } else {
                touched_normal = true;
                if order >= MAX_LOCAL_BUDDY_ORDER {
                    global_normal.get().insert_chunk(addr, order);
                } else {
                    local_pool.insert_chunk(addr, order);
                }
            }
        });
    });

    if touched_normal {
        balancing::balance(local_pool.deref_mut(), &mut global_normal);
    }
}

pub(super) fn dealloc_dma(
    _guard: &DisabledLocalIrqGuard,
    segments: impl Iterator<Item = (Paddr, usize)>,
) {
    let mut global_dma = OnDemandGlobalLock::dma();
    let limit = dma_addr_limit();
    segments.for_each(|(addr, size)| {
        debug_assert!(
            addr < limit,
            "dealloc_dma of {:#x} which is at or above the DMA limit {:#x}",
            addr,
            limit
        );
        split_to_chunks(addr, size).for_each(|(addr, order)| {
            global_dma.get().insert_chunk(addr, order);
        });
    });
}

// Original upstream implementation.
// pub(super) fn add_free_memory(_guard: &DisabledLocalIrqGuard, addr: Paddr, size: usize) {
//     let mut global_pool = OnDemandGlobalLock::new();
//
//     split_to_chunks(addr, size).for_each(|(addr, order)| {
//         global_pool.get().insert_chunk(addr, order);
//     });
// }
pub(super) fn add_free_memory(_guard: &DisabledLocalIrqGuard, addr: Paddr, size: usize) {
    let (low, high) = split_at_dma_limit(addr, size, dma_addr_limit());
    if let Some((addr, size)) = low {
        let mut global_dma = OnDemandGlobalLock::dma();
        split_to_chunks(addr, size).for_each(|(addr, order)| {
            global_dma.get().insert_chunk(addr, order);
        });
    }
    if let Some((addr, size)) = high {
        let mut global_normal = OnDemandGlobalLock::normal();
        split_to_chunks(addr, size).for_each(|(addr, order)| {
            global_normal.get().insert_chunk(addr, order);
        });
    }
}

fn try_alloc(
    mut local: Option<&mut BuddySet<MAX_LOCAL_BUDDY_ORDER>>,
    global: &mut OnDemandGlobalLock,
    layout: Layout,
) -> Option<Paddr> {
    let size_order = greater_order_of(layout.size());
    let align_order = greater_order_of(layout.align());
    let order = size_order.max(align_order);

    let mut chunk_addr = None;

    if order < MAX_LOCAL_BUDDY_ORDER
        && let Some(ref mut local) = local
    {
        chunk_addr = local.alloc_chunk(order);
    }

    if chunk_addr.is_none() {
        chunk_addr = global.get().alloc_chunk(order);
    }

    let chunk_addr = chunk_addr?;

    let allocated_size = size_of_order(order);
    if allocated_size > layout.size() {
        let leftover_addr = chunk_addr + layout.size();
        let leftover_size = allocated_size - layout.size();
        if let Some(local) = local {
            do_dealloc(local, global, [(leftover_addr, leftover_size)].into_iter());
        } else {
            split_to_chunks(leftover_addr, leftover_size).for_each(|(addr, order)| {
                global.get().insert_chunk(addr, order);
            });
        }
    }

    Some(chunk_addr)
}

fn do_dealloc(
    local_pool: &mut BuddySet<MAX_LOCAL_BUDDY_ORDER>,
    global_pool: &mut OnDemandGlobalLock,
    segments: impl Iterator<Item = (Paddr, usize)>,
) {
    segments.for_each(|(addr, size)| {
        split_to_chunks(addr, size).for_each(|(addr, order)| {
            if order >= MAX_LOCAL_BUDDY_ORDER {
                global_pool.get().insert_chunk(addr, order);
            } else {
                local_pool.insert_chunk(addr, order);
            }
        });
    });
}

type GlobalLockGuard = SpinLockGuard<'static, BuddySet<MAX_BUDDY_ORDER>, LocalIrqDisabled>;

// Original upstream global-pool implementation.
// /// An on-demand guard that locks the global pool when needed.
// ///
// /// It helps to avoid unnecessarily locking the global pool, and also avoids
// /// repeatedly locking the global pool when it is needed multiple times.
// struct OnDemandGlobalLock {
//     guard: Option<GlobalLockGuard>,
// }
//
// impl OnDemandGlobalLock {
//     fn new() -> Self {
//         Self { guard: None }
//     }
//
//     fn get(&mut self) -> &mut GlobalLockGuard {
//         self.guard.get_or_insert_with(|| GLOBAL_POOL.lock())
//     }
//
//     /// Returns the size of the global pool.
//     ///
//     /// If the global pool is locked, returns the actual size of the global pool.
//     /// Otherwise, returns the last snapshot of the global pool size by loading
//     /// [`GLOBAL_POOL_SIZE`].
//     fn get_global_size(&self) -> usize {
//         if let Some(guard) = self.guard.as_ref() {
//             guard.total_size()
//         } else {
//             GLOBAL_POOL_SIZE.load(Ordering::Relaxed)
//         }
//     }
// }
//
// impl Drop for OnDemandGlobalLock {
//     fn drop(&mut self) {
//         // Updates [`GLOBAL_POOL_SIZE`] if the global pool is locked.
//         if let Some(guard) = self.guard.as_ref() {
//             GLOBAL_POOL_SIZE.store(guard.total_size(), Ordering::Relaxed);
//         }
//     }
// }

enum PoolKind {
    Dma,
    Normal,
}

/// An on-demand guard that locks one global pool when needed.
struct OnDemandGlobalLock {
    kind: PoolKind,
    guard: Option<GlobalLockGuard>,
}

impl OnDemandGlobalLock {
    fn dma() -> Self {
        Self {
            kind: PoolKind::Dma,
            guard: None,
        }
    }

    fn normal() -> Self {
        Self {
            kind: PoolKind::Normal,
            guard: None,
        }
    }

    fn get(&mut self) -> &mut GlobalLockGuard {
        match self.kind {
            PoolKind::Dma => self.guard.get_or_insert_with(|| GLOBAL_DMA_POOL.lock()),
            PoolKind::Normal => self.guard.get_or_insert_with(|| GLOBAL_NORMAL_POOL.lock()),
        }
    }

    /// Returns the size of the selected global pool.
    fn get_global_size(&self) -> usize {
        if let Some(guard) = self.guard.as_ref() {
            guard.total_size()
        } else {
            match self.kind {
                PoolKind::Dma => GLOBAL_DMA_POOL_SIZE.load(Ordering::Relaxed),
                PoolKind::Normal => GLOBAL_NORMAL_POOL_SIZE.load(Ordering::Relaxed),
            }
        }
    }
}

impl Drop for OnDemandGlobalLock {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.as_ref() {
            let size = guard.total_size();
            match self.kind {
                PoolKind::Dma => GLOBAL_DMA_POOL_SIZE.store(size, Ordering::Relaxed),
                PoolKind::Normal => GLOBAL_NORMAL_POOL_SIZE.store(size, Ordering::Relaxed),
            }
        }
    }
}
// K1 MUSE Book ---
