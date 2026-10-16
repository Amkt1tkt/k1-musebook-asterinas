// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! PRIME: sharing buffers as file descriptors.
//!
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_prime.c>.
//!
//! # Why this needs a new kernel object
//!
//! PRIME turns a GEM handle into a file descriptor that other processes and
//! other drivers can import. Asterinas has no dma-buf equivalent, so [`DmaBuf`]
//! below is a new object rather than a wrapper: a refcounted buffer with an
//! attached [`FenceSet`], reachable through a file descriptor and independent
//! of whichever DRM device created it.
//!
//! That independence is the point. The GPU renders into a buffer and the
//! display engine scans it out, and on the K1 those are two separate devices
//! with separate drivers. Without a device-neutral buffer object there is no
//! way to hand a frame from one to the other.
//!
//! # The two identities user space depends on
//!
//! Both are equality properties, and both break silently when they are wrong —
//! the buffer still works, the *sharing* stops working:
//!
//!  - **One object exports to one [`DmaBuf`].** Mesa recognizes a buffer it
//!    already knows by comparing what came back. A second `DmaBuf` for one
//!    object makes it treat a buffer it is already using as a stranger and
//!    lose the implicit synchronization that comes with the [`FenceSet`].
//!  - **One [`DmaBuf`] imports to one handle per file.** Importing the same
//!    descriptor twice must return the same handle number, which is why
//!    [`HandleTable::find`] exists. A client that gets two handles for one
//!    buffer will close one of them and be surprised that the buffer is still
//!    referenced — or, worse, treat them as two buffers and render into the
//!    one being scanned out.
//!
//! [`HandleTable::find`]: crate::gem::HandleTable::find

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use ostd::{
    mm::{Segment, UFrame},
    sync::SpinLock,
};

mod fd;
pub mod ioctl;
#[cfg(ktest)]
mod test;

pub use self::fd::{Descriptors, DrmExport, Eventfd, ExportFlags};
use crate::{
    device::DrmDevice,
    error::{DrmError, DrmResult},
    fence::FenceSet,
    gem::{BufferUse, GemObject},
};

/// A buffer shared by file descriptor.
///
/// Holds a strong reference to its backing object, so the buffer outlives the
/// GEM handle it was exported from. User space depends on this: it routinely
/// closes the handle immediately after exporting.
#[derive(Debug)]
pub struct DmaBuf {
    object: Arc<dyn GemObject>,
    /// The device that exported the buffer.
    ///
    /// Weak, because a buffer must not keep a device registered; the only
    /// thing this is used for is recognizing an import back into the device
    /// the buffer came from, and a buffer whose device has gone can only be
    /// imported as a foreign one.
    device: Weak<DrmDevice>,
    fences: FenceSet,
}

impl DmaBuf {
    /// Returns the backing object.
    pub fn object(&self) -> &Arc<dyn GemObject> {
        &self.object
    }

    /// Returns the fences that must be observed before using this buffer.
    pub fn fences(&self) -> &FenceSet {
        &self.fences
    }

    /// Returns the buffer's size in bytes.
    pub fn size(&self) -> usize {
        self.object.size()
    }

    /// Returns whether this buffer was exported by `device`.
    pub fn is_from(&self, device: &Arc<DrmDevice>) -> bool {
        self.device
            .upgrade()
            .is_some_and(|exporter| Arc::ptr_eq(&exporter, device))
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        crate::live::note_dmabuf_free();
    }
}

/// A buffer over `object`, as though another device had exported it.
///
/// Test-only, and specifically for tests in *other* crates: `aster-pvr`'s import
/// tests have to start from a [`DmaBuf`], and cannot build one. [`export`] needs a
/// [`DrmDevice`], and [`DrmDevice::new_unregistered`] is `pub(crate)` on purpose,
/// so that a driver cannot obtain a device without registering it. Rather than
/// widen that, this hands out the one shape an importer needs.
///
/// The exporting device is left dangling, which is not a shortcut but the case
/// under test: [`DmaBuf::is_from`] then answers `false` for every device, so the
/// buffer is foreign to whoever imports it — and a foreign buffer is the only
/// kind that reaches [`crate::device::DrmDriver::import_buffer`].
#[cfg(any(test, ktest))]
pub fn foreign_buffer_for_test(object: Arc<dyn GemObject>) -> Arc<DmaBuf> {
    Arc::new(DmaBuf {
        object,
        device: Weak::new(),
        fences: FenceSet::new(),
    })
}

