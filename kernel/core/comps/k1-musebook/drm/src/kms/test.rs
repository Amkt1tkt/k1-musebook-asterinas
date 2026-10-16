// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Tests for the mode-setting object model, atomic commits, and events.
//!
//! The centrepiece is [`legacy_set_crtc_equals_the_atomic_commit`] and its two
//! siblings. They are the evidence for the decision recorded in `../../ABI.md`
//! that there is no legacy path: each drives the same change through both ioctls
//! and asserts the committed state is identical. Without them "we translate
//! instead of duplicating" would be a claim rather than a property.

use alloc::vec;

use ostd::prelude::ktest;
use ostd_pod::{IntoBytes, Pod};

use super::*;
use crate::{
    UserMem,
    device::{DriverFeatures, DriverInfo, DrmDevice, DrmDriver, DrmMinor},
    file::{DrmEvent, DrmFile, Privilege},
    gem::BufferUse,
    ioctl::CallerContext,
    uapi::{DrmEventVblank, ioctl_nr::nr},
};

/// A buffer object with no memory behind it.
///
/// Framebuffer validation only asks how large the object is, so a size is all a
/// test of this module needs, and allocating megabytes of contiguous memory per
/// test would be a slow way to learn nothing. Anything that tries to reach the
/// pages fails, which is exactly what a KMS test should never do.
#[derive(Debug)]
struct FakeObject {
    size: usize,
}

impl GemObject for FakeObject {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn size(&self) -> usize {
        self.size
    }

    fn buffer_use(&self) -> BufferUse {
        BufferUse::Scanout
    }

    fn segment(&self) -> Option<&ostd::mm::Segment<()>> {
        None
    }

    fn fault(&self, _pgoff: usize) -> DrmResult<ostd::mm::UFrame> {
        Err(DrmError::BadAddress)
    }
}

/// A [`UserMem`] over a fake address space, so that the ioctl entry points can be
/// exercised without a user process.
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

    fn read_val<T: Pod>(&self, offset: usize) -> T {
        let bytes = self.bytes.lock();
        T::from_bytes(&bytes[offset..offset + size_of::<T>()])
    }

    fn write_val<T: Pod>(&self, offset: usize, value: &T) {
        let bytes = value.as_bytes();
        self.bytes.lock()[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
}

impl FakeUserMem {
    /// The caller context a handler is given. Memory only, like the in-kernel
    /// caller this fixture stands in for.
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

/// The commit hook of the test driver, which records instead of programming.
#[derive(Debug)]
struct RecordingDriver {
    applied: SpinLock<Option<AtomicState>>,
    commits: SpinLock<u32>,
    checks: SpinLock<u32>,
    /// Whether [`AtomicDriver::atomic_check`] should refuse everything, so that a
    /// driver's own veto can be told apart from the core's.
    refuse: SpinLock<bool>,
    destroy_during_check: SpinLock<Option<(Weak<DrmDevice>, ObjectId, FileId)>>,
}

impl RecordingDriver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            applied: SpinLock::new(None),
            commits: SpinLock::new(0),
            checks: SpinLock::new(0),
            refuse: SpinLock::new(false),
            destroy_during_check: SpinLock::new(None),
        })
    }
}

impl AtomicDriver for RecordingDriver {
    fn atomic_check(&self, _state: &AtomicState) -> DrmResult<()> {
        *self.checks.lock() += 1;
        let destroy = self.destroy_during_check.lock().take();
        if let Some((device, blob, owner)) = destroy {
            config_of(&device.upgrade().unwrap())
                .destroy_blob(blob, owner)
                .unwrap();
        }
        if *self.refuse.lock() {
            return Err(DrmError::NotSupported);
        }
        Ok(())
    }

    fn atomic_commit(&self, state: &AtomicState) -> DrmResult<()> {
        *self.applied.lock() = Some(state.clone());
        *self.commits.lock() += 1;
        Ok(())
    }
}

/// A driver with one CRTC, two planes, one encoder and one connector.
#[derive(Debug)]
struct TestDriver {
    mode_config: ModeConfig,
}

/// The object IDs of the test driver's pipeline, so that tests can name them.
#[derive(Clone, Copy, Debug)]
struct Ids {
    crtc: ObjectId,
    primary: ObjectId,
    overlay: ObjectId,
    encoder: ObjectId,
    connector: ObjectId,
}

#[derive(Debug)]
struct TestCrtc {
    id: ObjectId,
    primary: Arc<TestPlane>,
}

impl Crtc for TestCrtc {
    fn id(&self) -> ObjectId {
        self.id
    }
    fn index(&self) -> u32 {
        0
    }
    fn primary_plane(&self) -> Arc<dyn Plane> {
        self.primary.clone()
    }
}

#[derive(Debug)]
struct TestPlane {
    id: ObjectId,
    plane_type: PlaneType,
}

const TEST_FORMATS: &[u32] = &[fourcc::XRGB8888];
const TEST_MODIFIERS: &[u64] = &[fourcc::MOD_LINEAR];

impl Plane for TestPlane {
    fn id(&self) -> ObjectId {
        self.id
    }
    fn plane_type(&self) -> PlaneType {
        self.plane_type
    }
    fn possible_crtcs(&self) -> u32 {
        1
    }
    fn formats(&self) -> &[u32] {
        TEST_FORMATS
    }
    fn modifiers(&self) -> &[u64] {
        TEST_MODIFIERS
    }
}

#[derive(Debug)]
struct TestEncoder {
    id: ObjectId,
}

impl Encoder for TestEncoder {
    fn id(&self) -> ObjectId {
        self.id
    }
    fn encoder_type(&self) -> u32 {
        mode::encoder_type::VIRTUAL
    }
    fn possible_crtcs(&self) -> u32 {
        1
    }
}

#[derive(Debug)]
struct TestConnector {
    id: ObjectId,
    encoder: ObjectId,
}

impl Connector for TestConnector {
    fn id(&self) -> ObjectId {
        self.id
    }
    fn connector_type(&self) -> u32 {
        mode::connector_type::VIRTUAL
    }
    fn connector_type_id(&self) -> u32 {
        1
    }
    fn detect(&self) -> ConnectionStatus {
        ConnectionStatus::Connected
    }
    fn modes(&self) -> Vec<ModeInfo> {
        vec![test_mode()]
    }
    fn physical_size_mm(&self) -> (u32, u32) {
        (340, 190)
    }
    fn encoders(&self) -> Vec<ObjectId> {
        vec![self.encoder]
    }
    fn edid(&self) -> Option<Vec<u8>> {
        Some(vec![0u8; 128])
    }
}

impl DrmDriver for TestDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "test",
            date: "20260729",
            desc: "KMS test driver",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::MODESET | DriverFeatures::GEM | DriverFeatures::ATOMIC
    }

    fn mode_config(&self) -> Option<&ModeConfig> {
        Some(&self.mode_config)
    }
}

/// The mode every test programs: 640x480, with plausible timings.
fn test_mode() -> ModeInfo {
    mode_info(
        "640x480",
        25_175,
        (640, 656, 752, 800),
        (480, 490, 492, 525),
        mode::mode_flag::NHSYNC | mode::mode_flag::NVSYNC,
        mode::mode_type::DRIVER | mode::mode_type::PREFERRED,
    )
}

/// Builds a device, its file, and the IDs of its pipeline.
fn fixture() -> (Arc<DrmDevice>, Arc<DrmFile>, Ids, Arc<RecordingDriver>) {
    fixture_inner(false)
}

/// The same pipeline, with the CRTC already active the way a boot console leaves it.
fn fixture_seeded() -> (Arc<DrmDevice>, Arc<DrmFile>, Ids, Arc<RecordingDriver>) {
    fixture_inner(true)
}

fn fixture_inner(seed: bool) -> (Arc<DrmDevice>, Arc<DrmFile>, Ids, Arc<RecordingDriver>) {
    let ids = ObjectIds::new();
    let primary = Arc::new(TestPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Primary,
    });
    let overlay = Arc::new(TestPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Overlay,
    });
    let crtc = Arc::new(TestCrtc {
        id: ids.allocate(),
        primary: primary.clone(),
    });
    let encoder = Arc::new(TestEncoder { id: ids.allocate() });
    let connector = Arc::new(TestConnector {
        id: ids.allocate(),
        encoder: encoder.id,
    });

    let named = Ids {
        crtc: crtc.id,
        primary: primary.id,
        overlay: overlay.id,
        encoder: encoder.id,
        connector: connector.id,
    };
    let initial = if seed {
        vec![InitialCrtc {
            crtc: named.crtc,
            mode: test_mode(),
            connectors: vec![named.connector],
        }]
    } else {
        Vec::new()
    };

    let atomic = RecordingDriver::new();
    let mode_config = ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        max_size: (4096, 4096),
        crtcs: vec![crtc],
        planes: vec![primary, overlay],
        encoders: vec![encoder],
        connectors: vec![connector],
        atomic: atomic.clone(),
        initial,
    })
    .expect("the test pipeline is consistent");

    let device = DrmDevice::new_unregistered(Arc::new(TestDriver { mode_config }), Some(0), None);
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .expect("opening the primary node");

    (device, file, named, atomic)
}

fn config_of(device: &Arc<DrmDevice>) -> &ModeConfig {
    device
        .driver()
        .mode_config()
        .expect("the test driver has a mode configuration")
}

/// Registers a framebuffer big enough for [`test_mode`].
fn add_framebuffer(config: &ModeConfig, file: &DrmFile, width: u32, height: u32) -> ObjectId {
    let pitch = width * 4;
    config
        .add_framebuffer(Framebuffer {
            id: ObjectId::NONE,
            width,
            height,
            pixel_format: fourcc::XRGB8888,
            modifier: fourcc::MOD_LINEAR,
            objects: [
                Some(Arc::new(FakeObject {
                    size: (pitch * height) as usize,
                })),
                None,
                None,
                None,
            ],
            pitches: [pitch, 0, 0, 0],
            offsets: [0; 4],
            owner: file.id(),
        })
        .expect("the framebuffer is valid")
}

/// What "the same display state" means, for the equivalence tests.
///
/// Deliberately excludes the `MODE_ID` blob: the two paths create their own blob,
/// so the IDs differ while the timings they carry — which is what the hardware
/// sees — must not.
type Snapshot = (
    Vec<(u32, bool, Option<(u32, u16, u16)>, Vec<u32>)>,
    Vec<(u32, u32, u32, (i32, i32, u32, u32), (u32, u32, u32, u32))>,
);

fn snapshot(state: &AtomicState) -> Snapshot {
    let crtcs = state
        .crtcs
        .iter()
        .map(|crtc| {
            (
                crtc.crtc.get(),
                crtc.active,
                crtc.mode
                    .map(|mode| (mode.clock, mode.hdisplay, mode.vdisplay)),
                crtc.connectors.iter().map(|id| id.get()).collect(),
            )
        })
        .collect();
    let planes = state
        .planes
        .iter()
        .map(|plane| {
            (
                plane.plane.get(),
                plane.crtc.unwrap_or(ObjectId::NONE).get(),
                plane
                    .framebuffer
                    .as_ref()
                    .map_or(0, |framebuffer| framebuffer.id.get()),
                plane.crtc_rect,
                plane.src_rect,
            )
        })
        .collect();
    (crtcs, planes)
}

