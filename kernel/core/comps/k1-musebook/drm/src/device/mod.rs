// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! DRM devices and the driver interface.
//!
//! A [`DrmDevice`] is the kernel-side object behind a pair of character device
//! nodes. A [`DrmDriver`] is what a hardware driver implements to back one.
//!
//! The driver supplies mode-setting hooks, platform identity, and PRIME sharing.

use alloc::{format, string::String, sync::Arc, vec::Vec};

use bitflags::bitflags;
use ostd::sync::SpinLock;

use crate::{
    error::{DrmError, DrmResult},
    file::{CallerIdentity, DrmFile, Privilege},
    gem::{GemDevice, GemObject},
    ioctl::{self, CallerContext, IoctlDesc},
    kms::{ModeConfig, ObjectKind},
    prime::DmaBuf,
    uapi::{cap, mode},
};

/// Which of a device's nodes a file was opened through.
///
/// This is a capability, not a label: render nodes exist precisely so that
/// unprivileged clients can submit work without gaining the ability to reprogram
/// the display. Every ioctl handler must respect it.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/Documentation/gpu/drm-uapi.rst>.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrmMinor {
    /// `/dev/dri/cardN`: mode setting and legacy interfaces.
    Primary,
    /// `/dev/dri/renderDN`: rendering and buffer management only.
    Render,
}

bitflags! {
    /// The `DRIVER_*` feature bits a driver advertises.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_drv.h#L36>.
    pub struct DriverFeatures: u32 {
        /// The driver drives displays and exposes a render node's sibling `cardN`
        /// with mode setting.
        const MODESET = 1 << 0;
        /// The driver supports GEM.
        const GEM = 1 << 1;
        /// The driver exposes a render node.
        const RENDER = 1 << 2;
        /// The driver supports the atomic mode-setting interface.
        const ATOMIC = 1 << 3;
        /// The driver supports sync objects.
        const SYNCOBJ = 1 << 4;
        /// The driver supports timeline sync objects.
        const SYNCOBJ_TIMELINE = 1 << 5;
    }
}

/// The identity a driver reports through `DRM_IOCTL_VERSION`.
///
/// The `name` is load-bearing, not cosmetic: Mesa's loader matches on it to pick
/// a user-space driver, so `aster-pvr` must report `"powervr"` exactly as the
/// upstream driver does.
#[derive(Clone, Debug)]
pub struct DriverInfo {
    /// Driver name, as reported in `drm_version.name`.
    pub name: &'static str,
    /// Driver date, as reported in `drm_version.date`.
    pub date: &'static str,
    /// Human-readable description.
    pub desc: &'static str,
    /// Interface major version.
    pub major: i32,
    /// Interface minor version.
    pub minor: i32,
    /// Interface patch level.
    pub patchlevel: i32,
}

/// Arguments for creating a dumb buffer, validated before reaching the driver.
#[derive(Clone, Copy, Debug)]
pub struct DumbCreateArgs {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per pixel.
    pub bpp: u32,
}

/// The result of creating a dumb buffer.
#[derive(Debug)]
pub struct DumbBuffer {
    /// The backing object.
    pub object: Arc<dyn GemObject>,
    /// Stride in bytes, which the driver may round up for hardware alignment.
    pub pitch: u32,
    /// Total size in bytes.
    pub size: u64,
}

/// What a hardware driver implements to back a [`DrmDevice`].
///
/// Only [`info`](Self::info) and [`features`](Self::features) are mandatory. A
/// driver that advertises [`DriverFeatures::MODESET`] must also return a
/// [`ModeConfig`]. GEM support does not imply dumb-buffer support; drivers
/// advertise the latter through [`Self::get_cap`]. Lifecycle hooks default to
/// no-ops, and unsupported allocation hooks report [`DrmError::NotSupported`].
///
/// [`DrmError::NotSupported`]: crate::error::DrmError::NotSupported
pub trait DrmDriver: Send + Sync + core::fmt::Debug {
    /// Downcast escape hatch for recovering a concrete driver from
    /// `Arc<dyn DrmDriver>`.
    ///
    /// Provides an explicit, object-safe downcast hook without exposing the
    /// concrete driver's layout to this crate.
    ///
    /// This is how a handler gets from the [`DrmDevice`] it was passed back to
    /// its own driver state, which is what upstream reaches through
    /// `to_pvr_device(drm_dev)`. Without it a driver has to keep its state in a
    /// global and can therefore only ever have one device.
    ///
    /// [`GemObject::as_any`]: crate::gem::GemObject::as_any
    fn as_any(&self) -> &dyn core::any::Any;

