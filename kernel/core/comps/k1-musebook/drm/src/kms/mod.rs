// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Kernel mode setting: the display pipeline objects and atomic commits.
//!
//!
//! # One path, not two
//!
//! Linux carries both a legacy and an atomic mode-setting path, for historical
//! reasons we do not share. Here, the legacy ioctls (`SETCRTC`, `PAGE_FLIP`,
//! `SETPLANE`) are translated into atomic commits by the core, so drivers only
//! ever implement [`AtomicDriver`]. Two code paths for the same hardware state is
//! how mode-setting bugs become unreproducible.
//!
//! The translation is not a parallel implementation that happens to agree: each
//! legacy request is turned into the very list of property assignments an atomic
//! client would have sent (see [`legacy`]), and that list goes through
//! [`ModeConfig::commit`] like any other. So there is exactly one place that
//! interprets a property, one place that validates a state, and one place that
//! hands it to the driver.
//!
//! # What a driver provides
//!
//! A display driver builds a [`ModeConfig`] out of its own [`Crtc`], [`Plane`],
//! [`Encoder`] and [`Connector`] objects plus an [`AtomicDriver`], and returns it
//! from [`DrmDriver::mode_config`]. Everything user space can ask about those
//! objects — resources, properties, framebuffers, blobs, commits, events — is
//! answered from here without the driver being consulted again.
//!
//! A driver whose hardware was programmed before the DRM device existed — a boot
//! console, typically — lists those CRTCs in [`ModeConfigDesc::initial`]. The
//! committed state then describes the pipeline that is already running, so a
//! client that queries before committing anything is told the truth. What cannot
//! be listed there, and why, is on [`InitialCrtc`].
//!
//! [`DrmDriver::mode_config`]: crate::device::DrmDriver::mode_config

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};

use ostd::sync::{Mutex, SpinLock};

use crate::{
    error::{DrmError, DrmResult},
    file::FileId,
    gem::GemObject,
    uapi::{fourcc, mode, mode::ModeInfo},
};

pub mod atomic;
pub mod handlers;
pub mod legacy;
pub mod property;
#[cfg(ktest)]
mod test;
pub mod vblank;

pub use atomic::{AtomicDriver, AtomicRequest, AtomicState, CommitFlags, CrtcState, PlaneState};
pub use handlers::KMS_IOCTLS;
pub use property::{Properties, Property, PropertyAssignment, PropertyKind, StandardProperty};
pub use vblank::{FlipTiming, HardwareObservation, Vblank};

/// An object ID in the KMS ID space.
///
/// A single ID space covers CRTCs, connectors, encoders, planes, framebuffers,
/// properties and blobs; IDs are never reused across types while an object lives.
/// Zero is not a valid ID, which is what lets user space spell "no object" as a
/// zero in `drm_mode_set_plane::fb_id` and friends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u32);

impl ObjectId {
    /// The absent object.
    pub const NONE: Self = Self(0);

    /// Returns the raw ID as user space sees it.
    pub fn get(self) -> u32 {
        self.0
    }

    /// Returns whether this is the absent object.
    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Interprets a raw ID from user space, mapping zero to `None`.
    pub fn from_raw(id: u32) -> Option<Self> {
        (id != 0).then_some(Self(id))
    }
}

/// The KMS object ID allocator of one device.
///
/// A driver takes one of these, numbers its objects from it, and hands it to
/// [`ModeConfig::new`], which keeps allocating from it for the properties,
/// framebuffers and blobs created later. Sharing one allocator is what makes the
/// ID space single, as user space requires: a property ID and a CRTC ID can be
/// compared for equality and must never collide.
///
/// IDs are never reused. Linux's `drm_mode_object_add` uses an `idr` that does
/// reuse them, but a stale ID that suddenly names a different object is a far
/// worse failure than running out after four billion allocations.
#[derive(Debug)]
pub struct ObjectIds {
    next: AtomicU32,
}

impl Default for ObjectIds {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectIds {
    /// Creates an allocator whose first ID is 1.
    pub fn new() -> Self {
        Self {
            next: AtomicU32::new(1),
        }
    }

    /// Allocates the next ID.
    ///
    /// Used while a driver builds its fixed display objects. User-controlled
    /// allocations must use [`Self::try_allocate`] and propagate exhaustion.
    ///
    /// # Panics
    ///
    /// Panics if the ID space is exhausted during driver initialization.
    pub fn allocate(&self) -> ObjectId {
        self.try_allocate()
            .expect("the KMS object ID space is exhausted")
    }

    /// Allocates an ID without reusing a previous ID after exhaustion.
    ///
    /// Returns [`DrmError::NoSpace`] once all nonzero IDs have been issued.
    pub fn try_allocate(&self) -> DrmResult<ObjectId> {
        self.next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                (next != 0).then(|| next.wrapping_add(1))
            })
            .map(ObjectId)
            .map_err(|_| DrmError::NoSpace)
    }
}