/// Builds the assignment list an atomic client sends to light a pipeline up.
fn light_up_assignments(
    config: &ModeConfig,
    ids: Ids,
    framebuffer: ObjectId,
    mode_blob: ObjectId,
) -> Vec<PropertyAssignment> {
    let property = |standard| config.properties().id(standard);
    let assign = |object, standard, value| PropertyAssignment {
        object,
        property: property(standard),
        value,
    };

    vec![
        assign(
            ids.crtc,
            StandardProperty::CrtcModeId,
            mode_blob.get() as u64,
        ),
        assign(ids.crtc, StandardProperty::CrtcActive, 1),
        assign(
            ids.connector,
            StandardProperty::ConnectorCrtcId,
            ids.crtc.get() as u64,
        ),
        assign(
            ids.primary,
            StandardProperty::PlaneCrtcId,
            ids.crtc.get() as u64,
        ),
        assign(
            ids.primary,
            StandardProperty::PlaneFbId,
            framebuffer.get() as u64,
        ),
        assign(ids.primary, StandardProperty::PlaneCrtcX, 0),
        assign(ids.primary, StandardProperty::PlaneCrtcY, 0),
        assign(ids.primary, StandardProperty::PlaneCrtcW, 640),
        assign(ids.primary, StandardProperty::PlaneCrtcH, 480),
        assign(ids.primary, StandardProperty::PlaneSrcX, 0),
        assign(ids.primary, StandardProperty::PlaneSrcY, 0),
        assign(ids.primary, StandardProperty::PlaneSrcW, 640 << 16),
        assign(ids.primary, StandardProperty::PlaneSrcH, 480 << 16),
    ]
}

/// A `SETCRTC` request that lights the pipeline up with `framebuffer`.
fn set_crtc_request(ids: Ids, framebuffer: ObjectId) -> mode::Crtc {
    mode::Crtc {
        set_connectors_ptr: 0,
        count_connectors: 1,
        crtc_id: ids.crtc.get(),
        fb_id: framebuffer.get(),
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 1,
        mode: test_mode(),
    }
}

#[ktest]
fn mode_config_rejects_an_inconsistent_pipeline() {
    // A plane that claims a CRTC index the device does not have would make
    // `possible_crtcs` meaningless to user space.
    let ids = ObjectIds::new();
    let primary = Arc::new(TestPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Primary,
    });
    let crtc = Arc::new(TestCrtc {
        id: ids.allocate(),
        primary: primary.clone(),
    });
    let encoder = Arc::new(TestEncoder { id: ids.allocate() });

    // A primary plane that is not in the plane list: user space would see a CRTC
    // whose plane it cannot enumerate.
    let orphan = Arc::new(TestPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Primary,
    });
    let result = ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        max_size: (16, 16),
        crtcs: vec![crtc],
        planes: vec![orphan],
        encoders: vec![encoder],
        connectors: Vec::new(),
        atomic: RecordingDriver::new(),
        initial: Vec::new(),
    });
    assert_eq!(result.err(), Some(DrmError::InvalidArgument));

    // Two objects with the same ID.
    let ids = ObjectIds::new();
    let id = ids.allocate();
    let primary = Arc::new(TestPlane {
        id,
        plane_type: PlaneType::Primary,
    });
    let crtc = Arc::new(TestCrtc {
        id,
        primary: primary.clone(),
    });
    let result = ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        max_size: (16, 16),
        crtcs: vec![crtc],
        planes: vec![primary],
        encoders: Vec::new(),
        connectors: Vec::new(),
        atomic: RecordingDriver::new(),
        initial: Vec::new(),
    });
    assert_eq!(result.err(), Some(DrmError::InvalidArgument));
}

/// The shipped software driver has to pass the same consistency checks, or
/// `modetest` under QEMU would find a device with no pipeline.
#[ktest]
fn the_software_driver_has_a_consistent_pipeline() {
    let driver = crate::drivers::soft::SoftDriver::new();
    let config = driver
        .mode_config()
        .expect("the software driver has a mode configuration");

    assert_eq!(config.crtcs().len(), 1);
    assert_eq!(config.planes().len(), 3);
    assert_eq!(config.connectors().len(), 1);
    assert_eq!(config.connectors()[0].modes().len(), 2);

    // The preferred mode is what user space picks by default, so exactly one of
    // them must claim it.
    let preferred = config.connectors()[0]
        .modes()
        .iter()
        .filter(|mode| mode.type_ & mode::mode_type::PREFERRED != 0)
        .count();
    assert_eq!(preferred, 1);

    let identity = driver
        .platform_identity()
        .expect("the software driver publishes itself in sysfs");
    assert_eq!(identity.of_fullname, "/soft/aster-drm");
}

#[ktest]
fn mode_objects_enumerate_by_type() {
    let (device, _file, ids, _atomic) = fixture();
    let config = config_of(&device);

    assert_eq!(
        device.mode_object_ids(mode::object_type::CRTC),
        vec![ids.crtc.get()]
    );
    assert_eq!(
        device.mode_object_ids(mode::object_type::PLANE),
        vec![ids.primary.get(), ids.overlay.get()]
    );
    assert_eq!(
        device.mode_object_ids(mode::object_type::ENCODER),
        vec![ids.encoder.get()]
    );
    assert_eq!(
        device.mode_object_ids(mode::object_type::CONNECTOR),
        vec![ids.connector.get()]
    );
    assert!(device.mode_object_ids(mode::object_type::FB).is_empty());

    // Every ID resolves to the kind it was enumerated under, and nothing else
    // does.
    assert_eq!(config.object_kind(ids.crtc), Some(ObjectKind::Crtc));
    assert_eq!(config.object_kind(ids.primary), Some(ObjectKind::Plane));
    assert_eq!(config.object_kind(ids.encoder), Some(ObjectKind::Encoder));
    assert_eq!(
        config.object_kind(ids.connector),
        Some(ObjectKind::Connector)
    );
    assert_eq!(config.object_kind(ObjectId::NONE), None);
    assert_eq!(config.object_kind(ObjectId(0xdead_beef)), None);

    // A driver with no display reports no mode objects, rather than panicking the
    // way this did before it was implemented.
    let render_only = DrmDevice::new_unregistered(
        Arc::new(RenderOnlyDriver),
        None,
        Some(crate::DRM_RENDER_MINOR_BASE),
    );
    for object_type in [
        mode::object_type::CRTC,
        mode::object_type::PLANE,
        mode::object_type::ENCODER,
        mode::object_type::CONNECTOR,
        mode::object_type::FB,
    ] {
        assert!(render_only.mode_object_ids(object_type).is_empty());
    }
}

/// A driver with a render node and no display at all, which is the shape
/// `aster-pvr` has.
#[derive(Debug)]
struct RenderOnlyDriver;

impl DrmDriver for RenderOnlyDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "render",
            date: "20260729",
            desc: "render-only test driver",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::RENDER | DriverFeatures::GEM
    }
}

#[ktest]
fn properties_report_their_type_and_current_value() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let properties = config.properties();
    let state = config.state();

    // An immutable enum: user space matches planes by this to find the primary.
    let plane_type = properties.id(StandardProperty::PlaneType);
    let property = properties.lookup(plane_type).expect("the property exists");
    assert_eq!(property.standard.name(), "type");
    assert!(property.standard.is_immutable());
    assert!(!property.standard.is_atomic());
    assert_eq!(
        property.standard.flags(),
        mode::prop_flags::ENUM | mode::prop_flags::IMMUTABLE
    );
    assert_eq!(
        config
            .property_value(ids.primary, StandardProperty::PlaneType, &state)
            .unwrap(),
        1
    );
    assert_eq!(
        config
            .property_value(ids.overlay, StandardProperty::PlaneType, &state)
            .unwrap(),
        0
    );

    // `FB_ID` is an object reference and part of the atomic interface, so a legacy
    // client must not see it.
    let fb_id = properties.id(StandardProperty::PlaneFbId);
    let property = properties.lookup(fb_id).unwrap();
    assert_eq!(
        property.standard.flags(),
        mode::prop_flags::OBJECT | mode::prop_flags::ATOMIC
    );
    assert!(
        !properties
            .visible(ObjectKind::Plane, false)
            .iter()
            .any(|entry| entry.id == fb_id)
    );
    assert!(
        properties
            .visible(ObjectKind::Plane, true)
            .iter()
            .any(|entry| entry.id == fb_id)
    );

    // A connector with EDID reports a blob ID, and it resolves.
    let edid = config
        .property_value(ids.connector, StandardProperty::ConnectorEdid, &state)
        .unwrap();
    assert_eq!(config.blob(ObjectId(edid as u32)).unwrap().data.len(), 128);

    // Every plane's `IN_FORMATS` blob is its own, and each resolves.
    let first = config
        .property_value(ids.primary, StandardProperty::PlaneInFormats, &state)
        .unwrap();
    let second = config
        .property_value(ids.overlay, StandardProperty::PlaneInFormats, &state)
        .unwrap();
    assert_ne!(first, second);
    assert!(config.blob(ObjectId(first as u32)).is_ok());

    // Writing an immutable property is refused rather than ignored.
    let request = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.primary,
            property: plane_type,
            value: 2,
        }],
        flags: CommitFlags::default(),
    };
    assert_eq!(
        config.commit(&request, &file),
        Err(DrmError::InvalidArgument)
    );

    // So is a value outside a property's declared range, and a property on the
    // wrong kind of object.
    let active = properties.id(StandardProperty::CrtcActive);
    let out_of_range = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.crtc,
            property: active,
            value: 2,
        }],
        flags: CommitFlags {
            allow_modeset: true,
            ..CommitFlags::default()
        },
    };
    assert_eq!(
        config.commit(&out_of_range, &file),
        Err(DrmError::InvalidArgument)
    );

    let wrong_object = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.primary,
            property: active,
            value: 1,
        }],
        flags: CommitFlags::default(),
    };
    assert_eq!(config.commit(&wrong_object, &file), Err(DrmError::NotFound));
}

#[ktest]
fn empty_atomic_commits_preserve_firmware_state() {
    let (device, file, ids, atomic) = fixture_seeded();
    let config = config_of(&device);
    let state = config.state();
    let checks = *atomic.checks.lock();
    let generation = config.vblank().generation(ids.crtc);
    let sequence = config.vblank().sequence(ids.crtc);
    assert!(state.crtc(ids.crtc).unwrap().active);
    assert!(state.plane(ids.primary).unwrap().framebuffer.is_none());
    *atomic.refuse.lock() = true;

    for flags in [
        CommitFlags::default(),
        CommitFlags {
            allow_modeset: true,
            ..CommitFlags::default()
        },
        CommitFlags {
            test_only: true,
            ..CommitFlags::default()
        },
        CommitFlags {
            allow_modeset: true,
            nonblock: true,
            ..CommitFlags::default()
        },
    ] {
        config
            .commit(
                &AtomicRequest {
                    assignments: Vec::new(),
                    flags,
                },
                &file,
            )
            .unwrap();
    }
    let event = AtomicRequest {
        assignments: Vec::new(),
        flags: CommitFlags {
            event: true,
            ..CommitFlags::default()
        },
    };
    assert_eq!(config.commit(&event, &file), Err(DrmError::InvalidArgument));
    assert_eq!(*atomic.checks.lock(), checks);
    assert_eq!(*atomic.commits.lock(), 0);
    assert!(atomic.applied.lock().is_none());
    assert!(Arc::ptr_eq(&state, &config.state()));
    assert_eq!(config.vblank().generation(ids.crtc), generation);
    assert_eq!(config.vblank().sequence(ids.crtc), sequence);
    assert!(!config.vblank().has_pending(ids.crtc));
    assert!(!file.has_events());
    assert!(file.reserve_events(128));
    file.release_events(128);
}

#[ktest]
fn same_value_atomic_assignments_still_reach_the_driver() {
    let (device, file, ids, atomic) = fixture_seeded();
    let config = config_of(&device);
    let state = config.state();
    let checks = *atomic.checks.lock();
    let request = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.crtc,
            property: config.properties().id(StandardProperty::CrtcActive),
            value: 1,
        }],
        flags: CommitFlags {
            allow_modeset: true,
            ..CommitFlags::default()
        },
    };
    *atomic.refuse.lock() = true;
    assert_eq!(config.commit(&request, &file), Err(DrmError::NotSupported));
    assert_eq!(*atomic.checks.lock(), checks + 1);
    assert_eq!(*atomic.commits.lock(), 0);
    assert!(Arc::ptr_eq(&state, &config.state()));

    *atomic.refuse.lock() = false;
    config.commit(&request, &file).unwrap();
    assert_eq!(*atomic.checks.lock(), checks + 2);
    assert_eq!(*atomic.commits.lock(), 1);
    assert!(!Arc::ptr_eq(&state, &config.state()));
}

