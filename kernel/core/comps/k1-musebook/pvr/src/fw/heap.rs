// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: owned DMA firmware objects with explicit cache policy.

//! The firmware heap and the objects allocated from it.
//!
//! Ports the firmware-object half of `pvr_fw.c` (`pvr_fw_heap_info_init`,
//! `pvr_fw_object_*`, `pvr_fw_object_fw_map`) and the parts of `pvr_gem.c` a
//! firmware object needs.
//!
//! # Why this does not sit on [`crate::mm::gem`]
//!
//! Upstream layers firmware objects on GEM: `pvr_fw_object` owns a
//! `pvr_gem_object`, and `pvr_fw_object_fw_map()` reserves firmware address
//! space out of a `drm_mm`. Two things make that indirection pointless here.
//!
//! Firmware objects are never exported to user space, never mapped into a
//! process, and never shared — the four things GEM exists to provide. And on a
//! MIPS firmware processor they are also never mapped through the Rogue MMU:
//! `pvr_fw_defs_mips::vm_map` is [`crate::mm::vm_mips`], and
//! `pvr_fw_start()` skips `rogue_bif_init()` entirely for MIPS. So a firmware
//! object is exactly a contiguous DMA allocation plus an offset in the firmware
//! heap, which is what [`FwObject`] is.
//!
//! Recorded as a deliberate divergence in `ABI.md` compatibility limits. The firmware
//! sees identical addresses either way; this only changes who owns the pages.
//!
//! # The K1 rule
//!
//! Every allocation here is firmware-visible, and the K1 has no hardware DMA
//! coherency, so every one is a [`DmaStream`] — never a `DmaCoherent`. See
//! `CONVENTIONS.md` DMA contract.
//!
//! Most objects use the write-back linear mapping and
//! [`FwObject`]'s accessors issue `sync_to_device` / `sync_from_device` on the
//! caller's behalf, so a forgotten sync has to be a deliberate use of
//! [`FwObject::dma`] rather than an omission.
//!
//! Small words the firmware writes and the CPU polls are the exception: they
//! take [`FwCache::POLLED`], which is [`DmaStream::alloc_uncached`]. The CPU
//! mapping is write-combining (RISC-V `PBMT_NC`); `sync_*` still succeeds, as
//! a no-op because those aliases bypass the CPU cache. MMIO publication still
//! needs an I/O ordering barrier. The MIPS-side
//! [`FwObject::is_uncached`] flag is a different bit
//! (`PVR_BO_FW_FLAGS_DEVICE_UNCACHED`) and is not overloaded.

use alloc::vec::Vec;

use ostd::mm::{Daddr, HasDaddr, HasSize, PAGE_SIZE, VmIo, dma::DmaStream};
use ostd_pod::Pod;

use crate::{
    error::{PvrError, PvrResult},
    rogue::{fwif::FwAddr, heap::ROGUE_FW_HEAP_BASE},
};

/// Whether the GPU can reach main memory coherently. It cannot, on the K1.
///
/// Named rather than written as a bare `false` at each [`DmaStream::alloc`] call
/// so that grepping for it finds every firmware allocation at once, and so that
/// a port to a coherent SoC is one edit. See `CONVENTIONS.md` DMA contract.
///
/// Not used by [`DmaStream::alloc_uncached`]: those objects never take the
/// write-back path, even when cache maintenance is available.
pub const GPU_CACHE_COHERENT: bool = false;

/// Cache attributes for one firmware-heap allocation.
///
/// Two orthogonal flags. Do not overload them: [`Self::device_uncached`] is
/// the MIPS page-table policy (`PVR_BO_FW_FLAGS_DEVICE_UNCACHED`);
/// [`Self::cpu_uncached`] is the host CPU mapping ([`DmaStream::alloc_uncached`]).
/// Two bare `bool`s in a row would be an easy swap at the fifteen call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwCache {
    /// Firmware maps this uncached. Upstream `PVR_BO_FW_FLAGS_DEVICE_UNCACHED`.
    pub device_uncached: bool,
    /// CPU reaches this through an uncacheable mapping.
    ///
    /// For words the firmware writes and the CPU polls. Large objects and
    /// objects the CPU writes stay `false`.
    pub cpu_uncached: bool,
}