/// One way of reaching a shared buffer: an object, and the buffer it belongs
/// to.
///
/// Both halves are weak. The buffer is kept alive by the descriptors user
/// space holds, the object by the handles and mappings that refer to it, and
/// neither should be kept alive by a lookup table. Dead entries are reaped on
/// the next export rather than by a destructor, because nothing calls back
/// into this table when an [`Arc`] reaches zero — the same arrangement as
/// [`NameTable`] and [`MmapOffsets`].
///
/// [`NameTable`]: crate::gem::NameTable
/// [`MmapOffsets`]: crate::gem::MmapOffsets
#[derive(Debug)]
struct Export {
    object: Weak<dyn GemObject>,
    buffer: Weak<DmaBuf>,
}

/// Every object that maps to a shared buffer, so that one object exports to
/// one [`DmaBuf`].
///
/// There are two entries per shared buffer that has crossed a device
/// boundary: the exporter's object, and the [`ImportedObject`] the importer
/// received. Looking either one up yields the same [`DmaBuf`], which is what
/// makes re-exporting an imported buffer return the original rather than
/// wrapping it again — upstream reaches the same place through
/// `obj->import_attach->dmabuf`.
///
/// A device-wide table would do, since a GEM object belongs to one device, but
/// it is keyed by object identity and that is already global; splitting it
/// would gain nothing and would have to live in [`DrmDevice`].
static EXPORTS: SpinLock<Vec<Export>> = SpinLock::new(Vec::new());

/// Returns the buffer an object already belongs to, if any.
fn exported_buffer(object: &Arc<dyn GemObject>) -> Option<Arc<DmaBuf>> {
    let mut exports = EXPORTS.lock();
    reap(&mut exports);

    find_buffer(&exports, object)
}

fn find_buffer(exports: &[Export], object: &Arc<dyn GemObject>) -> Option<Arc<DmaBuf>> {
    let identity = Arc::downgrade(object);
    exports
        .iter()
        .find(|entry| Weak::ptr_eq(&entry.object, &identity))
        .and_then(|entry| entry.buffer.upgrade())
}

/// Records that `object` is a way of reaching `buffer`.
fn record_export(object: &Arc<dyn GemObject>, buffer: &Arc<DmaBuf>) {
    let mut exports = EXPORTS.lock();
    // Reaped on both paths, so that a workload which only ever imports —
    // a compositor taking frames from its clients — does not grow the table
    // without bound.
    reap(&mut exports);

    exports.push(Export {
        object: Arc::downgrade(object),
        buffer: Arc::downgrade(buffer),
    });
}

/// Drops the entries whose object or buffer has been released.
fn reap(exports: &mut Vec<Export>) {
    exports.retain(|export| export.object.strong_count() > 0 && export.buffer.strong_count() > 0);
}