#[ktest]
fn atomic_commit_checks_before_it_applies() {
    let (device, file, ids, atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    let mode_blob = config
        .create_blob(test_mode().as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();

    let assignments = light_up_assignments(config, ids, framebuffer, mode_blob);

    // A commit that needs a modeset without permission to do one is refused, and
    // the driver never sees it.
    let refused = AtomicRequest {
        assignments: assignments.clone(),
        flags: CommitFlags::default(),
    };
    assert_eq!(
        config.commit(&refused, &file),
        Err(DrmError::InvalidArgument)
    );
    assert_eq!(*atomic.commits.lock(), 0);

    // A test commit reaches the driver's check but changes nothing.
    let test_only = AtomicRequest {
        assignments: assignments.clone(),
        flags: CommitFlags {
            allow_modeset: true,
            test_only: true,
            ..CommitFlags::default()
        },
    };
    assert!(config.commit(&test_only, &file).is_ok());
    assert_eq!(*atomic.checks.lock(), 1);
    assert_eq!(*atomic.commits.lock(), 0);
    assert!(!config.state().crtc(ids.crtc).unwrap().active);

    // The real thing.
    let request = AtomicRequest {
        assignments: assignments.clone(),
        flags: CommitFlags {
            allow_modeset: true,
            ..CommitFlags::default()
        },
    };
    assert!(config.commit(&request, &file).is_ok());
    assert_eq!(*atomic.commits.lock(), 1);

    let state = config.state();
    let crtc = state.crtc(ids.crtc).unwrap();
    assert!(crtc.active);
    assert_eq!(crtc.mode.unwrap().hdisplay, 640);
    assert_eq!(crtc.connectors, vec![ids.connector]);
    let plane = state.plane(ids.primary).unwrap();
    assert_eq!(plane.crtc, Some(ids.crtc));
    assert_eq!(plane.framebuffer.as_ref().unwrap().id, framebuffer);

    // The state the driver was handed is the state that is now current.
    let applied = atomic.applied.lock().clone().unwrap();
    assert_eq!(snapshot(&applied), snapshot(&state));

    // The driver's own veto is honoured, and leaves the previous state in place.
    // The request itself is a perfectly valid flip, so a rejection can only have
    // come from the driver.
    *atomic.refuse.lock() = true;
    let other = add_framebuffer(config, &file, 640, 480);
    let flip = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.primary,
            property: config.properties().id(StandardProperty::PlaneFbId),
            value: other.get() as u64,
        }],
        flags: CommitFlags::default(),
    };
    assert_eq!(config.commit(&flip, &file), Err(DrmError::NotSupported));
    assert_eq!(*atomic.commits.lock(), 1);
    assert_eq!(
        config
            .state()
            .plane(ids.primary)
            .unwrap()
            .framebuffer
            .as_ref()
            .unwrap()
            .id,
        framebuffer
    );
}

#[ktest]
fn atomic_check_refuses_states_the_hardware_cannot_hold() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    let property = |standard| config.properties().id(standard);
    let assign = |object, standard, value| PropertyAssignment {
        object,
        property: property(standard),
        value,
    };
    let modeset = CommitFlags {
        allow_modeset: true,
        ..CommitFlags::default()
    };

    // Active without a mode: the display engine would have no timings.
    let request = AtomicRequest {
        assignments: vec![assign(ids.crtc, StandardProperty::CrtcActive, 1)],
        flags: modeset,
    };
    assert_eq!(
        config.commit(&request, &file),
        Err(DrmError::InvalidArgument)
    );

    // A framebuffer without a CRTC, and a CRTC without a framebuffer: neither is
    // on nor off.
    let request = AtomicRequest {
        assignments: vec![assign(
            ids.primary,
            StandardProperty::PlaneFbId,
            framebuffer.get() as u64,
        )],
        flags: modeset,
    };
    assert_eq!(
        config.commit(&request, &file),
        Err(DrmError::InvalidArgument)
    );

    // A source rectangle that runs off the end of the framebuffer, which is the
    // check that stops a plane from scanning out unrelated memory.
    let small = add_framebuffer(config, &file, 320, 240);
    let request = AtomicRequest {
        assignments: vec![
            assign(
                ids.primary,
                StandardProperty::PlaneCrtcId,
                ids.crtc.get() as u64,
            ),
            assign(ids.primary, StandardProperty::PlaneFbId, small.get() as u64),
            assign(ids.primary, StandardProperty::PlaneCrtcW, 640),
            assign(ids.primary, StandardProperty::PlaneCrtcH, 480),
            assign(ids.primary, StandardProperty::PlaneSrcW, 640 << 16),
            assign(ids.primary, StandardProperty::PlaneSrcH, 480 << 16),
        ],
        flags: modeset,
    };
    assert_eq!(
        config.commit(&request, &file),
        Err(DrmError::InvalidArgument)
    );

    // Scaling on a plane that has no scaler is unsupported rather than invalid:
    // the request is well formed, this plane just cannot serve it.
    let request = AtomicRequest {
        assignments: vec![
            assign(
                ids.primary,
                StandardProperty::PlaneCrtcId,
                ids.crtc.get() as u64,
            ),
            assign(
                ids.primary,
                StandardProperty::PlaneFbId,
                framebuffer.get() as u64,
            ),
            assign(ids.primary, StandardProperty::PlaneCrtcW, 320),
            assign(ids.primary, StandardProperty::PlaneCrtcH, 240),
            assign(ids.primary, StandardProperty::PlaneSrcW, 640 << 16),
            assign(ids.primary, StandardProperty::PlaneSrcH, 480 << 16),
        ],
        flags: modeset,
    };
    assert_eq!(config.commit(&request, &file), Err(DrmError::NotSupported));

    // A mode blob whose timings contradict themselves.
    let mut broken = test_mode();
    broken.htotal = 1;
    let blob = config
        .create_blob(broken.as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();
    let request = AtomicRequest {
        assignments: vec![assign(
            ids.crtc,
            StandardProperty::CrtcModeId,
            blob.get() as u64,
        )],
        flags: modeset,
    };
    assert_eq!(
        config.commit(&request, &file),
        Err(DrmError::InvalidArgument)
    );

    // A blob of the wrong size is not a mode.
    let blob = config
        .create_blob(vec![0u8; 8], BlobOwner::File(file.id()))
        .unwrap();
    let request = AtomicRequest {
        assignments: vec![assign(
            ids.crtc,
            StandardProperty::CrtcModeId,
            blob.get() as u64,
        )],
        flags: modeset,
    };
    assert_eq!(
        config.commit(&request, &file),
        Err(DrmError::InvalidArgument)
    );
}

/// The evidence for "no legacy path": `SETCRTC` and the atomic commit it
/// translates to leave the device in the same state.
#[ktest]
fn legacy_set_crtc_equals_the_atomic_commit() {
    // The legacy path.
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(
            &set_crtc_request(ids, framebuffer),
            &[ids.connector.get()],
            &file,
        )
        .expect("the legacy modeset is accepted");
    let through_legacy = snapshot(&config.state());

    // The atomic path, on a fresh device so that the object IDs match.
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    let mode_blob = config
        .create_blob(test_mode().as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();
    config
        .commit(
            &AtomicRequest {
                assignments: light_up_assignments(config, ids, framebuffer, mode_blob),
                flags: CommitFlags {
                    allow_modeset: true,
                    ..CommitFlags::default()
                },
            },
            &file,
        )
        .expect("the atomic commit is accepted");
    let through_atomic = snapshot(&config.state());

    assert_eq!(through_legacy, through_atomic);

    // And the state is the one that was asked for, rather than two identical
    // wrong answers.
    let (crtcs, planes) = &through_legacy;
    assert!(crtcs[0].1);
    assert_eq!(crtcs[0].2, Some((25_175, 640, 480)));
    assert_eq!(crtcs[0].3, vec![ids.connector.get()]);
    assert_eq!(planes[0].1, ids.crtc.get());
    assert_eq!(planes[0].3, (0, 0, 640, 480));
    assert_eq!(planes[0].4, (0, 0, 640 << 16, 480 << 16));
}

/// The same for `PAGE_FLIP`, including the completion event it produces.
#[ktest]
fn legacy_page_flip_equals_the_atomic_commit() {
    let flip_through = |legacy: bool| {
        let (device, file, ids, _atomic) = fixture();
        let config = config_of(&device);
        let first = add_framebuffer(config, &file, 640, 480);
        let second = add_framebuffer(config, &file, 640, 480);

        config
            .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
            .expect("the initial modeset is accepted");

        if legacy {
            config
                .legacy_page_flip(
                    &mode::CrtcPageFlip {
                        crtc_id: ids.crtc.get(),
                        fb_id: second.get(),
                        flags: mode::page_flip_flags::EVENT,
                        reserved: 0,
                        user_data: 0xf00d,
                    },
                    &file,
                )
                .expect("the legacy flip is accepted");
        } else {
            let property = |standard| config.properties().id(standard);
            config
                .commit(
                    &AtomicRequest {
                        assignments: vec![
                            PropertyAssignment {
                                object: ids.primary,
                                property: property(StandardProperty::PlaneCrtcId),
                                value: ids.crtc.get() as u64,
                            },
                            PropertyAssignment {
                                object: ids.primary,
                                property: property(StandardProperty::PlaneFbId),
                                value: second.get() as u64,
                            },
                        ],
                        flags: CommitFlags {
                            event: true,
                            user_data: 0xf00d,
                            ..CommitFlags::default()
                        },
                    },
                    &file,
                )
                .expect("the atomic flip is accepted");
        }

        let event = read_one_event(&file).expect("a completion event was queued");
        (snapshot(&config.state()), event, second.get())
    };

    let (legacy_state, legacy_event, legacy_fb) = flip_through(true);
    let (atomic_state, atomic_event, atomic_fb) = flip_through(false);

    assert_eq!(legacy_state, atomic_state);
    // The flip changed which framebuffer the primary plane holds, and nothing else.
    assert_eq!(legacy_state.1[0].2, legacy_fb);
    assert_eq!(atomic_state.1[0].2, atomic_fb);

    // Both events carry the same cookie, kind and CRTC; only the timestamp may
    // differ, which is why it is not compared.
    assert_eq!(legacy_event.base.type_, atomic_event.base.type_);
    assert_eq!(
        legacy_event.base.type_,
        crate::uapi::event_type::FLIP_COMPLETE
    );
    assert_eq!(legacy_event.user_data, atomic_event.user_data);
    assert_eq!(legacy_event.user_data, 0xf00d);
    assert_eq!(legacy_event.crtc_id, atomic_event.crtc_id);
    assert_eq!(legacy_event.sequence, atomic_event.sequence);
}

/// A driver that reports [`FlipTiming::AtVblank`].
#[derive(Debug)]
struct DeferredDriver {
    vblank: spin::Once<Vblank>,
    crtc: ObjectId,
    complete_during_commit: bool,
}

impl AtomicDriver for DeferredDriver {
    fn atomic_check(&self, _state: &AtomicState) -> DrmResult<()> {
        Ok(())
    }

    fn atomic_commit(&self, _state: &AtomicState) -> DrmResult<()> {
        let vblank = self.vblank.get().expect("bound vblank");
        let generation = vblank.begin_update(self.crtc)?;
        if self.complete_during_commit {
            vblank.arm_update(self.crtc, generation);
            vblank.tick_update(self.crtc, generation);
        }
        Ok(())
    }

    fn flip_timing(&self) -> FlipTiming {
        FlipTiming::AtVblank
    }
}

fn deferred_fixture() -> (Arc<DrmDevice>, Arc<DrmFile>, Ids) {
    deferred_fixture_with(false)
}

fn deferred_fixture_with(complete_during_commit: bool) -> (Arc<DrmDevice>, Arc<DrmFile>, Ids) {
    let ids = ObjectIds::new();
    let primary = Arc::new(TestPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Primary,
    });
    let overlay = Arc::new(TestPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Overlay,
    });
    let crtc = Arc::new(TestCrtc {
        id: ids.allocate(),
        primary: primary.clone(),
    });
    let encoder = Arc::new(TestEncoder { id: ids.allocate() });
    let connector = Arc::new(TestConnector {
        id: ids.allocate(),
        encoder: encoder.id,
    });

    let named = Ids {
        crtc: crtc.id,
        primary: primary.id,
        overlay: overlay.id,
        encoder: encoder.id,
        connector: connector.id,
    };

    let atomic = Arc::new(DeferredDriver {
        vblank: spin::Once::new(),
        crtc: named.crtc,
        complete_during_commit,
    });
    let mode_config = ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        max_size: (4096, 4096),
        crtcs: vec![crtc],
        planes: vec![primary, overlay],
        encoders: vec![encoder],
        connectors: vec![connector],
        atomic: atomic.clone(),
        initial: Vec::new(),
    })
    .expect("the deferred pipeline is consistent");
    atomic.vblank.call_once(|| mode_config.vblank().clone());

    let device = DrmDevice::new_unregistered(Arc::new(TestDriver { mode_config }), Some(0), None);
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .expect("opening the primary node");

    (device, file, named)
}

