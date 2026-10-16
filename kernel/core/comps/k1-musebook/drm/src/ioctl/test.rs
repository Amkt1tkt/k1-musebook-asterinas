// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Tests for the dispatch table, its permission checks, and the copy-out
//! protocols.
//!
//! The permission matrix is the reason this file is as long as it is. Every
//! combination of node type and flag set is spelled out rather than generated,
//! because the point of the test is to state the intended answer independently of
//! the code that produces it.

use alloc::{string::String, sync::Arc, vec};

use ostd::{prelude::ktest, sync::SpinLock};

use super::*;
use crate::{
    device::{DriverFeatures, DriverInfo, DrmDriver},
    uapi::{DrmGetCap, DrmSetClientCap, DrmSetVersion, DrmUnique, DrmVersion, cap},
};

/// A [`UserMem`] over a fake address space, so that the two-pass copy protocols
/// can be exercised without a user process.
struct FakeUserMem {
    bytes: SpinLock<Vec<u8>>,
}

impl FakeUserMem {
    /// An arbitrary non-zero base, so that a zero address is distinguishable from
    /// the start of the buffer the way a null user pointer is.
    const BASE: u64 = 0x1000;

    fn new(len: usize) -> Self {
        Self {
            bytes: SpinLock::new(vec![0u8; len]),
        }
    }

    fn address_of(offset: usize) -> u64 {
        Self::BASE + offset as u64
    }

    fn range(&self, addr: u64, len: usize) -> DrmResult<core::ops::Range<usize>> {
        let start = addr
            .checked_sub(Self::BASE)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or(DrmError::BadAddress)?;
        let end = start.checked_add(len).ok_or(DrmError::BadAddress)?;
        if end > self.bytes.lock().len() {
            return Err(DrmError::BadAddress);
        }
        Ok(start..end)
    }

    fn read(&self, offset: usize, len: usize) -> Vec<u8> {
        self.bytes.lock()[offset..offset + len].to_vec()
    }
}

impl FakeUserMem {
    /// The caller context a handler is given. Memory only: this fixture stands
    /// in for an in-kernel caller, which has no descriptor table and no thread
    /// to block.
    fn context(&self) -> CallerContext<'_> {
        CallerContext::new(self)
    }
}

impl UserMem for FakeUserMem {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> DrmResult<()> {
        let range = self.range(addr, buf.len())?;
        buf.copy_from_slice(&self.bytes.lock()[range]);
        Ok(())
    }

    fn write_bytes(&self, addr: u64, buf: &[u8]) -> DrmResult<()> {
        let range = self.range(addr, buf.len())?;
        self.bytes.lock()[range].copy_from_slice(buf);
        Ok(())
    }
}

#[derive(Debug)]
struct TestDriver {
    features: DriverFeatures,
}

impl DrmDriver for TestDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "test",
            date: "20260729",
            desc: "dispatch table test driver",
            major: 1,
            minor: 2,
            patchlevel: 3,
        }
    }

    fn features(&self) -> DriverFeatures {
        self.features
    }

    fn unique(&self) -> String {
        String::from("platform:test:00")
    }
}

fn device_with(features: DriverFeatures) -> Arc<DrmDevice> {
    DrmDevice::new_unregistered(Arc::new(TestDriver { features }), Some(0), Some(128))
}

/// A device with both node kinds, mode setting, and no atomic support: the shape
/// used by the ioctl permission tests.
fn both_nodes() -> Arc<DrmDevice> {
    device_with(DriverFeatures::MODESET | DriverFeatures::GEM | DriverFeatures::RENDER)
}

fn desc(flags: IoctlFlags) -> IoctlDesc {
    IoctlDesc {
        nr: 0,
        name: "TEST",
        flags,
        handler: |_, _, _, _| Ok(()),
    }
}

const DENIED: DrmResult<()> = Err(DrmError::PermissionDenied);

#[ktest]
fn core_ioctls_are_sorted_and_unique() {
    for pair in CORE_IOCTLS.windows(2) {
        assert!(
            pair[0].nr < pair[1].nr,
            "{} and {} are out of order or duplicated",
            pair[0].name,
            pair[1].name
        );
    }
}