/// What kind of thing an [`ObjectId`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    /// A CRTC.
    Crtc,
    /// A plane.
    Plane,
    /// An encoder.
    Encoder,
    /// A connector.
    Connector,
    /// A framebuffer.
    Framebuffer,
    /// A property.
    Property,
    /// A property blob.
    Blob,
}

impl ObjectKind {
    /// Returns the `DRM_MODE_OBJECT_*` code user space knows this kind by.
    pub fn to_uapi(self) -> u32 {
        match self {
            Self::Crtc => mode::object_type::CRTC,
            Self::Plane => mode::object_type::PLANE,
            Self::Encoder => mode::object_type::ENCODER,
            Self::Connector => mode::object_type::CONNECTOR,
            Self::Framebuffer => mode::object_type::FB,
            Self::Property => mode::object_type::PROPERTY,
            Self::Blob => mode::object_type::BLOB,
        }
    }

    /// Interprets a `DRM_MODE_OBJECT_*` code.
    pub fn from_uapi(object_type: u32) -> Option<Self> {
        Some(match object_type {
            mode::object_type::CRTC => Self::Crtc,
            mode::object_type::PLANE => Self::Plane,
            mode::object_type::ENCODER => Self::Encoder,
            mode::object_type::CONNECTOR => Self::Connector,
            mode::object_type::FB => Self::Framebuffer,
            mode::object_type::PROPERTY => Self::Property,
            mode::object_type::BLOB => Self::Blob,
            _ => return None,
        })
    }
}

/// A display controller: the thing that reads a framebuffer and produces timings.
pub trait Crtc: Send + Sync + core::fmt::Debug {
    /// Returns this CRTC's object ID.
    fn id(&self) -> ObjectId;

    /// Returns this CRTC's index, which is its position in the device's CRTC list.
    ///
    /// Plane and encoder `possible_crtcs` bitmasks are indexed by this, not by
    /// object ID.
    fn index(&self) -> u32;

    /// Returns the size of the gamma ramp, or zero if unsupported.
    fn gamma_size(&self) -> u32 {
        0
    }

    /// Returns the primary plane, which every CRTC must have.
    fn primary_plane(&self) -> Arc<dyn Plane>;

    /// Returns the cursor plane, if this CRTC has one.
    fn cursor_plane(&self) -> Option<Arc<dyn Plane>> {
        None
    }
}

/// The role a plane plays, which decides whether legacy clients can see it.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_plane.h#L727>.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaneType {
    /// The plane that legacy `SETCRTC` implicitly programs.
    Primary,
    /// The plane that legacy `MODE_CURSOR` programs.
    Cursor,
    /// An additional plane, visible to legacy clients too.
    Overlay,
}

impl PlaneType {
    /// Returns the value of the `type` property, as `drm_plane_type` numbers it.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_plane.h#L727>.
    pub fn to_uapi(self) -> u64 {
        match self {
            Self::Overlay => 0,
            Self::Primary => 1,
            Self::Cursor => 2,
        }
    }
}

/// A scanout layer within a CRTC.
pub trait Plane: Send + Sync + core::fmt::Debug {
    /// Returns this plane's object ID.
    fn id(&self) -> ObjectId;

    /// Returns this plane's role.
    fn plane_type(&self) -> PlaneType;

    /// Returns a bitmask of CRTC indices this plane can attach to.
    fn possible_crtcs(&self) -> u32;

    /// Returns the FourCC pixel formats this plane accepts.
    fn formats(&self) -> &[u32];

    /// Returns the format modifiers this plane accepts.
    ///
    /// GBM asks for these when `DRM_CAP_ADDFB2_MODIFIERS` is advertised. A driver
    /// that only handles linear buffers should report just `DRM_FORMAT_MOD_LINEAR`
    /// rather than an empty list, because an empty list means "modifiers
    /// unsupported" and pushes Mesa onto a different path.
    fn modifiers(&self) -> &[u64] {
        &[fourcc::MOD_LINEAR]
    }

    /// Returns whether this plane can scale, i.e. whether the source and
    /// destination rectangles of a commit may differ in size.
    ///
    /// The core rejects a scaling request for a plane that says no, because the
    /// alternative is a driver silently ignoring the destination size.
    fn can_scale(&self) -> bool {
        false
    }
}

/// Whether a sink is attached to a connector.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_connector.h#L45>.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    /// A sink is present.
    Connected = 1,
    /// No sink is present.
    Disconnected = 2,
    /// Detection is not possible; user space should assume a sink may be there.
    Unknown = 3,
}

/// A signal encoder between a CRTC and a connector.
pub trait Encoder: Send + Sync + core::fmt::Debug {
    /// Returns this encoder's object ID.
    fn id(&self) -> ObjectId;

