// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The fences attached to a buffer.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-resv.c>
//! (`dma_resv`, which this is the reduced form of).

use alloc::{sync::Arc, vec::Vec};

use ostd::sync::SpinLock;

use super::Fence;

/// A set of fences that must signal before a buffer may be used.
///
/// Attached to a buffer rather than to a client, so that an importer learns
/// what it must wait for without the exporter and importer having to agree on
/// anything else. This is what makes implicit synchronization work across the
/// dma-buf boundary, and it is why the set lives on [`DmaBuf`] rather than in
/// either driver.
///
/// # The readers/writer rule
///
/// One exclusive fence and any number of shared ones, which is
/// `dma_resv`'s model reduced to what this port needs:
///
///  - a *reader* must wait for the exclusive fence, because the last write to
///    the buffer has to have landed before it looks;
///  - a *writer* must wait for the exclusive fence *and* every shared one,
///    because it may not overwrite data a reader is still fetching.
///
/// Adding a write therefore retires the shared fences: a writer that has
/// already waited for all of them makes them redundant, and keeping them would
/// make every later writer wait for reads that finished long ago.
#[derive(Debug)]
pub struct FenceSet {
    inner: SpinLock<Inner>,
}

impl Default for FenceSet {
    fn default() -> Self {
        Self {
            inner: SpinLock::new(Inner::default()),
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// The last write. A reader waits for this and nothing else.
    exclusive: Option<Arc<dyn Fence>>,
    /// Reads in flight. Only a writer waits for these.
    shared: Vec<Arc<dyn Fence>>,
}

impl FenceSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a read of the buffer.
    pub fn add_read(&self, fence: Arc<dyn Fence>) {
        let retired = {
            let mut inner = self.inner.lock();
            let retired: Vec<_> = inner
                .shared
                .extract_if(.., |fence| fence.is_signaled())
                .collect();
            inner.shared.push(fence);
            retired
        };
        drop(retired);
    }

    /// Records a write of the buffer, retiring the reads it supersedes.
    pub fn add_write(&self, fence: Arc<dyn Fence>) {
        let retired = {
            let mut inner = self.inner.lock();
            (
                core::mem::take(&mut inner.shared),
                inner.exclusive.replace(fence),
            )
        };
        drop(retired);
    }

    /// Returns the fences a reader must wait for.
    pub fn read_dependencies(&self) -> Vec<Arc<dyn Fence>> {
        let inner = self.inner.lock();
        inner
            .exclusive
            .iter()
            .filter(|fence| !fence.is_signaled())
            .cloned()
            .collect()
    }

    /// Returns the fences a writer must wait for.
    pub fn write_dependencies(&self) -> Vec<Arc<dyn Fence>> {
        let inner = self.inner.lock();
        inner
            .exclusive
            .iter()
            .chain(inner.shared.iter())
            .filter(|fence| !fence.is_signaled())
            .cloned()
            .collect()
    }

    /// Drops every fence that has already signalled.
    ///
    /// Nothing calls this automatically: a fence does not tell the sets it
    /// belongs to when it completes. A buffer that is used repeatedly is
    /// pruned by [`add_read`](Self::add_read) as a side effect; a buffer that
    /// is written once and then held needs this if the fence is large.
    pub fn prune(&self) {
        let retired = {
            let mut inner = self.inner.lock();
            let exclusive = if inner
                .exclusive
                .as_ref()
                .is_some_and(|fence| fence.is_signaled())
            {
                inner.exclusive.take()
            } else {
                None
            };
            let shared: Vec<_> = inner
                .shared
                .extract_if(.., |fence| fence.is_signaled())
                .collect();
            (exclusive, shared)
        };
        drop(retired);
    }

    /// Returns whether every fence in the set has signalled.
    pub fn is_idle(&self) -> bool {
        self.write_dependencies().is_empty()
    }
}
