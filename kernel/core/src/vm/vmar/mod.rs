// SPDX-License-Identifier: MPL-2.0

//! User address space management.

mod handle;
mod interval_set;
mod rmap;
mod util;
mod vm_mapping;

mod vmar_impls;
// K1 MUSE Book --- (demand-paged GEM/device mmap used by Mesa/COSMIC)
pub(crate) mod on_demand;
// K1 MUSE Book ---

use ostd::mm::Vaddr;

// K1 MUSE Book --- (SIGBUS for a refused device page)
pub(crate) use self::vmar_impls::page_fault::{PageFaultFailure, PageFaultSignal};
// K1 MUSE Book ---
pub(crate) use self::{
    handle::VmarHandle,
    rmap::{Rmap, RmapEntry},
    vmar_impls::{
        RssType, Vmar,
        map::{MmapMode, VmarMapOffset},
        page_fault::PageFaultInfo,
        remap::RemapOldMappingAction,
    },
};
pub use self::{vm_mapping::MapHandle, vmar_impls::map::FileMmapRequest};

pub(crate) const VMAR_LOWEST_ADDR: Vaddr = 0x001_0000; // 64 KiB is the Linux configurable default
pub(crate) const VMAR_CAP_ADDR: Vaddr = ostd::mm::MAX_USERSPACE_VADDR;

/// Returns whether the input `vaddr` is a legal user space virtual address.
pub(crate) fn is_userspace_vaddr(vaddr: Vaddr) -> bool {
    (VMAR_LOWEST_ADDR..VMAR_CAP_ADDR).contains(&vaddr)
}

/// Returns whether `vaddr` and `len` specify a legal user space virtual address range.
fn is_userspace_vaddr_range(vaddr: Vaddr, len: usize) -> bool {
    vaddr >= VMAR_LOWEST_ADDR
        && VMAR_CAP_ADDR
            .checked_sub(vaddr)
            .is_some_and(|gap| gap >= len)
}