    /// Returns the `DRM_MODE_ENCODER_*` type.
    fn encoder_type(&self) -> u32;

    /// Returns a bitmask of CRTC indices this encoder can be driven by.
    fn possible_crtcs(&self) -> u32;

    /// Returns a bitmask of encoders that may be cloned with this one.
    fn possible_clones(&self) -> u32 {
        0
    }
}

/// A physical output.
pub trait Connector: Send + Sync + core::fmt::Debug {
    /// Returns this connector's object ID.
    fn id(&self) -> ObjectId;

    /// Returns the `DRM_MODE_CONNECTOR_*` type.
    fn connector_type(&self) -> u32;

    /// Returns the per-type connector number.
    fn connector_type_id(&self) -> u32;

    /// Detects whether a sink is attached.
    fn detect(&self) -> ConnectionStatus;

    /// Returns the modes this connector supports, preferred mode first.
    ///
    /// A connector reporting no modes is treated by user space as unusable even
    /// when [`detect`](Self::detect) says connected, so a fixed panel must report
    /// its native mode here rather than relying on EDID.
    fn modes(&self) -> Vec<ModeInfo>;

    /// Returns the physical size of the sink in millimetres.
    fn physical_size_mm(&self) -> (u32, u32) {
        (0, 0)
    }

    /// Returns the subpixel order, one of the `DRM_MODE_SUBPIXEL_*` values.
    fn subpixel_order(&self) -> u32 {
        mode::subpixel::UNKNOWN
    }

    /// Returns the encoders that can drive this connector.
    fn encoders(&self) -> Vec<ObjectId>;

    /// Returns the raw EDID blob, if one is available.
    fn edid(&self) -> Option<Vec<u8>> {
        None
    }
}

/// A framebuffer: a pixel format plus the buffer objects holding the pixels.
#[derive(Debug)]
pub struct Framebuffer {
    /// This framebuffer's object ID.
    pub id: ObjectId,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// FourCC pixel format.
    pub pixel_format: u32,
    /// Format modifier, uniform across planes.
    pub modifier: u64,
    /// Backing objects, one per plane; `None` for unused planes.
    pub objects: [Option<Arc<dyn GemObject>>; 4],
    /// Stride in bytes per plane.
    pub pitches: [u32; 4],
    /// Byte offset into each object.
    pub offsets: [u32; 4],
    /// The file that created this framebuffer.
    ///
    /// `MODE_GETRESOURCES` reports only the caller's own framebuffers and `RMFB`
    /// only removes them, exactly as Linux's per-file `fbs` list does. Sharing a
    /// framebuffer between clients goes through PRIME, not through guessing IDs.
    pub owner: FileId,
}

/// The source of a property blob's initial owning reference.
///
/// A state or reader can retain the blob after its creator releases ownership.
/// Such blobs remain resolvable by ID through a weak registry entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobOwner {
    /// Created by the driver. Lives as long as the device: `IN_FORMATS` and
    /// `EDID` describe hardware that does not change.
    Device,
    /// Created by a client through `CREATEPROPBLOB`. Its owning reference is
    /// released when that client destroys it or closes its file.
    File(FileId),
    /// Created by the core for a legacy mode request. Initial ownership is
    /// collected after commit; request and state references determine lifetime.
    Mode,
}

/// An opaque blob of data user space can attach to a property.
///
/// Blobs carry modes (`MODE_ID`), EDID, and plane format tables.
#[derive(Debug)]
pub struct PropertyBlob {
    /// This blob's object ID.
    pub id: ObjectId,
    /// The blob's contents.
    pub data: Vec<u8>,
    /// The source of the blob's initial owning reference.
    pub owner: BlobOwner,
}

// Owned entries retain device/file blobs until their owner releases them.
// Retained entries preserve ID lookup without extending a state/reader lifetime.
#[derive(Debug)]
enum BlobEntry {
    Owned(Arc<PropertyBlob>),
    Retained(Weak<PropertyBlob>),
}

impl BlobEntry {
    fn lookup(&self) -> Option<Arc<PropertyBlob>> {
        match self {
            Self::Owned(blob) => Some(blob.clone()),
            Self::Retained(blob) => blob.upgrade(),
        }
    }

    fn is_alive(&self) -> bool {
        match self {
            Self::Owned(_) => true,
            Self::Retained(blob) => blob.strong_count() != 0,
        }
    }

    fn is_owned_by(&self, owner: BlobOwner) -> bool {
        matches!(self, Self::Owned(blob) if blob.owner == owner)
    }

    // Return the owning reference for destruction after releasing the table lock.
    fn release_owner(&mut self) -> Option<Arc<PropertyBlob>> {
        let Self::Owned(blob) = self else {
            return None;
        };
        let retained = Self::Retained(Arc::downgrade(blob));
        let Self::Owned(blob) = core::mem::replace(self, retained) else {
            unreachable!();
        };
        Some(blob)
    }
}

