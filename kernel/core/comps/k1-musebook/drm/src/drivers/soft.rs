// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! A DRM driver with no hardware behind it.
//!
//! Modelled on Linux's `vkms`. It reports one CRTC with a primary, a cursor and an
//! overlay plane, one encoder, and one always-connected virtual connector, and its
//! commits simply record the state that was asked for. That is enough for
//! `drm_smoke` and `modetest` to exercise the ioctl surface under QEMU.
//!
//! Its driver name is `"aster"`, deliberately distinct from `"spacemit"` and
//! `"pvr"` so that a test accidentally running against the wrong device is
//! obvious rather than confusing.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use crate::{
    device::{DriverFeatures, DriverInfo, DrmDriver, DumbBuffer, DumbCreateArgs, PlatformIdentity},
    error::{DrmError, DrmResult},
    gem::{BufferUse, GemObject, ShmemObject},
    kms::{
        AtomicDriver, AtomicState, ConnectionStatus, Connector, Crtc, Encoder, ModeConfig,
        ModeConfigDesc, ObjectId, ObjectIds, Plane, PlaneType, mode_info,
    },
    uapi::{cap, fourcc, mode},
};

/// The device tree path the software driver claims.
///
/// There is no device tree under QEMU, so this is invented — `libdrm` treats it as
/// an opaque identity. It only has to differ from every other DRM device's, which
/// is what stops `drmFoldDuplicatedDevices` from merging two of them; see
/// `../../ABI.md`, section 1.
const OF_FULLNAME: &str = "/soft/aster-drm";

/// The formats every plane of the software driver accepts.
///
/// The two 32-bit formats every KMS client can be assumed to produce. A shorter
/// list would make `modetest` fail to allocate; a longer one would claim support
/// that has never been exercised.
const FORMATS: &[u32] = &[fourcc::XRGB8888, fourcc::ARGB8888];

/// The modifiers every plane accepts.
///
/// Linear only, but reported explicitly: an empty list means "modifiers
/// unsupported" to Mesa, which is a different thing from "linear only".
const MODIFIERS: &[u64] = &[fourcc::MOD_LINEAR];

/// The virtual driver.
#[derive(Debug)]
pub struct SoftDriver {
    mode_config: Option<ModeConfig>,
}

impl SoftDriver {
    /// Creates the driver with its display pipeline.
    pub fn new() -> Arc<Self> {
        let mode_config = match build_mode_config() {
            Ok(config) => Some(config),
            Err(error) => {
                // The pipeline is built from constants, so this cannot depend on
                // anything a user did; report it and carry on as a device with no
                // display rather than refusing to boot.
                ostd::error!("the software DRM driver has no display: {:?}", error);
                None
            }
        };
        Arc::new(Self { mode_config })
    }
}

/// Builds the software driver's mode objects.
fn build_mode_config() -> DrmResult<ModeConfig> {
    let ids = ObjectIds::new();

    let primary = Arc::new(SoftPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Primary,
    });
    let cursor = Arc::new(SoftPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Cursor,
    });
    let overlay = Arc::new(SoftPlane {
        id: ids.allocate(),
        plane_type: PlaneType::Overlay,
    });
    let crtc = Arc::new(SoftCrtc {
        id: ids.allocate(),
        primary: primary.clone(),
        cursor: cursor.clone(),
    });
    let encoder = Arc::new(SoftEncoder { id: ids.allocate() });
    let connector = Arc::new(SoftConnector {
        id: ids.allocate(),
        encoder: encoder.id(),
    });

    ModeConfig::new(ModeConfigDesc {
        ids,
        min_size: (1, 1),
        // Generous but finite: a bogus `ADDFB2` has to be refused somewhere, and
        // this is the only limit the software driver has.
        max_size: (8192, 8192),
        crtcs: vec![crtc],
        planes: vec![primary, cursor, overlay],
        encoders: vec![encoder],
        connectors: vec![connector],
        atomic: Arc::new(SoftAtomicDriver),
        // The software device has no boot-programmed hardware to describe.
        initial: Vec::new(),
    })
}

