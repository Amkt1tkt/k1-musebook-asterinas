// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: Rogue GPU page tables and deferred cache invalidation.

//! GPU MMU page tables.
//!
//! The address walk uses three table levels followed by 4 KiB leaf pages. Page-table
//! entries are DMA-backed and synchronized when written; device translation-cache
//! invalidation is deferred until the pending MMU flush is consumed.
//!
//! Allocate the required tables before publishing mappings so allocation failure
//! does not expose a partial range. Table-memory synchronization and GPU MMU cache
//! invalidation are separate operations and neither replaces the other.

use alloc::{sync::Arc, vec::Vec};
#[cfg(any(test, ktest))]
use core::sync::atomic::AtomicU64;
use core::{
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

use ostd::mm::{Daddr, HasDaddr, PAGE_SIZE, VmIo, dma::DmaStream};
use ostd_pod::FromZeros;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::GPU_CACHE_COHERENT,
    rogue::{
        fwif::{RogueFwifKccbCmd, defines as fwif},
        mmu as ctrl,
    },
};

/// Log2 of the page size the GPU MMU is programmed with.
///
/// Upstream `PVR_DEVICE_PAGE_SHIFT`, which is the host's `PAGE_SHIFT`. The MMU
/// can also do 16 KiB through 2 MiB pages; upstream selects the entry-format
/// constants for whichever the host uses with a `#if` ladder over
/// `PVR_DEVICE_PAGE_SIZE`. Asterinas has 4 KiB pages on both architectures it
/// targets, so this port takes the 4 KiB arm and pins that choice with the
/// assertion below rather than carrying five untested alternatives.
pub const DEVICE_PAGE_SHIFT: u32 = PAGE_SIZE.ilog2();

/// The GPU MMU's page size in bytes. Upstream `PVR_DEVICE_PAGE_SIZE`.
pub const DEVICE_PAGE_SIZE: u64 = 1 << DEVICE_PAGE_SHIFT;

/// The 4 KiB arm of upstream's `#if (PVR_DEVICE_PAGE_SIZE == SZ_4K)` ladder.
const _: () = assert!(DEVICE_PAGE_SHIFT == ctrl::ROGUE_MMUCTRL_PAGE_4KB_RANGE_SHIFT);

/// Size of the device-virtual address space the hierarchy can describe.
///
/// Upstream `PVR_PAGE_TABLE_ADDR_SPACE_SIZE` (`SZ_1T`). `pvr_vm_create_context()`
/// refuses a GPU whose `virtual_address_space_bits` feature disagrees, because
/// the index shifts below are only correct for this size.
pub const ADDR_SPACE_SIZE: u64 = 1 << ADDR_BITS;

/// Bits of device-virtual address the hierarchy indexes.
///
/// Upstream `PVR_PAGE_TABLE_ADDR_BITS`. Ten bits of catalogue index, nine of
/// directory, nine of table and twelve of page offset.
pub const ADDR_BITS: u32 = ctrl::ROGUE_MMUCTRL_VADDR_PC_INDEX_SHIFT + PC_INDEX_BITS;

/// Width of the page-catalogue index field.
const PC_INDEX_BITS: u32 = ENTRIES_PC.ilog2();

const _: () = assert!(ADDR_BITS == 40);

/// Size of the memory backing one table at any level.
///
/// Upstream `PVR_MMU_BACKING_PAGE_SIZE` (`SZ_4K`). The host page size must be at
/// least this; upstream `static_assert`s the same thing.
const BACKING_PAGE_SIZE: usize = 4096;
const _: () = assert!(PAGE_SIZE >= BACKING_PAGE_SIZE);

/// Entries in a page catalogue. Upstream `ROGUE_MMUCTRL_ENTRIES_PC_VALUE`.
const ENTRIES_PC: usize = ctrl::ROGUE_MMUCTRL_ENTRIES_PC_VALUE as usize;
/// Entries in a page directory. Upstream `ROGUE_MMUCTRL_ENTRIES_PD_VALUE`.
const ENTRIES_PD: usize = ctrl::ROGUE_MMUCTRL_ENTRIES_PD_VALUE as usize;
/// Entries in a page table.
///
/// Upstream `ROGUE_MMUCTRL_ENTRIES_PT_VALUE_X`, which divides the 4 KiB-page
/// count down for larger device pages. At a 4 KiB device page the shift is zero.
const ENTRIES_PT: usize = (ctrl::ROGUE_MMUCTRL_ENTRIES_PT_VALUE
    >> (DEVICE_PAGE_SHIFT - ctrl::ROGUE_MMUCTRL_PAGE_4KB_RANGE_SHIFT))
    as usize;

/// Bytes in one page-catalogue entry. Upstream asserts this equals 32 bits.
const ENTRY_BYTES_PC: usize = ctrl::ROGUE_MMUCTRL_ENTRY_SIZE_PC_VALUE as usize / 8;
/// Bytes in one page-directory entry. 64 bits.
const ENTRY_BYTES_PD: usize = ctrl::ROGUE_MMUCTRL_ENTRY_SIZE_PD_VALUE as usize / 8;
/// Bytes in one page-table entry. 64 bits.
const ENTRY_BYTES_PT: usize = ctrl::ROGUE_MMUCTRL_ENTRY_SIZE_PT_VALUE as usize / 8;

/// A table of any level must be exactly one backing page, or the address
/// arithmetic that turns an index into a byte offset would run off the end.
///
/// Upstream carries the same three assertions on `struct pvr_page_table_l{2,1,0}_raw`.
const _: () = {
    assert!(ENTRIES_PC * ENTRY_BYTES_PC == BACKING_PAGE_SIZE);
    assert!(ENTRIES_PD * ENTRY_BYTES_PD == BACKING_PAGE_SIZE);
    assert!(ENTRIES_PT * ENTRY_BYTES_PT <= BACKING_PAGE_SIZE);
};

/// The MMU cache invalidation flags, by the level of table that changed.
///
/// Upstream `PVR_MMU_SYNC_LEVEL_{0,1,2}_FLAGS`. The levels nest: touching a page
/// catalogue entry also invalidates the directories and tables below it, so
/// [`LEVEL_2`](flush_flags::LEVEL_2) is a superset of
/// [`LEVEL_1`](flush_flags::LEVEL_1) is a superset of
/// [`LEVEL_0`](flush_flags::LEVEL_0).
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_mmu.c>.
pub mod flush_flags {
    use crate::rogue::fwif::defines as fwif;

    /// A page table (leaf) entry changed. Upstream `PVR_MMU_SYNC_LEVEL_0_FLAGS`.
    pub const LEVEL_0: u32 = fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_PT
        | fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_INTERRUPT
        | fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_TLB;
    /// A page directory entry changed. Upstream `PVR_MMU_SYNC_LEVEL_1_FLAGS`.
    pub const LEVEL_1: u32 = LEVEL_0 | fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_PD;
    /// A page catalogue entry changed. Upstream `PVR_MMU_SYNC_LEVEL_2_FLAGS`.
    pub const LEVEL_2: u32 = LEVEL_1 | fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_PC;
}

/// Which level of the hierarchy a table sits at.
///
/// Carries the three things that differ between levels — entry width, entry
/// count and invalidation flags — so that [`BackingPage::store`] can be one
/// function rather than three near-copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    /// The root. Upstream calls it level 2.
    Catalogue,
    /// Upstream level 1.
    Directory,
    /// The leaf table, whose entries name pages. Upstream level 0.
    Table,
}

impl Level {
    const fn entry_bytes(self) -> usize {
        match self {
            Self::Catalogue => ENTRY_BYTES_PC,
            Self::Directory => ENTRY_BYTES_PD,
            Self::Table => ENTRY_BYTES_PT,
        }
    }

    const fn entries(self) -> usize {
        match self {
            Self::Catalogue => ENTRIES_PC,
            Self::Directory => ENTRIES_PD,
            Self::Table => ENTRIES_PT,
        }
    }

    /// What the firmware must invalidate after an entry at this level changes.
    const fn flush_flags(self) -> u32 {
        match self {
            Self::Catalogue => flush_flags::LEVEL_2,
            Self::Directory => flush_flags::LEVEL_1,
            Self::Table => flush_flags::LEVEL_0,
        }
    }
}