/// Completing at vblank means the event is not queued from `commit`.
///
/// smithay treats the event as "the previous buffer is free". Delivering it
/// from `atomic_commit` is what licensed the compositor to render into a
/// buffer the DPU was still scanning out.
#[ktest]
fn a_deferred_flip_event_waits_for_vblank() {
    let (device, file, ids) = deferred_fixture();
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);

    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .expect("the initial modeset is accepted");

    config
        .legacy_page_flip(
            &mode::CrtcPageFlip {
                crtc_id: ids.crtc.get(),
                fb_id: second.get(),
                flags: mode::page_flip_flags::EVENT,
                reserved: 0,
                user_data: 0xabc,
            },
            &file,
        )
        .expect("the flip is accepted");

    assert!(read_one_event(&file).is_none());
    assert!(config.vblank().has_pending(ids.crtc));

    // A tick before the latch counts the vblank but does not complete the flip.
    assert_eq!(config.vblank().tick(ids.crtc), 1);
    assert!(read_one_event(&file).is_none());
    assert!(config.vblank().has_pending(ids.crtc));

    config.vblank().arm(ids.crtc);
    assert_eq!(config.vblank().tick(ids.crtc), 2);

    let event = read_one_event(&file).expect("the flip completed at vblank");
    assert_eq!(event.base.type_, crate::uapi::event_type::FLIP_COMPLETE);
    assert_eq!(event.user_data, 0xabc);
    assert_eq!(event.sequence, 2);
    assert_eq!(event.crtc_id, ids.crtc.get());
    assert!(!config.vblank().has_pending(ids.crtc));
}

/// One pending flip per CRTC: a second event-bearing commit is `EBUSY`
/// until the interrupt delivers the first.
#[ktest]
fn a_second_deferred_flip_is_busy_until_vblank() {
    let (device, file, ids) = deferred_fixture();
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);
    let third = add_framebuffer(config, &file, 640, 480);

    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .expect("the initial modeset is accepted");

    let flip = |framebuffer: ObjectId, cookie: u64| {
        config.legacy_page_flip(
            &mode::CrtcPageFlip {
                crtc_id: ids.crtc.get(),
                fb_id: framebuffer.get(),
                flags: mode::page_flip_flags::EVENT,
                reserved: 0,
                user_data: cookie,
            },
            &file,
        )
    };

    flip(second, 1).expect("the first flip is accepted");
    assert_eq!(flip(third, 2), Err(DrmError::Busy));

    config.vblank().arm(ids.crtc);
    config.vblank().tick(ids.crtc);
    assert_eq!(read_one_event(&file).expect("first flip").user_data, 1);

    flip(third, 2).expect("the second flip is accepted after vblank");
    config.vblank().arm(ids.crtc);
    config.vblank().tick(ids.crtc);
    assert_eq!(read_one_event(&file).expect("second flip").user_data, 2);
}

/// And for `SETPLANE`, which is the same translation applied to any plane.
#[ktest]
fn legacy_set_plane_equals_the_atomic_commit() {
    let through = |legacy: bool| {
        let (device, file, ids, _atomic) = fixture();
        let config = config_of(&device);
        let framebuffer = add_framebuffer(config, &file, 640, 480);
        let overlay_fb = add_framebuffer(config, &file, 64, 64);
        config
            .legacy_set_crtc(
                &set_crtc_request(ids, framebuffer),
                &[ids.connector.get()],
                &file,
            )
            .expect("the initial modeset is accepted");

        if legacy {
            config
                .legacy_set_plane(
                    &mode::SetPlane {
                        plane_id: ids.overlay.get(),
                        crtc_id: ids.crtc.get(),
                        fb_id: overlay_fb.get(),
                        flags: 0,
                        crtc_x: 16,
                        crtc_y: 32,
                        crtc_w: 64,
                        crtc_h: 64,
                        src_x: 0,
                        src_y: 0,
                        src_w: 64 << 16,
                        src_h: 64 << 16,
                    },
                    &file,
                )
                .expect("the legacy plane update is accepted");
        } else {
            let property = |standard| config.properties().id(standard);
            let assign = |standard, value| PropertyAssignment {
                object: ids.overlay,
                property: property(standard),
                value,
            };
            config
                .commit(
                    &AtomicRequest {
                        assignments: vec![
                            assign(StandardProperty::PlaneCrtcId, ids.crtc.get() as u64),
                            assign(StandardProperty::PlaneFbId, overlay_fb.get() as u64),
                            assign(StandardProperty::PlaneCrtcX, 16),
                            assign(StandardProperty::PlaneCrtcY, 32),
                            assign(StandardProperty::PlaneCrtcW, 64),
                            assign(StandardProperty::PlaneCrtcH, 64),
                            assign(StandardProperty::PlaneSrcW, 64 << 16),
                            assign(StandardProperty::PlaneSrcH, 64 << 16),
                        ],
                        flags: CommitFlags::default(),
                    },
                    &file,
                )
                .expect("the atomic plane update is accepted");
        }

        snapshot(&config.state())
    };

    let legacy = through(true);
    let atomic = through(false);
    assert_eq!(legacy, atomic);
    // The overlay ended up where it was asked to be, off the origin.
    assert_eq!(legacy.1[1].3, (16, 32, 64, 64));
}

/// Turning the pipeline off is the same through either interface too.
#[ktest]
fn legacy_disable_equals_the_atomic_commit() {
    let through = |legacy: bool| {
        let (device, file, ids, _atomic) = fixture();
        let config = config_of(&device);
        let framebuffer = add_framebuffer(config, &file, 640, 480);
        config
            .legacy_set_crtc(
                &set_crtc_request(ids, framebuffer),
                &[ids.connector.get()],
                &file,
            )
            .expect("the initial modeset is accepted");

        if legacy {
            config
                .legacy_set_crtc(
                    &mode::Crtc {
                        crtc_id: ids.crtc.get(),
                        ..mode::Crtc::default()
                    },
                    &[],
                    &file,
                )
                .expect("the legacy disable is accepted");
        } else {
            let property = |standard| config.properties().id(standard);
            config
                .commit(
                    &AtomicRequest {
                        assignments: vec![
                            PropertyAssignment {
                                object: ids.connector,
                                property: property(StandardProperty::ConnectorCrtcId),
                                value: 0,
                            },
                            PropertyAssignment {
                                object: ids.crtc,
                                property: property(StandardProperty::CrtcActive),
                                value: 0,
                            },
                            PropertyAssignment {
                                object: ids.crtc,
                                property: property(StandardProperty::CrtcModeId),
                                value: 0,
                            },
                            PropertyAssignment {
                                object: ids.primary,
                                property: property(StandardProperty::PlaneCrtcId),
                                value: 0,
                            },
                            PropertyAssignment {
                                object: ids.primary,
                                property: property(StandardProperty::PlaneFbId),
                                value: 0,
                            },
                        ],
                        flags: CommitFlags {
                            allow_modeset: true,
                            ..CommitFlags::default()
                        },
                    },
                    &file,
                )
                .expect("the atomic disable is accepted");
        }

        snapshot(&config.state())
    };

    assert_eq!(through(true), through(false));
    let (crtcs, planes) = through(true);
    assert!(!crtcs[0].1);
    assert_eq!(crtcs[0].2, None);
    assert!(crtcs[0].3.is_empty());
    assert_eq!(planes[0].2, 0);
}

/// `DPMS` is the third legacy interface that turns into an atomic commit: setting
/// it to off must leave the CRTC configured but inactive, which is what lets a
/// compositor come back without a modeset.
#[ktest]
fn dpms_turns_into_the_crtc_active_property() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(
            &set_crtc_request(ids, framebuffer),
            &[ids.connector.get()],
            &file,
        )
        .unwrap();

    let state = config.state();
    assert_eq!(
        config
            .property_value(ids.connector, StandardProperty::ConnectorDpms, &state)
            .unwrap(),
        mode::dpms::ON
    );

    let dpms = config.properties().id(StandardProperty::ConnectorDpms);
    config
        .commit(
            &AtomicRequest {
                assignments: vec![PropertyAssignment {
                    object: ids.connector,
                    property: dpms,
                    value: mode::dpms::OFF,
                }],
                flags: CommitFlags {
                    allow_modeset: true,
                    ..CommitFlags::default()
                },
            },
            &file,
        )
        .expect("turning the output off is accepted");

    let state = config.state();
    assert!(!state.crtc(ids.crtc).unwrap().active);
    // Still configured: the mode and the connector routing survive.
    assert!(state.crtc(ids.crtc).unwrap().mode.is_some());
    assert_eq!(
        state.crtc(ids.crtc).unwrap().connectors,
        vec![ids.connector]
    );
    assert_eq!(
        config
            .property_value(ids.connector, StandardProperty::ConnectorDpms, &state)
            .unwrap(),
        mode::dpms::OFF
    );
}

