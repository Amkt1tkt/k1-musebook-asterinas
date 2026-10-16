// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: notify cgroup.events watchers when subtree population changes.

use super::{inode::CgroupInode, systree_node::CgroupNode};
use crate::{
    fs::vfs::{inode::Inode, inode_ext::InodeExt, notify::FsEvents},
    prelude::*,
};

/// One cgroup can have inodes in multiple mounted or namespace-rooted views.
/// Keep weak references so notification does not extend any inode's lifetime.
pub(super) struct CgroupEvents {
    inodes: Mutex<Vec<Weak<CgroupInode>>>,
}

impl CgroupEvents {
    pub(super) fn new() -> Self {
        Self {
            inodes: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn register(&self, inode: &Arc<CgroupInode>) {
        let mut inodes = self.inodes.lock();
        inodes.retain(|inode| inode.strong_count() > 0);
        inodes.push(Arc::downgrade(inode));
    }

    pub(super) fn notify(&self) {
        let inodes: Vec<_> = {
            let mut inodes = self.inodes.lock();
            inodes.retain(|inode| inode.strong_count() > 0);
            inodes.iter().filter_map(Weak::upgrade).collect()
        };
        // Never enter an inode publisher or wake epoll with the registry or a
        // cgroup inner/children lock held. Inotify only queues the event here;
        // userspace subsequently re-reads the current populated value.
        for inode in inodes {
            if let Some(publisher) = (&*inode as &dyn Inode).fs_event_publisher() {
                publisher.publish_event(FsEvents::MODIFY, None);
            }
        }
    }
}

/// Accumulate every 0 <-> 1 transition before publishing any notification.
/// Migration adds to the destination before removing from the source, so a
/// common ancestor which stays populated never emits a spurious empty event.
#[derive(Default)]
pub(super) struct PopulatedChanges(Vec<Arc<CgroupNode>>);

impl PopulatedChanges {
    pub(super) fn record(&mut self, node: Arc<CgroupNode>) {
        self.0.push(node);
    }

    pub(super) fn notify(self) {
        // The caller still holds the membership guard to serialize migration
        // and exit, but all node inner/children locks have been released.
        // FS event delivery never acquires the cgroup membership lock.
        for node in self.0 {
            node.notify_populated_changed();
        }
    }
}