/// State shared by every table in one hierarchy.
///
/// Held by [`Arc`] rather than passed down, so that [`BackingPage::store`] needs
/// no arguments beyond the entry it is writing. That is what lets the sync and
/// the flush request live inside the write instead of beside it.
struct Hierarchy {
    /// The device whose MMU these tables describe, for the flush request.
    device: Arc<PvrDevice>,
    /// Number of `sync_to_device` calls made on any table of this hierarchy.
    ///
    /// Diagnostic, and what `every_entry_write_is_synced` asserts on. Not part
    /// of any ABI.
    #[cfg(any(test, ktest))]
    syncs: AtomicU64,
}

impl core::fmt::Debug for Hierarchy {
    /// Reports the GPU ID but not the device, which is not [`Debug`] and
    /// whose contents would swamp any table dump anyway.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hierarchy")
            .field("gpu_id", &self.device.gpu_id)
            .finish_non_exhaustive()
    }
}

/// One 4 KiB page holding the raw entries of a table at some level.
///
/// Ports upstream `struct pvr_mmu_backing_page`. Upstream keeps a `vmap`ped
/// write-combining alias of the page and a separate DMA mapping; a
/// [`DmaStream`] is both, and its `sync_to_device` is the `cbo.clean` the K1
/// needs.
#[derive(Debug)]
struct BackingPage {
    dma: Option<DmaStream>,
    published: AtomicBool,
    level: Level,
    hierarchy: Arc<Hierarchy>,
}

impl BackingPage {
    /// Allocates a zeroed table page and makes the zeroes visible to the GPU.
    ///
    /// The flush of the zero fill is not optional and not obvious:
    /// [`DmaStream::alloc`] zeroes through the CPU's cached linear mapping, so
    /// on the K1 an unsynced fresh table is whatever the previous owner of those
    /// frames left there as far as the GPU is concerned — which is to say, a
    /// page of entries with random valid bits. No flush is *requested* here
    /// because nothing points at the page yet.
    fn new(level: Level, hierarchy: Arc<Hierarchy>) -> PvrResult<Self> {
        let dma = DmaStream::alloc(BACKING_PAGE_SIZE / PAGE_SIZE, GPU_CACHE_COHERENT)
            .map_err(|_| PvrError::NoMemory)?;

        // A table's own bus address goes into its parent's entry, where only
        // bits 39..12 survive. A page above the 40-bit window would be silently
        // truncated into a pointer to somebody else's memory, so refuse it here
        // instead. Upstream has no equivalent check; a `dma_addr_t` wider than
        // the MMU's address field is a platform bug either way.
        let page = Self {
            dma: Some(dma),
            published: AtomicBool::new(false),
            level,
            hierarchy,
        };
        // Counted for the life of this value, including the two failure
        // returns below: `Drop` is what decrements, so a refused address
        // or a failed sync cannot leave the counter high.
        crate::live::note_mmu_page_alloc();
        let daddr = page.daddr();
        if daddr as u64 >= ADDR_SPACE_SIZE || !daddr.is_multiple_of(BACKING_PAGE_SIZE) {
            ostd::error!(
                "page table page at bus address {:#x} is outside the MMU's 40-bit window \
                 or misaligned",
                daddr
            );
            return Err(PvrError::NoMemory);
        }

        page.sync(0..BACKING_PAGE_SIZE)?;
        Ok(page)
    }

    fn dma(&self) -> &DmaStream {
        self.dma.as_ref().expect("backing page owns DMA until drop")
    }

    /// Bus address of the page, which is what its parent entry names.
    fn daddr(&self) -> Daddr {
        self.dma().daddr()
    }

    /// **The only function that writes a page table entry.**
    ///
    /// Writes `value` at `index`, flushes exactly those bytes to the device, and
    /// accumulates this level's invalidation flags. The three steps are one
    /// operation on purpose: see the module documentation.
    ///
    /// Clearing an entry is a write like any other and goes through here too — a
    /// stale valid entry left in a cache line is how a freed buffer stays
    /// readable by the GPU.
    fn store(&self, index: usize, value: u64) -> PvrResult<()> {
        let width = self.level.entry_bytes();
        if index >= self.level.entries() {
            return Err(PvrError::InvalidArgument);
        }
        let offset = index * width;

        if width == ENTRY_BYTES_PC {
            // A catalogue entry is 32 bits; every field of it fits, which the
            // encoder guarantees by bounding the child address to 40 bits.
            debug_assert!(value <= u32::MAX as u64);
            self.dma()
                .write_val(offset, &(value as u32))
                .map_err(|_| PvrError::InvalidArgument)?;
        } else {
            self.dma()
                .write_val(offset, &value)
                .map_err(|_| PvrError::InvalidArgument)?;
        }

        self.sync(offset..offset + width)?;
        request_flush(&self.hierarchy.device, self.level.flush_flags());
        Ok(())
    }

    /// Reads an entry back.
    ///
    /// No invalidation first: the GPU never writes these tables, so the only
    /// writer is the CPU and its own cache is authoritative.
    fn load(&self, index: usize) -> PvrResult<u64> {
        if index >= self.level.entries() {
            return Err(PvrError::InvalidArgument);
        }
        let offset = index * self.level.entry_bytes();
        if self.level.entry_bytes() == ENTRY_BYTES_PC {
            self.dma()
                .read_val::<u32>(offset)
                .map(u64::from)
                .map_err(|_| PvrError::InvalidArgument)
        } else {
            self.dma()
                .read_val::<u64>(offset)
                .map_err(|_| PvrError::InvalidArgument)
        }
    }