/// A driver-seeded initial state is what makes `modetest` report DPMS On for a
/// panel that boot already lit, and GETCRTC must agree with the derived
/// properties — they are two views of the same committed state.
#[ktest]
fn a_seeded_initial_state_reports_dpms_on_and_agrees_with_getcrtc() {
    let (device, file, ids, atomic) = fixture_seeded();
    let config = config_of(&device);
    let state = config.state();

    // Installing the seed is not a commit: the hardware was already doing this.
    assert_eq!(*atomic.commits.lock(), 0);
    assert!(*atomic.checks.lock() >= 1);

    let crtc = state.crtc(ids.crtc).unwrap();
    assert!(crtc.active);
    assert_eq!(crtc.mode.unwrap().hdisplay, 640);
    assert_eq!(crtc.connectors, vec![ids.connector]);
    assert!(crtc.mode_blob.is_some());
    // No DRM framebuffer exists for a buffer nobody created via ADDFB2.
    let plane = state.plane(ids.primary).unwrap();
    assert!(plane.crtc.is_none());
    assert!(plane.framebuffer.is_none());

    assert_eq!(
        config
            .property_value(ids.connector, StandardProperty::ConnectorDpms, &state)
            .unwrap(),
        mode::dpms::ON
    );
    assert_eq!(
        config
            .property_value(ids.crtc, StandardProperty::CrtcActive, &state)
            .unwrap(),
        1
    );
    assert_eq!(
        config
            .property_value(ids.connector, StandardProperty::ConnectorCrtcId, &state)
            .unwrap(),
        ids.crtc.get() as u64
    );
    let mode_id = config
        .property_value(ids.crtc, StandardProperty::CrtcModeId, &state)
        .unwrap();
    assert_ne!(mode_id, 0);
    let blob = config.blob(ObjectId(mode_id as u32)).unwrap();
    assert_eq!(ModeInfo::from_bytes(&blob.data).hdisplay, 640);

    let user = FakeUserMem::new(256);
    let args = mode::Crtc {
        crtc_id: ids.crtc.get(),
        ..mode::Crtc::default()
    };
    let mut payload = args.as_bytes().to_vec();
    device
        .ioctl(&file, nr::MODE_GETCRTC, &mut payload, &user.context())
        .unwrap();
    let reported = mode::Crtc::from_bytes(&payload);
    assert_eq!(reported.mode_valid, 1);
    assert_eq!(reported.mode.hdisplay, 640);
    assert_eq!(reported.mode.vdisplay, 480);
    assert_eq!(reported.fb_id, 0);
    assert_eq!(reported.x, 0);
    assert_eq!(reported.y, 0);

    // A plane-only first commit is no longer a modeset: the CRTC is already
    // configured. A compositor that only binds a framebuffer therefore does
    // not need `ALLOW_MODESET`.
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    let property = |standard| config.properties().id(standard);
    let assign = |object, standard, value| PropertyAssignment {
        object,
        property: property(standard),
        value,
    };
    config
        .commit(
            &AtomicRequest {
                assignments: vec![
                    assign(
                        ids.primary,
                        StandardProperty::PlaneCrtcId,
                        ids.crtc.get() as u64,
                    ),
                    assign(
                        ids.primary,
                        StandardProperty::PlaneFbId,
                        framebuffer.get() as u64,
                    ),
                    assign(ids.primary, StandardProperty::PlaneCrtcW, 640),
                    assign(ids.primary, StandardProperty::PlaneCrtcH, 480),
                    assign(ids.primary, StandardProperty::PlaneSrcW, 640 << 16),
                    assign(ids.primary, StandardProperty::PlaneSrcH, 480 << 16),
                ],
                flags: CommitFlags::default(),
            },
            &file,
        )
        .expect("binding the plane to an already-active CRTC is not a modeset");
}

/// A seed that the object model would refuse is a driver bug, reported at
/// registration rather than left for the first client to trip over.
#[ktest]
fn mode_config_rejects_an_inconsistent_initial_state() {
    let pipeline = || {
        let ids = ObjectIds::new();
        let primary = Arc::new(TestPlane {
            id: ids.allocate(),
            plane_type: PlaneType::Primary,
        });
        let crtc = Arc::new(TestCrtc {
            id: ids.allocate(),
            primary: primary.clone(),
        });
        let encoder = Arc::new(TestEncoder { id: ids.allocate() });
        let connector = Arc::new(TestConnector {
            id: ids.allocate(),
            encoder: encoder.id,
        });
        (ids, primary, crtc, encoder, connector)
    };

    let (ids, primary, crtc, encoder, connector) = pipeline();
    // A CRTC the device does not have.
    let result = ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        max_size: (4096, 4096),
        crtcs: vec![crtc],
        planes: vec![primary],
        encoders: vec![encoder],
        connectors: vec![connector],
        atomic: RecordingDriver::new(),
        initial: vec![InitialCrtc {
            crtc: ObjectId(0xdead),
            mode: test_mode(),
            connectors: Vec::new(),
        }],
    });
    assert_eq!(result.err(), Some(DrmError::InvalidArgument));

    let (ids, primary, crtc, encoder, connector) = pipeline();
    // Mode without connectors: the "enabled/connectors mismatch".
    let result = ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        max_size: (4096, 4096),
        crtcs: vec![crtc.clone()],
        planes: vec![primary],
        encoders: vec![encoder],
        connectors: vec![connector],
        atomic: RecordingDriver::new(),
        initial: vec![InitialCrtc {
            crtc: crtc.id,
            mode: test_mode(),
            connectors: Vec::new(),
        }],
    });
    assert_eq!(result.err(), Some(DrmError::InvalidArgument));
}

/// Reads one event out of a file, or `None` if none is waiting.
fn read_one_event(file: &DrmFile) -> Option<DrmEventVblank> {
    let mut buffer = [0u8; size_of::<DrmEventVblank>()];
    let read = file
        .read_event::<()>(buffer.len(), |bytes| {
            buffer.copy_from_slice(bytes);
            Ok(())
        })
        .unwrap();
    (read != 0).then(|| DrmEventVblank::from_bytes(&buffer))
}

#[ktest]
fn events_are_delivered_whole_and_in_order() {
    let (_device, file, _ids, _atomic) = fixture();

    assert!(!file.has_events());
    assert!(read_one_event(&file).is_none());

    for sequence in 1..=3 {
        file.queue_event(DrmEvent::FlipComplete {
            user_data: u64::from(sequence),
            tv_sec: 1,
            tv_usec: 2,
            sequence,
            crtc_id: 7,
        });
    }
    assert!(file.has_events());

    // A buffer too small for a whole event returns nothing and keeps the event:
    // events are self-delimiting only if read whole, so a partial read would
    // desynchronize the client forever.
    let short = file
        .read_event::<()>(size_of::<DrmEventVblank>() - 1, |_| {
            panic!("a partial event was offered");
        })
        .unwrap();
    assert_eq!(short, 0);
    assert!(file.has_events());

    // A failing copy also keeps the event, rather than costing the client a frame.
    let failed = file.read_event::<u32>(size_of::<DrmEventVblank>(), |_| Err(7));
    assert_eq!(failed, Err(7));

    // Then they come out in order, exactly once each.
    for sequence in 1..=3 {
        let event = read_one_event(&file).expect("an event is waiting");
        assert_eq!(event.sequence, sequence);
        assert_eq!(event.user_data, u64::from(sequence));
        assert_eq!(event.crtc_id, 7);
        assert_eq!(event.base.length, size_of::<DrmEventVblank>() as u32);
    }
    assert!(!file.has_events());
    assert!(read_one_event(&file).is_none());
}

#[ktest]
fn the_event_budget_is_finite() {
    let (_device, file, _ids, _atomic) = fixture();
    let capacity = 4096 / size_of::<DrmEventVblank>();

    // The budget is what a commit reserves against, and it must run out rather
    // than grow without bound when a client stops reading.
    assert!(file.reserve_events(capacity));
    assert!(!file.reserve_events(1));

    // Giving a reservation back makes room again, which is what a failed commit
    // does.
    file.release_events(capacity);
    assert!(file.reserve_events(1));
}

/// A flip whose event cannot be promised fails before the hardware is touched,
/// because an event that never arrives would leave the client waiting forever.
#[ktest]
fn a_flip_without_room_for_its_event_is_refused() {
    let (device, file, ids, atomic) = fixture();
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .unwrap();

    let commits = *atomic.commits.lock();
    assert!(file.reserve_events(4096 / size_of::<DrmEventVblank>()));

    let refused = config.legacy_page_flip(
        &mode::CrtcPageFlip {
            crtc_id: ids.crtc.get(),
            fb_id: second.get(),
            flags: mode::page_flip_flags::EVENT,
            reserved: 0,
            user_data: 0,
        },
        &file,
    );
    assert_eq!(refused, Err(DrmError::OutOfMemory));
    assert_eq!(*atomic.commits.lock(), commits);

    // Without an event, the same flip goes through.
    assert!(
        config
            .legacy_page_flip(
                &mode::CrtcPageFlip {
                    crtc_id: ids.crtc.get(),
                    fb_id: second.get(),
                    flags: 0,
                    reserved: 0,
                    user_data: 0,
                },
                &file,
            )
            .is_ok()
    );
}

#[ktest]
fn a_flip_needs_something_to_flip_from() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);

    // Before any modeset, `EBUSY`: the request is not malformed, the pipeline is
    // just not ready.
    let result = config.legacy_page_flip(
        &mode::CrtcPageFlip {
            crtc_id: ids.crtc.get(),
            fb_id: framebuffer.get(),
            flags: 0,
            reserved: 0,
            user_data: 0,
        },
        &file,
    );
    assert_eq!(result, Err(DrmError::Busy));

    // An asynchronous flip is refused, because `DRM_CAP_ASYNC_PAGE_FLIP` says 0.
    let result = config.legacy_page_flip(
        &mode::CrtcPageFlip {
            crtc_id: ids.crtc.get(),
            fb_id: framebuffer.get(),
            flags: mode::page_flip_flags::ASYNC,
            reserved: 0,
            user_data: 0,
        },
        &file,
    );
    assert_eq!(result, Err(DrmError::InvalidArgument));
}

#[ktest]
fn framebuffers_belong_to_the_file_that_created_them() {
    let (device, file, _ids, _atomic) = fixture();
    let config = config_of(&device);
    let other = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();

    let mine = add_framebuffer(config, &file, 64, 64);
    let theirs = add_framebuffer(config, &other, 64, 64);

    assert_eq!(config.framebuffer_ids_of(file.id()), vec![mine.get()]);
    assert_eq!(config.framebuffer_ids_of(other.id()), vec![theirs.get()]);

    // Any client may resolve any ID, as `drm_framebuffer_lookup` does, but only
    // the owner may remove one — and a foreign framebuffer is reported as absent
    // rather than as forbidden, because that is what looking it up in the caller's
    // own list produces.
    assert!(config.framebuffer(theirs).is_ok());
    assert_eq!(
        config.remove_framebuffer(theirs, file.id()),
        Err(DrmError::NotFound)
    );
    assert!(config.remove_framebuffer(mine, file.id()).is_ok());
    assert_eq!(
        config.remove_framebuffer(mine, file.id()),
        Err(DrmError::NotFound)
    );

    // Closing a file takes its framebuffers with it.
    let id = other.id();
    drop(other);
    assert!(config.framebuffer_ids_of(id).is_empty());
    assert_eq!(config.framebuffer(theirs).err(), Some(DrmError::NotFound));
}

#[ktest]
fn framebuffers_are_validated_against_their_buffer() {
    let (device, file, _ids, _atomic) = fixture();
    let config = config_of(&device);

    let template = |width: u32, height: u32, pitch: u32, size: usize, format: u32| Framebuffer {
        id: ObjectId::NONE,
        width,
        height,
        pixel_format: format,
        modifier: fourcc::MOD_LINEAR,
        objects: [Some(Arc::new(FakeObject { size })), None, None, None],
        pitches: [pitch, 0, 0, 0],
        offsets: [0; 4],
        owner: file.id(),
    };

    // A pitch too small for the width would make every line overlap the previous.
    assert!(
        config
            .add_framebuffer(template(64, 64, 63 * 4, 64 * 64 * 4, fourcc::XRGB8888))
            .is_err()
    );
    // A buffer too small for the last line is the case that would have the display
    // engine read whatever follows it.
    assert!(
        config
            .add_framebuffer(template(64, 64, 64 * 4, 64 * 63 * 4, fourcc::XRGB8888))
            .is_err()
    );
    // A format nothing understands.
    assert!(
        config
            .add_framebuffer(template(64, 64, 64 * 4, 64 * 64 * 4, 0x1234_5678))
            .is_err()
    );
    // Larger than the device can scan out.
    assert!(
        config
            .add_framebuffer(template(
                8192,
                64,
                8192 * 4,
                8192 * 64 * 4,
                fourcc::XRGB8888
            ))
            .is_err()
    );
    assert!(
        config
            .add_framebuffer(template(64, 64, 64 * 4, 64 * 64 * 4, fourcc::XRGB8888))
            .is_ok()
    );
}