/// A CRTC that is already scanning out when the device is registered.
///
/// Only the routing and the mode. A framebuffer is deliberately absent:
/// [`Framebuffer`] is owned by the file that created it via `ADDFB2`, and there
/// is no owner for "a buffer nobody created". Binding the primary plane without
/// a framebuffer is refused by the same check a commit would hit. The leftover
/// the core can represent honestly — an active CRTC with a mode and connectors
/// — is enough for `DPMS` to read `On`.
#[derive(Clone, Debug)]
pub struct InitialCrtc {
    /// The CRTC that boot already programmed.
    pub crtc: ObjectId,
    /// The mode that CRTC is outputting.
    pub mode: ModeInfo,
    /// The connectors that CRTC is driving.
    pub connectors: Vec<ObjectId>,
}

/// What a driver hands to [`ModeConfig::new`].
///
/// A struct rather than a long argument list, because the four object vectors
/// are easy to swap by accident and the compiler cannot tell them apart.
#[derive(Debug)]
pub struct ModeConfigDesc {
    /// The allocator the objects below were numbered from.
    pub ids: ObjectIds,
    /// Minimum framebuffer dimensions.
    pub min_size: (u32, u32),
    /// Maximum framebuffer dimensions.
    pub max_size: (u32, u32),
    /// The device's CRTCs, in index order.
    pub crtcs: Vec<Arc<dyn Crtc>>,
    /// The device's planes.
    pub planes: Vec<Arc<dyn Plane>>,
    /// The device's encoders.
    pub encoders: Vec<Arc<dyn Encoder>>,
    /// The device's connectors.
    pub connectors: Vec<Arc<dyn Connector>>,
    /// The hardware side of mode setting.
    pub atomic: Arc<dyn AtomicDriver>,
    /// CRTCs that boot already programmed, if any.
    ///
    /// Empty for a device whose hardware starts dark. A non-empty list is
    /// installed as the committed state after the object-model checks, so a
    /// client querying before any commit sees the pipeline that is running
    /// rather than an inactive CRTC. The driver is not consulted to program
    /// anything: the hardware is already doing this.
    pub initial: Vec<InitialCrtc>,
}

/// The set of mode objects a device exposes, plus its atomic driver.
///
/// This owns everything with a KMS object ID: the driver's fixed objects, the
/// properties the core derives from them, the framebuffers and blobs clients
/// create, and the committed display state.
#[derive(Debug)]
pub struct ModeConfig {
    ids: ObjectIds,
    min_size: (u32, u32),
    max_size: (u32, u32),
    crtcs: Vec<Arc<dyn Crtc>>,
    planes: Vec<Arc<dyn Plane>>,
    encoders: Vec<Arc<dyn Encoder>>,
    connectors: Vec<Arc<dyn Connector>>,
    atomic: Arc<dyn AtomicDriver>,
    properties: Properties,
    /// The `IN_FORMATS` blob of each plane, built once at construction because
    /// the plane's format list cannot change.
    plane_in_formats: BTreeMap<ObjectId, ObjectId>,
    /// The `EDID` blob of each connector that has one.
    connector_edid: BTreeMap<ObjectId, ObjectId>,
    framebuffers: SpinLock<BTreeMap<ObjectId, Arc<Framebuffer>>>,
    blobs: SpinLock<BTreeMap<ObjectId, BlobEntry>>,
    /// The committed display state.
    ///
    /// Held behind an `Arc` so that a reader gets a consistent snapshot without
    /// holding the lock: `GETCONNECTOR` reports several properties derived from
    /// the state, and reporting halves of two different states would be worse
    /// than reporting a stale one.
    state: SpinLock<Arc<AtomicState>>,
    /// Serializes commits, so that two clients cannot each validate against a
    /// state the other is about to replace.
    ///
    /// A sleepable lock rather than a spin lock: a driver's commit may have to
    /// wait for hardware, and this is only ever taken from an ioctl.
    commit_lock: Mutex<()>,
    /// Per-CRTC vblank sequence numbers and parked flip events.
    vblank: Vblank,
}