    /// Flushes a byte range; tests also count the synchronization calls.
    fn sync(&self, range: Range<usize>) -> PvrResult<()> {
        self.dma()
            .sync_to_device(range)
            .map_err(|_| PvrError::InvalidArgument)?;
        #[cfg(any(test, ktest))]
        self.hierarchy.syncs.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for BackingPage {
    fn drop(&mut self) {
        if self.published.load(Ordering::Relaxed)
            && !self.hierarchy.device.quiesced.load(Ordering::Acquire)
        {
            let dma = self.dma.take().expect("backing page owns DMA until drop");
            self.hierarchy
                .device
                .retired_mappings
                .lock()
                .pages
                .push(dma);
            flush_request_all(&self.hierarchy.device);
        } else {
            crate::live::note_mmu_page_free();
        }
    }
}

/// A page directory and the leaf tables hanging off it.
#[derive(Debug)]
struct Directory {
    page: BackingPage,
    /// Mirror of the raw entries. Upstream keeps the same array in
    /// `pvr_page_table_l1::entries` for exactly the same reason: the raw entry
    /// holds a bus address, from which the owning object cannot be recovered.
    tables: Vec<Option<Table>>,
    /// Number of valid entries. The table is freed when this reaches zero.
    valid: usize,
}

/// A leaf page table.
#[derive(Debug)]
struct Table {
    page: BackingPage,
    /// Number of valid entries. The table is freed when this reaches zero.
    valid: usize,
}

/// A page table hierarchy for one GPU address space.
///
/// Ports upstream `struct pvr_mmu_context` plus the `pvr_page_table_l*` mirror
/// types. Upstream threads a `pvr_mmu_op_context` through every operation, which
/// carries a cursor, two preallocation lists and two free lists; that shape
/// exists to serve `drm_gpuvm`'s split/merge callbacks, which we do not have
/// (see [`crate::mm::vm`]). Here a map or an unmap is one call that walks the
/// hierarchy itself.
#[derive(Debug)]
pub struct PageTable {
    hierarchy: Arc<Hierarchy>,
    /// The page catalogue — the root, and the address the firmware is told.
    root: BackingPage,
    /// Mirror of the catalogue's entries.
    directories: Vec<Option<Directory>>,
    /// Number of valid catalogue entries.
    valid: usize,
}

impl PageTable {
    /// Creates an empty hierarchy with only the page catalogue allocated.
    ///
    /// Ports upstream `pvr_mmu_context_create()`.
    pub fn new(device: Arc<PvrDevice>) -> PvrResult<Self> {
        let hierarchy = Arc::new(Hierarchy {
            device,
            #[cfg(any(test, ktest))]
            syncs: AtomicU64::new(0),
        });
        let root = BackingPage::new(Level::Catalogue, hierarchy.clone())?;

        let mut directories = Vec::new();
        directories.resize_with(ENTRIES_PC, || None);

        Ok(Self {
            hierarchy,
            root,
            directories,
            valid: 0,
        })
    }

    /// Bus address of the page catalogue.
    ///
    /// Ports upstream `pvr_mmu_get_root_table_dma_addr()`. This is what goes
    /// into `rogue_fwif_fwmemcontext::pc_dev_paddr` and into
    /// `ROGUE_CR_BIF_CAT_BASE0`, so it must stay stable for the life of the
    /// context — it does, because the catalogue is never reallocated.
    pub fn root_daddr(&self) -> Daddr {
        self.root.published.store(true, Ordering::Relaxed);
        self.root.daddr()
    }

    /// How many times any table of this hierarchy has been flushed to the device.
    ///
    /// One per entry written, plus one per table allocated (for its zero fill).
    #[cfg(any(test, ktest))]
    pub fn syncs_issued(&self) -> u64 {
        self.hierarchy.syncs.load(Ordering::Relaxed)
    }

    /// Number of page directories currently allocated. Diagnostics and tests.
    pub fn directory_count(&self) -> usize {
        self.valid
    }

    /// Number of leaf page tables currently allocated. Diagnostics and tests.
    pub fn table_count(&self) -> usize {
        self.directories
            .iter()
            .flatten()
            .map(|directory| directory.valid)
            .sum()
    }

    /// Reads the leaf entry describing `device_addr`, if there is a table for it.
    ///
    /// `Ok(None)` means no page table covers that address, which is a different
    /// thing from a table with an invalid entry in it.
    pub fn page_entry(&self, device_addr: super::vm::DeviceAddr) -> PvrResult<Option<u64>> {
        let (pc, pd, pt) = split_addr(device_addr.0)?;
        let Some(directory) = self.directories[pc].as_ref() else {
            return Ok(None);
        };
        let Some(table) = directory.tables[pd].as_ref() else {
            return Ok(None);
        };
        table.page.load(pt).map(Some)
    }

    /// Maps `len` bytes at `device_addr`.
    ///
    /// Allocates intermediate levels as needed. Must be all-or-nothing: a partial
    /// mapping left behind by a mid-way failure is very hard to diagnose later.
    ///
    /// Ports upstream `pvr_mmu_map()` and `pvr_mmu_map_sgl()`. Upstream walks a
    /// scatter-gather table because a shmem object's pages are discontiguous;
    /// [`PvrObject`](super::gem::PvrObject) is one contiguous run (see its
    /// documentation), so the bus address of page `n` is addition.
    pub fn map(
        &mut self,
        device_addr: super::vm::DeviceAddr,
        object: &super::gem::PvrObject,
        offset: u64,
        len: u64,
    ) -> PvrResult<()> {
        let pages = self.check_range(device_addr, len)?;
        // Upstream checks the object bounds in `pvr_vm_bind_op_map_init()` and
        // the alignment in `pvr_vm_gpuva_map()`; both are the caller's promise
        // rather than something the MMU could recover from, so both are checked
        // here as well.
        if !offset.is_multiple_of(DEVICE_PAGE_SIZE)
            || offset
                .checked_add(len)
                .is_none_or(|end| end > object.size() as u64)
        {
            return Err(PvrError::InvalidArgument);
        }

        let flags = page_flags(object.flags());
        let start = device_addr.0;

        // Everything the walk could need, allocated before the first entry is
        // written. Upstream does the same in `pvr_mmu_op_context_create()`, and
        // it is what reduces the failure modes inside the loop to "already
        // mapped" and a cache-maintenance error.
        let mut spare = self.preallocate(start, pages)?;

        let mut journal = Journal::default();
        let mut result = Ok(());
        for page in 0..pages {
            let addr = start + page * DEVICE_PAGE_SIZE;
            let daddr = match object.daddr_at((offset + page * DEVICE_PAGE_SIZE) as usize) {
                Ok(daddr) => daddr,
                Err(err) => {
                    result = Err(err);
                    break;
                }
            };
            if let Err(err) = self.insert_page(addr, daddr, flags, &mut spare, &mut journal) {
                result = Err(err);
                break;
            }
            journal.pages += 1;
        }

        if let Err(err) = result {
            self.unwind(start, &journal);
            return Err(err);
        }
        Ok(())
    }

    /// Removes a mapping and frees any intermediate levels that became empty.
    ///
    /// Ports upstream `pvr_mmu_unmap()`. Unmapping a range that is not mapped is
    /// not an error, matching upstream's `pvr_page_destroy()` — the caller that
    /// cares about that distinction is [`super::vm::VmContext::unmap`], which
    /// consults its own record of live mappings first.
    pub fn unmap(&mut self, device_addr: super::vm::DeviceAddr, len: u64) -> PvrResult<()> {
        let pages = self.check_range(device_addr, len)?;
        for page in 0..pages {
            self.remove_page(device_addr.0 + page * DEVICE_PAGE_SIZE)?;
        }
        Ok(())
    }

    /// Validates an address range and returns its length in device pages.
    ///
    /// Ports upstream `pvr_device_addr_and_size_are_valid()`, including the
    /// corner it calls out: testing `device_addr` and `device_addr + size`
    /// separately reports a false negative for a range that ends exactly at the
    /// top of the address space, so the end is compared rather than validated.
    fn check_range(&self, device_addr: super::vm::DeviceAddr, len: u64) -> PvrResult<u64> {
        let start = device_addr.0;
        if len == 0
            || !start.is_multiple_of(DEVICE_PAGE_SIZE)
            || !len.is_multiple_of(DEVICE_PAGE_SIZE)
            || start
                .checked_add(len)
                .is_none_or(|end| end > ADDR_SPACE_SIZE)
        {
            return Err(PvrError::InvalidArgument);
        }
        Ok(len >> DEVICE_PAGE_SHIFT)
    }

    /// Allocates every intermediate table the range could need.
    ///
    /// Only the missing ones: the mirror already says which spans have a
    /// directory or a leaf table, so a map into a populated region allocates
    /// nothing.
    ///
    /// Divergence: upstream derives its counts from `sgt_offset` — the offset
    /// into the *buffer object* — rather than from the device address
    /// (`pvr_mmu_op_context_create()`), while the walk that consumes them is
    /// indexed by the device address. Where the two disagree upstream comes up
    /// short and the map fails with `-ENOMEM`; counting the spans actually
    /// walked cannot. Recorded in `ABI.md` compatibility limits.
    fn preallocate(&self, start: u64, pages: u64) -> PvrResult<Spare> {
        let last = start + (pages - 1) * DEVICE_PAGE_SIZE;
        let mut spare = Spare::default();

        // One iteration per 2 MiB span the range touches, not per page.
        let (first_span, last_span) = (start >> DIRECTORY_SHIFT, last >> DIRECTORY_SHIFT);
        let mut previous_pc = None;
        for span in first_span..=last_span {
            let pc =
                (span >> (ctrl::ROGUE_MMUCTRL_VADDR_PC_INDEX_SHIFT - DIRECTORY_SHIFT)) as usize;
            let pd = (span as usize) & (ENTRIES_PD - 1);

            match self.directories[pc].as_ref() {
                None => {
                    if previous_pc != Some(pc) {
                        spare
                            .directories
                            .push(BackingPage::new(Level::Directory, self.hierarchy.clone())?);
                    }
                    spare
                        .tables
                        .push(BackingPage::new(Level::Table, self.hierarchy.clone())?);
                }
                Some(directory) if directory.tables[pd].is_none() => {
                    spare
                        .tables
                        .push(BackingPage::new(Level::Table, self.hierarchy.clone())?);
                }
                Some(_) => {}
            }
            previous_pc = Some(pc);
        }

        Ok(spare)
    }

    /// Installs one page, creating the directory and table if they are missing.
    ///
    /// Ports upstream `pvr_page_create()` together with the
    /// `pvr_page_table_l{1,0}_get_or_insert()` pair.
    fn insert_page(
        &mut self,
        addr: u64,
        daddr: Daddr,
        flags: u64,
        spare: &mut Spare,
        journal: &mut Journal,
    ) -> PvrResult<()> {
        let (pc, pd, pt) = split_addr(addr)?;

        if self.directories[pc].is_none() {
            let page = spare.directories.pop().ok_or(PvrError::NoMemory)?;
            // The child is fully written and flushed before the parent entry
            // makes it reachable — upstream's `wmb()` before
            // `pvr_page_table_l2_insert()`, which `BackingPage::new`'s sync
            // provides here.
            self.root.store(pc, catalogue_entry(page.daddr())?)?;
            page.published.store(true, Ordering::Relaxed);
            let mut tables = Vec::new();
            tables.resize_with(ENTRIES_PD, || None);
            self.directories[pc] = Some(Directory {
                page,
                tables,
                valid: 0,
            });
            self.valid += 1;
            journal.directories.push(pc);
        }

        let directory = self.directories[pc]
            .as_mut()
            .expect("just inserted if absent");
        if directory.tables[pd].is_none() {
            let page = spare.tables.pop().ok_or(PvrError::NoMemory)?;
            directory.page.store(pd, directory_entry(page.daddr())?)?;
            page.published.store(true, Ordering::Relaxed);
            directory.tables[pd] = Some(Table { page, valid: 0 });
            directory.valid += 1;
            journal.tables.push((pc, pd));
        }

        let table = directory.tables[pd].as_mut().expect("just inserted");
        // Upstream's `pvr_page_create()` returns `-EEXIST` here. `PvrError` has
        // no such variant and `DrmError` has nothing that maps to `EEXIST`, so
        // this reports `EINVAL`; see `ABI.md` compatibility limits. Reaching it means
        // the caller's overlap bookkeeping is already wrong, so the value user
        // space sees matters less than the mapping not being silently replaced.
        if is_valid_page(table.page.load(pt)?) {
            return Err(PvrError::InvalidArgument);
        }
        table.page.store(pt, page_entry(daddr, flags)?)?;
        table.valid += 1;

        Ok(())
    }

    /// Removes one page and reclaims whatever became empty.
    ///
    /// Ports upstream `pvr_page_destroy()` and the cascade through
    /// `pvr_page_table_l0_remove()` → `pvr_page_table_l1_remove()` →
    /// `pvr_page_table_l2_remove()`.
    fn remove_page(&mut self, addr: u64) -> PvrResult<()> {
        let (pc, pd, pt) = split_addr(addr)?;

        let Some(directory) = self.directories[pc].as_mut() else {
            return Ok(());
        };
        let Some(table) = directory.tables[pd].as_mut() else {
            return Ok(());
        };
        if !is_valid_page(table.page.load(pt)?) {
            return Ok(());
        }

        table.page.store(pt, 0)?;
        table.valid -= 1;
        if table.valid == 0 {
            directory.page.store(pd, 0)?;
            directory.tables[pd] = None;
            directory.valid -= 1;
            if directory.valid == 0 {
                self.root.store(pc, 0)?;
                self.directories[pc] = None;
                self.valid -= 1;
            }
        }
        Ok(())
    }

    /// Puts the hierarchy back the way a failed [`Self::map`] found it.
    ///
    /// The first pass removes the pages that were written, which reclaims every
    /// intermediate table that only existed to hold them — upstream's
    /// `err_destroy_pages` path, and the one that does the work.
    ///
    /// The two passes after it catch a directory or leaf table that was
    /// inserted but never received an entry, which the first pass cannot see
    /// because it is not empty *as a result of* an unmap. Preallocating up front
    /// makes that unreachable with the current preallocation: every failure the loop can produce happens
    /// on a page whose table already existed, so a table this map created always
    /// has the entry that caused it to be created. The passes are here anyway,
    /// because the alternative is a comment asserting that property and an
    /// invariant that quietly stops holding the first time a new failure is
    /// added to the loop. Upstream patches the same hole in one specific place
    /// (`pvr_mmu_op_context_load_tables()`, removing an L1 whose L0 insert
    /// failed); a journal covers every case rather than one.
    ///
    /// Errors are logged and swallowed: this runs while returning an earlier
    /// error, and the caller needs that one, not a cache-maintenance failure
    /// from the cleanup.
    fn unwind(&mut self, start: u64, journal: &Journal) {
        for page in 0..journal.pages {
            if let Err(err) = self.remove_page(start + page * DEVICE_PAGE_SIZE) {
                ostd::error!("could not unwind a partial GPU mapping: {:?}", err);
            }
        }

        for (pc, pd) in journal.tables.iter().copied() {
            let Some(directory) = self.directories[pc].as_mut() else {
                continue;
            };
            if directory.tables[pd].as_ref().is_some_and(|t| t.valid == 0) {
                if let Err(err) = directory.page.store(pd, 0) {
                    ostd::error!("could not unwind an empty page table: {:?}", err);
                }
                directory.tables[pd] = None;
                directory.valid -= 1;
            }
        }

        for pc in journal.directories.iter().copied() {
            if self.directories[pc].as_ref().is_some_and(|d| d.valid == 0) {
                if let Err(err) = self.root.store(pc, 0) {
                    ostd::error!("could not unwind an empty page directory: {:?}", err);
                }
                self.directories[pc] = None;
                self.valid -= 1;
            }
        }
    }
}

/// Intermediate tables allocated up front by [`PageTable::preallocate`].
///
/// Anything left over is dropped when the map returns, freeing its pages.
#[derive(Debug, Default)]
struct Spare {
    directories: Vec<BackingPage>,
    tables: Vec<BackingPage>,
}

/// What one [`PageTable::map`] has changed so far, so that it can be undone.
#[derive(Debug, Default)]
struct Journal {
    /// Leaf entries written, counted from the start of the mapping.
    pages: u64,
    /// Catalogue slots this map filled.
    directories: Vec<usize>,
    /// Directory slots this map filled, as `(catalogue index, directory index)`.
    tables: Vec<(usize, usize)>,
}

/// Bit position of the page-directory index. Upstream
/// `ROGUE_MMUCTRL_VADDR_PD_INDEX_SHIFT`, i.e. one directory entry per 2 MiB.
const DIRECTORY_SHIFT: u32 = ctrl::ROGUE_MMUCTRL_VADDR_PD_INDEX_SHIFT;

/// Splits a device-virtual address into its three table indices.
///
/// Ports upstream `pvr_page_table_l{2,1,0}_idx()`. Upstream does not bounds
/// check; an address above the 40-bit space would silently alias index zero, so
/// this rejects it.
fn split_addr(addr: u64) -> PvrResult<(usize, usize, usize)> {
    if addr >= ADDR_SPACE_SIZE {
        return Err(PvrError::InvalidArgument);
    }
    let pc = (addr & !ctrl::ROGUE_MMUCTRL_VADDR_PC_INDEX_CLRMSK)
        >> ctrl::ROGUE_MMUCTRL_VADDR_PC_INDEX_SHIFT;
    let pd = (addr & !ctrl::ROGUE_MMUCTRL_VADDR_PD_INDEX_CLRMSK)
        >> ctrl::ROGUE_MMUCTRL_VADDR_PD_INDEX_SHIFT;
    let pt = (addr & !ctrl::ROGUE_MMUCTRL_VADDR_PT_INDEX_CLRMSK)
        >> ctrl::ROGUE_MMUCTRL_PAGE_4KB_RANGE_SHIFT;
    Ok((pc as usize, pd as usize, pt as usize))
}

/// Checks that a table's bus address can be named by a parent entry.
///
/// Both parent formats keep bits 39..12, so anything else in the address would
/// be dropped without trace.
fn checked_table_base(daddr: Daddr) -> PvrResult<u64> {
    let daddr = daddr as u64;
    if daddr >= ADDR_SPACE_SIZE || !daddr.is_multiple_of(BACKING_PAGE_SIZE as u64) {
        return Err(PvrError::InvalidArgument);
    }
    Ok(daddr)
}

/// Builds a page-catalogue entry pointing at a page directory.
///
/// Ports upstream `pvr_page_table_l2_entry_raw_set()`. The address is shifted
/// down by the 4 KiB alignment the field assumes and back up into the field's
/// position, which is how 28 address bits fit a 32-bit entry alongside the two
/// status bits.
///
/// `ENTRY_PENDING` stays clear. Upstream sets it false too: it is the hardware's
/// hook for demand paging, telling the MMU to stall rather than fault while the
/// driver fills an entry in, and this driver populates every entry before making
/// it reachable.
fn catalogue_entry(child: Daddr) -> PvrResult<u64> {
    let base = (checked_table_base(child)? >> ctrl::ROGUE_MMUCTRL_PC_DATA_PD_BASE_ALIGNSHIFT)
        << ctrl::ROGUE_MMUCTRL_PC_DATA_PD_BASE_SHIFT;
    Ok(
        (base & !ctrl::ROGUE_MMUCTRL_PC_DATA_PD_BASE_CLRMSK & u32::MAX as u64)
            | ctrl::ROGUE_MMUCTRL_PC_DATA_VALID_EN,
    )
}

/// Builds a page-directory entry pointing at a leaf page table.
///
/// Ports upstream `pvr_page_table_l1_entry_raw_set()`. Unlike the catalogue
/// entry the address needs no shifting — the field sits where the address bits
/// already are — and it carries the page size that the leaf table's entries are
/// to be interpreted with.
fn directory_entry(child: Daddr) -> PvrResult<u64> {
    let base = checked_table_base(child)? & !ctrl::ROGUE_MMUCTRL_PT_BASE_4KB_RANGE_CLRMSK;
    Ok(base
        | ctrl::ROGUE_MMUCTRL_PD_DATA_VALID_EN
        | ((ctrl::ROGUE_MMUCTRL_PD_DATA_PAGE_SIZE_4KB as u64)
            << ctrl::ROGUE_MMUCTRL_PD_DATA_PAGE_SIZE_SHIFT))
}

/// Builds a leaf entry pointing at a page of a buffer object.
///
/// Ports upstream `pvr_page_table_l0_entry_raw_set()`.
fn page_entry(daddr: Daddr, flags: u64) -> PvrResult<u64> {
    let daddr = daddr as u64;
    if daddr >= ADDR_SPACE_SIZE || !daddr.is_multiple_of(DEVICE_PAGE_SIZE) {
        return Err(PvrError::InvalidArgument);
    }
    Ok((daddr & !ctrl::ROGUE_MMUCTRL_PAGE_4KB_RANGE_CLRMSK)
        | ctrl::ROGUE_MMUCTRL_PT_DATA_VALID_EN
        | flags)
}

/// Whether a leaf entry describes a mapped page.
fn is_valid_page(entry: u64) -> bool {
    entry & ctrl::ROGUE_MMUCTRL_PT_DATA_VALID_EN != 0
}

/// Turns a buffer object's creation flags into leaf-entry permission bits.
///
/// Ports upstream `pvr_page_flags_raw_create()` as called from `pvr_mmu_map()`.
/// Two of its four arguments are constants at the only call site and stay
/// constant here:
///
/// - **read only** is false. The uapi has no read-only mapping flag, so there is
///   nothing for user space to ask for.
/// - **cache coherent** is false. That bit tells the GPU a page needs no cache
///   maintenance between CPU and device access, which on the K1 is never true —
///   it is the hardware property `CONVENTIONS.md` DMA contract exists because the SoC
///   lacks. Setting it would be the register-level spelling of the same mistake
///   as using `DmaCoherent`.
fn page_flags(bo_flags: u64) -> u64 {
    use crate::uapi::bo_flags;

    let mut flags = 0;
    if bo_flags & bo_flags::BYPASS_DEVICE_CACHE != 0 {
        flags |= ctrl::ROGUE_MMUCTRL_PT_DATA_SLC_BYPASS_CTRL_EN;
    }
    if bo_flags & bo_flags::PM_FW_PROTECT != 0 {
        flags |= ctrl::ROGUE_MMUCTRL_PT_DATA_PM_META_PROTECT_EN;
    }
    flags
}

/// Records that an MMU cache invalidation is needed before the next submission.
///
/// Deliberately does not flush. See the module documentation.
///
/// Ports upstream `pvr_mmu_set_flush_flags()`. `Release` pairs with the
/// `Acquire` in [`take_flush_flags`] so that every page table write this flag
/// stands for — and its `cbo.clean` — is complete before the flush path can
/// observe the flag and tell the firmware the tables are ready.
pub fn request_flush(device: &PvrDevice, flags: u32) {
    device.mmu_flush_flags.fetch_or(flags, Ordering::Release);
}

/// Records that every level of the MMU cache needs invalidating.
///
/// Ports upstream `pvr_mmu_flush_request_all()`.
pub fn flush_request_all(device: &PvrDevice) {
    request_flush(device, flush_flags::LEVEL_2);
}

/// The invalidation flags accumulated so far, without clearing them.
pub fn pending_flush_flags(device: &PvrDevice) -> u32 {
    device.mmu_flush_flags.load(Ordering::Acquire)
}

/// Takes the accumulated invalidation flags, leaving none.
///
/// Ports the `atomic_xchg` in upstream `pvr_mmu_flush_exec()`. Separate from
/// [`flush_if_pending`] because the caller must not lose the flags: they may
/// only be cleared by code that is able to report a failure to deliver the
/// invalidation, since a dropped flag leaves the GPU walking a stale cached
/// copy of a page table with no further symptom.
pub fn take_flush_flags(device: &PvrDevice) -> u32 {
    device.mmu_flush_flags.swap(0, Ordering::Acquire)
}

/// Backing detached from a VM but potentially cached by the GPU.
#[derive(Debug, Default)]
pub(crate) struct RetiredMappings {
    buffers: Vec<Arc<super::gem::PvrObject>>,
    pages: Vec<DmaStream>,
}

impl RetiredMappings {
    fn is_empty(&self) -> bool {
        self.buffers.is_empty() && self.pages.is_empty()
    }
}

impl Drop for RetiredMappings {
    fn drop(&mut self) {
        for _ in &self.pages {
            crate::live::note_mmu_page_free();
        }
    }
}

/// Keeps removed BO frames alive until stale GPU translations are invalidated.
pub(crate) fn retire_buffer(device: &PvrDevice, object: Arc<super::gem::PvrObject>) {
    if device.quiesced.load(Ordering::Acquire) {
        return;
    }
    device.retired_mappings.lock().buffers.push(object);
    flush_request_all(device);
}

/// Releases quarantined translations only after a complete hardware stop.
pub(crate) fn release_after_stop(device: &PvrDevice) {
    let _flush = device.mmu_flush_lock.lock();
    if !device.quiesced.load(Ordering::Acquire) {
        return;
    }
    let retired = {
        let mut pending = device.retired_mappings.lock();
        core::mem::take(&mut *pending)
    };
    drop(retired);
}

/// Issues pending invalidation and releases retired backing only after acknowledgement.
///
/// A sleeping mutex serializes callers through the entire firmware round trip.
/// Reset read exclusion covers each send; failed delivery releases it before reset.
/// Retirements added during this batch remain owned by the next batch.
pub fn flush_if_pending(device: &PvrDevice) -> PvrResult<()> {
    let _flush = device.mmu_flush_lock.lock();
    device.check_alive()?;
    let Some(fw) = device.fw.get() else {
        return Ok(());
    };
    if device.kccb.get().is_none() {
        return Ok(());
    }

    // Detach before consuming flags. Every later retirement requests a fresh full flush.
    let mut retired = {
        let mut pending = device.retired_mappings.lock();
        core::mem::take(&mut *pending)
    };
    let flags = take_flush_flags(device)
        | if retired.is_empty() {
            0
        } else {
            flush_flags::LEVEL_2
        };
    if flags == 0 {
        return Ok(());
    }

    let mut cmd = RogueFwifKccbCmd::new_zeroed();
    cmd.cmd_type = crate::sched::abi::KCCB_CMD_MMUCACHE;
    {
        let data = cmd.cmd_data.mmu_cache_data_mut();
        data.cache_flags = flags;
        data.mmu_cache_sync_fw_addr = fw.mem.mmucache_sync.fw_addr();
        data.mmu_cache_sync_update_value = 0;
    }

    let send = || {
        let _serialise = device.reset_lock.read();
        device.check_alive()?;
        crate::sched::ccb::send_and_wait(device, &cmd).and_then(|result| {
            if result & crate::sched::abi::KCCB_RTN_SLOT_POLL_FAILURE != 0 {
                Err(PvrError::Timeout)
            } else {
                Ok(result)
            }
        })
    };
    let result = send().or_else(|_| {
        ostd::error!("firmware did not acknowledge MMU invalidation; resetting the GPU");
        crate::power::reset(device, crate::power::ResetKind::Hard)?;
        send()
    });
    if result.is_ok() {
        // The acknowledgement covers this detached batch, never retirements added later.
        drop(retired);
        return Ok(());
    }

    // Return ownership before losing the device. An unconfirmed stop quarantines it.
    {
        let mut pending = device.retired_mappings.lock();
        pending.buffers.append(&mut retired.buffers);
        pending.pages.append(&mut retired.pages);
    }
    request_flush(device, flags);
    crate::power::lose_device(device);
    Err(PvrError::DeviceLost)
}

/// The `pvr_rogue_fwif.h` flag values this module's levels are built from.
///
/// Referenced here so that a regeneration that dropped one of them fails to
/// compile rather than silently narrowing an invalidation.
const _: () = {
    assert!(
        fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_PMTLB & fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_TLB != 0
    );
    assert!(flush_flags::LEVEL_0 & flush_flags::LEVEL_1 == flush_flags::LEVEL_0);
    assert!(flush_flags::LEVEL_1 & flush_flags::LEVEL_2 == flush_flags::LEVEL_1);
};

/// Tests for the entry encodings, the sync discipline and the unwind.
///
/// The encoding tests are pure and run on the host too; everything that walks a
/// hierarchy needs [`DmaStream::alloc`] and so a running kernel.
#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;
    use crate::uapi::bo_flags;
    #[cfg(ktest)]
    use crate::{
        device::{FwVersion, GpuId, RegisterWindow},
        device_info::DeviceInfo,
        mm::{gem::PvrObject, vm::DeviceAddr},
    };

    /// The three index fields must tile the 40-bit address exactly: ten bits of
    /// catalogue, nine of directory, nine of table, twelve of offset. An overlap
    /// or a gap would alias two addresses onto one entry.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn address_split_tiles_the_whole_space() {
        assert_eq!(split_addr(0).unwrap(), (0, 0, 0));

        // The last addressable page.
        let last = ADDR_SPACE_SIZE - DEVICE_PAGE_SIZE;
        assert_eq!(
            split_addr(last).unwrap(),
            (ENTRIES_PC - 1, ENTRIES_PD - 1, ENTRIES_PT - 1)
        );
        // One past the top has no entry at all.
        assert!(split_addr(ADDR_SPACE_SIZE).is_err());

        // Each index advances at its own stride and nothing else moves.
        assert_eq!(split_addr(DEVICE_PAGE_SIZE).unwrap(), (0, 0, 1));
        assert_eq!(split_addr(1 << DIRECTORY_SHIFT).unwrap(), (0, 1, 0));
        assert_eq!(
            split_addr(1 << ctrl::ROGUE_MMUCTRL_VADDR_PC_INDEX_SHIFT).unwrap(),
            (1, 0, 0)
        );

        // A general address, decomposed by hand.
        let addr = (0x321u64 << 30) | (0x0abu64 << 21) | (0x1cdu64 << 12) | 0x800;
        assert_eq!(split_addr(addr).unwrap(), (0x321, 0x0ab, 0x1cd));
    }

