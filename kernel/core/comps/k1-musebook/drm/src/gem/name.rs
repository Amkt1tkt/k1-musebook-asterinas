// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The global flink name space.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L830>
//! (`drm_gem_flink_ioctl`) and
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem.c#L880>
//! (`drm_gem_open_ioctl`).

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use ostd::sync::SpinLock;

use super::GemObject;
use crate::error::{DrmError, DrmResult};

/// The device-wide table of flink names.
///
/// A flink name is the oldest way of sharing a buffer between processes:
/// `GEM_FLINK` turns a handle into a small integer valid device-wide, and
/// `GEM_OPEN` turns that integer back into a handle in another file. The name
/// carries no notion of who may use it — any client that passes `GEM_OPEN`'s
/// `DRM_AUTH` check and guesses the number gets a handle — which is why PRIME
/// replaced it and why nothing new should be built on it. Mesa still takes
/// this path for some buffers, so it has to work.
///
/// # Locking
///
/// A `SpinLock`, matching the handle table: the critical sections are a
/// lookup or an insertion into a small map, and it is a leaf lock.
#[derive(Debug)]
pub struct NameTable {
    inner: SpinLock<Inner>,
}

impl Default for NameTable {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct Inner {
    /// Weak, so that a name does not keep its object alive.
    names: BTreeMap<u32, Weak<dyn GemObject>>,
    /// The next name to hand out. Names start at 1 and are never reused, as
    /// `idr_alloc(&dev->object_name_idr, obj, 1, 0, ...)` does not reuse a
    /// name while any client might still be holding it.
    next: u32,
}

impl NameTable {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(Inner {
                names: BTreeMap::new(),
                next: 1,
            }),
        }
    }

    /// Publishes an object under a global name, or returns the name it already
    /// has.
    ///
    /// Idempotent, matching `drm_gem_flink_ioctl`, which only allocates when
    /// `obj->name` is still zero. User space compares names for equality to
    /// decide whether two handles are the same buffer, so handing out a second
    /// name for one object would break that.
    pub fn publish(&self, object: &Arc<dyn GemObject>) -> DrmResult<u32> {
        let mut inner = self.inner.lock();

        inner.names.retain(|_, weak| weak.strong_count() > 0);

        let identity = Arc::downgrade(object);
        if let Some((name, _)) = inner
            .names
            .iter()
            .find(|(_, weak)| Weak::ptr_eq(weak, &identity))
        {
            return Ok(*name);
        }

        let name = inner.next;
        if name == u32::MAX {
            return Err(DrmError::NoSpace);
        }
        inner.next = name + 1;
        inner.names.insert(name, Arc::downgrade(object));

        Ok(name)
    }

    /// Resolves a global name.
    ///
    /// Returns `None` for a name that was never handed out and for one whose
    /// object has since been released. Linux removes the name when the last
    /// handle is closed, so both cases are `ENOENT` there too.
    pub fn lookup(&self, name: u32) -> Option<Arc<dyn GemObject>> {
        self.inner.lock().names.get(&name)?.upgrade()
    }

    /// Returns the name an object was published under, if any.
    pub fn name_of(&self, object: &Arc<dyn GemObject>) -> Option<u32> {
        self.inner
            .lock()
            .names
            .iter()
            .find(|(_, weak)| {
                weak.upgrade()
                    .is_some_and(|alive| Arc::ptr_eq(&alive, object))
            })
            .map(|(name, _)| *name)
    }
}