#[ktest]
fn open_grants_mastership_to_the_first_primary_client() {
    let device = both_nodes();

    let master = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    assert!(master.is_master());
    // Becoming master implies authentication, which is how a compositor that
    // never calls `AUTH_MAGIC` still passes the `DRM_AUTH` ioctls.
    assert!(master.is_authenticated());

    let second = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    assert!(!second.is_master());
    assert!(!second.is_authenticated());

    // A privileged client is authenticated from the start but is not handed
    // mastership behind the incumbent's back.
    let root = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    assert!(!root.is_master());
    assert!(root.is_authenticated());

    // Render clients never take part in mastership at all.
    let render = device
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();
    assert!(!render.is_master());
}

#[ktest]
fn primary_node_permissions() {
    let memory = FakeUserMem::new(0);
    let caller = memory.context();
    let root_owner = Arc::new(());
    let root_caller = memory.context().with_credentials(
        CallerIdentity::from_owner(&root_owner),
        Privilege::Privileged,
    );
    let device = both_nodes();
    let master = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    let guest = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    let root = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();

    // The unprivileged master satisfies everything except ROOT_ONLY.
    assert!(
        desc(IoctlFlags::empty())
            .check_permission(&master, &caller)
            .is_ok()
    );
    assert!(
        desc(IoctlFlags::AUTH)
            .check_permission(&master, &caller)
            .is_ok()
    );
    assert!(
        desc(IoctlFlags::MASTER)
            .check_permission(&master, &caller)
            .is_ok()
    );
    assert!(
        desc(IoctlFlags::RENDER_ALLOW)
            .check_permission(&master, &caller)
            .is_ok()
    );
    assert_eq!(
        desc(IoctlFlags::ROOT_ONLY).check_permission(&master, &caller),
        DENIED
    );

    // An unauthenticated, non-master client may only issue unrestricted ioctls.
    assert!(
        desc(IoctlFlags::empty())
            .check_permission(&guest, &caller)
            .is_ok()
    );
    assert!(
        desc(IoctlFlags::RENDER_ALLOW)
            .check_permission(&guest, &caller)
            .is_ok()
    );
    assert_eq!(
        desc(IoctlFlags::AUTH).check_permission(&guest, &caller),
        DENIED
    );
    assert_eq!(
        desc(IoctlFlags::MASTER).check_permission(&guest, &caller),
        DENIED
    );
    assert_eq!(
        desc(IoctlFlags::ROOT_ONLY).check_permission(&guest, &caller),
        DENIED
    );

    // Privilege covers AUTH and ROOT_ONLY, but never substitutes for mastership:
    // being root does not make it safe for two clients to program one display.
    assert!(
        desc(IoctlFlags::AUTH)
            .check_permission(&root, &root_caller)
            .is_ok()
    );
    assert!(
        desc(IoctlFlags::ROOT_ONLY)
            .check_permission(&root, &root_caller)
            .is_ok()
    );
    assert_eq!(
        desc(IoctlFlags::MASTER).check_permission(&root, &root_caller),
        DENIED
    );
    assert_eq!(
        desc(IoctlFlags::ROOT_ONLY.union(IoctlFlags::MASTER)).check_permission(&root, &root_caller),
        DENIED
    );
}

