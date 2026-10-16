// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The per-file GEM handle table.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L357>
//! (`drm_gem_handle_create_tail`) and
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L276>
//! (`drm_gem_handle_delete`).

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
    vec::Vec,
};

use ostd::sync::SpinLock;

use super::{GemHandle, GemObject};
use crate::{
    error::{DrmError, DrmResult},
    prime::DmaBuf,
};

/// A file's GEM handle table.
///
/// Handles are dense and reused, matching Linux's `idr` behaviour, because user
/// space occasionally assumes small handle values.
///
/// # Locking
///
/// A `SpinLock`, not a `Mutex`: [`find`](Self::find) is reached from the page
/// fault path, which runs with preemption disabled. The lock is a leaf — no
/// code under it calls back into the VM layer, and objects removed from the
/// table are dropped after it is released, so that freeing their frames does
/// not happen with the lock held.
#[derive(Debug)]
pub struct HandleTable {
    inner: SpinLock<Inner>,
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Inner {
    objects: BTreeMap<u32, Arc<dyn GemObject>>,
    // Per-file PRIME identities. Weak references must not keep buffers alive;
    // GEM_CLOSE removes every entry for the closing handle before its reuse.
    imports: Vec<Import>,
    /// Handles below `next` that have been closed and can be handed out again.
    ///
    /// A `BTreeSet` rather than a bitmap because the table is unbounded:
    /// Linux's `idr` grows on demand, and a fixed-size bitmap large enough to
    /// match it would cost more per open file than every realistic client's
    /// handles put together.
    reusable: BTreeSet<u32>,
    /// The lowest handle never yet handed out.
    ///
    /// Starts at 1: handle 0 must never be allocated, because user space uses
    /// it as "no buffer".
    next: u32,
}

#[derive(Debug)]
struct Import {
    buffer: Weak<DmaBuf>,
    object: Weak<dyn GemObject>,
    handle: u32,
}

impl Inner {
    fn imported_handle(&self, buffer: &Arc<DmaBuf>) -> Option<GemHandle> {
        let identity = Arc::downgrade(buffer);
        self.imports.iter().find_map(|entry| {
            let live = self.objects.get(&entry.handle)?;
            (Weak::ptr_eq(&entry.buffer, &identity)
                && Weak::ptr_eq(&entry.object, &Arc::downgrade(live)))
            .then_some(GemHandle(entry.handle))
        })
    }