    /// The three entry formats put the child address in three different places.
    /// Recovering it the way the hardware would is the only way to tell a
    /// forgotten shift from a correct one.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn entry_encodings_round_trip_their_addresses() {
        let child: Daddr = 0x9_8765_4000;

        // Catalogue: address bits 39..12 live in entry bits 31..4.
        let pc = catalogue_entry(child).unwrap();
        assert!(pc <= u32::MAX as u64, "a catalogue entry is 32 bits wide");
        assert_ne!(pc & ctrl::ROGUE_MMUCTRL_PC_DATA_VALID_EN, 0);
        assert_eq!(pc & ctrl::ROGUE_MMUCTRL_PC_DATA_ENTRY_PENDING_EN, 0);
        let recovered = ((pc & !ctrl::ROGUE_MMUCTRL_PC_DATA_PD_BASE_CLRMSK)
            >> ctrl::ROGUE_MMUCTRL_PC_DATA_PD_BASE_SHIFT)
            << ctrl::ROGUE_MMUCTRL_PC_DATA_PD_BASE_ALIGNSHIFT;
        assert_eq!(recovered, child as u64);

        // Directory: address bits 39..12 stay where they are.
        let pd = directory_entry(child).unwrap();
        assert_ne!(pd & ctrl::ROGUE_MMUCTRL_PD_DATA_VALID_EN, 0);
        assert_eq!(
            pd & !ctrl::ROGUE_MMUCTRL_PT_BASE_4KB_RANGE_CLRMSK,
            child as u64
        );
        // 4 KiB pages, which is what makes the leaf table 512 entries long.
        assert_eq!(
            (pd & !ctrl::ROGUE_MMUCTRL_PD_DATA_PAGE_SIZE_CLRMSK)
                >> ctrl::ROGUE_MMUCTRL_PD_DATA_PAGE_SIZE_SHIFT,
            ctrl::ROGUE_MMUCTRL_PD_DATA_PAGE_SIZE_4KB as u64
        );