impl FwCache {
    /// Device-uncached, CPU write-back. The common case for large or CPU-written
    /// objects.
    pub const DEVICE_UNCACHED: Self = Self {
        device_uncached: true,
        cpu_uncached: false,
    };

    /// Both sides uncached. For firmware-written words the CPU polls.
    pub const POLLED: Self = Self {
        device_uncached: true,
        cpu_uncached: true,
    };

    /// Both sides cached. Used by tests of the MIPS cached page-table path.
    pub const CACHED: Self = Self {
        device_uncached: false,
        cpu_uncached: false,
    };
}

/// Granularity of the configuration area at the top of the firmware heap.
///
/// Upstream `PVR_ROGUE_FW_CONFIG_HEAP_GRANULARITY`, `SZ_64K`. The three
/// structures the firmware reads before anything else is set up each get one
/// granule at a fixed offset, because the firmware's boot code has those offsets
/// compiled in.
pub const CONFIG_HEAP_GRANULARITY: u32 = 64 * 1024;

/// Size of the configuration area. Upstream `PVR_ROGUE_FW_CONFIG_HEAP_SIZE`.
pub const CONFIG_HEAP_SIZE: u32 = 3 * CONFIG_HEAP_GRANULARITY;

/// Offset of `rogue_fwif_connection_ctl` within the configuration area.
///
/// Upstream `PVR_ROGUE_FWIF_CONNECTION_CTL_OFFSET`.
pub const CONNECTION_CTL_OFFSET: u32 = 0;

/// Offset of `rogue_fwif_osinit` within the configuration area.
///
/// Upstream `PVR_ROGUE_FWIF_OSINIT_OFFSET`.
pub const OSINIT_OFFSET: u32 = CONNECTION_CTL_OFFSET + CONFIG_HEAP_GRANULARITY;

/// Offset of `rogue_fwif_sysinit` within the configuration area.
///
/// Upstream `PVR_ROGUE_FWIF_SYSINIT_OFFSET`.
pub const SYSINIT_OFFSET: u32 = OSINIT_OFFSET + CONFIG_HEAP_GRANULARITY;

/// Size and mask arithmetic for one firmware heap.
///
/// Ports upstream `pvr_fw_heap_info_init()` and the anonymous `fw_heap_info`
/// struct in `pvr_fw_device`.
///
/// The heap is a power-of-two window in GPU address space whose top
/// [`CONFIG_HEAP_SIZE`] bytes hold the fixed-offset configuration structures and
/// whose bottom [`Self::size`] bytes are available for general firmware
/// allocations. Code must start at offset 0 — the firmware processor cannot boot
/// otherwise — which has the useful side effect that firmware address 0 is
/// invalid for every other object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FwHeapInfo {
    /// Base of the heap in GPU address space. Upstream `ROGUE_FW_HEAP_BASE`.
    pub gpu_addr: u64,
    /// Bytes available for general allocations, i.e. the raw size less the
    /// configuration area and the processor's reserved area.
    pub size: u32,
    /// Mask turning a firmware address into an offset within the heap.
    pub offset_mask: u32,
    /// Raw size of the heap, configuration and reserved areas included.
    pub raw_size: u32,
    /// Log2 of [`Self::raw_size`].
    pub log2_size: u32,
    /// Offset of the configuration area within the heap.
    pub config_offset: u32,
    /// Bytes reserved by the firmware processor at the top of the general area.
    pub reserved_size: u32,
}

impl FwHeapInfo {
    /// Derives every size and mask from the raw heap size.
    ///
    /// Ports upstream `pvr_fw_heap_info_init()`. `log2_size` must be in
    /// `20..=31`: below 20 the configuration area would not fit, and at 32 the
    /// `u32` arithmetic upstream uses — and this port keeps, because
    /// `offset_mask` is a `u32` in the firmware ABI — would overflow.
    pub fn new(log2_size: u32, reserved_size: u32) -> PvrResult<Self> {
        if !(20..32).contains(&log2_size) {
            return Err(PvrError::InvalidArgument);
        }
        let raw_size = 1u32 << log2_size;
        let overhead = CONFIG_HEAP_SIZE
            .checked_add(reserved_size)
            .ok_or(PvrError::InvalidArgument)?;
        let size = raw_size
            .checked_sub(overhead)
            .ok_or(PvrError::InvalidArgument)?;
        Ok(Self {
            gpu_addr: ROGUE_FW_HEAP_BASE,
            size,
            offset_mask: raw_size - 1,
            raw_size,
            log2_size,
            config_offset: raw_size - CONFIG_HEAP_SIZE,
            reserved_size,
        })
    }
}