/// Exports an object as a shareable buffer.
///
/// Returns the same [`DmaBuf`] for repeated exports of one object; see the
/// module documentation for what depends on that.
///
/// Re-exporting an imported buffer returns the buffer it was imported from,
/// rather than wrapping it a second time, which is what
/// `drm_gem_prime_handle_to_fd` does with `obj->import_attach->dmabuf`. A
/// frame that travels GPU → compositor → display therefore keeps one identity
/// the whole way.
///
/// # Errors
///
/// [`DrmError::InvalidArgument`] (`EINVAL`) if the object refuses to be
/// exported. Upstream reports the same for a driver whose `gem_prime_export`
/// is absent or fails; the PowerVR objects that decline are the
/// firmware-protected ones, which must never be reachable from another process.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_prime.c#L400>
/// (`drm_gem_prime_handle_to_fd`).
pub fn export(device: &Arc<DrmDevice>, object: &Arc<dyn GemObject>) -> DrmResult<Arc<DmaBuf>> {
    if !object.is_exportable() {
        return Err(DrmError::InvalidArgument);
    }

    if let Some(existing) = exported_buffer(object) {
        return Ok(existing);
    }

    let buffer = Arc::new(DmaBuf {
        object: object.clone(),
        device: Arc::downgrade(device),
        fences: FenceSet::new(),
    });
    crate::live::note_dmabuf_alloc();
    // Allocation is outside the table lock. Two first exporters may both
    // allocate, but only one publishes an identity and its implicit fences.
    let existing = {
        let mut exports = EXPORTS.lock();
        reap(&mut exports);
        let existing = find_buffer(&exports, object);
        if existing.is_none() {
            exports.push(Export {
                object: Arc::downgrade(object),
                buffer: Arc::downgrade(&buffer),
            });
        }
        existing
    };
    // The losing candidate may release driver objects; never drop it under
    // EXPORTS, since their destruction may itself reach PRIME.
    Ok(existing.unwrap_or(buffer))
}

/// Imports a shared buffer into a device, producing an object usable by it.
///
/// When the buffer originated from this same device, returns the original
/// object rather than a wrapper, which is what lets [`HandleTable::find`] hand
/// back the existing handle. That is not merely an optimization: a
/// self-imported wrapper would be a second object over one allocation, and the
/// handle table would have no way to tell that a client already holds it.
///
/// A buffer from another device is offered to the importing driver first, through
/// [`DrmDriver::import_buffer`]. A driver whose hardware has its own address
/// space answers with an object it can actually use; one that declines gets the
/// generic [`ImportedObject`], which cannot be mapped — its pages belong to the
/// exporter, and a driver that only wraps them has no business handing them out.
/// Upstream refuses the same thing in `drm_gem_dumb_map_offset`, by checking
/// `obj->import_attach`.
///
/// [`HandleTable::find`]: crate::gem::HandleTable::find
/// [`DrmDriver::import_buffer`]: crate::device::DrmDriver::import_buffer
pub fn import(device: &Arc<DrmDevice>, buffer: &Arc<DmaBuf>) -> DrmResult<Arc<dyn GemObject>> {
    if buffer.is_from(device) {
        return Ok(buffer.object.clone());
    }

    let object: Arc<dyn GemObject> = match device.driver().import_buffer(buffer)? {
        Some(object) => object,
        None => Arc::new(ImportedObject {
            buffer: buffer.clone(),
        }),
    };
    // So that re-exporting this view hands back the buffer it came from
    // instead of wrapping it a second time.
    record_export(&object, buffer);

    Ok(object)
}

/// A buffer another device exported, seen from this one.
///
/// It forwards the properties an importer legitimately needs — how big the
/// buffer is, and what kind of memory it is — and refuses everything that
/// would require touching pages it does not own.
#[derive(Debug)]
pub struct ImportedObject {
    buffer: Arc<DmaBuf>,
}

impl ImportedObject {
    /// Returns the buffer this object is a view of.
    pub fn buffer(&self) -> &Arc<DmaBuf> {
        &self.buffer
    }
}

impl GemObject for ImportedObject {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn size(&self) -> usize {
        self.buffer.object.size()
    }

    fn buffer_use(&self) -> BufferUse {
        self.buffer.object.buffer_use()
    }

    /// Returns `None`: the pages belong to the exporting driver.
    ///
    /// Not a forward to the exporter's segment, even though one exists in the
    /// same-process case. A driver that got the frames this way would map them
    /// with its own cache policy and free them on its own schedule, and on a
    /// SoC with no DMA coherency that is how a buffer ends up written through
    /// two mappings with different attributes.
    fn segment(&self) -> Option<&Segment<()>> {
        None
    }

    /// Always fails: [`is_mappable`](Self::is_mappable) already said so.
    ///
    /// Reachable only through a fake `mmap` offset, and one is never handed
    /// out for an unmappable object, so this is a backstop rather than a path.
    fn fault(&self, _pgoff: usize) -> DrmResult<UFrame> {
        Err(DrmError::BadAddress)
    }

    fn is_mappable(&self) -> bool {
        false
    }
}