impl ModeConfig {
    /// Builds the mode configuration of a device.
    ///
    /// The consistency checks here are the ones whose violation user space would
    /// see as a nonsensical device rather than as an error: duplicate object IDs,
    /// a CRTC whose primary plane is not in the plane list, a `possible_crtcs`
    /// bit naming a CRTC that does not exist. They are driver bugs, so they are
    /// reported to the driver at registration rather than to a client later.
    pub fn new(desc: ModeConfigDesc) -> DrmResult<Self> {
        let ModeConfigDesc {
            ids,
            min_size,
            max_size,
            crtcs,
            planes,
            encoders,
            connectors,
            atomic,
            initial,
        } = desc;

        check_object_ids(&crtcs, &planes, &encoders, &connectors)?;

        let crtc_mask = crtc_mask(crtcs.len());
        for (index, crtc) in crtcs.iter().enumerate() {
            if crtc.index() != index as u32 {
                return Err(DrmError::InvalidArgument);
            }
            let primary = crtc.primary_plane();
            if primary.plane_type() != PlaneType::Primary
                || !planes.iter().any(|plane| plane.id() == primary.id())
            {
                return Err(DrmError::InvalidArgument);
            }
        }
        for plane in &planes {
            if plane.possible_crtcs() & !crtc_mask != 0 || plane.possible_crtcs() == 0 {
                return Err(DrmError::InvalidArgument);
            }
            if plane.formats().is_empty() {
                return Err(DrmError::InvalidArgument);
            }
        }
        for encoder in &encoders {
            if encoder.possible_crtcs() & !crtc_mask != 0 {
                return Err(DrmError::InvalidArgument);
            }
        }
        for connector in &connectors {
            for encoder in connector.encoders() {
                if !encoders.iter().any(|candidate| candidate.id() == encoder) {
                    return Err(DrmError::InvalidArgument);
                }
            }
        }

        let properties = Properties::new(&ids);
        let state = AtomicState::initial(&crtcs, &planes);
        let vblank = Vblank::new(crtcs.iter().map(|crtc| crtc.id()));

        let mut config = Self {
            ids,
            min_size,
            max_size,
            crtcs,
            planes,
            encoders,
            connectors,
            atomic,
            properties,
            plane_in_formats: BTreeMap::new(),
            connector_edid: BTreeMap::new(),
            framebuffers: SpinLock::new(BTreeMap::new()),
            blobs: SpinLock::new(BTreeMap::new()),
            state: SpinLock::new(Arc::new(state)),
            commit_lock: Mutex::new(()),
            vblank,
        };

        config.create_immutable_blobs()?;
        config.install_initial(initial)?;
        Ok(config)
    }

    /// Creates the blobs whose contents are fixed for the device's lifetime.
    fn create_immutable_blobs(&mut self) -> DrmResult<()> {
        let mut in_formats = BTreeMap::new();
        for plane in &self.planes {
            let blob = property::in_formats_blob(plane.formats(), plane.modifiers())?;
            in_formats.insert(plane.id(), self.insert_blob(blob, BlobOwner::Device)?.id);
        }

        let mut edids = BTreeMap::new();
        for connector in &self.connectors {
            if let Some(edid) = connector.edid() {
                edids.insert(
                    connector.id(),
                    self.insert_blob(edid, BlobOwner::Device)?.id,
                );
            }
        }

        self.plane_in_formats = in_formats;
        self.connector_edid = edids;
        Ok(())
    }

    /// Returns the minimum framebuffer dimensions.
    pub fn min_size(&self) -> (u32, u32) {
        self.min_size
    }

    /// Returns the maximum framebuffer dimensions.
    pub fn max_size(&self) -> (u32, u32) {
        self.max_size
    }

    /// Returns the device's CRTCs, in index order.
    pub fn crtcs(&self) -> &[Arc<dyn Crtc>] {
        &self.crtcs
    }

    /// Returns the shared vblank clock of this device.
    ///
    /// A driver that completes flips at vblank clones this and calls
    /// [`Vblank::tick`] from its bottom half. The software driver never does.
    pub fn vblank(&self) -> Vblank {
        self.vblank.clone()
    }

    /// Returns the device's planes.
    pub fn planes(&self) -> &[Arc<dyn Plane>] {
        &self.planes
    }

    /// Returns the device's encoders.
    pub fn encoders(&self) -> &[Arc<dyn Encoder>] {
        &self.encoders
    }

    /// Returns the device's connectors.
    pub fn connectors(&self) -> &[Arc<dyn Connector>] {
        &self.connectors
    }

    /// Returns the driver's mode-setting hooks.
    pub fn atomic_driver(&self) -> &Arc<dyn AtomicDriver> {
        &self.atomic
    }

    /// Returns the device's property table.
    pub fn properties(&self) -> &Properties {
        &self.properties
    }

    /// Returns the currently committed display state.
    pub fn state(&self) -> Arc<AtomicState> {
        self.state.lock().clone()
    }

    /// Looks up a CRTC by object ID.
    pub fn crtc(&self, id: ObjectId) -> DrmResult<&Arc<dyn Crtc>> {
        self.crtcs
            .iter()
            .find(|crtc| crtc.id() == id)
            .ok_or(DrmError::NotFound)
    }

    /// Looks up a plane by object ID.
    pub fn plane(&self, id: ObjectId) -> DrmResult<&Arc<dyn Plane>> {
        self.planes
            .iter()
            .find(|plane| plane.id() == id)
            .ok_or(DrmError::NotFound)
    }