/// One allocation in the firmware heap.
///
/// Ports upstream `struct pvr_fw_object`. Three addresses describe it and they
/// are not interchangeable — see the table in [`crate::fw`]:
///
/// - [`Self::fw_addr`] is what goes into a firmware structure;
/// - [`Self::gpu_addr`] is what the Rogue MMU would use, for the structures that
///   name a GPU address;
/// - [`Self::daddr`] is what the hardware puts on the bus, and what a MIPS page
///   table entry encodes.
pub struct FwObject {
    dma: DmaStream,
    /// Offset from the start of the firmware heap.
    fw_offset: u32,
    /// Firmware address of byte 0. Cached because computing it needs the heap's
    /// mask and the processor's heap base, neither of which the object holds.
    fw_addr: FwAddr,
    /// Base of the heap in GPU address space, for [`Self::gpu_addr`].
    gpu_heap_base: u64,
    /// Requested size. The allocation is rounded up to whole pages; this is what
    /// the caller asked for and what the accessors bound against.
    size: usize,
    /// Whether the firmware must map this uncached. Upstream
    /// `PVR_BO_FW_FLAGS_DEVICE_UNCACHED`, which it sets on every firmware
    /// structure.
    uncached: bool,
}

impl FwObject {
    /// Offset of this object from the start of the firmware heap.
    pub fn fw_offset(&self) -> u32 {
        self.fw_offset
    }

    /// Address the firmware processor sees for byte 0.
    pub fn fw_addr(&self) -> FwAddr {
        self.fw_addr
    }

    /// Address the firmware processor sees for byte `offset`.
    ///
    /// Ports upstream `pvr_fw_object_get_fw_addr_offset()`. Errors if `offset` is
    /// past the end, which upstream cannot because it returns a bare `u32`.
    pub fn fw_addr_at(&self, offset: u32) -> PvrResult<FwAddr> {
        if offset as usize >= self.size {
            return Err(PvrError::InvalidArgument);
        }
        Ok(FwAddr(
            self.fw_addr
                .0
                .checked_add(offset)
                .ok_or(PvrError::InvalidArgument)?,
        ))
    }

    /// Address the Rogue MMU would see for byte 0.
    ///
    /// Ports upstream `pvr_fw_obj_get_gpu_addr()`.
    pub fn gpu_addr(&self) -> crate::mm::vm::DeviceAddr {
        crate::mm::vm::DeviceAddr(self.gpu_heap_base + self.fw_offset as u64)
    }

    /// Bus address of byte 0.
    pub fn daddr(&self) -> Daddr {
        self.dma.daddr()
    }

    /// Bus address of byte `offset`.
    ///
    /// Ports upstream `pvr_fw_object_get_dma_addr()`. The allocation is
    /// physically contiguous, so this is addition rather than a page-table walk.
    pub fn daddr_at(&self, offset: usize) -> PvrResult<Daddr> {
        if offset >= self.allocated_size() {
            return Err(PvrError::InvalidArgument);
        }
        Ok(self.dma.daddr() + offset)
    }

    /// Size the caller asked for.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Size actually allocated, rounded up to whole pages.
    ///
    /// This, not [`Self::size`], is what the MIPS page table maps: a firmware
    /// page table entry covers a whole page, and leaving the tail of the last
    /// page unmapped would fault the firmware on a perfectly legal access to a
    /// structure that happens to end mid-page.
    pub fn allocated_size(&self) -> usize {
        self.dma.size()
    }

    /// Whether the firmware must map this uncached.
    pub fn is_uncached(&self) -> bool {
        self.uncached
    }

    /// Whether the CPU reaches this through an uncacheable mapping.
    ///
    /// True only for objects allocated with [`FwCache::cpu_uncached`]. Forwards
    /// to [`DmaStream::is_cpu_uncached`], so a table of expected mappings can
    /// assert against the stream rather than against the flag the caller passed.
    pub fn is_cpu_uncached(&self) -> bool {
        self.dma.is_cpu_uncached()
    }