    /// Returns the driver's identity.
    fn info(&self) -> DriverInfo;

    /// Returns the driver's feature bits.
    fn features(&self) -> DriverFeatures;

    /// Returns the bus ID reported by `DRM_IOCTL_GET_UNIQUE`.
    ///
    /// Linux derives this from the parent device's name on the bus, so a platform
    /// driver reports something like `"soc:gpu"`. Nothing in modern user space
    /// parses it — `libdrm` identifies devices through sysfs instead — but it must
    /// stay stable for the lifetime of the device, so drivers that know their bus
    /// address should report it here.
    ///
    /// Divergence: the default is the driver name, because there is no device
    /// model to ask yet.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L139>
    /// (`drm_set_busid`).
    fn unique(&self) -> String {
        String::from(self.info().name)
    }

    /// Answers a `DRM_IOCTL_GET_CAP` query.
    ///
    /// Returning `None` defers to the core, which derives the capabilities it can
    /// from [`features`](Self::features) and reports `EINVAL` for the rest. Only
    /// override the capabilities that depend on this driver's own hardware.
    fn get_cap(&self, capability: u64) -> Option<u64> {
        let _ = capability;
        None
    }

    /// Returns the mode configuration, if this driver drives displays.
    fn mode_config(&self) -> Option<&ModeConfig> {
        None
    }

    /// Returns how this device should appear in sysfs, if it should.
    ///
    /// Returning `None` means `drmGetDevices2` will not see the device at all,
    /// which is only right for a device no user-space library needs to find.
    /// Every driver that expects to be used through `libdrm` must answer.
    fn platform_identity(&self) -> Option<PlatformIdentity> {
        None
    }

    /// Allocates a dumb buffer.
    fn dumb_create(&self, args: DumbCreateArgs) -> DrmResult<DumbBuffer> {
        let _ = args;
        Err(DrmError::NotSupported)
    }

    /// Builds this driver's own object for a buffer another device exported.
    ///
    /// Ports upstream's `drm_driver::gem_prime_import`, and exists for the same
    /// reason: the generic wrapper [`crate::prime::ImportedObject`] can answer
    /// how big a buffer is and what kind of memory it holds, but it cannot be
    /// *used* by the importing hardware, because using it means knowing that
    /// hardware's address space. A GPU with its own MMU has to write the
    /// exporter's pages into its own page tables, and only the GPU's driver can
    /// do that.
    ///
    /// This is the mechanism a render-only GPU needs to draw into a display
    /// controller's scanout buffer — the PRIME direction that makes a
    /// split render/display SoC work at all.
    ///
    /// Returning `Ok(None)`, the default, means the generic wrapper: correct for
    /// a driver that only ever hands imported buffers on to hardware needing
    /// nothing but their pages. Returning `Err` refuses the import outright,
    /// which is right when the driver *should* be able to use the buffer but
    /// cannot — an unreadable exporter, or memory it has no way to address.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_prime.c#L919>
    /// (`drm_gem_prime_import_dev`, which calls the driver's hook and falls back
    /// to `drm_gem_prime_import_sg_table`).
    fn import_buffer(&self, buffer: &Arc<DmaBuf>) -> DrmResult<Option<Arc<dyn GemObject>>> {
        let _ = buffer;
        Ok(None)
    }

    /// Returns the driver-private ioctls, occupying `DRM_COMMAND_BASE..DRM_COMMAND_END`.
    ///
    /// This is how `aster-pvr` attaches its fourteen ioctls without this crate
    /// knowing anything about PowerVR.
    fn private_ioctls(&self) -> &[IoctlDesc] {
        &[]
    }

