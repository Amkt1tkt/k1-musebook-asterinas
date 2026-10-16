// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! GEM: buffer objects, handles, and dumb buffers.
//!
//! Device page faults resolve GEM buffer pages on demand through the VM layer.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c>.
//!
//! # The four namespaces
//!
//! A buffer object is reachable through four different names, and confusing
//! them is the usual source of GEM bugs:
//!
//!  - an [`Arc<dyn GemObject>`], the kernel-side identity of the object. This
//!    one is the truth; the other three are ways of getting to it.
//!  - a [`GemHandle`], valid only within the [`DrmFile`] that issued it. Two
//!    clients may hold the same handle number for different objects.
//!  - a *flink name*, valid device-wide, handed out by `GEM_FLINK` and
//!    resolved by `GEM_OPEN`. See [`NameTable`].
//!  - a *fake `mmap` offset*, also device-wide, handed out by `MODE_MAP_DUMB`
//!    and passed to `mmap` on the DRM node. See [`MmapOffsets`].
//!
//! [`Arc<dyn GemObject>`]: GemObject
//! [`DrmFile`]: crate::file::DrmFile
//!
//! # Lifetime and reference counting
//!
//! There is no destroy operation and no teardown callback. An object lives
//! exactly as long as some [`Arc`] to it does, and the holders are:
//!
//!  - every handle in every [`HandleTable`] (one clone per handle, so a file
//!    that opens the same object twice holds two);
//!  - an exported PRIME file descriptor;
//!  - a [`crate::kms::Framebuffer`] that scans the object out;
//!  - a kernel mapping handler, which retains the resolved object after the
//!    kernel validates the caller's handle and complete mapping range. Forked
//!    and split VMAs share that handler; the final unmap releases its reference.
//!
//! Handle closure is therefore not destruction, which is exactly the property
//! user space relies on when it closes a handle right after exporting a
//! dma-buf. Neither a flink name nor a fake `mmap` offset keeps an object
//! alive: both tables hold [`Weak`] references, matching Linux, where the
//! flink name is dropped when the last handle goes away and the offset when
//! the object is released.
//!
//! The pages themselves are reference counted one level down. [`fault`] hands
//! out a clone of a [`UFrame`] the object holds, so `munmap` drops the page
//! table's reference and the memory survives in the object. It is freed when
//! the object is, which is what makes a buffer outlive the mapping that
//! happened to be looking at it.
//!
//! [`fault`]: GemObject::fault
//! [`Weak`]: alloc::sync::Weak

use alloc::sync::Arc;

use ostd::mm::{CachePolicy, PAGE_SIZE, Segment, UFrame};

use crate::error::DrmResult;

mod dumb;
mod handle;
pub mod ioctl;
mod name;
mod object;
mod offset;
#[cfg(ktest)]
mod test;

pub use self::{
    handle::HandleTable,
    name::NameTable,
    object::ShmemObject,
    offset::{MMAP_OFFSET_BASE, MmapOffsets},
};

/// A GEM handle, valid only within the [`crate::file::DrmFile`] that issued it.
///
/// Handle `0` is never valid, which lets user space use zero as "no buffer" in
/// structures such as `drm_mode_fb_cmd2::handles`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GemHandle(pub u32);

/// How a buffer's memory will be reached, which decides how it must be allocated.
///
/// This distinction is not an optimization on the K1: the SoC has no hardware DMA
/// coherency, so a buffer the display engine scans out must be physically
/// contiguous and mapped uncached or write-combining, while a buffer only the CPU
/// touches should stay cached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferUse {
    /// Scanned out by the display engine.
    Scanout,
    /// Touched by the CPU and the GPU, but never scanned out.
    Render,
    /// Touched only by the CPU.
    CpuOnly,
}

impl BufferUse {
    /// Returns the cache policy a CPU mapping of such a buffer must use.
    ///
    /// The K1 has no hardware DMA coherency, and the VM layer performs no
    /// cache maintenance for
    /// demand-paged device mappings. Whatever is chosen here is what the page
    /// table gets, so the choice has to make the buffer correct on its own:
    ///
    ///  - [`Scanout`](Self::Scanout) is write-combining. The display engine
    ///    reads the buffer with no snooping and no driver-visible hand-off:
    ///    user space renders into the mapping and the DPU fetches whatever is
    ///    in memory at the next vblank, with no ioctl in between at which the
    ///    kernel could clean the D-cache. A writeback mapping would therefore
    ///    scan out whichever lines happened to have been evicted, which is
    ///    the classic "half the frame is stale" artefact. Write-combining
    ///    removes the problem rather than managing it, at the cost of slow
    ///    read-back — and read-back from a scanout buffer is not a pattern
    ///    worth optimizing for. This is also why `DRM_CAP_DUMB_PREFER_SHADOW`
    ///    is 0: clients that do want to read what they wrote should keep their
    ///    own shadow buffer in ordinary cached memory.
    ///  - [`Render`](Self::Render) is write-combining for the same reason,
    ///    one device further away: the GPU is not coherent either, and
    ///    upstream `drm/imagination` maps its buffer objects write-combining.
    ///  - [`CpuOnly`](Self::CpuOnly) is writeback. Nothing but the CPU ever
    ///    looks at it, so there is nothing to be coherent with and no reason
    ///    to give up the cache.
    pub const fn cache_policy(self) -> CachePolicy {
        match self {
            Self::Scanout | Self::Render => CachePolicy::WriteCombining,
            Self::CpuOnly => CachePolicy::Writeback,
        }
    }
}

