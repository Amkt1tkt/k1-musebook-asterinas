// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux-compatible DRM/KMS object model and user-space interface.
//! A Linux-ABI-compatible DRM/KMS subsystem.
//!
//! # Why this exists
//!
//! Modern graphics user space does not offer a choice. `cosmic-comp` reaches the
//! screen through GBM → EGL → DRM/KMS even when it falls back to software
//! rendering, and Mesa's PowerVR Vulkan driver only speaks to
//! `/dev/dri/renderD*`. A framebuffer device, however well it works, cannot
//! serve either. So this crate reproduces the parts of Linux's DRM interface
//! that those clients actually use.
//!
//! We are not designing an interface here, we are reproducing one. The
//! authoritative reference is the BSP's own uapi headers
//! (`include/uapi/drm/{drm,drm_mode,drm_fourcc}.h`); every structure and ioctl
//! carries a link to its Linux definition.
//!
//! # Layering
//!
//! This crate holds the device model and all of the logic, but it never touches
//! user memory: comp crates cannot reach `current_userspace!()` or the file
//! plumbing, which live in the kernel crate. The split mirrors
//! `aster-framebuffer` and `kernel/src/device/fb.rs`:
//!
//! - `kernel/core/src/device/dri.rs` owns the character devices, decodes ioctl
//!   numbers, copies arguments in and out, and maps [`DrmError`] to `Errno`.
//! - This crate receives already-copied structures, plus a [`UserMem`] handle for
//!   the variable-length ioctls that must write into user arrays themselves.
//!
//! That keeps this crate `#![deny(unsafe_code)]` and unit-testable.
//!
//! # Hardware independence
//!
//! Concrete display drivers do not live here. This crate offers the
//! [`DrmDriver`] slot; the K1 eDP pipeline is registered by
//! `aster-framebuffer`, which already owns the DPU registers. One owner means
//! fbdev and KMS cannot fight over the hardware, and it keeps this crate free of
//! board specifics.
#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
// Overrides the built-in `derive` so that `#[derive(Pod)]` works on the uapi
// structures. See the `ostd-pod` crate documentation.
#[macro_use]
extern crate ostd_pod;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "drm: "
    };
}

pub mod device;
pub mod drivers;
pub mod error;
pub mod fence;
pub mod file;
pub mod gem;
pub mod ioctl;
pub mod kms;
pub mod live;
pub mod prime;
pub mod uapi;

use alloc::{sync::Arc, vec::Vec};

use component::{ComponentInitError, init_component};
pub use error::{DrmError, DrmResult};
use ostd::sync::SpinLock;
use spin::Once;

pub use crate::{
    device::{DriverFeatures, DrmDevice, DrmDriver, DrmMinor, PlatformIdentity},
    file::{CallerIdentity, DrmEvent, DrmFile, EventNotifier, FileId, Privilege},
    ioctl::UserMem,
};

/// The Linux character-device major number for DRM.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/Documentation/admin-guide/devices.txt>.
pub const DRM_MAJOR: u32 = 226;

/// The first minor number used by render nodes.
///
/// Primary nodes (`cardN`) occupy minors 0..63, control nodes 64..127, and render
/// nodes (`renderDN`) start at 128.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_file.h>.
pub const DRM_RENDER_MINOR_BASE: u32 = 128;

/// How many nodes of each kind the minor number space has room for.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_drv.c#L108>
/// (`drm_minor_alloc`, which allocates from `64 * type..64 * type + 63`).
const MINORS_PER_KIND: u32 = 64;

/// Every registered DRM device, in registration order.
static DEVICES: Once<SpinLock<Vec<Arc<DrmDevice>>>> = Once::new();

/// Registers a DRM device and returns it.
///
/// Called by drivers during their own component init. The returned device is
/// already visible to [`devices`], but the character-device nodes are created
/// later, by the kernel crate's first-process init.
///
/// The two node kinds are numbered independently, exactly as Linux does: a
/// display-only driver takes `card0` and a render-only driver registered after it
/// still takes `renderD128`. That is what makes the split in `../ABI.md` — the K1
/// display pipeline on `card0`, PowerVR on `renderD128` — fall out without either
/// driver knowing about the other.
pub fn register_device(driver: Arc<dyn DrmDriver>) -> DrmResult<Arc<DrmDevice>> {
    let features = driver.features();
    if !features.intersects(DriverFeatures::MODESET | DriverFeatures::RENDER) {
        return Err(DrmError::InvalidArgument);
    }

    let devices = DEVICES.get().ok_or(DrmError::NoDevice)?;
    let mut devices = devices.lock();

    let primary_minor = features
        .contains(DriverFeatures::MODESET)
        .then(|| next_minor(&devices, DrmMinor::Primary))
        .transpose()?;
    let render_minor = features
        .contains(DriverFeatures::RENDER)
        .then(|| next_minor(&devices, DrmMinor::Render))
        .transpose()?;

    let device = DrmDevice::new_unregistered(driver, primary_minor, render_minor);
    devices.push(device.clone());
    drop(devices);

    ostd::info!(
        "registered {} as {:?}/{:?}",
        device.driver().info().name,
        device.node_name(DrmMinor::Primary),
        device.node_name(DrmMinor::Render)
    );

    Ok(device)
}

/// Picks the next free minor number of one kind.
///
/// Numbers are never reused, because devices are never unregistered. Handing out
/// a stale number would be worse than running out: user space caches the
/// `major:minor` pair it found a device under.
fn next_minor(devices: &[Arc<DrmDevice>], kind: DrmMinor) -> DrmResult<u32> {
    let base = match kind {
        DrmMinor::Primary => 0,
        DrmMinor::Render => DRM_RENDER_MINOR_BASE,
    };

    let next = devices
        .iter()
        .filter_map(|device| device.minor_number(kind))
        .max()
        .map_or(base, |highest| highest + 1);

    if next >= base + MINORS_PER_KIND {
        return Err(DrmError::NoSpace);
    }
    Ok(next)
}

/// Returns all registered DRM devices.
pub fn devices() -> Vec<Arc<DrmDevice>> {
    let Some(devices) = DEVICES.get() else {
        return Vec::new();
    };
    devices.lock().clone()
}

/// Looks up a device by the minor number of the node being opened.
pub fn device_by_minor(minor: u32) -> Option<(Arc<DrmDevice>, DrmMinor)> {
    let devices = DEVICES.get()?;
    let devices = devices.lock();

    devices.iter().find_map(|device| {
        if device.primary_minor() == Some(minor) {
            Some((device.clone(), DrmMinor::Primary))
        } else if device.render_minor() == Some(minor) {
            Some((device.clone(), DrmMinor::Render))
        } else {
            None
        }
    })
}

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    DEVICES.call_once(|| SpinLock::new(Vec::new()));
    Ok(())
}