#[ktest]
fn mode_blobs_do_not_accumulate() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);

    let blobs = || config.object_ids(ObjectKind::Blob).len();
    let initial = blobs();

    // Each legacy modeset creates a mode blob, and each one supersedes the last.
    for _ in 0..4 {
        config
            .legacy_set_crtc(
                &set_crtc_request(ids, framebuffer),
                &[ids.connector.get()],
                &file,
            )
            .unwrap();
    }
    assert_eq!(blobs(), initial + 1);

    // Turning the CRTC off drops the last one.
    config
        .legacy_set_crtc(
            &mode::Crtc {
                crtc_id: ids.crtc.get(),
                ..mode::Crtc::default()
            },
            &[],
            &file,
        )
        .unwrap();
    assert_eq!(blobs(), initial);
}

#[ktest]
fn client_blobs_survive_only_as_long_as_they_are_needed() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);

    let mine = config
        .create_blob(vec![1, 2, 3], BlobOwner::File(file.id()))
        .unwrap();
    assert_eq!(config.blob(mine).unwrap().data, vec![1, 2, 3]);

    // Another client cannot destroy it.
    let other = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    assert_eq!(
        config.destroy_blob(mine, other.id()),
        Err(DrmError::PermissionDenied)
    );
    assert!(config.destroy_blob(mine, file.id()).is_ok());
    assert_eq!(
        config.destroy_blob(mine, file.id()),
        Err(DrmError::NotFound)
    );

    // A blob still in force as a CRTC's mode outlives its creator's attempt to
    // destroy it, which is what Linux's reference count achieves.
    let in_use = config
        .create_blob(test_mode().as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();
    config
        .commit(
            &AtomicRequest {
                assignments: light_up_assignments(config, ids, framebuffer, in_use),
                flags: CommitFlags {
                    allow_modeset: true,
                    ..CommitFlags::default()
                },
            },
            &file,
        )
        .unwrap();
    assert!(config.destroy_blob(in_use, file.id()).is_ok());
    assert!(config.blob(in_use).is_ok());
}

#[ktest]
fn kms_ioctls_are_sorted_and_disjoint() {
    for pair in KMS_IOCTLS.windows(2) {
        assert!(
            pair[0].nr < pair[1].nr,
            "{} and {} are out of order or duplicated",
            pair[0].name,
            pair[1].name
        );
    }

    // The two tables are searched independently, so a number in both would let
    // whichever is searched first shadow the other.
    for kms in KMS_IOCTLS {
        assert!(
            !crate::ioctl::CORE_IOCTLS
                .iter()
                .any(|core| core.nr == kms.nr),
            "{} is in both dispatch tables",
            kms.name
        );
        // Nothing about a display belongs on a render node.
        assert!(
            !kms.flags.contains(crate::ioctl::IoctlFlags::RENDER_ALLOW),
            "{} is allowed on a render node",
            kms.name
        );
    }
}

/// The `IN_FORMATS` blob is read by GBM with a cast, so its offsets and counts
/// have to be right to the byte.
#[ktest]
fn in_formats_blob_has_the_layout_gbm_expects() {
    let (device, _file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let state = config.state();

    let blob = config
        .property_value(ids.primary, StandardProperty::PlaneInFormats, &state)
        .unwrap();
    let blob = config.blob(ObjectId(blob as u32)).unwrap();

    // The blob is a header followed by the format and modifier arrays, so it is
    // longer than the header. `Pod::from_bytes` requires an exact length and
    // would panic; the prefix form is what reads a header out of a larger buffer.
    let header = mode::FormatModifierBlob::from_first_bytes(&blob.data);
    assert_eq!(header.version, mode::FORMAT_BLOB_CURRENT);
    assert_eq!(header.count_formats, TEST_FORMATS.len() as u32);
    assert_eq!(
        header.formats_offset as usize,
        size_of::<mode::FormatModifierBlob>()
    );
    assert_eq!(header.count_modifiers, TEST_MODIFIERS.len() as u32);
    // The modifier array holds `__u64`s, so its offset must be eight-byte aligned.
    assert_eq!(header.modifiers_offset % 8, 0);

    let at = header.formats_offset as usize;
    assert_eq!(u32::from_bytes(&blob.data[at..at + 4]), fourcc::XRGB8888);

    let at = header.modifiers_offset as usize;
    let modifier = mode::FormatModifier::from_bytes(&blob.data[at..]);
    assert_eq!(modifier.modifier, fourcc::MOD_LINEAR);
    assert_eq!(modifier.offset, 0);
    // One bit per format the modifier applies to.
    assert_eq!(modifier.formats, (1 << TEST_FORMATS.len()) - 1);
    assert_eq!(
        blob.data.len(),
        at + size_of::<mode::FormatModifier>() * TEST_MODIFIERS.len()
    );
}

/// The two-pass protocol, through the real ioctl entry points. Getting this wrong
/// is the classic cause of `modetest` reporting an empty device.
#[ktest]
fn resources_follow_the_two_pass_protocol() {
    let (device, file, ids, _atomic) = fixture();
    let user = FakeUserMem::new(1024);

    // First pass: every count zero, every pointer null. Nothing may be written,
    // and the true counts must come back.
    let mut payload = vec![0u8; size_of::<mode::CardRes>()];
    device
        .ioctl(&file, nr::MODE_GETRESOURCES, &mut payload, &user.context())
        .unwrap();
    let probed = mode::CardRes::from_bytes(&payload);
    assert_eq!(probed.count_crtcs, 1);
    assert_eq!(probed.count_connectors, 1);
    assert_eq!(probed.count_encoders, 1);
    assert_eq!(probed.count_fbs, 0);
    assert_eq!((probed.max_width, probed.max_height), (4096, 4096));

    // Second pass: buffers sized from the first answer.
    let args = mode::CardRes {
        crtc_id_ptr: FakeUserMem::address_of(0),
        connector_id_ptr: FakeUserMem::address_of(16),
        encoder_id_ptr: FakeUserMem::address_of(32),
        count_crtcs: 1,
        count_connectors: 1,
        count_encoders: 1,
        ..mode::CardRes::default()
    };
    payload.copy_from_slice(args.as_bytes());
    device
        .ioctl(&file, nr::MODE_GETRESOURCES, &mut payload, &user.context())
        .unwrap();
    assert_eq!(user.read_val::<u32>(0), ids.crtc.get());
    assert_eq!(user.read_val::<u32>(16), ids.connector.get());
    assert_eq!(user.read_val::<u32>(32), ids.encoder.get());

    // A plane list is filtered by the universal-planes capability: a legacy client
    // has no way to program the primary plane, so it is not shown one.
    let mut payload = vec![0u8; size_of::<mode::GetPlaneRes>()];
    device
        .ioctl(
            &file,
            nr::MODE_GETPLANERESOURCES,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert_eq!(mode::GetPlaneRes::from_bytes(&payload).count_planes, 1);

    file.set_client_cap(crate::uapi::cap::client::UNIVERSAL_PLANES, 1)
        .unwrap();
    // Probe again from a clean payload. The previous call wrote the count back, and
    // reusing it would claim a one-element array at the null pointer still sitting in
    // `plane_id_ptr`, which the driver rightly reports as `EFAULT`.
    payload.fill(0);
    device
        .ioctl(
            &file,
            nr::MODE_GETPLANERESOURCES,
            &mut payload,
            &user.context(),
        )
        .unwrap();
    assert_eq!(mode::GetPlaneRes::from_bytes(&payload).count_planes, 2);
}

#[ktest]
fn getconnector_reports_modes_and_properties() {
    let (device, file, ids, _atomic) = fixture();
    let user = FakeUserMem::new(4096);

    let mut payload = vec![0u8; size_of::<mode::GetConnector>()];
    let probe = mode::GetConnector {
        connector_id: ids.connector.get(),
        ..mode::GetConnector::default()
    };
    payload.copy_from_slice(probe.as_bytes());
    device
        .ioctl(&file, nr::MODE_GETCONNECTOR, &mut payload, &user.context())
        .unwrap();

    let probed = mode::GetConnector::from_bytes(&payload);
    assert_eq!(probed.count_modes, 1);
    assert_eq!(probed.count_encoders, 1);
    assert_eq!(probed.connection, ConnectionStatus::Connected as u32);
    assert_eq!(probed.connector_type, mode::connector_type::VIRTUAL);
    assert_eq!((probed.mm_width, probed.mm_height), (340, 190));
    // A legacy client sees `EDID` and `DPMS` but not the atomic properties.
    assert_eq!(probed.count_props, 2);

    let args = mode::GetConnector {
        connector_id: ids.connector.get(),
        modes_ptr: FakeUserMem::address_of(0),
        count_modes: 1,
        encoders_ptr: FakeUserMem::address_of(256),
        count_encoders: 1,
        props_ptr: FakeUserMem::address_of(512),
        prop_values_ptr: FakeUserMem::address_of(768),
        count_props: 2,
        ..mode::GetConnector::default()
    };
    payload.copy_from_slice(args.as_bytes());
    device
        .ioctl(&file, nr::MODE_GETCONNECTOR, &mut payload, &user.context())
        .unwrap();

    let reported = user.read_val::<ModeInfo>(0);
    assert_eq!(reported.hdisplay, 640);
    assert_eq!(reported.vrefresh, 60);
    assert_eq!(&reported.name[..7], b"640x480");
    assert_eq!(user.read_val::<u32>(256), ids.encoder.get());

    let edid = config_of(&device)
        .properties()
        .id(StandardProperty::ConnectorEdid);
    assert_eq!(user.read_val::<u32>(512), edid.get());

    // With the atomic capability the same call reports more properties, which is
    // the client capability doing its job.
    file.set_client_cap(crate::uapi::cap::client::ATOMIC, 1)
        .unwrap();
    payload.copy_from_slice(probe.as_bytes());
    device
        .ioctl(&file, nr::MODE_GETCONNECTOR, &mut payload, &user.context())
        .unwrap();
    assert_eq!(mode::GetConnector::from_bytes(&payload).count_props, 3);
}

/// `MODE_ATOMIC` reads five parallel user arrays; a mistake in pairing them up
/// would apply the right values to the wrong properties.
#[ktest]
fn the_atomic_ioctl_pairs_its_arrays_correctly() {
    let (device, file, ids, atomic) = fixture();
    let config = config_of(&device);
    let user = FakeUserMem::new(4096);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    let mode_blob = config
        .create_blob(test_mode().as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();

    // The atomic interface is opt-in.
    let mut payload = vec![0u8; size_of::<mode::Atomic>()];
    let refused = mode::Atomic {
        flags: mode::atomic_flags::ALLOW_MODESET,
        ..mode::Atomic::default()
    };
    payload.copy_from_slice(refused.as_bytes());
    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::InvalidArgument)
    );
    file.set_client_cap(crate::uapi::cap::client::ATOMIC, 1)
        .unwrap();

    // Three objects with three different property counts, so that an off-by-one in
    // the pairing cannot leave every value on the right property anyway.
    let property = |standard| config.properties().id(standard).get();
    let objects = [ids.crtc.get(), ids.connector.get(), ids.primary.get()];
    let counts = [2u32, 1, 8];
    let properties = [
        property(StandardProperty::CrtcModeId),
        property(StandardProperty::CrtcActive),
        property(StandardProperty::ConnectorCrtcId),
        property(StandardProperty::PlaneCrtcId),
        property(StandardProperty::PlaneFbId),
        property(StandardProperty::PlaneCrtcX),
        property(StandardProperty::PlaneCrtcY),
        property(StandardProperty::PlaneCrtcW),
        property(StandardProperty::PlaneCrtcH),
        property(StandardProperty::PlaneSrcW),
        property(StandardProperty::PlaneSrcH),
    ];
    let values = [
        mode_blob.get() as u64,
        1,
        ids.crtc.get() as u64,
        ids.crtc.get() as u64,
        framebuffer.get() as u64,
        0,
        0,
        640,
        480,
        640 << 16,
        480 << 16,
    ];

    let (objs_at, counts_at, props_at, values_at) = (0usize, 64usize, 128usize, 256usize);
    for (index, object) in objects.iter().enumerate() {
        user.write_val(objs_at + index * 4, object);
    }
    for (index, count) in counts.iter().enumerate() {
        user.write_val(counts_at + index * 4, count);
    }
    for (index, id) in properties.iter().enumerate() {
        user.write_val(props_at + index * 4, id);
    }
    for (index, value) in values.iter().enumerate() {
        user.write_val(values_at + index * 8, value);
    }

    let args = mode::Atomic {
        flags: mode::atomic_flags::ALLOW_MODESET,
        count_objs: objects.len() as u32,
        objs_ptr: FakeUserMem::address_of(objs_at),
        count_props_ptr: FakeUserMem::address_of(counts_at),
        props_ptr: FakeUserMem::address_of(props_at),
        prop_values_ptr: FakeUserMem::address_of(values_at),
        reserved: 0,
        user_data: 0,
    };
    payload.copy_from_slice(args.as_bytes());
    device
        .ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context())
        .expect("the commit is well formed");
    assert_eq!(*atomic.commits.lock(), 1);

    // Every value landed on the property it was paired with, on the object it was
    // paired with.
    let state = config.state();
    let crtc = state.crtc(ids.crtc).unwrap();
    assert!(crtc.active);
    assert_eq!(crtc.mode_blob, Some(mode_blob));
    assert_eq!(crtc.connectors, vec![ids.connector]);
    let plane = state.plane(ids.primary).unwrap();
    assert_eq!(plane.crtc, Some(ids.crtc));
    assert_eq!(plane.framebuffer.as_ref().unwrap().id, framebuffer);
    assert_eq!(plane.crtc_rect, (0, 0, 640, 480));
    assert_eq!(plane.src_rect, (0, 0, 640 << 16, 480 << 16));

    // Now move the boundary by one, so that the connector's slot takes `ACTIVE` —
    // a CRTC property. It has to be refused rather than applied to whatever object
    // sits at that index.
    user.write_val(counts_at, &1u32);
    user.write_val(counts_at + 4, &2u32);
    payload.copy_from_slice(args.as_bytes());
    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::NotFound)
    );
    assert_eq!(*atomic.commits.lock(), 1);

    // A per-object count whose total overflows is refused before any array is read.
    user.write_val(counts_at, &u32::MAX);
    payload.copy_from_slice(args.as_bytes());
    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::InvalidArgument)
    );
}