    /// The underlying DMA allocation.
    ///
    /// Callers that reach for this own the sync discipline themselves. Prefer the
    /// accessors below, which sync.
    pub fn dma(&self) -> &DmaStream {
        &self.dma
    }

    /// Writes bytes and makes them visible to the firmware.
    pub fn write_bytes(&self, offset: usize, buf: &[u8]) -> PvrResult<()> {
        let end = self.check_range(offset, buf.len())?;
        self.dma
            .write_bytes(offset, buf)
            .map_err(|_| PvrError::InvalidArgument)?;
        self.sync_to_device(offset..end)
    }

    /// Reads bytes the firmware may have written.
    pub fn read_bytes(&self, offset: usize, buf: &mut [u8]) -> PvrResult<()> {
        let end = self.check_range(offset, buf.len())?;
        self.sync_from_device(offset..end)?;
        self.dma
            .read_bytes(offset, buf)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Writes a value and makes it visible to the firmware.
    pub fn write_val<T: Pod>(&self, offset: usize, value: &T) -> PvrResult<()> {
        let end = self.check_range(offset, size_of::<T>())?;
        self.dma
            .write_val(offset, value)
            .map_err(|_| PvrError::InvalidArgument)?;
        self.sync_to_device(offset..end)
    }

    /// Reads a value the firmware may have written.
    pub fn read_val<T: Pod>(&self, offset: usize) -> PvrResult<T> {
        let end = self.check_range(offset, size_of::<T>())?;
        self.sync_from_device(offset..end)?;
        self.dma
            .read_val(offset)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Fills the whole allocation with a repeating 32-bit word.
    ///
    /// What upstream's `fw_fault_page_init()` does, and the only bulk
    /// initialisation the firmware structures need.
    pub fn fill_u32(&self, value: u32) -> PvrResult<()> {
        // One page of pattern, written a page at a time: a word-at-a-time loop
        // over a 10 KiB structure is thousands of `VmWriter` set-ups on a path
        // that runs for every allocation on every hard reset.
        let mut pattern = [0u8; PAGE_SIZE];
        for word in pattern.as_chunks_mut::<4>().0 {
            word.copy_from_slice(&value.to_le_bytes());
        }

        let total = self.allocated_size();
        let mut offset = 0;
        while offset < total {
            let len = (total - offset).min(PAGE_SIZE);
            self.dma
                .write_bytes(offset, &pattern[..len])
                .map_err(|_| PvrError::InvalidArgument)?;
            offset += len;
        }
        self.sync_to_device(0..total)
    }

    /// Zeroes the whole allocation, then syncs.
    ///
    /// Used by the hard-reset path, which re-runs every structure's initialiser
    /// over a cleared object.
    pub fn zero(&self) -> PvrResult<()> {
        self.fill_u32(0)
    }

    /// Flushes a byte range so the firmware sees it.
    pub fn sync_to_device(&self, range: core::ops::Range<usize>) -> PvrResult<()> {
        self.dma
            .sync_to_device(range)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Invalidates a byte range so the CPU sees what the firmware wrote.
    pub fn sync_from_device(&self, range: core::ops::Range<usize>) -> PvrResult<()> {
        self.dma
            .sync_from_device(range)
            .map_err(|_| PvrError::InvalidArgument)
    }

    /// Bounds a `[offset, offset + len)` access against [`Self::size`].
    fn check_range(&self, offset: usize, len: usize) -> PvrResult<usize> {
        let end = offset.checked_add(len).ok_or(PvrError::InvalidArgument)?;
        if end > self.size {
            return Err(PvrError::InvalidArgument);
        }
        Ok(end)
    }
}

/// Owns one unpublished allocation while a fallible constructor runs.
///
/// No extra allocation or lock: the caller already exclusively borrows the heap.
/// Nested guards borrow the same heap through parts(). On error they return
/// ranges in reverse construction order, before their DMA backing is dropped.
pub(crate) struct FwAllocation<'a> {
    heap: &'a mut FwHeap,
    object: Option<FwObject>,
}

impl FwAllocation<'_> {
    pub(crate) fn object(&self) -> &FwObject {
        self.object
            .as_ref()
            .expect("allocation is owned until commit")
    }

    pub(crate) fn parts(&mut self) -> (&FwObject, &mut FwHeap) {
        (
            self.object
                .as_ref()
                .expect("allocation is owned until commit"),
            self.heap,
        )
    }

    /// Transfers both backing and range ownership to the existing resource owner.
    pub(crate) fn commit(mut self) -> FwObject {
        self.object.take().expect("allocation is committed once")
    }
}

impl Drop for FwAllocation<'_> {
    fn drop(&mut self) {
        if let Some(object) = &self.object {
            self.heap.free(object);
        }
    }
}

