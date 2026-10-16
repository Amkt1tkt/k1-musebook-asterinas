// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The fake `mmap` offset space.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_vma_manager.c>.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use ostd::{mm::PAGE_SIZE, sync::SpinLock};

use super::GemObject;
use crate::error::{DrmError, DrmResult};

/// The first byte offset the allocator hands out.
///
/// Linux starts its own offset space at 4 GiB
/// (`DRM_FILE_PAGE_OFFSET_START`, `0xFFFFFFFF >> PAGE_SHIFT` plus one page) and
/// so do we. The value is not ABI — user space always learns an offset from
/// `MODE_MAP_DUMB` and never computes one — but keeping the low 4 GiB empty
/// means a client that maps the DRM node at a plausible-looking small offset
/// gets a clean failure instead of somebody else's buffer.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_vma_manager.h#L37>.
pub const MMAP_OFFSET_BASE: u64 = 1 << 32;

/// Allocator for the fake `mmap` offsets handed out by `MODE_MAP_DUMB`.
///
/// Linux uses `drm_vma_offset_manager`, an interval tree over a per-device offset
/// space. We do not reproduce its exact values, but we do preserve the property
/// user space depends on: one object always maps at one offset, for as long as it
/// lives.
///
/// # The encoding
///
/// An object of `n` pages is given a run of `n` consecutive page numbers in a
/// device-wide space that starts at [`MMAP_OFFSET_BASE`], and its offset is
/// the first of them scaled to bytes:
///
/// ```text
/// offset = MMAP_OFFSET_BASE + first_page * PAGE_SIZE
/// ```
///
/// Decoding is the same statement read backwards, and both directions live in
/// this one file so that they cannot drift apart: given any offset in the
/// space, the entry with the largest starting page not above it is the only
/// candidate, and the offset belongs to that object if it lands within its
/// run. What comes out is both *which* object and *which page of it*, which is
/// what the fault path needs, since a mapping is identified to the VM layer by
/// nothing but this offset.
///
/// Page numbers are never reused. A recycled offset could be handed to a
/// client that is still holding a stale one for a since-destroyed buffer, and
/// its `mmap` would silently land on an unrelated object; with 2^52 pages of
/// space, refusing to recycle costs nothing.
#[derive(Debug)]
pub struct MmapOffsets {
    inner: SpinLock<Inner>,
}

impl Default for MmapOffsets {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Inner {
    /// Keyed by the first page number of each object's run, so that a lookup
    /// is one `range(..=page).next_back()`.
    entries: BTreeMap<usize, Entry>,
    /// The next page number to hand out. Never decreases.
    next_page: usize,
}

#[derive(Debug)]
struct Entry {
    /// Weak, so that an offset does not keep its object alive. Linux frees the
    /// offset in `drm_gem_object_release`; a dead weak reference is the same
    /// thing observed lazily.
    object: Weak<dyn GemObject>,
    num_pages: usize,
}

impl MmapOffsets {
    /// Creates an empty offset space.
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(Inner {
                entries: BTreeMap::new(),
                next_page: 0,
            }),
        }
    }

    /// Returns the offset for an object, assigning one on first use.
    ///
    /// Idempotent, as `drm_gem_create_mmap_offset` is: a client that calls
    /// `MODE_MAP_DUMB` twice for one handle must get one offset, or its second
    /// mapping would be of a different buffer.
    ///
    /// # Errors
    ///
    /// [`DrmError::NoSpace`] if the offset space is exhausted, and
    /// [`DrmError::InvalidArgument`] for a zero-sized object.
    pub fn offset_of(&self, object: &Arc<dyn GemObject>) -> DrmResult<u64> {
        let num_pages = super::num_pages(object);
        if num_pages == 0 {
            return Err(DrmError::InvalidArgument);
        }

        let mut inner = self.inner.lock();

        // Entries whose object is gone are dropped here rather than by
        // whoever released the object: nothing calls back into this table
        // when an `Arc` reaches zero, and this is the only path that walks it.
        inner
            .entries
            .retain(|_, entry| entry.object.strong_count() > 0);

        let identity = Arc::downgrade(object);
        let existing = inner
            .entries
            .iter()
            .find(|(_, entry)| Weak::ptr_eq(&entry.object, &identity));
        if let Some((page, _)) = existing {
            return Ok(offset_of_page(*page));
        }

        let first_page = inner.next_page;
        let next_page = first_page.checked_add(num_pages).ok_or(DrmError::NoSpace)?;
        // The end of the run must still be expressible as an `mmap` offset,
        // which is what makes the arithmetic in `offset_of_page` safe.
        next_page
            .checked_mul(PAGE_SIZE)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .and_then(|bytes| MMAP_OFFSET_BASE.checked_add(bytes))
            .ok_or(DrmError::NoSpace)?;
        inner.next_page = next_page;
        inner.entries.insert(
            first_page,
            Entry {
                object: Arc::downgrade(object),
                num_pages,
            },
        );

        Ok(offset_of_page(first_page))
    }

    /// Resolves an `mmap` offset back to its object.
    ///
    /// Returns the object, the offset its first page was assigned, and the
    /// index of the requested page within the object. The offset need not be
    /// the object's first: any offset inside its run resolves, which is what
    /// makes `mmap`ping a buffer at a non-zero offset work.
    ///
    /// Called by the character-device shell when user space maps the DRM node,
    /// from the page fault path, so it takes only a `SpinLock`.
    pub fn lookup(&self, offset: u64) -> Option<(Arc<dyn GemObject>, u64, usize)> {
        let page =
            usize::try_from(offset.checked_sub(MMAP_OFFSET_BASE)? / PAGE_SIZE as u64).ok()?;

        let inner = self.inner.lock();
        let (first_page, entry) = inner.entries.range(..=page).next_back()?;
        if page - first_page >= entry.num_pages {
            return None;
        }

        let object = entry.object.upgrade()?;
        Some((object, offset_of_page(*first_page), page - first_page))
    }

    /// Returns how many objects have been given an offset, including any whose
    /// object has since been released.
    #[cfg(ktest)]
    pub(super) fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

/// Scales a page number in the offset space to the offset user space sees.
const fn offset_of_page(page: usize) -> u64 {
    MMAP_OFFSET_BASE + (page * PAGE_SIZE) as u64
}