#[ktest]
fn render_node_permissions() {
    let memory = FakeUserMem::new(0);
    let caller = memory.context();
    let root_owner = Arc::new(());
    let root_caller = memory.context().with_credentials(
        CallerIdentity::from_owner(&root_owner),
        Privilege::Privileged,
    );
    let device = both_nodes();
    let render = device
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();
    let render_root = device
        .open(DrmMinor::Render, Privilege::Privileged)
        .unwrap();

    // Without RENDER_ALLOW nothing gets through, whatever else the flags say and
    // whoever the caller is. This is the whole reason render nodes are safe to
    // hand to unprivileged clients.
    for flags in [
        IoctlFlags::empty(),
        IoctlFlags::AUTH,
        IoctlFlags::MASTER,
        IoctlFlags::ROOT_ONLY,
        IoctlFlags::AUTH.union(IoctlFlags::MASTER),
    ] {
        assert_eq!(desc(flags).check_permission(&render, &caller), DENIED);
        assert_eq!(
            desc(flags).check_permission(&render_root, &root_caller),
            DENIED
        );
    }

    // With RENDER_ALLOW, AUTH is satisfied implicitly: a render client has no way
    // to obtain a magic token and is not supposed to need one.
    assert!(
        desc(IoctlFlags::RENDER_ALLOW)
            .check_permission(&render, &caller)
            .is_ok()
    );
    assert!(
        desc(IoctlFlags::AUTH.union(IoctlFlags::RENDER_ALLOW))
            .check_permission(&render, &caller)
            .is_ok()
    );

    // MASTER and ROOT_ONLY still mean what they say.
    assert_eq!(
        desc(IoctlFlags::MASTER.union(IoctlFlags::RENDER_ALLOW)).check_permission(&render, &caller),
        DENIED
    );
    assert_eq!(
        desc(IoctlFlags::ROOT_ONLY.union(IoctlFlags::RENDER_ALLOW))
            .check_permission(&render, &caller),
        DENIED
    );
    assert!(
        desc(IoctlFlags::ROOT_ONLY.union(IoctlFlags::RENDER_ALLOW))
            .check_permission(&render_root, &root_caller)
            .is_ok()
    );
    assert_eq!(
        desc(IoctlFlags::MASTER.union(IoctlFlags::RENDER_ALLOW))
            .check_permission(&render_root, &root_caller),
        DENIED
    );
}