/// `TEST_ONLY` with an event asks for a notification about something that will not
/// happen, and a flag we do not know about might mean anything.
#[ktest]
fn the_atomic_ioctl_validates_its_flags() {
    let (device, file, _ids, _atomic) = fixture();
    let user = FakeUserMem::new(64);
    file.set_client_cap(crate::uapi::cap::client::ATOMIC, 1)
        .unwrap();

    for flags in [
        mode::atomic_flags::TEST_ONLY | mode::atomic_flags::PAGE_FLIP_EVENT,
        mode::atomic_flags::PAGE_FLIP_ASYNC,
        1 << 31,
    ] {
        let args = mode::Atomic {
            flags,
            ..mode::Atomic::default()
        };
        let mut payload = args.as_bytes().to_vec();
        assert_eq!(
            device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
            Err(DrmError::InvalidArgument),
            "flags {flags:#x} were accepted"
        );
    }

    // A reserved field that is not zero means the caller is using a structure we
    // do not understand.
    let args = mode::Atomic {
        reserved: 1,
        ..mode::Atomic::default()
    };
    let mut payload = args.as_bytes().to_vec();
    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::InvalidArgument)
    );
}

#[ktest]
fn empty_atomic_ioctls_validate_access_and_objects() {
    let (device, file, ids, atomic) = fixture_seeded();
    let config = config_of(&device);
    let state = config.state();
    let checks = *atomic.checks.lock();
    let user = FakeUserMem::new(64);
    let mut args = mode::Atomic {
        flags: mode::atomic_flags::ALLOW_MODESET,
        ..mode::Atomic::default()
    };
    let mut payload = args.as_bytes().to_vec();
    *atomic.refuse.lock() = true;

    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::InvalidArgument),
        "empty commits still require the atomic client capability"
    );
    file.set_client_cap(crate::uapi::cap::client::ATOMIC, 1)
        .unwrap();
    let nonmaster = device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap();
    nonmaster
        .set_client_cap(crate::uapi::cap::client::ATOMIC, 1)
        .unwrap();
    assert_eq!(
        device.ioctl(&nonmaster, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::PermissionDenied)
    );
    device
        .ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context())
        .unwrap();

    // Listing objects with zero properties is equivalent to listing no objects.
    for (index, object) in [ids.crtc, ids.connector, ids.primary].iter().enumerate() {
        user.write_val(index * 4, &object.get());
        user.write_val(16 + index * 4, &0u32);
    }
    args.count_objs = 3;
    args.objs_ptr = FakeUserMem::address_of(0);
    args.count_props_ptr = FakeUserMem::address_of(16);
    payload.copy_from_slice(args.as_bytes());
    device
        .ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context())
        .unwrap();

    args.flags |= mode::atomic_flags::PAGE_FLIP_EVENT;
    payload.copy_from_slice(args.as_bytes());
    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::InvalidArgument)
    );
    args.flags = mode::atomic_flags::ALLOW_MODESET;
    args.count_objs = 1;
    let blob = config
        .create_blob(vec![0], BlobOwner::File(file.id()))
        .unwrap();
    for object in [
        0,
        u32::MAX,
        ids.encoder.get(),
        config.properties().id(StandardProperty::CrtcActive).get(),
        blob.get(),
    ] {
        user.write_val(0, &object);
        payload.copy_from_slice(args.as_bytes());
        assert_eq!(
            device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
            Err(DrmError::NotFound),
            "object {object} has no KMS properties"
        );
    }
    user.write_val(0, &ids.crtc.get());
    args.count_props_ptr = 0;
    payload.copy_from_slice(args.as_bytes());
    assert_eq!(
        device.ioctl(&file, nr::MODE_ATOMIC, &mut payload, &user.context()),
        Err(DrmError::BadAddress)
    );
    assert_eq!(*atomic.checks.lock(), checks);
    assert_eq!(*atomic.commits.lock(), 0);
    assert!(Arc::ptr_eq(&state, &config.state()));
}

/// A render-only device answers every mode-setting ioctl with `EOPNOTSUPP`, the
/// way Linux does when `DRIVER_MODESET` is absent. `aster-pvr` depends on this:
/// it has no display at all.
#[ktest]
fn mode_setting_ioctls_need_a_display() {
    let device = DrmDevice::new_unregistered(Arc::new(RenderOnlyDriver), Some(0), Some(128));
    let file = device
        .open(DrmMinor::Primary, Privilege::Privileged)
        .unwrap();
    let user = FakeUserMem::new(64);

    let mut payload = vec![0u8; size_of::<mode::CardRes>()];
    assert_eq!(
        device.ioctl(&file, nr::MODE_GETRESOURCES, &mut payload, &user.context()),
        Err(DrmError::NotSupported)
    );

    // And on the render node it is refused earlier still, by the node check.
    let render = device
        .open(DrmMinor::Render, Privilege::Privileged)
        .unwrap();
    assert_eq!(
        device.ioctl(
            &render,
            nr::MODE_GETRESOURCES,
            &mut payload,
            &user.context()
        ),
        Err(DrmError::PermissionDenied)
    );
}

#[ktest]
fn invalid_legacy_connector_does_not_leak_a_mode_blob() {
    let (device, file, ids, _atomic) = fixture();
    let config = config_of(&device);
    let fb = add_framebuffer(config, &file, 640, 480);
    let before = config.object_ids(ObjectKind::Blob);
    for _ in 0..3 {
        assert_eq!(
            config.legacy_set_crtc(&set_crtc_request(ids, fb), &[u32::MAX], &file),
            Err(DrmError::NotFound)
        );
        assert_eq!(config.object_ids(ObjectKind::Blob), before);
    }
}

#[ktest]
fn exhausted_object_ids_do_not_wrap_or_panic_in_user_allocations() {
    let ids = ObjectIds {
        next: AtomicU32::new(u32::MAX),
    };
    assert_eq!(ids.try_allocate(), Ok(ObjectId(u32::MAX)));
    for _ in 0..3 {
        assert_eq!(ids.try_allocate(), Err(DrmError::NoSpace));
    }

    let (device, file, _, _) = fixture();
    let config = config_of(&device);
    config.ids.next.store(0, Ordering::Relaxed);
    assert_eq!(
        config.create_blob(vec![1], BlobOwner::File(file.id())),
        Err(DrmError::NoSpace)
    );
    assert_eq!(
        config.add_framebuffer(Framebuffer {
            id: ObjectId::NONE,
            width: 1,
            height: 1,
            pixel_format: fourcc::XRGB8888,
            modifier: fourcc::MOD_LINEAR,
            objects: [Some(Arc::new(FakeObject { size: 4096 })), None, None, None],
            pitches: [4, 0, 0, 0],
            offsets: [0; 4],
            owner: file.id(),
        }),
        Err(DrmError::NoSpace)
    );
}

#[ktest]
fn a_prepared_commit_retains_its_mode_blob_during_destruction() {
    let (device, file, ids, atomic) = fixture();
    let config = config_of(&device);
    let framebuffer = add_framebuffer(config, &file, 640, 480);
    let blob = config
        .create_blob(test_mode().as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();
    *atomic.destroy_during_check.lock() = Some((Arc::downgrade(&device), blob, file.id()));
    config
        .commit(
            &AtomicRequest {
                assignments: light_up_assignments(config, ids, framebuffer, blob),
                flags: CommitFlags {
                    allow_modeset: true,
                    ..CommitFlags::default()
                },
            },
            &file,
        )
        .unwrap();
    assert_eq!(config.state().crtc(ids.crtc).unwrap().mode_blob, Some(blob));
    assert_eq!(config.blob(blob).unwrap().data, test_mode().as_bytes());

    // Replacing the mode retires the core-owned registration.
    let next = config
        .create_blob(test_mode().as_bytes().to_vec(), BlobOwner::File(file.id()))
        .unwrap();
    config
        .commit(
            &AtomicRequest {
                assignments: vec![PropertyAssignment {
                    object: ids.crtc,
                    property: config.properties().id(StandardProperty::CrtcModeId),
                    value: next.get() as u64,
                }],
                flags: CommitFlags {
                    allow_modeset: true,
                    ..CommitFlags::default()
                },
            },
            &file,
        )
        .unwrap();
    assert_eq!(config.blob(blob).unwrap_err(), DrmError::NotFound);
}

#[derive(Debug)]
struct ReentrantFramebufferObject {
    device: Weak<DrmDevice>,
    owner: FileId,
    dropped: Arc<core::sync::atomic::AtomicBool>,
}

impl Drop for ReentrantFramebufferObject {
    fn drop(&mut self) {
        assert!(
            config_of(&self.device.upgrade().unwrap())
                .framebuffer_ids_of(self.owner)
                .is_empty()
        );
        self.dropped.store(true, Ordering::Relaxed);
    }
}

impl GemObject for ReentrantFramebufferObject {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }
    fn size(&self) -> usize {
        4096
    }
    fn buffer_use(&self) -> BufferUse {
        BufferUse::Scanout
    }
    fn segment(&self) -> Option<&ostd::mm::Segment<()>> {
        None
    }
    fn fault(&self, _: usize) -> DrmResult<ostd::mm::UFrame> {
        Err(DrmError::BadAddress)
    }
}

#[ktest]
fn framebuffer_removal_releases_driver_objects_after_unlocking() {
    for release_file in [false, true] {
        let (device, file, _, _) = fixture();
        let config = config_of(&device);
        let dropped = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let id = config
            .add_framebuffer(Framebuffer {
                id: ObjectId::NONE,
                width: 1,
                height: 1,
                pixel_format: fourcc::XRGB8888,
                modifier: fourcc::MOD_LINEAR,
                objects: [
                    Some(Arc::new(ReentrantFramebufferObject {
                        device: Arc::downgrade(&device),
                        owner: file.id(),
                        dropped: dropped.clone(),
                    })),
                    None,
                    None,
                    None,
                ],
                pitches: [4, 0, 0, 0],
                offsets: [0; 4],
                owner: file.id(),
            })
            .unwrap();
        if release_file {
            config.release_file(file.id());
        } else {
            config.remove_framebuffer(id, file.id()).unwrap();
        }
        assert!(dropped.load(Ordering::Relaxed));
    }
}

