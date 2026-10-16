// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: typed GPU and firmware memory management.

//! GPU buffer and address-space ownership.
//!
//! Native GEM backing is allocated eagerly; CPU mappings fault in those existing
//! frames. GPU virtual mappings retain the objects they address, independently of
//! userspace handle lifetime.
//!
//! K1 is not DMA coherent. Cached kernel access uses explicit DMA synchronization;
//! CPU user mappings use the cache policy chosen by the GEM implementation. The
//! `BYPASS_DEVICE_CACHE` flag controls GPU caching, not CPU cache maintenance.

pub mod gem;
pub mod mmu;
pub mod vm;
pub mod vm_mips;
