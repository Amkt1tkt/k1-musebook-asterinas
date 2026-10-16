// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Live counts of DRM objects that hold physical frames.
//!
//! Counters distinguish retained GEM buffers and dma-bufs from heap growth.
//! These objects keep physical frames alive independently of process RSS.
//!
//! # What each counter answers
//!
//! - **GEM objects / frames.** The pages live on the `Arc<dyn GemObject>`, not
//!   on the per-file handle. Closing a handle is not destruction (PRIME
//!   clients do it immediately after export). Counting objects and their
//!   frames is what answers "is GPU memory coming back?".
//! - **GEM handles.** Rising handle and object counts can indicate missing
//!   `GEM_CLOSE` calls. A mapping retains an object independently of its handles.
//! - **dma-bufs.** Each export holds a strong reference to the object, which
//!   is why a leaked descriptor keeps the pages. Re-export of the same object
//!   is the same `DmaBuf` and does not increment again.
//! - **open DRM files.** A climbing file count after clients exit means
//!   `DrmFile::drop` is not running. That is the destructor that releases
//!   handles, KMS objects, and `postclose`.

use core::sync::atomic::{AtomicU64, Ordering};

static GEM_OBJS: AtomicU64 = AtomicU64::new(0);
static GEM_FRAMES: AtomicU64 = AtomicU64::new(0);
static GEM_HANDLES: AtomicU64 = AtomicU64::new(0);
static GEM_CLOSE: AtomicU64 = AtomicU64::new(0);
static DMABUFS: AtomicU64 = AtomicU64::new(0);
static DRM_FILES: AtomicU64 = AtomicU64::new(0);

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

fn sub(counter: &AtomicU64, n: u64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(n))
    });
}

/// Records that a driver-owned GEM object of `frames` pages was created.
pub fn note_gem_alloc(frames: usize) {
    add(&GEM_OBJS, 1);
    add(&GEM_FRAMES, frames as u64);
}

/// Records that a driver-owned GEM object of `frames` pages was dropped.
pub fn note_gem_free(frames: usize) {
    sub(&GEM_OBJS, 1);
    sub(&GEM_FRAMES, frames as u64);
}

/// Records a newly created [`crate::prime::DmaBuf`] (not a re-export).
pub fn note_dmabuf_alloc() {
    add(&DMABUFS, 1);
}

/// Records that a [`crate::prime::DmaBuf`] was dropped.
pub fn note_dmabuf_free() {
    sub(&DMABUFS, 1);
}

/// Records that a [`crate::file::DrmFile`] was created.
pub fn note_file_alloc() {
    add(&DRM_FILES, 1);
}

/// Records that a [`crate::file::DrmFile`] was dropped.
pub fn note_file_free() {
    sub(&DRM_FILES, 1);
}

/// Records that a GEM handle was installed in a file's table.
pub fn note_gem_handle_alloc() {
    add(&GEM_HANDLES, 1);
}

/// Records that a GEM handle was removed from a file's table.
pub fn note_gem_handle_free() {
    sub(&GEM_HANDLES, 1);
}

/// Records that `n` GEM handles vanished with their table (file close).
pub fn note_gem_handles_drop(n: usize) {
    sub(&GEM_HANDLES, n as u64);
}

/// Records a successful `GEM_CLOSE` (or `MODE_DESTROY_DUMB`).
pub fn note_gem_close() {
    add(&GEM_CLOSE, 1);
}

/// Live GEM objects that this crate (or a caller such as `aster-pvr`) counted.
pub fn gem_objects() -> u64 {
    GEM_OBJS.load(Ordering::Relaxed)
}

/// Pages those objects hold. Imported views are not counted again.
pub fn gem_frames() -> u64 {
    GEM_FRAMES.load(Ordering::Relaxed)
}

/// Live dma-buf objects.
pub fn dmabufs() -> u64 {
    DMABUFS.load(Ordering::Relaxed)
}

/// Live [`crate::file::DrmFile`]s.
pub fn drm_files() -> u64 {
    DRM_FILES.load(Ordering::Relaxed)
}

/// Live GEM handles across every open file.
///
/// A climbing `gem` with a flat `h=` means objects are held by something
/// other than a handle: a VMA pin, a GPU VM mapping, a dma-buf, a
/// framebuffer. A climbing `h=` that tracks `gem` means user space is
/// not calling `GEM_CLOSE`.
pub fn gem_handles() -> u64 {
    GEM_HANDLES.load(Ordering::Relaxed)
}

/// Successful `GEM_CLOSE` / `MODE_DESTROY_DUMB` ioctls since boot.
///
/// A cumulative, like the bounce counter. Compare with the number of
/// `CREATE_BO` / `MODE_CREATE_DUMB` successes: a growing gap is
/// user space not closing handles, not a kernel table that only increments.
pub fn gem_closes() -> u64 {
    GEM_CLOSE.load(Ordering::Relaxed)
}

/// Zeros the counters. A test seam: calling this while real objects are
/// alive desynchronises the counts from the heap.
#[cfg(ktest)]
pub fn reset_for_test() {
    GEM_OBJS.store(0, Ordering::Relaxed);
    GEM_FRAMES.store(0, Ordering::Relaxed);
    GEM_HANDLES.store(0, Ordering::Relaxed);
    GEM_CLOSE.store(0, Ordering::Relaxed);
    DMABUFS.store(0, Ordering::Relaxed);
    DRM_FILES.store(0, Ordering::Relaxed);
}