#[ktest]
fn releasing_blob_ownership_preserves_readers_without_retaining_contents() {
    for close_file in [false, true] {
        let (device, file, _, _) = fixture();
        let config = config_of(&device);
        let id = config
            .create_blob(vec![1, 2, 3], BlobOwner::File(file.id()))
            .unwrap();
        let reader = config.blob(id).unwrap();
        let weak = Arc::downgrade(&reader);
        if close_file {
            config.release_file(file.id());
        } else {
            config.destroy_blob(id, file.id()).unwrap();
        }
        assert_eq!(config.blob(id).unwrap().data, vec![1, 2, 3]);
        assert_eq!(config.object_kind(id), Some(ObjectKind::Blob));
        assert_eq!(
            config.destroy_blob(id, file.id()),
            Err(DrmError::PermissionDenied)
        );
        drop(reader);
        assert!(weak.upgrade().is_none());
        assert_eq!(config.blob(id).unwrap_err(), DrmError::NotFound);
        assert_eq!(config.object_kind(id), None);
        assert!(!config.object_ids(ObjectKind::Blob).contains(&id.get()));
        // A later allocation reclaims dead metadata without retaining its data.
        config
            .create_blob(vec![4], BlobOwner::File(file.id()))
            .unwrap();
        assert!(!config.blobs.lock().contains_key(&id));
    }
}

#[ktest]
fn collecting_mode_ownership_cannot_remove_a_prepared_legacy_blob() {
    let (device, _, _, _) = fixture();
    let config = config_of(&device);
    let prepared = config
        .insert_blob(test_mode().as_bytes().to_vec(), BlobOwner::Mode)
        .unwrap();
    let id = prepared.id;
    config.collect_mode_blobs();
    assert_eq!(config.blob(id).unwrap().data, test_mode().as_bytes());
    drop(prepared);
    assert_eq!(config.blob(id).unwrap_err(), DrmError::NotFound);
}

fn page_flip_ioctl(
    device: &Arc<DrmDevice>,
    file: &DrmFile,
    ids: Ids,
    framebuffer: ObjectId,
    cookie: u64,
) -> DrmResult<()> {
    let user = FakeUserMem::new(0);
    let mut payload = mode::CrtcPageFlip {
        crtc_id: ids.crtc.get(),
        fb_id: framebuffer.get(),
        flags: mode::page_flip_flags::EVENT,
        reserved: 0,
        user_data: cookie,
    }
    .as_bytes()
    .to_vec();
    device.ioctl(file, nr::MODE_PAGE_FLIP, &mut payload, &user.context())
}

#[ktest]
fn page_flip_ioctl_preserves_completion_during_driver_commit() {
    let (device, file, ids) = deferred_fixture_with(true);
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .unwrap();
    page_flip_ioctl(&device, &file, ids, second, 0x93).unwrap();
    let event = read_one_event(&file).expect("IRQ before event parking is not lost");
    assert_eq!(event.sequence, 2);
    assert_eq!(event.user_data, 0x93);
    assert_eq!(
        config
            .state()
            .plane(ids.primary)
            .unwrap()
            .framebuffer
            .as_ref()
            .unwrap()
            .id,
        second
    );
    assert!(!config.vblank().has_pending(ids.crtc));
}

#[ktest]
fn unconfirmed_scanouts_survive_events_close_and_bounded_backpressure() {
    let (device, file, ids) = deferred_fixture();
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);
    let third = add_framebuffer(config, &file, 640, 480);
    let fourth = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .unwrap();
    config.vblank().arm(ids.crtc);
    config.vblank().tick(ids.crtc);
    let first_object = Arc::downgrade(
        config.framebuffer(first).unwrap().objects[0]
            .as_ref()
            .unwrap(),
    );
    let second_object = Arc::downgrade(
        config.framebuffer(second).unwrap().objects[0]
            .as_ref()
            .unwrap(),
    );

    page_flip_ioctl(&device, &file, ids, second, 1).unwrap();
    config.remove_framebuffer(first, file.id()).unwrap();
    for _ in 0..3 {
        config.vblank().tick(ids.crtc);
    }
    assert_eq!(read_one_event(&file).unwrap().user_data, 1);
    assert!(
        first_object.upgrade().is_some(),
        "event fallback is not DMA retirement proof"
    );

    page_flip_ioctl(&device, &file, ids, third, 2).unwrap();
    config.remove_framebuffer(second, file.id()).unwrap();
    for _ in 0..3 {
        config.vblank().tick(ids.crtc);
    }
    assert_eq!(read_one_event(&file).unwrap().user_data, 2);
    // Geometry updates retain the same framebuffer in current state;
    // they need no additional retirement slot and can acquire a later real ack.
    let geometry = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.primary,
            property: config.properties().id(StandardProperty::PlaneCrtcX),
            value: 0,
        }],
        flags: CommitFlags::default(),
    };
    let previous_generation = config.vblank().generation(ids.crtc);
    for _ in 0..16 {
        config.commit(&geometry, &file).unwrap();
    }
    let generation = config.vblank().generation(ids.crtc);
    assert_eq!(generation, previous_generation + 16);
    assert_eq!(
        page_flip_ioctl(&device, &file, ids, fourth, 3),
        Err(DrmError::Busy)
    );
    assert_eq!(
        config.vblank().generation(ids.crtc),
        generation,
        "Busy occurs before hardware publication"
    );
    assert!(
        file.reserve_events(128),
        "Busy returned its event reservation"
    );
    file.release_events(128);
    assert!(first_object.upgrade().is_some());
    assert!(second_object.upgrade().is_some());

    // Closing a client revokes its event and registry entries, not hardware DMA ownership.
    config.release_file(file.id());
    assert!(first_object.upgrade().is_some());
    assert!(second_object.upgrade().is_some());
    config.vblank().arm_update(ids.crtc, generation);
    config.vblank().tick_update(ids.crtc, generation);
    assert!(
        first_object.upgrade().is_some(),
        "bottom half does not destroy driver-owned objects"
    );
    config.vblank().retire_completed();
    assert!(first_object.upgrade().is_none());
    assert!(second_object.upgrade().is_none());
    assert!(config.vblank().can_update(ids.crtc, true));
}

#[ktest]
fn empty_atomic_commits_do_not_retire_scanouts_or_consume_events() {
    let (device, file, ids) = deferred_fixture();
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .unwrap();
    config.vblank().arm(ids.crtc);
    config.vblank().tick(ids.crtc);
    let object = Arc::downgrade(
        config.framebuffer(first).unwrap().objects[0]
            .as_ref()
            .unwrap(),
    );
    page_flip_ioctl(&device, &file, ids, second, 42).unwrap();
    config.remove_framebuffer(first, file.id()).unwrap();
    let state = config.state();
    let generation = config.vblank().generation(ids.crtc);
    config.vblank().arm_update(ids.crtc, generation);
    config.vblank().tick_update(ids.crtc, generation);
    let sequence = config.vblank().sequence(ids.crtc);
    assert!(object.upgrade().is_some());
    assert!(file.has_events());

    config.commit(&AtomicRequest::default(), &file).unwrap();
    assert!(Arc::ptr_eq(&state, &config.state()));
    assert_eq!(config.vblank().generation(ids.crtc), generation);
    assert_eq!(config.vblank().sequence(ids.crtc), sequence);
    assert!(object.upgrade().is_some());
    assert_eq!(read_one_event(&file).unwrap().user_data, 42);
    assert!(read_one_event(&file).is_none());

    config.vblank().retire_completed();
    assert!(object.upgrade().is_none());
}

#[ktest]
fn non_event_commits_also_retain_unconfirmed_scanout_objects() {
    let (device, file, ids) = deferred_fixture();
    let config = config_of(&device);
    let first = add_framebuffer(config, &file, 640, 480);
    let second = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(&set_crtc_request(ids, first), &[ids.connector.get()], &file)
        .unwrap();
    config.vblank().arm(ids.crtc);
    config.vblank().tick(ids.crtc);
    let object = Arc::downgrade(
        config.framebuffer(first).unwrap().objects[0]
            .as_ref()
            .unwrap(),
    );
    let request = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object: ids.primary,
            property: config.properties().id(StandardProperty::PlaneFbId),
            value: second.get() as u64,
        }],
        flags: CommitFlags::default(),
    };
    config.commit(&request, &file).unwrap();
    config.remove_framebuffer(first, file.id()).unwrap();
    assert!(object.upgrade().is_some());
    let generation = config.vblank().generation(ids.crtc);
    config.vblank().arm_update(ids.crtc, generation);
    config.vblank().tick_update(ids.crtc, generation);
    config.vblank().retire_completed();
    assert!(object.upgrade().is_none());
}

#[ktest]
fn continuous_coalesced_page_flip_ioctls_preserve_events_and_old_buffer_ownership() {
    let (device, file, ids) = deferred_fixture();
    let config = config_of(&device);
    let mut current = add_framebuffer(config, &file, 640, 480);
    config
        .legacy_set_crtc(
            &set_crtc_request(ids, current),
            &[ids.connector.get()],
            &file,
        )
        .unwrap();
    let mut previous_generation = config.vblank().generation(ids.crtc);
    config.vblank().observe_hardware(
        ids.crtc,
        HardwareObservation {
            vblank: true,
            flipped_update: Some(previous_generation),
            retired_update: None,
        },
    );
    let mut previous_owner: Option<Weak<dyn GemObject>> = None;
    for frame in 0..64 {
        let next = add_framebuffer(config, &file, 640, 480);
        let owner = Arc::downgrade(
            config.framebuffer(current).unwrap().objects[0]
                .as_ref()
                .unwrap(),
        );
        page_flip_ioctl(&device, &file, ids, next, frame).unwrap();
        config.remove_framebuffer(current, file.id()).unwrap();
        let generation = config.vblank().generation(ids.crtc);
        // The BSP event policy accepts the merged latch/vsync ACK. Only a latch
        // seen in a preceding ACK proves the older scanout can be retired.
        config.vblank().observe_hardware(
            ids.crtc,
            HardwareObservation {
                vblank: true,
                flipped_update: Some(generation),
                retired_update: Some(previous_generation),
            },
        );
        assert!(!config.vblank().is_update_retired(ids.crtc, generation));
        assert!(
            read_one_event(&file).is_none(),
            "IRQ observation does not deliver events"
        );
        config
            .vblank()
            .dispatch_observed_vblank(ids.crtc, generation);
        let event = read_one_event(&file).expect("every submitted frame completes its event");
        assert_eq!(event.user_data, frame);
        assert_eq!(event.sequence, frame as u32 + 2);
        assert!(read_one_event(&file).is_none());
        config.vblank().retire_completed();
        if let Some(previous) = previous_owner {
            assert!(previous.upgrade().is_none());
        }
        assert!(
            owner.upgrade().is_some(),
            "the same merged ACK cannot release this flip's old buffer"
        );
        previous_owner = Some(owner);
        previous_generation = generation;
        current = next;
    }
    config.vblank().observe_hardware(
        ids.crtc,
        HardwareObservation {
            vblank: true,
            flipped_update: None,
            retired_update: Some(previous_generation),
        },
    );
    config.vblank().retire_completed();
    assert!(previous_owner.unwrap().upgrade().is_none());
    assert!(config.vblank().can_update(ids.crtc, true));
    assert_eq!(
        config
            .state()
            .plane(ids.primary)
            .unwrap()
            .framebuffer
            .as_ref()
            .unwrap()
            .id,
        current
    );
}
