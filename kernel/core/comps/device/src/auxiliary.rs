// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: register auxiliary sysfs subtrees under device-model-owned roots.

//! Registration of sysfs subtrees that are not yet device-model classes or buses.
//!
//! The device model retains exclusive ownership of its roots and managed nodes.
//! These functions reserve ordinary [`DirNode`] children for an auxiliary
//! subsystem. A name already owned by a managed node is a conflict, even when
//! that node is a directory. Auxiliary directories persist for the kernel's
//! lifetime; callers may edit their children but cannot remove a shared root.
//! No device lifecycle or devtmpfs hooks run for auxiliary registrations.

use alloc::{string::ToString, sync::Arc};

use aster_systree::{DirNode, SysBranchNode};

use crate::common::{
    DevNum, Error, Result, SysStr,
    node::{Dir, SysTreeEdit, add_link},
    registry,
};

/// Returns or registers `/sys/devices/<name>`.
pub fn device_directory(name: SysStr) -> Result<Arc<DirNode>> {
    directory(registry::get().devices_root(), name)
}

/// Returns or registers `/sys/devices/virtual/<name>`.
pub fn virtual_directory(name: SysStr) -> Result<Arc<DirNode>> {
    directory(registry::get().virtual_root(), name)
}

/// Returns or registers `/sys/bus/<name>`.
pub fn bus_directory(name: SysStr) -> Result<Arc<DirNode>> {
    directory(registry::get().bus_root(), name)
}

/// Returns or registers `/sys/class/<name>`.
pub fn class_directory(name: SysStr) -> Result<Arc<DirNode>> {
    directory(registry::get().class_root(), name)
}

/// Adds a device-number index pointing to an absolute path within the sysfs tree.
///
/// The caller publishes the target first and rolls back only its own target if
/// this operation fails. Existing links are never replaced or removed. This API
/// serves persistent auxiliary nodes, not hot-unplug device lifetimes.
pub fn register_dev_link(number: DevNum, target_path: &str) -> Result<()> {
    if !target_path.starts_with('/') {
        return Err(Error::InvalidValue);
    }
    add_link(
        registry::get().dev_index(number.kind()).as_ref(),
        &number.to_string(),
        target_path,
    )
}

fn directory(parent: &Dir, name: SysStr) -> Result<Arc<DirNode>> {
    // DirNode's constructor validates names by panicking; return a registration
    // error instead, before allocating or touching the shared tree.
    if !aster_systree::is_valid_name(&name) {
        return Err(Error::InvalidName);
    }
    let candidate = DirNode::new(name.clone());
    // The parent serializes attachment. Concurrent callers either publish their
    // candidate or reuse the winner; a managed Dir is never downcast or adopted.
    match parent.attach_child(candidate.clone()) {
        Ok(()) => Ok(candidate),
        Err(Error::NameConflict) => parent
            .child(&name)
            .and_then(|node| node.as_any().downcast_ref::<DirNode>().map(DirNode::this))
            .ok_or(Error::NameConflict),
        Err(error) => Err(error),
    }
}

#[cfg(ktest)]
mod test {
    use aster_systree::SysObj;
    use device_id::{DeviceId, MajorId, MinorId};
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn auxiliary_directories_share_only_their_own_nodes() {
        crate::init_for_ktest();
        let first = device_directory("aux-dir-test".into()).unwrap();
        let second = device_directory("aux-dir-test".into()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(matches!(
            device_directory("virtual".into()),
            Err(Error::NameConflict)
        ));
        for invalid in ["", ".", "..", "bad/name", "bad\0name"] {
            assert!(matches!(
                device_directory(invalid.into()),
                Err(Error::InvalidName)
            ));
        }
        assert!(matches!(
            registry::get().devices_root().create_child("forbidden"),
            Err(aster_systree::Error::PermissionDenied)
        ));
        let class = class_directory("aux-class-test".into()).unwrap();
        let bus = bus_directory("aux-bus-test".into()).unwrap();
        let virtual_dir = virtual_directory("aux-virtual-test".into()).unwrap();
        assert_eq!(class.path().as_ref(), "/class/aux-class-test");
        assert_eq!(bus.path().as_ref(), "/bus/aux-bus-test");
        assert_eq!(
            virtual_dir.path().as_ref(),
            "/devices/virtual/aux-virtual-test"
        );
    }

    #[ktest]
    fn auxiliary_index_collision_preserves_the_original_link() {
        crate::init_for_ktest();
        let number = DevNum::char(DeviceId::new(MajorId::new(240), MinorId::new(900)));
        let target = device_directory("aux-index-test".into()).unwrap();
        register_dev_link(number, target.path().as_ref()).unwrap();
        let index = registry::get().dev_index(number.kind());
        let first = index.child("240:900").unwrap();
        assert!(matches!(
            register_dev_link(number, "/devices/elsewhere"),
            Err(Error::NameConflict)
        ));
        assert!(Arc::ptr_eq(&first, &index.child("240:900").unwrap()));
        assert_eq!(
            first.cast_to_symlink().unwrap().target_path(),
            "../../devices/aux-index-test"
        );
        assert!(matches!(
            register_dev_link(number, "relative"),
            Err(Error::InvalidValue)
        ));
    }
}
