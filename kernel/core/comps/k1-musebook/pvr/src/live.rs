// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: bounded GPU object lifetime diagnostics.

//! Low-frequency GPU resource lifetime counters.
//!
//! BO counts track owned objects, mapping counts track VM references, and page-table
//! counts track DMA-backed tables. A mapping can retain a BO after its handle is
//! closed, so these populations must be interpreted separately. Allocated and freed
//! totals supplement live counts; they do not identify the cause of retained memory.

use core::sync::atomic::{AtomicU64, Ordering};

static VM_MAPS: AtomicU64 = AtomicU64::new(0);
static MMU_PAGES: AtomicU64 = AtomicU64::new(0);
static CREATE_BO: AtomicU64 = AtomicU64::new(0);
static VM_MAP_IOCTL: AtomicU64 = AtomicU64::new(0);
static VM_UNMAP_IOCTL: AtomicU64 = AtomicU64::new(0);

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

fn sub(counter: &AtomicU64, n: u64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
        Some(v.saturating_sub(n))
    });
}

/// Records that a GPU VM mapping was installed.
pub fn note_vm_map() {
    add(&VM_MAPS, 1);
}

/// Records that a GPU VM mapping was removed.
pub fn note_vm_unmap() {
    sub(&VM_MAPS, 1);
}

/// Records that a GPU MMU backing page was allocated.
pub fn note_mmu_page_alloc() {
    add(&MMU_PAGES, 1);
}

/// Records that a GPU MMU backing page was freed.
pub fn note_mmu_page_free() {
    sub(&MMU_PAGES, 1);
}

/// Records a successful `PVR_CREATE_BO`.
pub fn note_create_bo() {
    add(&CREATE_BO, 1);
}

/// Records a successful `PVR_VM_MAP`.
pub fn note_vm_map_ioctl() {
    add(&VM_MAP_IOCTL, 1);
}

/// Records a successful `PVR_VM_UNMAP`.
pub fn note_vm_unmap_ioctl() {
    add(&VM_UNMAP_IOCTL, 1);
}

/// Live GPU VM mappings across every context.
pub fn vm_maps() -> u64 {
    VM_MAPS.load(Ordering::Relaxed)
}

/// Live GPU MMU backing pages (catalogue + directories + leaf tables).
pub fn mmu_pages() -> u64 {
    MMU_PAGES.load(Ordering::Relaxed)
}

/// Successful `PVR_CREATE_BO` ioctls since boot.
pub fn create_bo() -> u64 {
    CREATE_BO.load(Ordering::Relaxed)
}

/// Successful `PVR_VM_MAP` ioctls since boot.
pub fn vm_map_ioctls() -> u64 {
    VM_MAP_IOCTL.load(Ordering::Relaxed)
}

/// Successful `PVR_VM_UNMAP` ioctls since boot.
pub fn vm_unmap_ioctls() -> u64 {
    VM_UNMAP_IOCTL.load(Ordering::Relaxed)
}

/// Zeros the counters. A test seam: calling this while real mappings
/// are alive desynchronises the counts from the heap.
#[cfg(ktest)]
pub fn reset_for_test() {
    VM_MAPS.store(0, Ordering::Relaxed);
    MMU_PAGES.store(0, Ordering::Relaxed);
    CREATE_BO.store(0, Ordering::Relaxed);
    VM_MAP_IOCTL.store(0, Ordering::Relaxed);
    VM_UNMAP_IOCTL.store(0, Ordering::Relaxed);
}
