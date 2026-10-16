// SPDX-License-Identifier: MPL-2.0

use super::fs::CgroupFs;
use crate::{
    fs::{
        cgroupfs::CgroupNode,
        file::InodeMode,
        utils::systree_inode::{SysTreeInodeTy, SysTreeNodeKind},
        vfs::{
            file_system::FileSystem,
            inode::{Extension, Inode, Metadata, RevalidationPolicy},
            path::Dentry,
        },
    },
    prelude::*,
};

/// An inode abstraction used in the cgroup file system.
pub(super) struct CgroupInode {
    /// The corresponding node in the SysTree.
    node_kind: SysTreeNodeKind,
    /// The metadata of this inode.
    metadata: Metadata,
    /// The extension of this inode.
    extension: Extension,
    /// The file mode (permissions) of this inode, protected by a lock.
    mode: RwLock<InodeMode>,
    /// Weak reference to the parent inode.
    parent: Weak<CgroupInode>,
    /// Weak self-reference for cyclic data structures.
    this: Weak<CgroupInode>,
}

impl SysTreeInodeTy for CgroupInode {
    // K1 MUSE Book --- (register every cgroup.events inode view for population notifications)
    // fn new_arc(
    //     node_kind: SysTreeNodeKind,
    //     metadata: Metadata,
    //     mode: InodeMode,
    //     parent: Weak<Self>,
    // ) -> Arc<Self>
    // where
    //     Self: Sized,
    // {
    //     Arc::new_cyclic(|this| Self {
    //         node_kind,
    //         metadata,
    //         extension: Extension::new(),
    //         mode: RwLock::new(mode),
    //         parent,
    //         this: this.clone(),
    //     })
    // }
    fn new_arc(
        node_kind: SysTreeNodeKind,
        metadata: Metadata,
        mode: InodeMode,
        parent: Weak<Self>,
    ) -> Arc<Self>
    where
        Self: Sized,
    {
        let inode = Arc::new_cyclic(|this| Self {
            node_kind,
            metadata,
            extension: Extension::new(),
            mode: RwLock::new(mode),
            parent,
            this: this.clone(),
        });
        if let SysTreeNodeKind::Attr(attr, node) = &inode.node_kind
            && attr.name().as_ref() == "cgroup.events"
        {
            let cgroup = Arc::downcast::<CgroupNode>(node.clone()).unwrap();
            cgroup.register_events_inode(&inode);
        }
        inode
    }
    // K1 MUSE Book ---

    fn node_kind(&self) -> &SysTreeNodeKind {
        &self.node_kind
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn extension(&self) -> &Extension {
        &self.extension
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(*self.mode.read())
    }

    fn set_mode(&self, mode: InodeMode) -> Result<()> {
        *self.mode.write() = mode;
        Ok(())
    }

    fn parent(&self) -> &Weak<Self> {
        &self.parent
    }

    fn this(&self) -> Arc<Self> {
        self.this
            .upgrade()
            .expect("invalid weak reference to `self`")
    }
}

impl Inode for CgroupInode {
    fn fs(&self) -> Arc<dyn FileSystem> {
        CgroupFs::singleton().clone()
    }

    fn rmdir(&self, child_dentry: &Dentry) -> Result<()> {
        let name = child_dentry.name();
        let SysTreeNodeKind::Branch(branch_node) = self.node_kind() else {
            return_errno_with_message!(Errno::ENOTDIR, "the current node is not a branch node");
        };

        let Some(child) = branch_node.child(&name) else {
            return_errno_with_message!(Errno::ENOENT, "the child node does not exist");
        };

        let target_node = Arc::downcast::<CgroupNode>(child).unwrap();

        // This will succeed only if the child is empty and has not been removed.
        target_node.mark_as_dead()?;

        // This is guaranteed to remove `child` because the dentry lock prevents
        // concurrent modification to the children, and there are no races because
        // `mark_as_dead` can succeed at most once.
        branch_node.remove_child(&name).unwrap();

        Ok(())
    }

    fn revalidation_policy(&self) -> RevalidationPolicy {
        match self.node_kind() {
            SysTreeNodeKind::Branch(_) | SysTreeNodeKind::Leaf(_) => {
                RevalidationPolicy::REVALIDATE_EXISTS | RevalidationPolicy::REVALIDATE_ABSENT
            }
            SysTreeNodeKind::Attr(..) | SysTreeNodeKind::Symlink(_) => RevalidationPolicy::empty(),
        }
    }

    // K1 MUSE Book --- (preserve the permanent events inode and its existing inotify watches)
    // fn revalidate_exists(&self, _name: &str, child: &dyn Inode) -> bool {
    //     // Attribute nodes in the cache should not be trusted because they
    //     // may be dynamically created or removed based on the state of the
    //     // cgroup controller.
    //     child
    //         .downcast_ref::<Self>()
    //         .is_some_and(|child| !matches!(child.node_kind(), SysTreeNodeKind::Attr(..)))
    // }
    fn revalidate_exists(&self, _name: &str, child: &dyn Inode) -> bool {
        let Some(child) = child.downcast_ref::<Self>() else {
            return false;
        };
        match child.node_kind() {
            // Unlike controller attributes, cgroup.events is permanent. Replacing
            // its inode on every open discards the publisher behind inotify watches.
            SysTreeNodeKind::Attr(attr, node) if attr.name().as_ref() == "cgroup.events" => {
                let SysTreeNodeKind::Branch(parent) = self.node_kind() else {
                    return false;
                };
                if parent.id() != node.id() {
                    return false;
                }
                Arc::downcast::<CgroupNode>(node.clone()).is_ok_and(|node| node.is_alive())
            }
            // Preserve revalidation for dynamically enabled controller attributes.
            SysTreeNodeKind::Attr(..) => false,
            _ => true,
        }
    }
    // K1 MUSE Book ---

    fn revalidate_absent(&self, _name: &str) -> bool {
        false
    }
}