    /// Called after a new file has been opened on this device.
    fn open(&self, file: &DrmFile) -> DrmResult<()> {
        let _ = file;
        Ok(())
    }

    /// Called after the last reference to a file has gone away.
    fn postclose(&self, file: &DrmFile) {
        let _ = file;
    }
}

/// A registered DRM device.
#[derive(Debug)]
pub struct DrmDevice {
    driver: Arc<dyn DrmDriver>,
    /// The primary node's minor number, or `None` for a render-only device.
    primary_minor: Option<u32>,
    /// The render node's minor number, or `None` for a display-only device.
    render_minor: Option<u32>,
    /// The bus ID reported by `DRM_IOCTL_GET_UNIQUE`.
    unique: String,
    /// The two device-wide GEM namespaces: flink names and `mmap` offsets.
    gem: GemDevice,
    /// Whether some open file currently holds mastership.
    ///
    /// Linux's `drm_is_current_master()` is the conjunction of a per-file flag and
    /// a device-wide pointer. Collapsing each side to a single bit is only safe
    /// while every transition happens under this lock, so mastership is never
    /// changed anywhere else. When both this lock and a file's state lock are
    /// needed, this one is taken first.
    master_held: SpinLock<bool>,
}

impl DrmDevice {
    /// Builds a device without publishing it in the crate's registry.
    ///
    /// [`crate::register_device`] is the way in for drivers; this exists so that
    /// tests can exercise a device without a global side effect.
    pub(crate) fn new_unregistered(
        driver: Arc<dyn DrmDriver>,
        primary_minor: Option<u32>,
        render_minor: Option<u32>,
    ) -> Arc<Self> {
        let unique = driver.unique();
        Arc::new(Self {
            driver,
            primary_minor,
            render_minor,
            unique,
            gem: GemDevice::new(),
            master_held: SpinLock::new(false),
        })
    }

    /// Returns the driver backing this device.
    pub fn driver(&self) -> &Arc<dyn DrmDriver> {
        &self.driver
    }

    /// Returns the GEM state shared by every file open on this device.
    ///
    /// Both of its namespaces exist to let one client reach a buffer another
    /// one created, so neither can live in [`DrmFile`]. Linux keeps them in
    /// `drm_device` for the same reason.
    pub fn gem(&self) -> &GemDevice {
        &self.gem
    }

    /// Returns the primary node's minor number, if the device has one.
    pub fn primary_minor(&self) -> Option<u32> {
        self.primary_minor
    }

    /// Returns the render node's minor number, if the device has one.
    pub fn render_minor(&self) -> Option<u32> {
        self.render_minor
    }

    /// Returns the minor number of one of this device's nodes.
    pub fn minor_number(&self, minor: DrmMinor) -> Option<u32> {
        match minor {
            DrmMinor::Primary => self.primary_minor,
            DrmMinor::Render => self.render_minor,
        }
    }

    /// Returns the bus ID for `DRM_IOCTL_GET_UNIQUE`.
    pub fn unique(&self) -> &str {
        &self.unique
    }

    /// Returns the devtmpfs node name for a minor, such as `card0` or `renderD128`.
    pub fn node_name(&self, minor: DrmMinor) -> Option<String> {
        let number = self.minor_number(minor)?;
        Some(match minor {
            DrmMinor::Primary => format!("card{number}"),
            DrmMinor::Render => format!("renderD{number}"),
        })
    }