impl DrmDriver for SoftDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "aster",
            date: "20260729",
            desc: "Asterinas virtual DRM device",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        // Sync objects are claimed even though `vkms` does not claim them, for
        // the same reason the render node is: this driver exists to be tested
        // against. Nothing about the claim is a promise this driver cannot
        // keep — all twelve ioctls are generic, backed by `SyncObjTable` and
        // CPU-signalled fences, with nothing for a driver to implement. Only
        // claiming them here lets `drm_smoke` exercise the real path, ioctl
        // decode and user-space copies included, which a `#[ktest]` calling
        // handlers directly cannot reach.
        DriverFeatures::MODESET
            | DriverFeatures::GEM
            | DriverFeatures::RENDER
            | DriverFeatures::ATOMIC
            | DriverFeatures::SYNCOBJ
            | DriverFeatures::SYNCOBJ_TIMELINE
    }

    fn unique(&self) -> String {
        // Shaped like a platform device's bus name, which is what `libdrm` expects
        // to find for a non-PCI device. There is no bus to enumerate under QEMU,
        // so the address is fixed.
        String::from("platform:aster:00")
    }

    fn get_cap(&self, capability: u64) -> Option<u64> {
        match capability {
            cap::device::DUMB_BUFFER => Some(1),
            cap::device::DUMB_PREFERRED_DEPTH => Some(24),
            cap::device::DUMB_PREFER_SHADOW => Some(0),
            cap::device::TIMESTAMP_MONOTONIC => Some(1),
            // The planes report an `IN_FORMATS` blob, so a client may ask for a
            // framebuffer with an explicit modifier.
            cap::device::ADDFB2_MODIFIERS => Some(1),
            _ => None,
        }
    }

    fn mode_config(&self) -> Option<&ModeConfig> {
        self.mode_config.as_ref()
    }

    /// Publishes the software device in sysfs so that `drmGetDevices2` finds it.
    ///
    /// Without a sysfs entry, `libdrm` enumerates zero devices and `modetest`
    /// cannot discover the driver through its ioctl interface alone.
    fn platform_identity(&self) -> Option<PlatformIdentity> {
        Some(PlatformIdentity {
            name: "aster-drm".to_string(),
            of_fullname: OF_FULLNAME.to_string(),
            of_compatible: vec!["aster,soft-drm".to_string()],
        })
    }

    /// Allocates a dumb buffer with a 64-byte-aligned pitch.
    ///
    /// The alignment is not needed by anything under QEMU; it is here because
    /// the K1 DPU fetches scanout lines in bursts and every real driver rounds
    /// the pitch up for its hardware. Doing it in the software driver too
    /// keeps `drm_smoke` honest about the one thing user space must not
    /// assume: that `pitch` equals `width * bpp / 8`.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_gem_dma_helper.c#L295>
    /// (`drm_gem_dma_dumb_create_internal`, which rounds to
    /// `pitch_align`).
    fn dumb_create(&self, args: DumbCreateArgs) -> DrmResult<DumbBuffer> {
        // The core has already rejected the requests whose pitch or size
        // overflows 32 bits, so only the rounding can still overflow here.
        let cpp = args.bpp.div_ceil(8);
        let pitch = args
            .width
            .checked_mul(cpp)
            .and_then(|min_pitch| min_pitch.checked_next_multiple_of(PITCH_ALIGN))
            .ok_or(DrmError::InvalidArgument)?;
        let size = u64::from(pitch)
            .checked_mul(u64::from(args.height))
            .and_then(|size| usize::try_from(size).ok())
            .ok_or(DrmError::InvalidArgument)?;

        // `Scanout`, because that is what a dumb buffer is for, and it is what
        // decides that the memory is contiguous and mapped write-combining.
        let object = ShmemObject::new(size, BufferUse::Scanout)?;

        // The size reported back is the object's, i.e. rounded up to a page,
        // as `drm_gem_shmem_dumb_create` reports `PAGE_ALIGN(pitch * height)`.
        // User space passes it straight to `mmap`, which would fail on a
        // length that does not cover whole pages.
        let size = object.size() as u64;

        Ok(DumbBuffer {
            object,
            pitch,
            size,
        })
    }
}

/// The pitch alignment the software driver pretends its hardware needs.
const PITCH_ALIGN: u32 = 64;

