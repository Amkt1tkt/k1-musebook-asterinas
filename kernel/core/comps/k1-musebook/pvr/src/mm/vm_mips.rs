// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: MIPS firmware page tables with explicit DMA synchronization.

//! Firmware processor page tables for client address spaces.
//!
//! The MIPS firmware uses a flat 16 MiB window: 4096 entries at four bytes each,
//! occupying four 4 KiB pages. Each entry translates a firmware virtual page to a
//! physical page. Writes synchronize the changed entry before firmware can use it.
//! Owners must remove their mappings before releasing the backing allocation.

use alloc::sync::Weak;
#[cfg(any(test, ktest))]
use core::sync::atomic::{AtomicU64, Ordering};

use ostd::{
    mm::{Daddr, HasDaddr, PAGE_SIZE, dma::DmaStream},
    sync::SpinLock,
};

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwHeapInfo, FwObject, GPU_CACHE_COHERENT},
    rogue::mips::{
        ROGUE_MIPSFW_CACHED_POLICY, ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT,
        ROGUE_MIPSFW_ENTRYLO_CACHE_POLICY_SHIFT, ROGUE_MIPSFW_ENTRYLO_DIRTY_EN,
        ROGUE_MIPSFW_ENTRYLO_GLOBAL_EN, ROGUE_MIPSFW_ENTRYLO_PFN_MASK,
        ROGUE_MIPSFW_ENTRYLO_PFN_MASK_ABOVE_32BIT, ROGUE_MIPSFW_ENTRYLO_PFN_SHIFT,
        ROGUE_MIPSFW_ENTRYLO_READ_INHIBIT_EN, ROGUE_MIPSFW_ENTRYLO_VALID_EN,
        ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K, ROGUE_MIPSFW_LOG2_PTE_ENTRY_SIZE,
        ROGUE_MIPSFW_MAX_NUM_PAGETABLE_PAGES, ROGUE_MIPSFW_PAGE_MASK_4K,
        ROGUE_MIPSFW_UNCACHED_CACHE_POLICY,
    },
};

/// Host pages backing the table.
///
/// Upstream `PVR_MIPS_PT_PAGE_COUNT`, which is
/// `DIV_ROUND_UP(ROGUE_MIPSFW_MAX_NUM_PAGETABLE_PAGES * ROGUE_MIPSFW_PAGE_SIZE_4K, PAGE_SIZE)`.
/// Always allocated in full, whatever the heap size, because the firmware's boot
/// data has a fixed-length array of their physical addresses.
pub const PT_PAGE_COUNT: usize =
    ROGUE_MIPSFW_MAX_NUM_PAGETABLE_PAGES as usize * ROGUE_MIPSFW_PAGE_SIZE_4K_USIZE / PAGE_SIZE;

/// [`crate::rogue::mips::ROGUE_MIPSFW_PAGE_SIZE_4K`] as a `usize`.
const ROGUE_MIPSFW_PAGE_SIZE_4K_USIZE: usize =
    crate::rogue::mips::ROGUE_MIPSFW_PAGE_SIZE_4K as usize;

/// Bytes per table entry. `1 << ROGUE_MIPSFW_LOG2_PTE_ENTRY_SIZE`.
pub const PTE_SIZE: usize = 1 << ROGUE_MIPSFW_LOG2_PTE_ENTRY_SIZE;