    /// Looks up an encoder by object ID.
    pub fn encoder(&self, id: ObjectId) -> DrmResult<&Arc<dyn Encoder>> {
        self.encoders
            .iter()
            .find(|encoder| encoder.id() == id)
            .ok_or(DrmError::NotFound)
    }

    /// Looks up a connector by object ID.
    pub fn connector(&self, id: ObjectId) -> DrmResult<&Arc<dyn Connector>> {
        self.connectors
            .iter()
            .find(|connector| connector.id() == id)
            .ok_or(DrmError::NotFound)
    }

    /// Returns the index of a CRTC, which is what `possible_crtcs` masks use.
    pub fn crtc_index(&self, id: ObjectId) -> DrmResult<u32> {
        Ok(self.crtc(id)?.index())
    }

    /// Returns what kind of object an ID names, or `None` if it names nothing.
    pub fn object_kind(&self, id: ObjectId) -> Option<ObjectKind> {
        if id.is_none() {
            return None;
        }
        if self.crtcs.iter().any(|object| object.id() == id) {
            return Some(ObjectKind::Crtc);
        }
        if self.planes.iter().any(|object| object.id() == id) {
            return Some(ObjectKind::Plane);
        }
        if self.encoders.iter().any(|object| object.id() == id) {
            return Some(ObjectKind::Encoder);
        }
        if self.connectors.iter().any(|object| object.id() == id) {
            return Some(ObjectKind::Connector);
        }
        if self.framebuffers.lock().contains_key(&id) {
            return Some(ObjectKind::Framebuffer);
        }
        if self.properties.lookup(id).is_some() {
            return Some(ObjectKind::Property);
        }
        if self.blobs.lock().get(&id).is_some_and(BlobEntry::is_alive) {
            return Some(ObjectKind::Blob);
        }
        None
    }

    /// Returns the IDs of every object of one kind.
    ///
    /// Framebuffers are deliberately not covered: they belong to the file that
    /// created them, so enumerating them needs to know whose they are. Use
    /// [`framebuffer_ids_of`](Self::framebuffer_ids_of).
    pub fn object_ids(&self, kind: ObjectKind) -> Vec<u32> {
        match kind {
            ObjectKind::Crtc => self.crtcs.iter().map(|object| object.id().get()).collect(),
            ObjectKind::Plane => self.planes.iter().map(|object| object.id().get()).collect(),
            ObjectKind::Encoder => self
                .encoders
                .iter()
                .map(|object| object.id().get())
                .collect(),
            ObjectKind::Connector => self
                .connectors
                .iter()
                .map(|object| object.id().get())
                .collect(),
            ObjectKind::Framebuffer => self.framebuffers.lock().keys().map(|id| id.get()).collect(),
            ObjectKind::Property => self.properties.ids(),
            ObjectKind::Blob => self
                .blobs
                .lock()
                .iter()
                .filter(|(_, entry)| entry.is_alive())
                .map(|(id, _)| id.get())
                .collect(),
        }
    }

    /// Returns the `IN_FORMATS` blob of a plane.
    pub fn plane_in_formats(&self, plane: ObjectId) -> ObjectId {
        self.plane_in_formats
            .get(&plane)
            .copied()
            .unwrap_or(ObjectId::NONE)
    }

    /// Returns the `EDID` blob of a connector, or [`ObjectId::NONE`].
    pub fn connector_edid(&self, connector: ObjectId) -> ObjectId {
        self.connector_edid
            .get(&connector)
            .copied()
            .unwrap_or(ObjectId::NONE)
    }

    /// Registers a framebuffer created by `ADDFB`/`ADDFB2`.
    ///
    /// `framebuffer.id` is ignored and replaced by a freshly allocated ID.
    pub fn add_framebuffer(&self, mut framebuffer: Framebuffer) -> DrmResult<ObjectId> {
        self.check_framebuffer(&framebuffer)?;

        let id = self.ids.try_allocate()?;
        framebuffer.id = id;
        self.framebuffers.lock().insert(id, Arc::new(framebuffer));
        Ok(id)
    }