/// The software driver's single CRTC.
#[derive(Debug)]
struct SoftCrtc {
    id: ObjectId,
    primary: Arc<SoftPlane>,
    cursor: Arc<SoftPlane>,
}

impl Crtc for SoftCrtc {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn index(&self) -> u32 {
        0
    }

    fn primary_plane(&self) -> Arc<dyn Plane> {
        self.primary.clone()
    }

    fn cursor_plane(&self) -> Option<Arc<dyn Plane>> {
        Some(self.cursor.clone())
    }
}

/// One of the software driver's planes.
#[derive(Debug)]
struct SoftPlane {
    id: ObjectId,
    plane_type: PlaneType,
}

impl Plane for SoftPlane {
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
        FORMATS
    }

    fn modifiers(&self) -> &[u64] {
        MODIFIERS
    }
}

/// The software driver's single encoder.
#[derive(Debug)]
struct SoftEncoder {
    id: ObjectId,
}

impl Encoder for SoftEncoder {
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

/// The software driver's single connector, which is always connected.
#[derive(Debug)]
struct SoftConnector {
    id: ObjectId,
    encoder: ObjectId,
}

impl Connector for SoftConnector {
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

    fn modes(&self) -> Vec<mode::ModeInfo> {
        // Two modes rather than one, so that `modetest` has something to choose
        // between and the mode-list handling is exercised. The timings are the
        // standard CEA/VESA ones for these resolutions, because user space
        // occasionally recognizes them by their numbers.
        vec![
            mode_info(
                "1920x1080",
                148_500,
                (1920, 2008, 2052, 2200),
                (1080, 1084, 1089, 1125),
                mode::mode_flag::PHSYNC | mode::mode_flag::PVSYNC,
                mode::mode_type::DRIVER | mode::mode_type::PREFERRED,
            ),
            mode_info(
                "1024x768",
                65_000,
                (1024, 1048, 1184, 1344),
                (768, 771, 777, 806),
                mode::mode_flag::NHSYNC | mode::mode_flag::NVSYNC,
                mode::mode_type::DRIVER,
            ),
        ]
    }

    fn subpixel_order(&self) -> u32 {
        mode::subpixel::UNKNOWN
    }

    fn encoders(&self) -> Vec<ObjectId> {
        vec![self.encoder]
    }
}

/// The software driver's commit path, which has nothing to program.
///
/// Logs distinguish an accepted state from a request that never reached the
/// driver; this software implementation has no visible scanout.
#[derive(Debug)]
struct SoftAtomicDriver;

impl AtomicDriver for SoftAtomicDriver {
    fn atomic_check(&self, state: &AtomicState) -> DrmResult<()> {
        // Nothing to add: with no hardware there is no bandwidth to run out of, and
        // everything that follows from the objects themselves has already been
        // checked by the core.
        let _ = state;
        Ok(())
    }

    fn atomic_commit(&self, state: &AtomicState) -> DrmResult<()> {
        for crtc in &state.crtcs {
            ostd::debug!(
                "soft: crtc {} {} {}x{}",
                crtc.crtc.get(),
                if crtc.active { "active" } else { "off" },
                crtc.mode.map_or(0, |mode| mode.hdisplay),
                crtc.mode.map_or(0, |mode| mode.vdisplay)
            );
        }
        for plane in &state.planes {
            ostd::debug!(
                "soft: plane {} crtc {} fb {}",
                plane.plane.get(),
                plane.crtc.unwrap_or(ObjectId::NONE).get(),
                plane
                    .framebuffer
                    .as_ref()
                    .map_or(0, |framebuffer| framebuffer.id.get())
            );
        }
        Ok(())
    }
}

/// Registers the virtual driver.
///
/// Called by `kernel/core/src/device/dri.rs` when no real display driver claimed a
/// device, so that `/dev/dri` is never empty in QEMU. It cannot be called from
/// this crate's own component init: component initialization runs in dependency
/// order, so `aster-framebuffer` — which registers the K1 pipeline — has not had
/// its turn yet at that point, and the fallback would always win.
pub fn register() -> DrmResult<()> {
    crate::register_device(SoftDriver::new()).map(|_| ())
}