    fn alloc(&mut self) -> DrmResult<u32> {
        if let Some(handle) = self.reusable.pop_first() {
            return Ok(handle);
        }

        let handle = self.next;
        if handle == u32::MAX {
            return Err(DrmError::NoSpace);
        }
        self.next = handle + 1;

        Ok(handle)
    }
}

impl HandleTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(Inner {
                objects: BTreeMap::new(),
                imports: Vec::new(),
                reusable: BTreeSet::new(),
                next: 1,
            }),
        }
    }

    /// Installs an object and returns its new handle.
    ///
    /// The table takes its own reference, so the caller keeps ownership of the
    /// [`Arc`] it passed in. Installing the same object twice yields two
    /// handles, both of which must be closed before the table lets go of it,
    /// exactly like Linux's per-object `handle_count`.
    pub fn insert(&self, object: Arc<dyn GemObject>) -> DrmResult<GemHandle> {
        let mut inner = self.inner.lock();

        let handle = inner.alloc()?;
        inner.objects.insert(handle, object);
        crate::live::note_gem_handle_alloc();

        Ok(GemHandle(handle))
    }

    /// Resolves a PRIME identity without invoking the importing driver.
    pub(crate) fn find_import(&self, buffer: &Arc<DmaBuf>) -> Option<GemHandle> {
        self.inner.lock().imported_handle(buffer)
    }

    /// Installs a driver-produced view, rechecking identity under one lock.
    ///
    /// Another importer may have run while the driver built `object`. The
    /// loser is dropped after unlocking, just like an object removed by close.
    pub(crate) fn insert_import(
        &self,
        buffer: &Arc<DmaBuf>,
        object: Arc<dyn GemObject>,
    ) -> DrmResult<GemHandle> {
        let mut inner = self.inner.lock();
        if let Some(handle) = inner.imported_handle(buffer) {
            return Ok(handle);
        }
        // Re-exported self-imports can already have an ordinary GEM handle.
        let handle = if let Some((&handle, _)) = inner
            .objects
            .iter()
            .find(|(_, live)| Arc::ptr_eq(live, &object))
        {
            handle
        } else {
            let handle = inner.alloc()?;
            inner.objects.insert(handle, object.clone());
            crate::live::note_gem_handle_alloc();
            handle
        };
        // A self-import does not keep DmaBuf alive. Reap its dead metadata on
        // subsequent imports, as well as removing entries at GEM_CLOSE.
        inner
            .imports
            .retain(|entry| entry.buffer.strong_count() != 0);
        inner.imports.push(Import {
            buffer: Arc::downgrade(buffer),
            object: Arc::downgrade(&object),
            handle,
        });
        Ok(GemHandle(handle))
    }

    /// Resolves a handle.
    ///
    /// # Errors
    ///
    /// [`DrmError::NotFound`] (`ENOENT`) if the handle is not in this table,
    /// which is what `drm_gem_object_lookup` failing reports for every ioctl
    /// that takes a handle — except the two that delete one, see
    /// [`remove`](Self::remove).
    pub fn get(&self, handle: GemHandle) -> DrmResult<Arc<dyn GemObject>> {
        self.inner
            .lock()
            .objects
            .get(&handle.0)
            .cloned()
            .ok_or(DrmError::NotFound)
    }

    /// Removes a handle, dropping this table's reference to the object.
    ///
    /// # Errors
    ///
    /// [`DrmError::InvalidArgument`] (`EINVAL`) if the handle is not in this
    /// table. That is deliberately not the `ENOENT` of [`get`](Self::get):
    /// `drm_gem_handle_delete` reports a bad handle as `EINVAL`, and both
    /// `GEM_CLOSE` and `MODE_DESTROY_DUMB` return it unchanged.
    pub fn remove(&self, handle: GemHandle) -> DrmResult<()> {
        let object = {
            let mut inner = self.inner.lock();
            let object = inner
                .objects
                .remove(&handle.0)
                .ok_or(DrmError::InvalidArgument)?;
            inner.imports.retain(|entry| entry.handle != handle.0);
            inner.reusable.insert(handle.0);
            object
        };

        crate::live::note_gem_handle_free();
        // Dropped outside the lock on purpose: this may be the last reference,
        // in which case the object's frames go back to the frame allocator.
        drop(object);

        Ok(())
    }

    /// Returns whether the table holds a given object, used by PRIME import to
    /// return the existing handle instead of creating a duplicate.
    ///
    /// Also the first half of the check that decides whether a file may `mmap`
    /// an object when the kernel validates a new mapping.
    ///
    /// Linear in the number of open handles. Linux keeps a red-black tree
    /// keyed by object address for this (`drm_prime_file_private`), which is
    /// worth having once a client holds thousands of buffers; a scan is enough
    /// while the callers are import and the fault path's permission check.
    pub fn find(&self, object: &Arc<dyn GemObject>) -> Option<GemHandle> {
        self.inner
            .lock()
            .objects
            .iter()
            .find(|(_, candidate)| Arc::ptr_eq(candidate, object))
            .map(|(handle, _)| GemHandle(*handle))
    }

    /// Returns how many handles are open.
    pub fn len(&self) -> usize {
        self.inner.lock().objects.len()
    }

    /// Returns whether no handle is open.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns every open handle, in increasing order.
    pub fn handles(&self) -> Vec<GemHandle> {
        self.inner
            .lock()
            .objects
            .keys()
            .map(|handle| GemHandle(*handle))
            .collect()
    }
}

impl Drop for HandleTable {
    fn drop(&mut self) {
        // Remaining handles die with the file. Counted here rather than by
        // walking `remove`, which would need a handle the destructor does
        // not have and would reuse numbers on a table that is going away.
        let n = self.inner.lock().objects.len();
        crate::live::note_gem_handles_drop(n);
    }
}