        // Leaf: likewise, plus the flag bits.
        let pt = page_entry(child, 0).unwrap();
        assert!(is_valid_page(pt));
        assert_eq!(
            pt & !ctrl::ROGUE_MMUCTRL_PAGE_4KB_RANGE_CLRMSK,
            child as u64
        );

        // Anything outside the 40-bit window is refused rather than truncated.
        assert!(catalogue_entry(ADDR_SPACE_SIZE as usize).is_err());
        assert!(directory_entry(ADDR_SPACE_SIZE as usize).is_err());
        assert!(page_entry(ADDR_SPACE_SIZE as usize, 0).is_err());
        // As is a misaligned one.
        assert!(page_entry(0x1000 + 8, 0).is_err());
    }

    /// The two buffer-object flags that reach the page tables, and the two bits
    /// that must never be set on the K1.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn page_flags_follow_the_buffer_objects_flags() {
        assert_eq!(page_flags(0), 0);
        assert_eq!(
            page_flags(bo_flags::BYPASS_DEVICE_CACHE),
            ctrl::ROGUE_MMUCTRL_PT_DATA_SLC_BYPASS_CTRL_EN
        );
        assert_eq!(
            page_flags(bo_flags::PM_FW_PROTECT),
            ctrl::ROGUE_MMUCTRL_PT_DATA_PM_META_PROTECT_EN
        );

        // Never read-only, and never cache-coherent: the K1 has no hardware DMA
        // coherency, so a page that claims to need no cache maintenance is a
        // lie the GPU would act on.
        for flags in 0..=bo_flags::MASK {
            let raw = page_flags(flags);
            assert_eq!(raw & ctrl::ROGUE_MMUCTRL_PT_DATA_READ_ONLY_EN, 0);
            assert_eq!(raw & ctrl::ROGUE_MMUCTRL_PT_DATA_CC_EN, 0);
        }
    }

    /// The three invalidation levels nest, so a catalogue change invalidates
    /// everything below it.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn flush_levels_nest() {
        assert_eq!(Level::Table.flush_flags(), flush_flags::LEVEL_0);
        assert_eq!(Level::Directory.flush_flags(), flush_flags::LEVEL_1);
        assert_eq!(Level::Catalogue.flush_flags(), flush_flags::LEVEL_2);
        assert_eq!(
            flush_flags::LEVEL_2 & flush_flags::LEVEL_0,
            flush_flags::LEVEL_0
        );
        // The TLB must always be invalidated, whatever changed.
        assert_ne!(
            flush_flags::LEVEL_0 & fwif::ROGUE_FWIF_MMUCACHEDATA_FLAGS_TLB,
            0
        );
    }

    #[cfg(ktest)]
    fn test_device() -> Arc<PvrDevice> {
        PvrDevice::new(
            RegisterWindow::simulated(),
            GpuId::K1_BXE_2_32,
            DeviceInfo::default(),
            FwVersion { major: 1, minor: 1 },
        )
    }

    #[cfg(ktest)]
    fn test_object(pages: usize) -> Arc<PvrObject> {
        PvrObject::new(
            (pages * PAGE_SIZE) as u64,
            bo_flags::ALLOW_CPU_USERSPACE_ACCESS,
        )
        .expect("buffer object")
    }

    /// An address in the general heap, far enough in that neighbouring spans are
    /// untouched.
    #[cfg(ktest)]
    const BASE: u64 = crate::rogue::heap::ROGUE_GENERAL_HEAP_BASE;

    /// A fresh hierarchy has a catalogue and nothing else, and its zeroes have
    /// been made visible to the GPU.
    #[ktest]
    fn a_new_hierarchy_is_one_flushed_catalogue() {
        let table = PageTable::new(test_device()).expect("catalogue");

        assert_eq!(table.directory_count(), 0);
        assert_eq!(table.table_count(), 0);
        // The zero fill is entries too, and they have been flushed.
        assert_eq!(table.syncs_issued(), 1);
        assert!(table.root_daddr().is_multiple_of(BACKING_PAGE_SIZE));
        assert_eq!(table.page_entry(DeviceAddr(BASE)).unwrap(), None);
    }

    /// **The K1 rule.** Every entry written is flushed, and the counter is the
    /// only way to assert that without reading the code.
    ///
    /// Mapping into virgin address space also allocates a directory and a leaf
    /// table, so the first map costs more than its pages; the second map, into
    /// the table the first one built, is exactly one flush per page.
    #[ktest]
    fn every_entry_write_is_synced() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(6);

        let before = table.syncs_issued();
        table
            .map(DeviceAddr(BASE), &object, 0, 3 * DEVICE_PAGE_SIZE)
            .expect("map");
        assert_eq!(
            table.syncs_issued() - before,
            // Two table allocations (their zero fill), the catalogue entry, the
            // directory entry, and one leaf entry per page.
            2 + 1 + 1 + 3,
            "every table page allocated and every entry written must be flushed"
        );

        // Three more pages into the leaf table that now exists.
        let before = table.syncs_issued();
        table
            .map(
                DeviceAddr(BASE + 3 * DEVICE_PAGE_SIZE),
                &object,
                3 * DEVICE_PAGE_SIZE,
                3 * DEVICE_PAGE_SIZE,
            )
            .expect("map");
        assert_eq!(
            table.syncs_issued() - before,
            3,
            "mapping N pages into an existing table is exactly N flushes"
        );

        // Clearing an entry is a write and needs the same flush: six leaf
        // entries, then the directory entry and the catalogue entry as the two
        // emptied tables are reclaimed.
        let before = table.syncs_issued();
        table
            .unmap(DeviceAddr(BASE), 6 * DEVICE_PAGE_SIZE)
            .expect("unmap");
        assert_eq!(table.syncs_issued() - before, 6 + 1 + 1);
    }

    /// A mapping must name the object's own pages, in order, and touch nothing
    /// on either side of itself.
    #[ktest]
    fn map_writes_one_entry_per_page_and_leaves_neighbours_alone() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(3);

        table
            .map(
                DeviceAddr(BASE + DEVICE_PAGE_SIZE),
                &object,
                0,
                3 * DEVICE_PAGE_SIZE,
            )
            .expect("map");

        for page in 0..3u64 {
            let addr = DeviceAddr(BASE + (page + 1) * DEVICE_PAGE_SIZE);
            let entry = table.page_entry(addr).unwrap().expect("mapped");
            let daddr = object.daddr_at((page * DEVICE_PAGE_SIZE) as usize).unwrap();
            assert_eq!(entry, page_entry(daddr, 0).unwrap(), "page {page}");
        }
        // The pages either side are in the same leaf table and still invalid.
        assert_eq!(table.page_entry(DeviceAddr(BASE)).unwrap(), Some(0));
        assert_eq!(
            table
                .page_entry(DeviceAddr(BASE + 4 * DEVICE_PAGE_SIZE))
                .unwrap(),
            Some(0)
        );
    }

    /// A buffer object's flags reach the leaf entries.
    #[ktest]
    fn buffer_object_flags_reach_the_leaf_entries() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = PvrObject::new(DEVICE_PAGE_SIZE, bo_flags::BYPASS_DEVICE_CACHE)
            .expect("uncached object");

        table
            .map(DeviceAddr(BASE), &object, 0, DEVICE_PAGE_SIZE)
            .expect("map");

        let entry = table.page_entry(DeviceAddr(BASE)).unwrap().unwrap();
        assert_ne!(entry & ctrl::ROGUE_MMUCTRL_PT_DATA_SLC_BYPASS_CTRL_EN, 0);
        assert_eq!(entry & ctrl::ROGUE_MMUCTRL_PT_DATA_CC_EN, 0);
    }

    /// **Intermediate reclaim.** An unmap that empties a leaf table must free it
    /// *and* clear the directory entry that pointed at it, all the way up. A
    /// directory left behind with no tables makes a 1 GiB span look present.
    #[ktest]
    fn emptied_intermediate_levels_are_reclaimed() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(2);

        // Two pages 2 MiB apart: one directory, two leaf tables.
        let second = BASE + (1 << DIRECTORY_SHIFT);
        table
            .map(DeviceAddr(BASE), &object, 0, DEVICE_PAGE_SIZE)
            .expect("first");
        table
            .map(
                DeviceAddr(second),
                &object,
                DEVICE_PAGE_SIZE,
                DEVICE_PAGE_SIZE,
            )
            .expect("second");
        assert_eq!(table.directory_count(), 1);
        assert_eq!(table.table_count(), 2);

        // Dropping the first leaves the directory alive for the second.
        table
            .unmap(DeviceAddr(BASE), DEVICE_PAGE_SIZE)
            .expect("unmap first");
        assert_eq!(table.directory_count(), 1);
        assert_eq!(table.table_count(), 1);
        assert_eq!(table.page_entry(DeviceAddr(BASE)).unwrap(), None);

        // Dropping the second takes the directory and the catalogue entry too.
        table
            .unmap(DeviceAddr(second), DEVICE_PAGE_SIZE)
            .expect("unmap second");
        assert_eq!(table.directory_count(), 0);
        assert_eq!(table.table_count(), 0);
        assert_eq!(table.page_entry(DeviceAddr(second)).unwrap(), None);
        // And the catalogue entry itself is cleared, not just the mirror.
        assert_eq!(table.root.load(split_addr(BASE).unwrap().0).unwrap(), 0);
    }

    /// Mapping over a live page is refused, and the refusal leaves the earlier
    /// mapping exactly as it was.
    #[ktest]
    fn an_overlapping_map_is_refused() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(4);

        table
            .map(DeviceAddr(BASE), &object, 0, 2 * DEVICE_PAGE_SIZE)
            .expect("first");
        let entry = table.page_entry(DeviceAddr(BASE)).unwrap().unwrap();

        // Starts one page into the live mapping, so the very first page of the
        // request collides.
        assert_eq!(
            table.map(
                DeviceAddr(BASE + DEVICE_PAGE_SIZE),
                &object,
                0,
                2 * DEVICE_PAGE_SIZE
            ),
            Err(PvrError::InvalidArgument)
        );

        // The original two pages are untouched and the speculative third is not
        // there: all or nothing.
        assert_eq!(table.page_entry(DeviceAddr(BASE)).unwrap(), Some(entry));
        assert!(is_valid_page(
            table
                .page_entry(DeviceAddr(BASE + DEVICE_PAGE_SIZE))
                .unwrap()
                .unwrap()
        ));
        assert!(!is_valid_page(
            table
                .page_entry(DeviceAddr(BASE + 2 * DEVICE_PAGE_SIZE))
                .unwrap()
                .unwrap()
        ));
    }

    /// **The full unwind.** A map that fails part way through must leave no
    /// entries *and* no intermediate tables — including a directory and a leaf
    /// table it allocated for the very page that failed.
    #[ktest]
    fn a_failed_map_unwinds_every_level_it_created() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(4);

        // Occupy one page 2 MiB along, so the collision happens in a span whose
        // directory and leaf table this map did not create...
        let far = BASE + (1 << DIRECTORY_SHIFT);
        table
            .map(DeviceAddr(far), &object, 0, DEVICE_PAGE_SIZE)
            .expect("obstacle");
        let directories = table.directory_count();
        let tables = table.table_count();
        let syncs = table.syncs_issued();

        // ...and map a range that starts in virgin space and runs into it. The
        // walk allocates a leaf table for the first span, fills it, then hits
        // the obstacle.
        let start = far - DEVICE_PAGE_SIZE;
        assert_eq!(
            table.map(DeviceAddr(start), &object, 0, 2 * DEVICE_PAGE_SIZE),
            Err(PvrError::InvalidArgument)
        );

        // Nothing survives the failure: not the page it managed to write, not
        // the leaf table it had to allocate to write it.
        assert_eq!(table.page_entry(DeviceAddr(start)).unwrap(), None);
        assert_eq!(table.directory_count(), directories);
        assert_eq!(table.table_count(), tables);
        // The obstacle is still mapped.
        assert!(is_valid_page(
            table.page_entry(DeviceAddr(far)).unwrap().unwrap()
        ));
        // The unwind is itself made of entry writes, so it flushed too.
        assert!(table.syncs_issued() > syncs);
    }

    /// The unwind reaches the page *directory* too, not just the leaf table.
    ///
    /// A range that crosses a 1 GiB boundary into an occupied span makes the
    /// failing map allocate a whole branch — directory, table, one entry — and
    /// then fail. All three must go.
    #[ktest]
    fn a_failed_map_unwinds_a_directory_it_created() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(2);

        // `BASE` is 1 GiB aligned, so the page before it lives in the previous
        // catalogue entry, which nothing has touched.
        assert!(BASE.is_multiple_of(1 << ctrl::ROGUE_MMUCTRL_VADDR_PC_INDEX_SHIFT));
        table
            .map(DeviceAddr(BASE), &object, 0, DEVICE_PAGE_SIZE)
            .expect("obstacle");
        assert_eq!((table.directory_count(), table.table_count()), (1, 1));

        assert_eq!(
            table.map(
                DeviceAddr(BASE - DEVICE_PAGE_SIZE),
                &object,
                0,
                2 * DEVICE_PAGE_SIZE
            ),
            Err(PvrError::InvalidArgument)
        );

        assert_eq!(
            (table.directory_count(), table.table_count()),
            (1, 1),
            "the directory and table the failed map created must both be gone"
        );
        assert_eq!(
            table
                .page_entry(DeviceAddr(BASE - DEVICE_PAGE_SIZE))
                .unwrap(),
            None
        );
        assert!(is_valid_page(
            table.page_entry(DeviceAddr(BASE)).unwrap().unwrap()
        ));
    }

    /// Out-of-range and misaligned requests are refused before anything is
    /// written.
    #[ktest]
    fn out_of_range_requests_are_rejected() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(2);
        let syncs = table.syncs_issued();

        // Past the top of the 40-bit space, including a range that wraps.
        assert!(
            table
                .map(DeviceAddr(ADDR_SPACE_SIZE), &object, 0, DEVICE_PAGE_SIZE)
                .is_err()
        );
        assert!(
            table
                .map(
                    DeviceAddr(ADDR_SPACE_SIZE - DEVICE_PAGE_SIZE),
                    &object,
                    0,
                    2 * DEVICE_PAGE_SIZE
                )
                .is_err()
        );
        // The highest page-aligned address there is, which would wrap.
        assert!(
            table
                .map(
                    DeviceAddr(!(DEVICE_PAGE_SIZE - 1)),
                    &object,
                    0,
                    DEVICE_PAGE_SIZE
                )
                .is_err()
        );
        // Misaligned address, misaligned length, zero length.
        assert!(
            table
                .map(DeviceAddr(BASE + 8), &object, 0, DEVICE_PAGE_SIZE)
                .is_err()
        );
        assert!(table.map(DeviceAddr(BASE), &object, 0, 8).is_err());
        assert!(table.map(DeviceAddr(BASE), &object, 0, 0).is_err());
        // Past the end of the object.
        assert!(
            table
                .map(DeviceAddr(BASE), &object, 0, 3 * DEVICE_PAGE_SIZE)
                .is_err()
        );
        assert!(
            table
                .map(
                    DeviceAddr(BASE),
                    &object,
                    2 * DEVICE_PAGE_SIZE,
                    DEVICE_PAGE_SIZE
                )
                .is_err()
        );
        // A misaligned offset into the object has no entry to describe it.
        assert!(
            table
                .map(DeviceAddr(BASE), &object, 8, DEVICE_PAGE_SIZE)
                .is_err()
        );

        // Rejected before any entry was written.
        assert_eq!(table.syncs_issued(), syncs);
        assert_eq!(table.directory_count(), 0);

        // The same bounds apply to unmap.
        assert!(
            table
                .unmap(DeviceAddr(ADDR_SPACE_SIZE), DEVICE_PAGE_SIZE)
                .is_err()
        );
        assert!(table.unmap(DeviceAddr(BASE), 0).is_err());
    }

    /// A mapping that straddles a directory boundary needs two leaf tables, and
    /// the second must be allocated too — the case upstream's preallocation
    /// count can miss.
    #[ktest]
    fn a_mapping_across_a_directory_boundary_gets_both_tables() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let object = test_object(2);

        let start = BASE + (1 << DIRECTORY_SHIFT) - DEVICE_PAGE_SIZE;
        table
            .map(DeviceAddr(start), &object, 0, 2 * DEVICE_PAGE_SIZE)
            .expect("straddling map");

        assert_eq!(table.table_count(), 2);
        assert!(is_valid_page(
            table.page_entry(DeviceAddr(start)).unwrap().unwrap()
        ));
        assert!(is_valid_page(
            table
                .page_entry(DeviceAddr(start + DEVICE_PAGE_SIZE))
                .unwrap()
                .unwrap()
        ));

        table
            .unmap(DeviceAddr(start), 2 * DEVICE_PAGE_SIZE)
            .expect("unmap");
        assert_eq!(table.table_count(), 0);
        assert_eq!(table.directory_count(), 0);
    }

    /// Unmapping a hole is not an error; upstream skips it too.
    #[ktest]
    fn unmapping_an_absent_range_is_a_no_op() {
        let mut table = PageTable::new(test_device()).expect("catalogue");
        let syncs = table.syncs_issued();

        table
            .unmap(DeviceAddr(BASE), 4 * DEVICE_PAGE_SIZE)
            .expect("no-op unmap");
        assert_eq!(table.syncs_issued(), syncs, "nothing was written");
    }

    /// The flush flags accumulate as tables change, nest by level, and are
    /// cleared only by the caller that is about to deliver them.
    #[ktest]
    fn flush_flags_accumulate_and_clear() {
        let device = test_device();
        assert_eq!(pending_flush_flags(&device), 0);

        let mut table = PageTable::new(device.clone()).expect("catalogue");
        // Allocating the catalogue changes nothing the GPU can see yet.
        assert_eq!(pending_flush_flags(&device), 0);

        let object = test_object(1);
        table
            .map(DeviceAddr(BASE), &object, 0, DEVICE_PAGE_SIZE)
            .expect("map");
        // The map touched all three levels, so all three are pending.
        assert_eq!(pending_flush_flags(&device), flush_flags::LEVEL_2);

        // Taking them leaves none, and is the only thing that clears them.
        assert_eq!(take_flush_flags(&device), flush_flags::LEVEL_2);
        assert_eq!(pending_flush_flags(&device), 0);

        // A map into the existing leaf table only invalidates the leaf level.
        let object = test_object(1);
        table
            .map(
                DeviceAddr(BASE + DEVICE_PAGE_SIZE),
                &object,
                0,
                DEVICE_PAGE_SIZE,
            )
            .expect("map");
        assert_eq!(pending_flush_flags(&device), flush_flags::LEVEL_0);

        // Requests are cumulative, never replacing.
        request_flush(&device, flush_flags::LEVEL_1);
        assert_eq!(pending_flush_flags(&device), flush_flags::LEVEL_1);
        flush_request_all(&device);
        assert_eq!(pending_flush_flags(&device), flush_flags::LEVEL_2);
        assert_eq!(take_flush_flags(&device), flush_flags::LEVEL_2);
    }

    /// With nothing pending, or with no firmware to tell, the flush path is a
    /// no-op rather than a round trip — and it must not eat the flags.
    #[ktest]
    fn flushing_without_firmware_keeps_the_flags() {
        let device = test_device();
        assert_eq!(flush_if_pending(&device), Ok(()));

        flush_request_all(&device);
        // No firmware has been loaded, so there is nobody to send the command
        // to; the flags survive for whoever boots the GPU.
        assert_eq!(flush_if_pending(&device), Ok(()));
        assert_eq!(pending_flush_flags(&device), flush_flags::LEVEL_2);

        // A lost device reports the same errno upstream's `drm_dev_enter()`
        // failure does.
        device.declare_lost();
        assert_eq!(flush_if_pending(&device), Err(PvrError::DeviceLost));
    }
}