/// The firmware processor's page table over the firmware heap.
///
/// Ports upstream `struct pvr_fw_mips_data`'s table half: `pt_pages`, `pt`,
/// `pt_dma_addr`, `pfn_mask` and `cache_policy`.
///
/// Every method takes `&self`: the table is written through a
/// [`DmaStream`], which is interior-mutable, and a spin lock serialises the
/// read-modify-write of an entry so that two concurrent maps cannot interleave.
/// That is stricter than upstream, which relies on `WRITE_ONCE` plus the caller
/// holding `fw_objs.lock`.
pub struct MipsPageTable {
    /// The table's pages, contiguous. Upstream allocates
    /// [`PT_PAGE_COUNT`] separate pages and `vmap`s them; one contiguous
    /// allocation is simpler and gives the same physical addresses to the boot
    /// data. Recorded in `ABI.md` compatibility limits.
    pages: DmaStream,
    /// Owning device, for the Rogue MMU invalidation that
    /// [`Self::map`] / [`Self::unmap`] accumulate.
    ///
    /// `Weak`, not `Arc`: [`PvrDevice::fw`] owns this table through
    /// [`crate::fw::mips::MipsData`], so a strong back-pointer would cycle and
    /// pin the device forever. Upstream's `pvr_fw_mips_data` reaches its
    /// `pvr_device` the same way (the firmware blob lives on the device).
    device: Weak<PvrDevice>,
    /// Number of entries the heap size calls for.
    entry_count: u32,
    /// Mask applied to a shifted page frame number.
    pfn_mask: u32,
    /// Cache policy for cached mappings.
    cache_policy: u32,
    /// Mask turning a firmware address into a heap offset.
    offset_mask: u32,
    /// Serialises entry writes.
    write_lock: SpinLock<()>,
    /// Number of `sync_to_device` calls made on the table.
    ///
    /// Diagnostic, and the thing
    /// `page_table_writes_are_synced_to_the_device` asserts on. Not part of the
    /// firmware ABI.
    #[cfg(any(test, ktest))]
    syncs: AtomicU64,
    #[cfg(ktest)]
    fail_write_after: AtomicU64,
}

impl MipsPageTable {
    /// Allocates and zeroes a table sized for `heap`.
    ///
    /// Ports upstream `pvr_vm_mips_init()`. `phys_bus_width` selects the entry
    /// format: a bus wider than 32 bits has four more page-frame-number bits and
    /// uses write-through rather than write-back caching, which is the one place
    /// the entry encoding depends on the SoC.
    ///
    /// `device` is the GPU whose Rogue MMU invalidation flags
    /// [`Self::map`] / [`Self::unmap`] must accumulate — upstream's
    /// `pvr_vm_mips_map()` ends in `pvr_mmu_flush_request_all(pvr_dev)`. It is
    /// [`Weak`] because the device owns this table; see the field comment.
    ///
    /// The MIPS firmware path requires more than 32 bits of bus, which upstream
    /// enforces in `pvr_mips_wrapper_init()` with a `WARN_ON`; this rejects it
    /// here instead, before anything has been written.
    pub fn new(device: Weak<PvrDevice>, heap: &FwHeapInfo, phys_bus_width: u32) -> PvrResult<Self> {
        if phys_bus_width <= 32 {
            ostd::error!(
                "MIPS firmware needs a physical bus wider than 32 bits; this GPU reports {}",
                phys_bus_width
            );
            return Err(PvrError::Unsupported);
        }

        // Upstream `ROGUE_MIPSFW_LOG2_PAGETABLE_SIZE_4K(pvr_dev)`.
        let log2_table_size = heap
            .log2_size
            .checked_sub(ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K)
            .and_then(|shift| shift.checked_add(ROGUE_MIPSFW_LOG2_PTE_ENTRY_SIZE))
            .ok_or(PvrError::InvalidArgument)?;
        let table_size = 1usize
            .checked_shl(log2_table_size)
            .ok_or(PvrError::InvalidArgument)?;
        // Upstream's `pt_size > ROGUE_MIPSFW_MAX_NUM_PAGETABLE_PAGES * SZ_4K`.
        if table_size > PT_PAGE_COUNT * PAGE_SIZE {
            ostd::error!(
                "firmware heap of 2^{} needs a {}-byte page table; the firmware accepts at most {}",
                heap.log2_size,
                table_size,
                PT_PAGE_COUNT * PAGE_SIZE
            );
            return Err(PvrError::InvalidArgument);
        }

        let pages =
            DmaStream::alloc(PT_PAGE_COUNT, GPU_CACHE_COHERENT).map_err(|_| PvrError::NoMemory)?;

        let table = Self {
            pages,
            device,
            entry_count: (table_size / PTE_SIZE) as u32,
            pfn_mask: ROGUE_MIPSFW_ENTRYLO_PFN_MASK_ABOVE_32BIT,
            cache_policy: ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT,
            offset_mask: heap.offset_mask,
            write_lock: SpinLock::new(()),
            #[cfg(any(test, ktest))]
            syncs: AtomicU64::new(0),
            #[cfg(ktest)]
            fail_write_after: AtomicU64::new(u64::MAX),
        };
        // `DmaStream::alloc` zeroes through the CPU's cache, so the zeroes are
        // not yet visible to the firmware. This is the first instance of the rule
        // this module exists to enforce.
        table.sync_all()?;
        Ok(table)
    }

