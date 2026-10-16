// SPDX-License-Identifier: MPL-2.0

//! Direct Memory Access (DMA).
//!
//! This module provides [`DmaCoherent`] and [`DmaStream`] abstractions for
//! managing DMA memory regions with different remapping, caching and
//! synchronization requirements.
//!
//! # Usage in IRQs
//!
//! Creating DMA objects (via `alloc`, `alloc_uninit`, or `map` constructors)
//! requires IRQs to be enabled, to avoid deadlocks during cross-CPU TLB
//! flushes. Note that it means DMA objects cannot be created from (hard)
//! interrupt context.
//!
//! Other operations on DMA objects may still be performed in any context,
//! even with IRQs disabled. For example, it is valid to drop a [`DmaStream`]
//! from an IRQ handler after the device has finished processing it.

#[cfg(ktest)]
mod test;

mod dma_coherent;
mod dma_stream;
mod util;

pub use dma_coherent::DmaCoherent;
pub use dma_stream::{DmaDirection, DmaStream, FromAndToDevice, FromDevice, ToDevice};

// K1 MUSE Book --- (name a DMA address the device cannot reach; used by KMS/eDP)
use crate::mm::Paddr;

/// Logs if a device-visible physical range is at or above [`crate::mm::dma_addr_limit`].
///
/// Kept as a public hook for display/GPU callers. Rate-limited so a hot I/O
/// path cannot stall the UART.
pub fn warn_if_addr_unreachable(paddr: Paddr, size: usize) {
    let limit = crate::mm::dma_addr_limit();
    let end = paddr.saturating_add(size);
    if paddr < limit && end <= limit {
        return;
    }

    static HITS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
    let n = HITS
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    if n > 4 && !n.is_multiple_of(64) {
        return;
    }

    crate::warn!(
        "DMA address {:#x}..{:#x} is at or above the platform limit {:#x} \
         (a device may not reach this memory). count={}",
        paddr,
        end,
        limit,
        n
    );
}
// K1 MUSE Book ---

// K1 MUSE Book --- (explicit CPU-memory/MMIO ordering at device handoff)
/// Orders this CPU's earlier memory and MMIO accesses before later accesses.
///
/// Drivers use this after preparing DMA data and before publishing an MMIO
/// doorbell, including when the CPU mapping is non-cacheable. Unlike an atomic
/// or compiler fence, it includes the architecture's device-I/O domain.
///
/// This does not clean or invalidate CPU caches, wait for a posted MMIO write
/// to reach its device, or establish that DMA has completed. Those operations
/// remain part of the driver's cache and device-lifetime protocols.
pub fn dma_io_fence() {
    crate::arch::mm::dma_io_fence()
}
// K1 MUSE Book ---