/// A GEM buffer object.
pub trait GemObject: Send + Sync + core::fmt::Debug {
    /// Downcast escape hatch for recovering a concrete object from
    /// `Arc<dyn GemObject>`.
    ///
    /// Provides an explicit, object-safe downcast hook. PowerVR uses it to
    /// resolve its hardware-specific buffer view during `VM_MAP`.
    ///
    /// Paired with [`Self::into_any`], which is the `Arc` half of the same
    /// escape hatch: [`Arc::downcast`] needs `Arc<dyn Any + Send + Sync>`, and
    /// getting there from `Arc<dyn GemObject>` without `unsafe` requires a
    /// method whose `Self`-typed receiver the implementor can return as-is.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Consumes an `Arc<dyn GemObject>` into an `Arc<dyn Any + Send + Sync>`.
    ///
    /// Every implementor returns `self`. See [`Self::as_any`].
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync>;

    /// Returns the object's size in bytes, which is always page-aligned.
    fn size(&self) -> usize;

    /// Returns how this object's memory is intended to be used.
    fn buffer_use(&self) -> BufferUse;

    /// Returns the backing frames, if the object is backed by system memory.
    ///
    /// Objects backed by device-private memory return `None` and must be reached
    /// through [`fault`](Self::fault) instead.
    fn segment(&self) -> Option<&Segment<()>>;

    /// Resolves one page of this object into the frame that backs it.
    ///
    /// `pgoff` is the page index within *this object*, not within the fake
    /// `mmap` offset space. The kernel binds a mapping to one resolved object
    /// and computes its page index without another global offset lookup.
    ///
    /// The returned [`UFrame`] is a *clone* of the reference the object keeps:
    /// the page table takes one reference and the object keeps its own, so
    /// `munmap` does not take the page away from the object. See the module
    /// documentation.
    ///
    /// # Implementation constraints
    ///
    /// The page-fault path calls this with preemption disabled and the faulting
    /// page's page-table lock held. Implementations must not sleep, block, take
    /// a `Mutex`, or re-enter the VM layer. Use a `SpinLock` if a lock is needed.
    /// See `kernel/core/src/vm/vmar/on_demand.rs` for the full contract.
    ///
    /// It must also be idempotent: the same `pgoff` must always resolve to the
    /// same frame, because two address spaces mapping one object can fault on
    /// the same page concurrently and both must end up sharing that page.
    ///
    /// # Errors
    ///
    /// - [`crate::error::DrmError::BadAddress`] if `pgoff` is past the end of the object, or
    ///   falls in a hole of a sparsely populated one. User space sees
    ///   `SIGBUS` when a device-backed page cannot be resolved.
    /// - [`crate::error::DrmError::OutOfMemory`] if a frame could not be allocated.
    fn fault(&self, pgoff: usize) -> DrmResult<UFrame>;

    /// Prepares one object page for a CPU read through its cached kernel alias.
    ///
    /// Called only before copying an original object page to a private CPU page,
    /// with the same nonblocking constraints as [`Self::fault`]. Noncoherent
    /// objects synchronize their retained DMA owner; coherent/CPU-only objects
    /// need no operation. This does not wait for outstanding GPU jobs.
    fn prepare_cpu_read(&self, _pgoff: usize) -> DrmResult<()> {
        Ok(())
    }

    /// Returns whether this object may be exported through PRIME.
    fn is_exportable(&self) -> bool {
        true
    }

    /// Returns whether this object may be mapped into a process.
    ///
    /// Linux refuses to hand out an `mmap` offset for an imported object,
    /// because the pages belong to the exporting driver. Objects in
    /// device-private memory, such as PowerVR firmware buffers, also return
    /// `false`.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L314>
    /// (`drm_gem_dumb_map_offset`, which rejects `obj->import_attach`).
    fn is_mappable(&self) -> bool {
        true
    }
}

/// Returns the number of pages an object occupies.
pub(crate) fn num_pages(object: &Arc<dyn GemObject>) -> usize {
    object.size().div_ceil(PAGE_SIZE)
}

/// The GEM state that belongs to a device rather than to one open file.
///
/// Both tables are device-wide in Linux too: `drm_device::object_name_idr` for
/// flink names and `drm_device::vma_offset_manager` for `mmap` offsets. They
/// have to be, because their whole purpose is to let *another* file reach an
/// object.
#[derive(Debug, Default)]
pub struct GemDevice {
    offsets: MmapOffsets,
    names: NameTable,
}

impl GemDevice {
    /// Creates empty tables.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the fake `mmap` offset space.
    pub fn offsets(&self) -> &MmapOffsets {
        &self.offsets
    }

    /// Returns the global flink name space.
    pub fn names(&self) -> &NameTable {
        &self.names
    }
}