/// The firmware heap's address-space allocator.
///
/// Replaces upstream's `drm_mm` over `fw_dev->fw_mm`. Firmware allocations are
/// few — a dozen structures, two trace buffers, the code and data images — and
/// are made once at boot and freed all together at teardown, so a sorted vector
/// of live ranges with first fit is the whole allocator.
///
/// Every offset and size is page-aligned, because a MIPS page table entry is the
/// unit in which the firmware can be given access. Sub-page allocations would
/// silently expose their neighbours.
pub struct FwHeap {
    info: FwHeapInfo,
    /// Base of the heap in the firmware's own address space, for
    /// [`FwObject::fw_addr`].
    fw_base: u32,
    /// Live `[start, end)` ranges as offsets from the heap base, sorted and
    /// non-overlapping.
    used: Vec<(u32, u32)>,
    #[cfg(ktest)]
    fail_after: Option<usize>,
}

impl FwHeap {
    /// Creates an empty heap.
    ///
    /// `fw_base` is where the firmware processor sees the heap: for MIPS,
    /// [`crate::fw::mips::FW_HEAP_MIPS_BASE`].
    pub fn new(info: FwHeapInfo, fw_base: u32) -> Self {
        Self {
            info,
            fw_base,
            used: Vec::new(),
            #[cfg(ktest)]
            fail_after: None,
        }
    }

    /// The heap's sizes and masks.
    pub fn info(&self) -> &FwHeapInfo {
        &self.info
    }