    /// Answers a `DRM_IOCTL_GET_CAP` query.
    ///
    /// The driver gets first refusal, then the core answers what it can derive
    /// from the feature bits. The order matters for the four capabilities Linux
    /// handles before its own mode-setting check, because they are meaningful even
    /// for a render-only driver.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L231>
    /// (`drm_getcap`).
    pub fn capability(&self, capability: u64) -> DrmResult<u64> {
        if let Some(value) = self.driver.get_cap(capability) {
            return Ok(value);
        }

        let features = self.driver.features();

        match capability {
            cap::device::TIMESTAMP_MONOTONIC => return Ok(1),
            // Drivers with GEM buffers support both PRIME import and export,
            // allowing render clients to share frames with the display.
            cap::device::PRIME => {
                return Ok(if features.contains(DriverFeatures::GEM) {
                    cap::device::PRIME_IMPORT | cap::device::PRIME_EXPORT
                } else {
                    0
                });
            }
            cap::device::SYNCOBJ => {
                return Ok(features.contains(DriverFeatures::SYNCOBJ) as u64);
            }
            cap::device::SYNCOBJ_TIMELINE => {
                return Ok(features.contains(DriverFeatures::SYNCOBJ_TIMELINE) as u64);
            }
            _ => {}
        }

        // Everything below describes a display pipeline, so a render-only driver
        // has no answer to give rather than a negative one.
        if !features.contains(DriverFeatures::MODESET) {
            return Err(DrmError::NotSupported);
        }

        match capability {
            // A driver that can allocate dumb buffers says so by overriding
            // `DrmDriver::get_cap`; the core cannot see whether `dumb_create` is
            // implemented.
            cap::device::DUMB_BUFFER
            | cap::device::DUMB_PREFERRED_DEPTH
            | cap::device::DUMB_PREFER_SHADOW
            | cap::device::ASYNC_PAGE_FLIP
            | cap::device::ADDFB2_MODIFIERS
            | cap::device::PAGE_FLIP_TARGET => Ok(0),
            cap::device::VBLANK_HIGH_CRTC | cap::device::CRTC_IN_VBLANK_EVENT => Ok(1),
            cap::device::CURSOR_WIDTH | cap::device::CURSOR_HEIGHT => Ok(64),
            _ => Err(DrmError::InvalidArgument),
        }
    }

    /// Opens without an opener identity, for callers with no user thread.
    /// Such a file has no identity-based mastership-reclaim exception.
    pub fn open(
        self: &Arc<Self>,
        minor: DrmMinor,
        privilege: Privilege,
    ) -> DrmResult<Arc<DrmFile>> {
        self.open_inner(minor, privilege, None)
    }

    /// Opens a user file with the actual opening thread's identity.
    /// `privilege` selects initial authentication only; privileged ioctl checks
    /// use the current caller's credentials supplied on each request.
    pub fn open_with_identity(
        self: &Arc<Self>,
        minor: DrmMinor,
        privilege: Privilege,
        opener: CallerIdentity,
    ) -> DrmResult<Arc<DrmFile>> {
        self.open_inner(minor, privilege, Some(opener))
    }

    fn open_inner(
        self: &Arc<Self>,
        minor: DrmMinor,
        privilege: Privilege,
        opener: Option<CallerIdentity>,
    ) -> DrmResult<Arc<DrmFile>> {
        if self.minor_number(minor).is_none() {
            return Err(DrmError::NoDevice);
        }

        let file = DrmFile::new(Arc::downgrade(self), minor, privilege, opener);

        // The first client on the primary node becomes master, so that a single
        // compositor works without ever calling `SET_MASTER`. Render clients never
        // get a master object at all.
        if minor == DrmMinor::Primary {
            let mut held = self.master_held.lock();
            if !*held {
                *held = true;
                file.become_master();
            }
        }

        self.driver.open(&file)?;
        file.mark_driver_opened();

        Ok(file)
    }

    /// Claims device-wide mastership for `file`.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_auth.c#L248>
    /// (`drm_setmaster_ioctl`).
    pub fn set_master(&self, file: &DrmFile, caller: &CallerContext) -> DrmResult<()> {
        let mut held = self.master_held.lock();

        // Linux permits the original opener of a former master, or a caller
        // with current CAP_SYS_ADMIN. Passed descriptors convey neither check.
        if !caller.may_manage_master(file) {
            return Err(DrmError::PermissionDenied);
        }

        if file.is_master() {
            return Ok(());
        }

        if *held {
            return Err(DrmError::Busy);
        }

        // Unreachable through the ioctl table, which refuses `SET_MASTER` on a
        // render node before the handler runs, but mastership is a security
        // boundary and this is the only place that grants it.
        if file.minor() != DrmMinor::Primary {
            return Err(DrmError::InvalidArgument);
        }

        *held = true;
        file.become_master();
        Ok(())
    }