    /// Validates a framebuffer against the device's limits and its own geometry.
    ///
    /// The size check is the one that matters: a framebuffer whose last line runs
    /// past the end of its buffer object would make the display engine read
    /// whatever memory follows, so it is refused here rather than at scanout.
    fn check_framebuffer(&self, framebuffer: &Framebuffer) -> DrmResult<()> {
        if framebuffer.width < self.min_size.0
            || framebuffer.height < self.min_size.1
            || framebuffer.width > self.max_size.0
            || framebuffer.height > self.max_size.1
        {
            return Err(DrmError::InvalidArgument);
        }

        let bytes_per_pixel =
            fourcc::bytes_per_pixel(framebuffer.pixel_format).ok_or(DrmError::InvalidArgument)?;

        // Only single-plane formats are accepted, so every plane after the first
        // must be empty rather than merely unused.
        if framebuffer.objects[1..].iter().any(Option::is_some) {
            return Err(DrmError::InvalidArgument);
        }
        let object = framebuffer.objects[0]
            .as_ref()
            .ok_or(DrmError::InvalidArgument)?;

        let minimum_pitch = framebuffer
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(DrmError::InvalidArgument)?;
        if framebuffer.pitches[0] < minimum_pitch {
            return Err(DrmError::InvalidArgument);
        }

        let end = u64::from(framebuffer.pitches[0])
            .checked_mul(u64::from(framebuffer.height))
            .and_then(|size| size.checked_add(u64::from(framebuffer.offsets[0])))
            .ok_or(DrmError::InvalidArgument)?;
        if end > object.size() as u64 {
            return Err(DrmError::InvalidArgument);
        }

        Ok(())
    }

    /// Looks up a framebuffer by object ID.
    ///
    /// Any client may resolve any framebuffer ID, as Linux's
    /// `drm_framebuffer_lookup` does; ownership only governs enumeration and
    /// removal.
    pub fn framebuffer(&self, id: ObjectId) -> DrmResult<Arc<Framebuffer>> {
        self.framebuffers
            .lock()
            .get(&id)
            .cloned()
            .ok_or(DrmError::NotFound)
    }

    /// Removes a framebuffer on behalf of the file that created it.
    ///
    /// A framebuffer another client owns is reported as absent rather than as
    /// forbidden, which is what Linux does by looking the ID up in the caller's
    /// own list.
    ///
    /// The framebuffer may still be scanned out; the state that references it
    /// holds its own reference, so the memory survives until the next commit
    /// replaces it. Linux behaves the same way, and it is what lets user space
    /// destroy a framebuffer immediately after flipping away from it.
    pub fn remove_framebuffer(&self, id: ObjectId, owner: FileId) -> DrmResult<()> {
        let removed = {
            let mut framebuffers = self.framebuffers.lock();
            if !framebuffers
                .get(&id)
                .is_some_and(|framebuffer| framebuffer.owner == owner)
            {
                return Err(DrmError::NotFound);
            }
            framebuffers.remove(&id)
        };
        // A last GEM reference may release DMA mappings or driver resources.
        drop(removed);
        Ok(())
    }

    /// Returns the IDs of the framebuffers a file created.
    pub fn framebuffer_ids_of(&self, owner: FileId) -> Vec<u32> {
        self.framebuffers
            .lock()
            .values()
            .filter(|framebuffer| framebuffer.owner == owner)
            .map(|framebuffer| framebuffer.id.get())
            .collect()
    }

    /// Drops everything a closing file owns.
    ///
    /// Called from the file's destructor. Linux does the same in
    /// `drm_fb_release`: a compositor that crashed must not leave its
    /// framebuffers and mode blobs behind for the rest of the boot.
    pub fn release_file(&self, owner: FileId) {
        self.vblank.drop_file(owner);
        self.vblank.retire_completed();
        let removed = {
            let mut framebuffers = self.framebuffers.lock();
            let owned: Vec<ObjectId> = framebuffers
                .iter()
                .filter(|(_, framebuffer)| framebuffer.owner == owner)
                .map(|(id, _)| *id)
                .collect();
            owned
                .into_iter()
                .filter_map(|id| framebuffers.remove(&id))
                .collect::<Vec<_>>()
        };
        drop(removed);

        let retired = {
            let mut blobs = self.blobs.lock();
            blobs
                .values_mut()
                .filter(|entry| entry.is_owned_by(BlobOwner::File(owner)))
                .filter_map(BlobEntry::release_owner)
                .collect::<Vec<_>>()
        };
        drop(retired);
    }

    /// Creates a property blob.
    pub fn create_blob(&self, data: Vec<u8>, owner: BlobOwner) -> DrmResult<ObjectId> {
        if data.is_empty() {
            return Err(DrmError::InvalidArgument);
        }
        Ok(self.insert_blob(data, owner)?.id)
    }

    fn insert_blob(&self, data: Vec<u8>, owner: BlobOwner) -> DrmResult<Arc<PropertyBlob>> {
        let id = self.ids.try_allocate()?;
        let blob = Arc::new(PropertyBlob { id, data, owner });
        let mut blobs = self.blobs.lock();
        blobs.retain(|_, entry| entry.is_alive());
        blobs.insert(id, BlobEntry::Owned(blob.clone()));
        Ok(blob)
    }

    /// Looks up a property blob by object ID.
    pub fn blob(&self, id: ObjectId) -> DrmResult<Arc<PropertyBlob>> {
        self.blobs
            .lock()
            .get(&id)
            .and_then(BlobEntry::lookup)
            .ok_or(DrmError::NotFound)
    }