    /// Number of entries, i.e. 4 KiB pages of firmware heap the table covers.
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }

    /// The `pfn_mask` chosen for this bus width. Diagnostics and tests.
    pub fn pfn_mask(&self) -> u32 {
        self.pfn_mask
    }

    /// The cache policy chosen for this bus width. Diagnostics and tests.
    pub fn cache_policy(&self) -> u32 {
        self.cache_policy
    }

    /// Bus address of the `page_nr`-th 4 KiB page of the table.
    ///
    /// What the firmware's boot data needs: `rogue_mipsfw_boot_data::pt_phys_addr`
    /// is where the bootloader learns to find the table at all, before any
    /// translation is active.
    pub fn page_daddr(&self, page_nr: usize) -> PvrResult<Daddr> {
        if page_nr >= PT_PAGE_COUNT {
            return Err(PvrError::InvalidArgument);
        }
        Ok(self.pages.daddr() + page_nr * ROGUE_MIPSFW_PAGE_SIZE_4K_USIZE)
    }

    /// How many times the table has been flushed to the device.
    #[cfg(any(test, ktest))]
    pub fn syncs_issued(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    /// Maps a firmware object into the firmware processor's address space.
    ///
    /// Ports upstream `pvr_vm_mips_map()`. Every page of the object's allocation
    /// gets a read/write, valid, global entry with the object's cache policy.
    ///
    /// All or nothing: a failure part-way through clears the entries it had
    /// already written, because a half-mapped firmware structure produces a
    /// firmware fault at an address that has nothing to do with the bug.
    pub fn map(&self, object: &FwObject) -> PvrResult<()> {
        let (start_pfn, end_pfn) = self.pfn_range(object)?;
        let cache_policy = if object.is_uncached() {
            ROGUE_MIPSFW_UNCACHED_CACHE_POLICY
        } else {
            self.cache_policy
        };
        let flags = pte_flags(true, true, cache_policy);

        for pfn in start_pfn..=end_pfn {
            let offset = (pfn - start_pfn) as usize * ROGUE_MIPSFW_PAGE_SIZE_4K_USIZE;
            let result = object
                .daddr_at(offset)
                .and_then(|daddr| self.write_entry(pfn, encode_pte(daddr, self.pfn_mask, flags)));
            if let Err(err) = result {
                // Upstream's `err_unmap_pages` unwinds the same way.
                for done in start_pfn..pfn {
                    let _ = self.write_entry(done, 0);
                }
                return Err(err);
            }
        }

        // Upstream `pvr_vm_mips_map()` ends here with
        // `pvr_mmu_flush_request_all()`, which coalesces a Rogue MMU
        // invalidation for the next submission. That is the GPU MMU, not this
        // table — the firmware core's own TLB is kept coherent by the syncs
        // above — so it costs nothing but keeps the two paths in step. The order
        // matches upstream: every entry is written before the flag is raised.
        self.request_rogue_flush();
        Ok(())
    }

    /// Removes an object's mappings.
    ///
    /// Ports upstream `pvr_vm_mips_unmap()`. Upstream spells the range twice —
    /// map computes `end = start + size - 1` and iterates `start_pfn..=end_pfn`,
    /// unmap computes `end = start + size` and iterates `start_pfn..end_pfn` —
    /// which are two ways of writing the same range for the page-aligned sizes
    /// firmware objects always have. Both paths here go through one
    /// [`Self::pfn_range`] instead, so the symmetry is constructed rather than
    /// maintained; a drift between two copies would leave a freed object's last
    /// page readable by the firmware. Recorded in `ABI.md` compatibility limits.
    pub fn unmap(&self, object: &FwObject) -> PvrResult<()> {
        let (start_pfn, end_pfn) = self.pfn_range(object)?;
        for pfn in start_pfn..=end_pfn {
            self.write_entry(pfn, 0)?;
        }
        // As in [`Self::map`], upstream's `pvr_vm_mips_unmap()` raises the Rogue
        // MMU invalidation flag once every entry has been cleared.
        self.request_rogue_flush();
        Ok(())
    }

    /// Accumulates a full Rogue MMU invalidation on this table's device.
    ///
    /// Ports the `pvr_mmu_flush_request_all(pvr_dev)` call at the end of
    /// upstream `pvr_vm_mips_map()` / `pvr_vm_mips_unmap()`. A dead `Weak` is a
    /// no-op: unit tests construct tables without a live device, and during
    /// teardown the device is already going away so there is nothing left to
    /// invalidate for.
    fn request_rogue_flush(&self) {
        if let Some(device) = self.device.upgrade() {
            crate::mm::mmu::flush_request_all(&device);
        }
    }

    /// Reads an entry back.
    ///
    /// Only the firmware writes to its own TLB, never to this table, so the
    /// invalidate before the read is belt-and-braces; it costs one cache line and
    /// keeps the accessor honest about the direction of travel.
    pub fn entry(&self, pfn: u32) -> PvrResult<u32> {
        use ostd::mm::VmIo;

        let offset = self.entry_offset(pfn)?;
        self.pages
            .sync_from_device(offset..offset + PTE_SIZE)
            .map_err(|_| PvrError::InvalidArgument)?;
        self.pages
            .read_val::<u32>(offset)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Writes one entry and makes it visible to the firmware core.
    ///
    /// The sync is not separable from the write. See the module documentation.
    fn write_entry(&self, pfn: u32, pte: u32) -> PvrResult<()> {
        use ostd::mm::VmIo;

        let offset = self.entry_offset(pfn)?;
        let _guard = self.write_lock.lock();
        #[cfg(ktest)]
        {
            let left = self.fail_write_after.load(Ordering::Relaxed);
            if left == 0 {
                // One-shot: rollback writes must be allowed to finish.
                self.fail_write_after.store(u64::MAX, Ordering::Relaxed);
                return Err(PvrError::InvalidArgument);
            }
            if left != u64::MAX {
                self.fail_write_after.store(left - 1, Ordering::Relaxed);
            }
        }
        self.pages
            .write_val(offset, &pte)
            .map_err(|_| PvrError::InvalidArgument)?;
        self.pages
            .sync_to_device(offset..offset + PTE_SIZE)
            .map_err(|_| PvrError::InvalidArgument)?;
        #[cfg(any(test, ktest))]
        self.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    #[cfg(ktest)]
    pub(crate) fn fail_write_after_for_test(&self, count: Option<u64>) {
        self.fail_write_after
            .store(count.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    #[cfg(ktest)]
    pub(crate) fn entries_for_test(&self) -> alloc::vec::Vec<u32> {
        (0..self.entry_count)
            .map(|pfn| self.entry(pfn).expect("read PTE"))
            .collect()
    }

    /// Flushes the whole table.
    fn sync_all(&self) -> PvrResult<()> {
        self.pages
            .sync_to_device(0..PT_PAGE_COUNT * PAGE_SIZE)
            .map_err(|_| PvrError::InvalidArgument)?;
        #[cfg(any(test, ktest))]
        self.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Byte offset of an entry, bounded by [`Self::entry_count`].
    fn entry_offset(&self, pfn: u32) -> PvrResult<usize> {
        if pfn >= self.entry_count {
            return Err(PvrError::InvalidArgument);
        }
        Ok(pfn as usize * PTE_SIZE)
    }

    /// The inclusive page-number range an object occupies.
    ///
    /// Ports the bounds checks of upstream `pvr_vm_mips_map()`: the object must
    /// lie wholly inside the firmware heap and start and end on a firmware page
    /// boundary.
    fn pfn_range(&self, object: &FwObject) -> PvrResult<(u32, u32)> {
        let start = object.fw_offset();
        let size = u32::try_from(object.allocated_size()).map_err(|_| PvrError::InvalidArgument)?;
        let end = start
            .checked_add(size)
            .and_then(|end| end.checked_sub(1))
            .ok_or(PvrError::InvalidArgument)?;

        if start & ROGUE_MIPSFW_PAGE_MASK_4K != 0 || (end + 1) & ROGUE_MIPSFW_PAGE_MASK_4K != 0 {
            return Err(PvrError::InvalidArgument);
        }
        let start_pfn = (start & self.offset_mask) >> ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K;
        let end_pfn = (end & self.offset_mask) >> ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K;
        if start_pfn > end_pfn || end_pfn >= self.entry_count {
            return Err(PvrError::InvalidArgument);
        }
        Ok((start_pfn, end_pfn))
    }
}

/// Says whether a firmware address the processor faulted on is mapped, and
/// where the hole around it begins and ends.
///
/// # What this answers
///
/// The firmware reports a page fault twice over: in its own trace, as
/// `Mips page fault detected (BadVAddr, EntryLo0, EntryLo1)`, and to the host
/// through `FWCCB_CMD_CONTEXT_FW_PF_NOTIFICATION`. Both give an address and
/// neither says whose it is. But the driver *built* the table being walked, so
/// the address can simply be looked up — and the interesting part is not the one
/// page but the run it belongs to, because a missing mapping is normally a whole
/// object's worth of pages.
///
/// Both halves of the TLB pair are reported, because that is the shape the
/// firmware's own message has: a MIPS TLB entry covers two consecutive pages,
/// `EntryLo0` for the even one and `EntryLo1` for the odd, and a fault on either
/// prints both.
pub fn report_fault(device: &PvrDevice, fw_addr: u64) {
    let Some(fw) = device.fw.get() else {
        ostd::debug!("[k1-gpu] page fault at {:#x}: no firmware loaded", fw_addr);
        return;
    };
    let table = &fw.mips.page_table;

    let base = crate::fw::mips::FW_HEAP_MIPS_BASE as u64;
    let Some(offset) = fw_addr.checked_sub(base) else {
        ostd::debug!(
            "[k1-gpu] page fault at {:#x}: below the firmware heap base {:#x}, so it is not \
             an address this table describes",
            fw_addr,
            base
        );
        return;
    };
    let pfn = (offset >> ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K) as u32;
    if pfn >= table.entry_count {
        ostd::debug!(
            "[k1-gpu] page fault at {:#x}: page {} is past the {} the table covers",
            fw_addr,
            pfn,
            table.entry_count
        );
        return;
    }

    // The pair the firmware's TLB entry would have held.
    let even = pfn & !1;
    let entry_of = |pfn: u32| table.entry(pfn).unwrap_or(0);
    ostd::debug!(
        "[k1-gpu] page fault at {:#x} = heap page {}: EntryLo0(page {})={:#x} \
         EntryLo1(page {})={:#x}",
        fw_addr,
        pfn,
        even,
        entry_of(even),
        even + 1,
        entry_of(even + 1)
    );

    // The run of unmapped pages the fault sits in, and the mapped pages either
    // side of it: "pages 95..104 have no entry, 94 and 104 do" localises the
    // missing object far better than one page number does.
    if entry_of(pfn) != 0 {
        ostd::debug!(
            "[k1-gpu] page fault at {:#x}: this page *is* mapped, so the fault is a \
             permission or cache-policy mismatch rather than a missing entry",
            fw_addr
        );
        return;
    }
    let mut first = pfn;
    while first > 0 && entry_of(first - 1) == 0 {
        first -= 1;
    }
    let mut last = pfn;
    while last + 1 < table.entry_count && entry_of(last + 1) == 0 {
        last += 1;
    }
    ostd::debug!(
        "[k1-gpu] page fault at {:#x}: unmapped run is pages {}..={} ({:#x}..={:#x}), \
         {} pages; the mapped neighbours are {} and {}",
        fw_addr,
        first,
        last,
        base + ((first as u64) << ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K),
        base + ((last as u64) << ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K),
        last - first + 1,
        if first == 0 {
            "(start of heap)"
        } else {
            "the page below"
        },
        if last + 1 >= table.entry_count {
            "(end of table)"
        } else {
            "the page above"
        }
    );
}

/// Builds the permission and caching bits of a MIPS `EntryLo`.
///
/// Ports upstream `get_mips_pte_flags()`. `DIRTY` is MIPS' name for "writable",
/// not "has been written"; `READ_INHIBIT` is how a write-only page is spelled.
/// A page that is neither readable nor writable is a caller bug — upstream
/// `WARN_ON`s it — and here produces an entry with neither bit, which the
/// firmware will fault on rather than silently accept.
pub fn pte_flags(read: bool, write: bool, cache_policy: u32) -> u32 {
    let mut flags = 0;
    if read && write {
        flags |= ROGUE_MIPSFW_ENTRYLO_DIRTY_EN;
    } else if write {
        flags |= ROGUE_MIPSFW_ENTRYLO_READ_INHIBIT_EN;
    }
    flags |= cache_policy << ROGUE_MIPSFW_ENTRYLO_CACHE_POLICY_SHIFT;
    flags | ROGUE_MIPSFW_ENTRYLO_VALID_EN | ROGUE_MIPSFW_ENTRYLO_GLOBAL_EN
}

/// Encodes a bus address and flags into a MIPS `EntryLo`.
///
/// Ports the two lines of upstream `pvr_vm_mips_map()` that build the entry. The
/// page frame number is the address shifted down by the firmware page size and
/// back up by [`ROGUE_MIPSFW_ENTRYLO_PFN_SHIFT`], then masked — which is how a
/// 36- or 40-bit bus address fits in a 32-bit entry.
pub fn encode_pte(daddr: Daddr, pfn_mask: u32, flags: u32) -> u32 {
    let pfn = (daddr >> ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K) as u64;
    let positioned = (pfn << ROGUE_MIPSFW_ENTRYLO_PFN_SHIFT) as u32;
    (positioned & pfn_mask) | flags
}

/// Entry format constants that only apply to a 32-bit bus.
///
/// [`MipsPageTable::new`] refuses a bus of 32 bits or less, so these are never
/// selected; they are referenced here so that a future 32-bit port has to delete
/// this comment rather than discover the two values it forgot.
const _: () = {
    assert!(ROGUE_MIPSFW_ENTRYLO_PFN_MASK != ROGUE_MIPSFW_ENTRYLO_PFN_MASK_ABOVE_32BIT);
    assert!(ROGUE_MIPSFW_CACHED_POLICY != ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT);
};

/// The table must cover the whole heap, or a firmware address in the heap would
/// have no entry to describe it.
const _: () = assert!(PT_PAGE_COUNT == 4);

/// Tests for the entry encoding and for the sync discipline.
///
/// The encoding tests run under both `cargo test` and `cargo osdk test`; the
/// rest need [`DmaStream::alloc`] and so a running kernel, and are `#[ktest]`
/// only.
#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;
    #[cfg(ktest)]
    use crate::fw::heap::{FwCache, FwHeap};

    /// Read/write, valid and global, with the cache policy in its field. Every
    /// bit here is one the firmware acts on, and a wrong one produces a fault at
    /// an address unrelated to the mistake.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn pte_flags_encode_permissions_and_caching() {
        let cached = pte_flags(true, true, ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT);
        assert_ne!(cached & ROGUE_MIPSFW_ENTRYLO_DIRTY_EN, 0);
        assert_ne!(cached & ROGUE_MIPSFW_ENTRYLO_VALID_EN, 0);
        assert_ne!(cached & ROGUE_MIPSFW_ENTRYLO_GLOBAL_EN, 0);
        assert_eq!(cached & ROGUE_MIPSFW_ENTRYLO_READ_INHIBIT_EN, 0);
        assert_eq!(
            (cached >> ROGUE_MIPSFW_ENTRYLO_CACHE_POLICY_SHIFT) & 0x7,
            ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT
        );

        // Write-only is spelled "reads inhibited", not "not readable".
        let write_only = pte_flags(false, true, ROGUE_MIPSFW_UNCACHED_CACHE_POLICY);
        assert_ne!(write_only & ROGUE_MIPSFW_ENTRYLO_READ_INHIBIT_EN, 0);
        assert_eq!(write_only & ROGUE_MIPSFW_ENTRYLO_DIRTY_EN, 0);

        // Upstream's `ROGUE_MIPSFW_ENTRYLO_DVG` and `_DVG_UNCACHED` are the two
        // combinations it names; the builder must reproduce them.
        assert_eq!(
            pte_flags(true, true, 0) & 0x7,
            crate::rogue::mips::ROGUE_MIPSFW_ENTRYLO_DVG
        );
        assert_eq!(
            pte_flags(true, true, ROGUE_MIPSFW_UNCACHED_CACHE_POLICY) & 0x1f,
            crate::rogue::mips::ROGUE_MIPSFW_ENTRYLO_DVG_UNCACHED
        );
    }

    /// A 40-bit bus address does not fit a 32-bit entry, and only survives
    /// because the low twelve bits are implied and the result is shifted back up
    /// by six. Dropping either shift silently maps the wrong page.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn encode_pte_packs_a_wide_bus_address() {
        let flags = pte_flags(true, true, ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT);
        let daddr: Daddr = 0x9_8765_4000;
        let pte = encode_pte(daddr, ROGUE_MIPSFW_ENTRYLO_PFN_MASK_ABOVE_32BIT, flags);

        // Recover the page frame number the way the hardware does.
        let pfn = ((pte & ROGUE_MIPSFW_ENTRYLO_PFN_MASK_ABOVE_32BIT)
            >> ROGUE_MIPSFW_ENTRYLO_PFN_SHIFT) as usize;
        assert_eq!(pfn << ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K, daddr);
        assert_eq!(pte & !ROGUE_MIPSFW_ENTRYLO_PFN_MASK_ABOVE_32BIT, flags);

        // The narrower 32-bit-bus mask would lose the top four frame bits, which
        // is why `new()` refuses that bus width outright.
        let narrow = encode_pte(daddr, ROGUE_MIPSFW_ENTRYLO_PFN_MASK, flags);
        assert_ne!(narrow, pte);
    }

    #[cfg(ktest)]
    fn test_heap() -> FwHeapInfo {
        FwHeapInfo::new(
            crate::fw::mips::FW_HEAP_MIPS_LOG2_SIZE,
            crate::fw::mips::FW_HEAP_MIPS_RESERVED_SIZE,
        )
        .expect("16 MiB heap")
    }

    /// The table covers the whole heap and fits the four pages the firmware's
    /// bootloader can be told about.
    #[ktest]
    fn table_geometry_covers_the_heap() {
        let info = test_heap();
        let table = MipsPageTable::new(Weak::new(), &info, 40).expect("40-bit bus");

        assert_eq!(table.entry_count(), info.raw_size / PAGE_SIZE as u32);
        assert_eq!(
            table.entry_count() as usize * PTE_SIZE,
            PT_PAGE_COUNT * PAGE_SIZE
        );
        // Every page of the table has a distinct bus address, and one past the
        // end has none.
        let first = table.page_daddr(0).unwrap();
        assert_eq!(
            table.page_daddr(PT_PAGE_COUNT - 1).unwrap(),
            first + 3 * PAGE_SIZE
        );
        assert!(table.page_daddr(PT_PAGE_COUNT).is_err());
    }

    /// **The K1 rule.** Every entry written must be flushed, and the count is the
    /// only way to assert that without reading the code. Three pages mapped means
    /// three syncs beyond the one `new()` issues over the zeroed table.
    #[ktest]
    fn page_table_writes_are_synced_to_the_device() {
        let info = test_heap();
        let mut heap = FwHeap::new(info, crate::fw::mips::FW_HEAP_MIPS_BASE);
        let table = MipsPageTable::new(Weak::new(), &info, 40).expect("40-bit bus");

        // `new()` flushes the zeroed table: those zeroes are entries too.
        assert_eq!(table.syncs_issued(), 1);

        let object = heap
            .alloc(3 * PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("three pages");
        let before = table.syncs_issued();
        table.map(&object).expect("map");
        assert_eq!(
            table.syncs_issued() - before,
            3,
            "one flush per entry written, not one per map"
        );

        let before = table.syncs_issued();
        table.unmap(&object).expect("unmap");
        assert_eq!(
            table.syncs_issued() - before,
            3,
            "clearing an entry is a write and needs the same flush"
        );
    }

    /// The entries a map writes must name the object's own pages, in order, with
    /// the object's cache policy — and nothing outside its range.
    #[ktest]
    fn map_writes_one_entry_per_page_and_leaves_neighbours_alone() {
        let info = test_heap();
        let mut heap = FwHeap::new(info, crate::fw::mips::FW_HEAP_MIPS_BASE);
        let table = MipsPageTable::new(Weak::new(), &info, 40).expect("40-bit bus");

        // Uncached, as every firmware structure is.
        let object = heap
            .alloc(3 * PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("three pages");
        let start_pfn = object.fw_offset() / PAGE_SIZE as u32;
        table.map(&object).expect("map");

        let expected_flags = pte_flags(true, true, ROGUE_MIPSFW_UNCACHED_CACHE_POLICY);
        for page in 0..3u32 {
            let pte = table.entry(start_pfn + page).expect("entry in range");
            let daddr = object.daddr_at(page as usize * PAGE_SIZE).unwrap();
            assert_eq!(
                pte,
                encode_pte(daddr, table.pfn_mask(), expected_flags),
                "entry {} of the object",
                page
            );
        }
        // The page after the object is untouched.
        assert_eq!(table.entry(start_pfn + 3).unwrap(), 0);

        table.unmap(&object).expect("unmap");
        for page in 0..3u32 {
            assert_eq!(
                table.entry(start_pfn + page).unwrap(),
                0,
                "unmap must clear entry {page}, including the last one"
            );
        }
    }

    /// A cached object gets the bus width's cached policy rather than the
    /// uncached one, which is the only difference between the two paths.
    #[ktest]
    fn a_cached_object_gets_the_cached_policy() {
        let info = test_heap();
        let mut heap = FwHeap::new(info, crate::fw::mips::FW_HEAP_MIPS_BASE);
        let table = MipsPageTable::new(Weak::new(), &info, 40).expect("40-bit bus");

        let object = heap.alloc(PAGE_SIZE, FwCache::CACHED).expect("one page");
        table.map(&object).expect("map");
        let pte = table.entry(object.fw_offset() / PAGE_SIZE as u32).unwrap();
        assert_eq!(
            (pte >> ROGUE_MIPSFW_ENTRYLO_CACHE_POLICY_SHIFT) & 0x7,
            table.cache_policy()
        );
        assert_eq!(table.cache_policy(), ROGUE_MIPSFW_CACHED_POLICY_ABOVE_32BIT);
    }

    /// The CPU mapping must not change the MIPS page-table policy, and the
    /// MIPS-side flag must not change the CPU mapping. The two bits are
    /// orthogonal; a single `uncached` that drove both would fail this.
    #[ktest]
    fn cpu_and_device_cache_flags_are_orthogonal_in_the_pte() {
        let info = test_heap();
        let mut heap = FwHeap::new(info, crate::fw::mips::FW_HEAP_MIPS_BASE);
        let table = MipsPageTable::new(Weak::new(), &info, 40).expect("40-bit bus");

        let cache_bits = |pte: u32| (pte >> ROGUE_MIPSFW_ENTRYLO_CACHE_POLICY_SHIFT) & 0x7;

        // Device-cached + CPU-uncached: the PTE must still be the cached policy.
        let cpu_only = heap
            .alloc(
                PAGE_SIZE,
                FwCache {
                    device_uncached: false,
                    cpu_uncached: true,
                },
            )
            .expect("cpu-only");
        assert!(!cpu_only.is_uncached());
        assert!(cpu_only.is_cpu_uncached());
        table.map(&cpu_only).expect("map");
        let pte = table
            .entry(cpu_only.fw_offset() / PAGE_SIZE as u32)
            .unwrap();
        assert_eq!(cache_bits(pte), table.cache_policy());

        // Device-uncached + CPU write-back: the PTE must still be uncached.
        let device_only = heap
            .alloc(PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("device-only");
        assert!(device_only.is_uncached());
        assert!(!device_only.is_cpu_uncached());
        table.map(&device_only).expect("map");
        let pte = table
            .entry(device_only.fw_offset() / PAGE_SIZE as u32)
            .unwrap();
        assert_eq!(cache_bits(pte), ROGUE_MIPSFW_UNCACHED_CACHE_POLICY);
    }

    /// The MIPS firmware path does not exist on a 32-bit bus, and finding out
    /// after the table has been allocated and half-filled would be worse than
    /// finding out now.
    #[ktest]
    fn a_32_bit_bus_is_refused() {
        let info = test_heap();
        assert_eq!(
            MipsPageTable::new(Weak::new(), &info, 32).err(),
            Some(PvrError::Unsupported)
        );
        assert_eq!(
            MipsPageTable::new(Weak::new(), &info, 0).err(),
            Some(PvrError::Unsupported)
        );
    }

    /// An object outside the heap, or one that is not page aligned, has no
    /// entries to write.
    #[ktest]
    fn out_of_range_entries_are_rejected() {
        let info = test_heap();
        let table = MipsPageTable::new(Weak::new(), &info, 40).expect("40-bit bus");
        assert_eq!(
            table.entry(table.entry_count()).err(),
            Some(PvrError::InvalidArgument)
        );
        assert!(table.entry(table.entry_count() - 1).is_ok());
    }
}