    /// Allocates an object whose range is rolled back unless explicitly committed.
    pub(crate) fn alloc_guard(
        &mut self,
        size: usize,
        cache: FwCache,
    ) -> PvrResult<FwAllocation<'_>> {
        let object = self.alloc(size, cache)?;
        Ok(FwAllocation {
            heap: self,
            object: Some(object),
        })
    }

    #[cfg(ktest)]
    pub(crate) fn fail_alloc_after_for_test(&mut self, count: Option<usize>) {
        self.fail_after = count;
    }

    #[cfg(ktest)]
    pub(crate) fn ranges_for_test(&self) -> Vec<(u32, u32)> {
        self.used.clone()
    }

    /// Allocates `size` bytes anywhere in the general area.
    ///
    /// Ports upstream `pvr_fw_object_create_and_map()`, whose `drm_mm` insert is
    /// bounded to `[gpu_addr, gpu_addr + size)` — the general area, excluding
    /// the configuration area at the top.
    ///
    /// `cache.device_uncached` is the MIPS page-table policy.
    /// `cache.cpu_uncached` selects [`DmaStream::alloc_uncached`].
    pub fn alloc(&mut self, size: usize, cache: FwCache) -> PvrResult<FwObject> {
        let pages = Self::pages_for(size)?;
        let span = (pages * PAGE_SIZE) as u32;

        // First fit over the gaps between live ranges. `used` is sorted, so the
        // gaps are the intervals between consecutive entries. Ranges at or past
        // the general area are skipped: those are the fixed-offset configuration
        // structures, which are not candidates to allocate around.
        let mut candidate = 0u32;
        for (start, end) in self
            .used
            .iter()
            .filter(|(start, _)| *start < self.info.size)
        {
            if *start >= candidate && start - candidate >= span {
                break;
            }
            candidate = candidate.max(*end);
        }
        let fits = candidate
            .checked_add(span)
            .is_some_and(|end| end <= self.info.size);
        if !fits {
            ostd::error!(
                "firmware heap exhausted: {} bytes requested, {} of {} in use",
                span,
                self.used.iter().map(|(s, e)| e - s).sum::<u32>(),
                self.info.size
            );
            return Err(PvrError::NoMemory);
        }
        self.alloc_at(candidate, size, cache)
    }

    /// Allocates `size` bytes at a fixed offset from the heap base.
    ///
    /// Ports upstream `pvr_fw_object_create_and_map_offset()`. The firmware's
    /// boot code has the offsets of the configuration structures compiled in, so
    /// those cannot be placed by the allocator; neither can the code section,
    /// which must start at offset 0.
    ///
    /// Unlike [`Self::alloc`] this may land in the configuration area, which is
    /// the point. `cache` has the same meaning as in [`Self::alloc`].
    pub fn alloc_at(&mut self, offset: u32, size: usize, cache: FwCache) -> PvrResult<FwObject> {
        let pages = Self::pages_for(size)?;
        let span = (pages * PAGE_SIZE) as u32;

        if !offset.is_multiple_of(PAGE_SIZE as u32) {
            return Err(PvrError::InvalidArgument);
        }
        let end = offset.checked_add(span).ok_or(PvrError::InvalidArgument)?;
        if end > self.info.raw_size {
            return Err(PvrError::InvalidArgument);
        }
        let index = match self.used.binary_search_by_key(&offset, |(start, _)| *start) {
            Ok(_) => return Err(PvrError::InvalidArgument),
            Err(index) => index,
        };
        // Overlap with the range before or after would give the firmware two
        // names for one page, which is not a failure it could report.
        if index > 0 && self.used[index - 1].1 > offset {
            return Err(PvrError::InvalidArgument);
        }
        if self.used.get(index).is_some_and(|(start, _)| *start < end) {
            return Err(PvrError::InvalidArgument);
        }

        #[cfg(ktest)]
        if let Some(left) = &mut self.fail_after {
            if *left == 0 {
                return Err(PvrError::NoMemory);
            }
            *left -= 1;
        }

        let dma = if cache.cpu_uncached {
            DmaStream::alloc_uncached(pages).map_err(|_| PvrError::NoMemory)?
        } else {
            DmaStream::alloc(pages, GPU_CACHE_COHERENT).map_err(|_| PvrError::NoMemory)?
        };
        // Upstream's `pvr_mips_get_fw_addr_with_offset()`: the heap offset masked
        // to the heap window, or-ed with the processor's view of the heap base.
        let fw_addr = FwAddr((offset & self.info.offset_mask) | self.fw_base);

        self.used.insert(index, (offset, end));
        Ok(FwObject {
            dma,
            fw_offset: offset,
            fw_addr,
            gpu_heap_base: self.info.gpu_addr,
            size,
            uncached: cache.device_uncached,
        })
    }

    /// Releases an object's address range.
    ///
    /// Dropping the [`FwObject`] frees its pages; this is what returns the
    /// address space. They are separate because the object does not know which
    /// heap it came from, and the teardown path drops the whole heap anyway.
    pub fn free(&mut self, object: &FwObject) {
        self.used.retain(|(start, _)| *start != object.fw_offset);
    }

    /// Rounds a request up to whole pages, rejecting zero and anything that
    /// would not fit the heap's `u32` arithmetic.
    fn pages_for(size: usize) -> PvrResult<usize> {
        if size == 0 {
            return Err(PvrError::InvalidArgument);
        }
        let pages = size.div_ceil(PAGE_SIZE);
        if pages
            .checked_mul(PAGE_SIZE)
            .is_none_or(|span| span > u32::MAX as usize)
        {
            return Err(PvrError::InvalidArgument);
        }
        Ok(pages)
    }
}