    /// Destroys a property blob on behalf of the file that created it.
    ///
    /// Divergence: Linux reports `EPERM` when a client destroys a blob it does
    /// not own. [`DrmError`] has no `EPERM` variant, so this is `EACCES`; nothing
    /// in user space distinguishes the two here, and adding a variant would make
    /// every existing mapping table ambiguous.
    pub fn destroy_blob(&self, id: ObjectId, owner: FileId) -> DrmResult<()> {
        let retired = {
            let mut blobs = self.blobs.lock();
            let entry = blobs
                .get_mut(&id)
                .filter(|entry| entry.is_alive())
                .ok_or(DrmError::NotFound)?;
            if !entry.is_owned_by(BlobOwner::File(owner)) {
                return Err(DrmError::PermissionDenied);
            }
            entry.release_owner()
        };
        drop(retired);
        Ok(())
    }

    /// Releases initial ownership of core-created mode blobs.
    ///
    /// Legacy requests and candidate/committed states hold their own references.
    /// The registry must not retain a failed or superseded request indefinitely.
    pub(super) fn collect_mode_blobs(&self) {
        let retired = {
            let mut blobs = self.blobs.lock();
            blobs.retain(|_, entry| entry.is_alive());
            blobs
                .values_mut()
                .filter(|entry| entry.is_owned_by(BlobOwner::Mode))
                .filter_map(BlobEntry::release_owner)
                .collect::<Vec<_>>()
        };
        drop(retired);
    }

    /// Returns and increments the vblank sequence number of a CRTC.
    ///
    /// Used only by the Immediate completion path, where the number still
    /// counts commits. A driver that reports [`FlipTiming::AtVblank`] leaves
    /// the counter to [`Vblank::tick`] instead, so the number is a real
    /// vblank count.
    fn next_sequence(&self, crtc: ObjectId) -> u32 {
        self.vblank.next_immediate(crtc)
    }
}

/// Builds a [`ModeInfo`] from a set of timings, deriving the fields that follow
/// from them.
///
/// Every driver has to hand user space its timings in this structure, and the two
/// derived fields are easy to get subtly wrong: `vrefresh` is rounded, not
/// truncated, and the name has to be NUL-terminated because user space prints it.
/// A name longer than `DRM_DISPLAY_MODE_LEN - 1` is truncated rather than refused,
/// since a mode is not worth losing over its label.
///
/// `horizontal` and `vertical` are `(display, sync_start, sync_end, total)`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_modes.c#L1181>
/// (`drm_mode_vrefresh`).
pub fn mode_info(
    name: &str,
    clock_khz: u32,
    horizontal: (u16, u16, u16, u16),
    vertical: (u16, u16, u16, u16),
    flags: u32,
    mode_type: u32,
) -> ModeInfo {
    let (hdisplay, hsync_start, hsync_end, htotal) = horizontal;
    let (vdisplay, vsync_start, vsync_end, vtotal) = vertical;

    let lines = u64::from(htotal) * u64::from(vtotal);
    let vrefresh = (u64::from(clock_khz) * 1000 + lines / 2)
        .checked_div(lines)
        .unwrap_or(0) as u32;

    let mut mode = ModeInfo {
        clock: clock_khz,
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        hskew: 0,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: 0,
        vrefresh,
        flags,
        type_: mode_type,
        name: [0; mode::DISPLAY_MODE_LEN],
    };

    let bytes = name.as_bytes();
    let length = bytes.len().min(mode::DISPLAY_MODE_LEN - 1);
    mode.name[..length].copy_from_slice(&bytes[..length]);
    mode
}

/// Returns a bitmask with one bit set per CRTC index.
fn crtc_mask(count: usize) -> u32 {
    if count >= u32::BITS as usize {
        u32::MAX
    } else {
        (1u32 << count) - 1
    }
}

/// Checks that every object has a distinct, non-zero ID.
///
/// A collision here would make two objects indistinguishable to user space,
/// which shows up as a client programming the wrong pipeline rather than as an
/// error.
fn check_object_ids(
    crtcs: &[Arc<dyn Crtc>],
    planes: &[Arc<dyn Plane>],
    encoders: &[Arc<dyn Encoder>],
    connectors: &[Arc<dyn Connector>],
) -> DrmResult<()> {
    let ids = crtcs
        .iter()
        .map(|object| object.id())
        .chain(planes.iter().map(|object| object.id()))
        .chain(encoders.iter().map(|object| object.id()))
        .chain(connectors.iter().map(|object| object.id()));

    let mut seen = alloc::collections::BTreeSet::new();
    for id in ids {
        if id.is_none() || !seen.insert(id) {
            return Err(DrmError::InvalidArgument);
        }
    }
    Ok(())
}