/// The dispatcher must refuse a primary-only ioctl on a render node before the
/// handler can look at, or write to, the payload.
#[ktest]
fn dispatch_refuses_primary_only_ioctls_on_render_nodes() {
    let device = both_nodes();
    let render = device
        .open(DrmMinor::Render, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(64);

    for nr in [ioctl_nr::nr::GET_UNIQUE, ioctl_nr::nr::SET_CLIENT_CAP] {
        let mut payload = [0u8; 32];
        assert_eq!(
            device.ioctl(&render, nr, &mut payload, &user.context()),
            DENIED,
            "ioctl {nr:#04x} reached a render node"
        );
        assert_eq!(payload, [0u8; 32], "ioctl {nr:#04x} touched the payload");
    }

    // The two that Linux does mark `DRM_RENDER_ALLOW` go through. `GET_CAP` needs a
    // capability that exists: upstream `drm_getcap` has no case for zero and returns
    // `EINVAL`, so a zeroed payload would fail the permission-independent check and
    // say nothing about node permissions.
    let mut payload = DrmGetCap {
        capability: cap::device::DUMB_BUFFER,
        value: 0,
    }
    .as_bytes()
    .to_vec();
    assert!(
        device
            .ioctl(
                &render,
                ioctl_nr::nr::GET_CAP,
                &mut payload,
                &user.context()
            )
            .is_ok()
    );

    let mut payload = vec![0u8; size_of::<DrmVersion>()];
    assert!(
        device
            .ioctl(
                &render,
                ioctl_nr::nr::VERSION,
                &mut payload,
                &user.context()
            )
            .is_ok()
    );
}

#[ktest]
fn dispatch_rejects_unknown_command_numbers() {
    let device = both_nodes();
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(8);

    // 0x03 is `DRM_IOCTL_IRQ_BUSID`, one of the legacy interfaces `../../ABI.md`
    // declines to implement; 0xC6 is the first of the lease ioctls.
    for nr in [0x03, 0xC6, 0xFF] {
        assert_eq!(
            device.ioctl(&file, nr, &mut [], &user.context()),
            Err(DrmError::InvalidArgument)
        );
    }
}

/// `VERSION`'s three strings are the protocol `libdrm` depends on, so both passes
/// are checked, including the zero-length probe that must write nothing.
#[ktest]
fn version_round_trip() {
    let device = both_nodes();
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(256);

    // First pass: all capacities zero, all pointers null. Nothing may be written,
    // but the true lengths must come back.
    let mut payload = vec![0u8; size_of::<DrmVersion>()];
    device
        .ioctl(&file, ioctl_nr::nr::VERSION, &mut payload, &user.context())
        .unwrap();

    let probed = DrmVersion::from_bytes(&payload);
    assert_eq!(probed.version_major, 1);
    assert_eq!(probed.version_minor, 2);
    assert_eq!(probed.version_patchlevel, 3);
    assert_eq!(probed.name_len, "test".len());
    assert_eq!(probed.date_len, "20260729".len());
    assert_eq!(probed.desc_len, "dispatch table test driver".len());
    assert_eq!(user.read(0, 32), vec![0u8; 32]);

    // Second pass: buffers sized from the first answer.
    let (name_at, date_at, desc_at) = (0usize, 32usize, 64usize);
    let mut args = DrmVersion {
        name_len: probed.name_len,
        name: FakeUserMem::address_of(name_at),
        date_len: probed.date_len,
        date: FakeUserMem::address_of(date_at),
        desc_len: probed.desc_len,
        desc: FakeUserMem::address_of(desc_at),
        ..DrmVersion::default()
    };
    payload.copy_from_slice(args.as_bytes());
    device
        .ioctl(&file, ioctl_nr::nr::VERSION, &mut payload, &user.context())
        .unwrap();

    args = DrmVersion::from_bytes(&payload);
    assert_eq!(args.name_len, "test".len());
    assert_eq!(user.read(name_at, "test".len()), b"test".to_vec());
    assert_eq!(user.read(date_at, "20260729".len()), b"20260729".to_vec());
    assert_eq!(
        user.read(desc_at, "dispatch table test driver".len()),
        b"dispatch table test driver".to_vec()
    );
    // The copy is not NUL-terminated; the byte after the name is untouched.
    assert_eq!(user.read(name_at + "test".len(), 1), vec![0u8]);
}

/// A buffer shorter than the value gets a truncated copy but still learns the
/// full length, which is what lets a caller retry instead of guessing.
#[ktest]
fn version_truncates_but_reports_the_full_length() {
    let user = FakeUserMem::new(16);

    let reported =
        copy_out_string(&user.context(), FakeUserMem::address_of(0), 2, "powervr").unwrap();
    assert_eq!(reported, "powervr".len());
    assert_eq!(user.read(0, 3), b"po\0".to_vec());
}

#[ktest]
fn version_zero_capacity_writes_nothing() {
    let user = FakeUserMem::new(16);

    // A null pointer with a zero capacity is exactly what `drmGetVersion` sends on
    // its first call, so it must succeed rather than fault.
    assert_eq!(
        copy_out_string(&user.context(), 0, 0, "powervr").unwrap(),
        7
    );
    assert_eq!(user.read(0, 16), vec![0u8; 16]);
}

/// The bus ID stays hidden until a client asks for interface 1.1.
///
/// Upstream's `drm_setversion` is where `drm_set_busid` is called from, and
/// nothing else ever fills it in; libdrm's `drmOpenByName` depends on the
/// emptiness, because a non-empty bus ID on a fresh descriptor makes it look for
/// a PCI device to match and fail. `modetest -M spacemit` failed exactly that way
/// before the driver started withholding it.
#[ktest]
fn get_unique_is_empty_until_set_version() {
    let device = both_nodes();
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(64);

    let probe = DrmUnique {
        unique_len: 0,
        unique: 0,
    };
    let mut payload = probe.as_bytes().to_vec();
    device
        .ioctl(
            &file,
            ioctl_nr::nr::GET_UNIQUE,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert_eq!(DrmUnique::from_bytes(&payload).unique_len, 0);
}

/// `GET_UNIQUE` is all or nothing, unlike `VERSION`: a half-copied bus ID would be
/// compared for equality and quietly mismatch.
#[ktest]
fn get_unique_is_all_or_nothing() {
    let device = both_nodes();
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(64);
    let unique = "platform:test:00";

    // Ask for interface 1.1 first, as libdrm does. Without this the bus ID is
    // empty and everything below would be testing the wrong thing -- and this is
    // also the only test that runs `SET_VERSION` at all, which is how it shipped
    // unreachable from user space: `aster-drm` had the handler, and the shell in
    // `kernel/core/src/device/dri.rs` had no arm to decode the command into it.
    let mut version = DrmSetVersion {
        drm_di_major: 1,
        drm_di_minor: 1,
        drm_dd_major: -1,
        drm_dd_minor: -1,
    }
    .as_bytes()
    .to_vec();
    device
        .ioctl(
            &file,
            ioctl_nr::nr::SET_VERSION,
            &mut version,
            &user.context(),
        )
        .unwrap();

    let probe = DrmUnique {
        unique_len: 0,
        unique: 0,
    };
    let mut payload = probe.as_bytes().to_vec();
    device
        .ioctl(
            &file,
            ioctl_nr::nr::GET_UNIQUE,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert_eq!(DrmUnique::from_bytes(&payload).unique_len, unique.len());
    assert_eq!(user.read(0, 16), vec![0u8; 16]);

    // One byte short: nothing is written, and the length still comes back.
    let short = DrmUnique {
        unique_len: unique.len() - 1,
        unique: FakeUserMem::address_of(0),
    };
    payload.copy_from_slice(short.as_bytes());
    device
        .ioctl(
            &file,
            ioctl_nr::nr::GET_UNIQUE,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert_eq!(DrmUnique::from_bytes(&payload).unique_len, unique.len());
    assert_eq!(user.read(0, 16), vec![0u8; 16]);

    let exact = DrmUnique {
        unique_len: unique.len(),
        unique: FakeUserMem::address_of(0),
    };
    payload.copy_from_slice(exact.as_bytes());
    device
        .ioctl(
            &file,
            ioctl_nr::nr::GET_UNIQUE,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert_eq!(user.read(0, unique.len()), unique.as_bytes().to_vec());
}

#[ktest]
fn capabilities_follow_the_feature_bits() {
    let display = both_nodes();

    // Answered for every driver, mode-setting or not.
    assert_eq!(display.capability(cap::device::TIMESTAMP_MONOTONIC), Ok(1));
    assert_eq!(display.capability(cap::device::SYNCOBJ), Ok(0));
    assert_eq!(display.capability(cap::device::SYNCOBJ_TIMELINE), Ok(0));

    // Derived from the mode-setting side.
    assert_eq!(display.capability(cap::device::VBLANK_HIGH_CRTC), Ok(1));
    assert_eq!(display.capability(cap::device::CRTC_IN_VBLANK_EVENT), Ok(1));
    assert_eq!(display.capability(cap::device::CURSOR_WIDTH), Ok(64));
    assert_eq!(
        display.capability(0xdead_beef),
        Err(DrmError::InvalidArgument)
    );

    let syncobj = device_with(
        DriverFeatures::RENDER | DriverFeatures::SYNCOBJ | DriverFeatures::SYNCOBJ_TIMELINE,
    );
    assert_eq!(syncobj.capability(cap::device::SYNCOBJ), Ok(1));
    assert_eq!(syncobj.capability(cap::device::SYNCOBJ_TIMELINE), Ok(1));

    // A render-only driver has no answer to give about a display pipeline, which
    // Linux reports as `EOPNOTSUPP` rather than a zero.
    assert_eq!(
        syncobj.capability(cap::device::DUMB_BUFFER),
        Err(DrmError::NotSupported)
    );
}

#[ktest]
fn client_capabilities_match_linux() {
    let device = both_nodes();
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();

    assert!(
        file.set_client_cap(cap::client::UNIVERSAL_PLANES, 1)
            .is_ok()
    );
    assert!(file.client_caps().universal_planes);

    assert_eq!(
        file.set_client_cap(cap::client::UNIVERSAL_PLANES, 2),
        Err(DrmError::InvalidArgument)
    );

    // This driver does not advertise atomic support, so the atomic capability is
    // unsupported rather than invalid — and `WRITEBACK_CONNECTORS` needs atomic.
    assert_eq!(
        file.set_client_cap(cap::client::ATOMIC, 1),
        Err(DrmError::NotSupported)
    );
    assert_eq!(
        file.set_client_cap(cap::client::WRITEBACK_CONNECTORS, 1),
        Err(DrmError::InvalidArgument)
    );

    // linux-6.6 has no cursor-hotspot capability.
    assert_eq!(
        file.set_client_cap(cap::client::CURSOR_PLANE_HOTSPOT, 1),
        Err(DrmError::InvalidArgument)
    );

    let atomic_device =
        device_with(DriverFeatures::MODESET | DriverFeatures::GEM | DriverFeatures::ATOMIC);
    let atomic_file = atomic_device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    assert!(atomic_file.set_client_cap(cap::client::ATOMIC, 1).is_ok());
    let caps = atomic_file.client_caps();
    assert!(caps.atomic);
    // Enabling atomic drags universal planes and aspect ratio along with it.
    assert!(caps.universal_planes);
    assert!(caps.aspect_ratio);
    assert!(
        atomic_file
            .set_client_cap(cap::client::WRITEBACK_CONNECTORS, 1)
            .is_ok()
    );

    // A render-only driver has no client capabilities to set at all.
    let render_device = device_with(DriverFeatures::RENDER);
    let render_file = render_device
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();
    assert_eq!(
        render_file.set_client_cap(cap::client::UNIVERSAL_PLANES, 1),
        Err(DrmError::NotSupported)
    );
}

#[ktest]
fn set_client_cap_payload_is_decoded() {
    let device = both_nodes();
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(8);

    let args = DrmSetClientCap {
        capability: cap::client::UNIVERSAL_PLANES,
        value: 1,
    };
    let mut payload = args.as_bytes().to_vec();
    device
        .ioctl(
            &file,
            ioctl_nr::nr::SET_CLIENT_CAP,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert!(file.client_caps().universal_planes);
}

#[ktest]
fn mastership_is_exclusive_and_reclaimable() {
    let device = both_nodes();
    let user = FakeUserMem::new(8);
    let first_owner = Arc::new(());
    let root_owner = Arc::new(());
    let first = device
        .open_with_identity(
            DrmMinor::Primary,
            Privilege::Unprivileged,
            CallerIdentity::from_owner(&first_owner),
        )
        .unwrap();
    let second = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    let caller = user.context().with_credentials(
        CallerIdentity::from_owner(&first_owner),
        Privilege::Unprivileged,
    );
    let root_caller = user.context().with_credentials(
        CallerIdentity::from_owner(&root_owner),
        Privilege::Privileged,
    );

    assert_eq!(device.set_master(&second, &caller), DENIED);
    assert!(device.set_master(&first, &caller).is_ok());
    assert!(device.drop_master(&first, &caller).is_ok());
    assert!(!first.is_master());
    assert_eq!(
        device.drop_master(&first, &caller),
        Err(DrmError::InvalidArgument)
    );
    assert!(device.set_master(&first, &caller).is_ok());
    assert!(first.is_master());

    let root = device
        .open_with_identity(
            DrmMinor::Primary,
            Privilege::Privileged,
            CallerIdentity::from_owner(&root_owner),
        )
        .unwrap();
    assert_eq!(device.set_master(&root, &root_caller), Err(DrmError::Busy));
    drop(first);
    assert!(device.set_master(&root, &root_caller).is_ok());
    device
        .ioctl(&root, ioctl_nr::nr::DROP_MASTER, &mut [], &root_caller)
        .unwrap();
    assert!(!root.is_master());
    device
        .ioctl(&root, ioctl_nr::nr::SET_MASTER, &mut [], &root_caller)
        .unwrap();
    assert!(root.is_master());
}

#[ktest]
fn arrays_follow_the_two_pass_convention() {
    let user = FakeUserMem::new(64);
    let items: [u32; 3] = [7, 8, 9];

    // Capacity zero: report the count, write nothing. Getting this wrong is why
    // `modetest` reports zero connectors.
    assert_eq!(copy_out_array(&user.context(), 0, 0, &items).unwrap(), 0);
    assert_eq!(user.read(0, 12), vec![0u8; 12]);

    // Short capacity: write what fits and report only that.
    assert_eq!(
        copy_out_array(&user.context(), FakeUserMem::address_of(0), 2, &items).unwrap(),
        2
    );
    assert_eq!(user.read(0, 4), 7u32.to_ne_bytes().to_vec());
    assert_eq!(user.read(4, 4), 8u32.to_ne_bytes().to_vec());
    assert_eq!(user.read(8, 4), vec![0u8; 4]);

    // Ample capacity: write everything and report the real count.
    assert_eq!(
        copy_out_array(&user.context(), FakeUserMem::address_of(16), 8, &items).unwrap(),
        3
    );

    let read_back: Vec<u32> =
        copy_in_array(&user.context(), FakeUserMem::address_of(16), 3).unwrap();
    assert_eq!(read_back, vec![7, 8, 9]);
    assert!(
        copy_in_array::<u32>(&user.context(), 0, 0)
            .unwrap()
            .is_empty()
    );

    // An implausible count must be refused before it becomes an allocation.
    assert_eq!(
        copy_in_array::<u32>(&user.context(), FakeUserMem::address_of(0), u32::MAX),
        Err(DrmError::InvalidArgument)
    );
}

#[ktest]
fn root_only_checks_current_privilege_instead_of_descriptor_opener() {
    let device = both_nodes();
    let root = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let guest = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    let user = FakeUserMem::new(0);
    let owner = Arc::new(());
    let unprivileged = user
        .context()
        .with_credentials(CallerIdentity::from_owner(&owner), Privilege::Unprivileged);
    let privileged = user
        .context()
        .with_credentials(CallerIdentity::from_owner(&owner), Privilege::Privileged);
    assert_eq!(
        desc(IoctlFlags::ROOT_ONLY).check_permission(&root, &unprivileged),
        DENIED
    );
    assert_eq!(
        desc(IoctlFlags::ROOT_ONLY).check_permission(&root, &user.context()),
        DENIED
    );
    assert!(
        desc(IoctlFlags::ROOT_ONLY)
            .check_permission(&guest, &privileged)
            .is_ok()
    );
    assert!(
        root.is_authenticated(),
        "initial authentication remains a per-file grant"
    );
}

#[ktest]
fn passed_master_descriptors_do_not_transfer_opener_reclaim_permission() {
    let device = both_nodes();
    let opener = Arc::new(());
    let recipient = Arc::new(());
    let file = device
        .open_with_identity(
            DrmMinor::Primary,
            Privilege::Privileged,
            CallerIdentity::from_owner(&opener),
        )
        .unwrap();
    let user = FakeUserMem::new(0);
    let original = user
        .context()
        .with_credentials(CallerIdentity::from_owner(&opener), Privilege::Unprivileged);
    let passed = user.context().with_credentials(
        CallerIdentity::from_owner(&recipient),
        Privilege::Unprivileged,
    );
    for caller in [&passed, &user.context()] {
        for nr in [ioctl_nr::nr::SET_MASTER, ioctl_nr::nr::DROP_MASTER] {
            assert_eq!(device.ioctl(&file, nr, &mut [], caller), DENIED);
            assert!(file.is_master());
        }
    }
    device
        .ioctl(&file, ioctl_nr::nr::DROP_MASTER, &mut [], &original)
        .unwrap();
    assert_eq!(
        device.ioctl(&file, ioctl_nr::nr::SET_MASTER, &mut [], &passed),
        DENIED
    );
    device
        .ioctl(&file, ioctl_nr::nr::SET_MASTER, &mut [], &original)
        .unwrap();
    let privileged_recipient = user.context().with_credentials(
        CallerIdentity::from_owner(&recipient),
        Privilege::Privileged,
    );
    device
        .ioctl(
            &file,
            ioctl_nr::nr::DROP_MASTER,
            &mut [],
            &privileged_recipient,
        )
        .unwrap();
    device
        .ioctl(
            &file,
            ioctl_nr::nr::SET_MASTER,
            &mut [],
            &privileged_recipient,
        )
        .unwrap();
}

#[ktest]
fn opener_identity_is_weak_and_cannot_alias_a_later_owner() {
    let device = both_nodes();
    let opener = Arc::new(());
    let weak = Arc::downgrade(&opener);
    let file = device
        .open_with_identity(
            DrmMinor::Primary,
            Privilege::Unprivileged,
            CallerIdentity::from_owner(&opener),
        )
        .unwrap();
    assert_eq!(
        Arc::strong_count(&opener),
        1,
        "DRM does not own the opening thread"
    );
    drop(opener);
    assert!(weak.upgrade().is_none());
    let later_owner = Arc::new(());
    let user = FakeUserMem::new(0);
    let caller = user.context().with_credentials(
        CallerIdentity::from_owner(&later_owner),
        Privilege::Unprivileged,
    );
    assert_eq!(
        device.ioctl(&file, ioctl_nr::nr::DROP_MASTER, &mut [], &caller),
        DENIED
    );
    assert!(file.is_master());
}