/// The firmware's page granularity and the host's must match, or a firmware page
/// table entry would cover memory the driver did not mean to expose.
///
/// Upstream copes with a larger host page by splitting one host page across
/// several firmware entries (`pvr_mips_fw_process()` divides by `PAGE_SHIFT`).
/// Asterinas on RISC-V has 4 KiB pages, the same as the firmware, so this port
/// asserts equality rather than carrying that arithmetic untested.
/// Recorded in `ABI.md` compatibility limits.
const _: () = assert!(PAGE_SIZE == crate::rogue::mips::ROGUE_MIPSFW_PAGE_SIZE_4K as usize);

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// The heap arithmetic is pure, so it runs on the host too. The allocator
    /// needs the frame allocator and is `#[ktest]` only.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn config_area_is_the_top_three_granules() {
        let info = FwHeapInfo::new(24, 1024 * 1024).expect("16 MiB heap");
        assert_eq!(info.config_offset + CONFIG_HEAP_SIZE, info.raw_size);
        assert_eq!(SYSINIT_OFFSET + CONFIG_HEAP_GRANULARITY, CONFIG_HEAP_SIZE);
        // The general area and the configuration area do not overlap.
        assert!(info.size <= info.config_offset);
        // A firmware address masks back to a heap offset.
        assert_eq!((0xc0ab_cdef_u32) & info.offset_mask, 0x00ab_cdef);
    }

    #[cfg(ktest)]
    fn heap() -> FwHeap {
        FwHeap::new(
            FwHeapInfo::new(24, 1024 * 1024).expect("16 MiB heap"),
            crate::fw::mips::FW_HEAP_MIPS_BASE,
        )
    }

    /// Firmware address zero must be invalid for everything except the code
    /// section, which is what makes a zero `FwAddr` in a firmware structure a
    /// detectable mistake rather than a valid pointer to something.
    #[ktest]
    fn code_at_offset_zero_makes_firmware_address_zero_unique() {
        let mut heap = heap();
        let code = heap
            .alloc_at(0, PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("code at zero");
        assert_eq!(code.fw_offset(), 0);
        assert_eq!(code.fw_addr().0, crate::fw::mips::FW_HEAP_MIPS_BASE);

        let other = heap
            .alloc(PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("second object");
        assert_ne!(other.fw_offset(), 0);
        assert_ne!(other.fw_addr().0, code.fw_addr().0);
    }

    /// Two allocations must never name the same page: the firmware has no way to
    /// report that it was handed one page under two names.
    #[ktest]
    fn allocations_do_not_overlap() {
        let mut heap = heap();
        let a = heap
            .alloc(3 * PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("a");
        let b = heap.alloc(PAGE_SIZE, FwCache::DEVICE_UNCACHED).expect("b");
        let c = heap
            .alloc(2 * PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("c");

        let ranges = [
            (a.fw_offset(), a.fw_offset() + a.allocated_size() as u32),
            (b.fw_offset(), b.fw_offset() + b.allocated_size() as u32),
            (c.fw_offset(), c.fw_offset() + c.allocated_size() as u32),
        ];
        for (i, (start, end)) in ranges.iter().enumerate() {
            for (other_start, other_end) in &ranges[i + 1..] {
                assert!(
                    end <= other_start || other_end <= start,
                    "{start:#x}..{end:#x} overlaps {other_start:#x}..{other_end:#x}"
                );
            }
        }
    }

    /// A fixed-offset allocation may land in the configuration area — that is
    /// what it is for — and the general allocator must not then hand the same
    /// page out again.
    #[ktest]
    fn fixed_offsets_reach_the_config_area_and_are_respected() {
        let mut heap = heap();
        let config = heap.info().config_offset;
        let sysinit = heap
            .alloc_at(config + SYSINIT_OFFSET, PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("sysinit at its fixed offset");
        assert!(sysinit.fw_offset() >= heap.info().size);

        // Asking for the same page again is refused, not silently aliased.
        assert!(
            heap.alloc_at(config + SYSINIT_OFFSET, PAGE_SIZE, FwCache::DEVICE_UNCACHED)
                .is_err()
        );
        // And an overlapping range is refused too.
        assert!(
            heap.alloc_at(
                config + SYSINIT_OFFSET - PAGE_SIZE as u32,
                2 * PAGE_SIZE,
                FwCache::DEVICE_UNCACHED
            )
            .is_err()
        );

        // A general allocation still comes from the bottom of the heap.
        let general = heap
            .alloc(PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("general");
        assert!(general.fw_offset() < heap.info().size);
    }

    #[ktest]
    fn requests_are_rounded_up_and_bounded() {
        let mut heap = heap();
        // A sub-page request still gets a whole page, because a page is the unit
        // the firmware can be given.
        let small = heap.alloc(4, FwCache::DEVICE_UNCACHED).expect("four bytes");
        assert_eq!(small.size(), 4);
        assert_eq!(small.allocated_size(), PAGE_SIZE);

        assert!(heap.alloc(0, FwCache::DEVICE_UNCACHED).is_err());
        // Bigger than the general area.
        assert!(
            heap.alloc(
                heap.info().size as usize + PAGE_SIZE,
                FwCache::DEVICE_UNCACHED
            )
            .is_err()
        );
        // Unaligned fixed offsets have no entry to describe them.
        assert!(
            heap.alloc_at(1, PAGE_SIZE, FwCache::DEVICE_UNCACHED)
                .is_err()
        );
    }

    /// Accessors bound against the requested size, not the rounded allocation, so
    /// a structure write that runs past its own end is an error rather than a
    /// silent scribble on the next page of firmware-visible memory.
    #[ktest]
    fn accessors_are_bounded_and_round_trip() {
        let mut heap = heap();
        let object = heap.alloc(64, FwCache::DEVICE_UNCACHED).expect("64 bytes");

        object.write_val(0, &0x1234_5678u32).expect("write");
        assert_eq!(object.read_val::<u32>(0).unwrap(), 0x1234_5678);
        object
            .write_bytes(60, &[1, 2, 3, 4])
            .expect("write at the end");
        let mut buf = [0u8; 4];
        object.read_bytes(60, &mut buf).expect("read at the end");
        assert_eq!(buf, [1, 2, 3, 4]);

        assert!(object.write_val(61, &0u32).is_err());
        assert!(object.read_val::<u32>(61).is_err());
        assert!(object.fw_addr_at(64).is_err());
        assert_eq!(object.fw_addr_at(16).unwrap().0, object.fw_addr().0 + 16);
    }

    /// The two flags are independent: the MIPS-side bit must not follow the
    /// CPU mapping, and the CPU mapping must not follow the MIPS-side bit.
    ///
    /// A single `uncached: bool` that drove both sides would pass a test that
    /// only ever allocated `DEVICE_UNCACHED` or `POLLED`. The mixed cases are
    /// the ones that go red if someone overloads the existing flag.
    #[ktest]
    fn cpu_and_device_cache_flags_are_orthogonal() {
        let mut heap = heap();

        let device_only = heap
            .alloc(PAGE_SIZE, FwCache::DEVICE_UNCACHED)
            .expect("device-uncached, CPU write-back");
        assert!(device_only.is_uncached());
        assert!(!device_only.is_cpu_uncached());

        let polled = heap
            .alloc(PAGE_SIZE, FwCache::POLLED)
            .expect("both uncached");
        assert!(polled.is_uncached());
        assert!(polled.is_cpu_uncached());

        let cpu_only = heap
            .alloc(
                PAGE_SIZE,
                FwCache {
                    device_uncached: false,
                    cpu_uncached: true,
                },
            )
            .expect("device-cached, CPU uncached");
        assert!(!cpu_only.is_uncached());
        assert!(cpu_only.is_cpu_uncached());

        let cached = heap.alloc(PAGE_SIZE, FwCache::CACHED).expect("both cached");
        assert!(!cached.is_uncached());
        assert!(!cached.is_cpu_uncached());
    }

    /// `read_val` / `write_val` keep calling `sync_*`. On a `cpu_uncached`
    /// object those must return `Ok` — `DmaStream::sync_impl` is a no-op —
    /// rather than issue `cbo.*` against an uncacheable address and fail.
    ///
    /// Out-of-range still errors, so a future "always Ok on cpu_uncached"
    /// shortcut that skipped the bounds check would turn this red.
    #[ktest]
    fn sync_on_cpu_uncached_object_succeeds_and_stays_bounded() {
        let mut heap = heap();
        let object = heap.alloc(64, FwCache::POLLED).expect("polled");
        assert!(object.is_cpu_uncached());

        object.write_val(0, &0xAABB_CCDDu32).expect("write+sync_to");
        assert_eq!(object.read_val::<u32>(0).unwrap(), 0xAABB_CCDD);
        object.sync_to_device(0..4).expect("explicit sync_to");
        object.sync_from_device(0..4).expect("explicit sync_from");

        // `sync_*` bounds against the rounded allocation, not the request.
        // Past that must still fail: a `cpu_uncached ⇒ Ok` shortcut on
        // `FwObject` that skipped the stream would hide a real overflow.
        let past = object.allocated_size() + 1;
        assert!(object.sync_from_device(0..past).is_err());
        assert!(object.sync_to_device(0..past).is_err());
    }
}