    /// Releases device-wide mastership held by `file`.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_auth.c#L299>
    /// (`drm_dropmaster_ioctl`).
    pub fn drop_master(&self, file: &DrmFile, caller: &CallerContext) -> DrmResult<()> {
        let mut held = self.master_held.lock();

        if !caller.may_manage_master(file) {
            return Err(DrmError::PermissionDenied);
        }

        if !file.is_master() {
            return Err(DrmError::InvalidArgument);
        }

        *held = false;
        file.resign_master();
        Ok(())
    }

    /// Releases mastership on behalf of a file that is going away.
    ///
    /// Called from [`DrmFile`]'s destructor, which is why it cannot report an
    /// error: a closing file must never leave the device without a way to acquire
    /// mastership again.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_auth.c#L360>
    /// (`drm_master_release`).
    pub(crate) fn release_master(&self, file: &DrmFile) {
        let mut held = self.master_held.lock();
        if file.is_master() {
            *held = false;
            file.resign_master();
        }
    }

    /// Dispatches an ioctl whose payload has already been copied in from user space.
    ///
    /// `payload` is the caller's argument structure; the character-device shell
    /// copies it in before the call and back out afterwards. `user` gives access
    /// to the caller's address space for the variable-length ioctls that must
    /// write into user arrays themselves.
    pub fn ioctl(
        self: &Arc<Self>,
        file: &DrmFile,
        nr: u32,
        payload: &mut [u8],
        caller: &CallerContext,
    ) -> DrmResult<()> {
        let Some(desc) = ioctl::lookup(self, nr) else {
            ostd::debug!("{} has no ioctl {:#04x}", self.driver.info().name, nr);
            return Err(DrmError::InvalidArgument);
        };

        desc.check_permission(file, caller)?;

        (desc.handler)(self, file, payload, caller)
    }

    /// Returns the object IDs of every mode object of the given type.
    ///
    /// An empty list is the honest answer for a device with no display pipeline,
    /// and for an object type this crate does not have: both mean "there are none
    /// of those here".
    ///
    /// Note what `DRM_MODE_OBJECT_FB` returns: every framebuffer on the device,
    /// not the caller's. `MODE_GETRESOURCES` reports only the framebuffers the
    /// calling file created, as Linux's per-file list does, so it uses
    /// [`ModeConfig::framebuffer_ids_of`] instead of this.
    pub fn mode_object_ids(&self, object_type: u32) -> Vec<u32> {
        debug_assert!(matches!(
            object_type,
            mode::object_type::CRTC
                | mode::object_type::CONNECTOR
                | mode::object_type::ENCODER
                | mode::object_type::PLANE
                | mode::object_type::FB
        ));

        let Some(config) = self.driver.mode_config() else {
            return Vec::new();
        };
        let Some(kind) = ObjectKind::from_uapi(object_type) else {
            return Vec::new();
        };
        config.object_ids(kind)
    }
}

/// How a device appears in the platform device model `libdrm` enumerates.
///
/// `drmGetDevices2` finds a DRM device by reading sysfs, not by asking the kernel,
/// so a device that does not publish itself there is invisible to every modern
/// client no matter how well its ioctls work. The kernel crate turns this into the
/// sysfs entries in `kernel/core/src/device/dri.rs`.
///
/// The one rule that is not obvious: [`of_fullname`](Self::of_fullname) must
/// differ between devices. `libdrm` folds platform devices whose `OF_FULLNAME`
/// matches into a single `drmDevice`, so two drivers sharing one would be reported
/// as one device with several nodes, and Mesa would pick a user-space driver by
/// the wrong name.
#[derive(Clone, Debug)]
pub struct PlatformIdentity {
    /// The directory name under `/sys/devices/platform`, such as `dpu@c0340000`.
    ///
    /// Linux names a platform device after its device tree node, unit address
    /// included.
    pub name: String,
    /// The full device tree path, reported as `OF_FULLNAME`. Must be unique.
    pub of_fullname: String,
    /// The device tree `compatible` strings, most specific first.
    pub of_compatible: Vec<String>,
}
