// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: /sys/devices + /sys/class + /sys/bus/platform device model (libdrm/libudev).

//! The minimal sysfs device model.
//!
//! This is not a general device model: it is the smallest tree that lets user
//! space find a character device and decide what it is. The motivating consumers
//! are `libdrm`, whose `drmGetDevices2()` enumerates DRM devices entirely through
//! sysfs, and `libudev`, whose enumerator walks `/sys/class`; both report zero
//! devices without saying why when sysfs is incomplete.
//!
//! Devices come in two shapes here, matching the two shapes Linux's
//! `get_device_parent` produces:
//!
//! - a device on the platform bus, published with [`register_platform_device`],
//!   whose character devices sit in a class-named directory inside it;
//! - a device on no bus at all, published with [`register_virtual_device`], which
//!   lands under `/sys/devices/virtual/<class>/` and whose character devices sit
//!   directly inside it.
//!
//! # What `libdrm` reads
//!
//! Given a node in `/dev/dri`, `libdrm` `stat`s it for a major/minor pair and
//! then, for a device on the platform bus, touches exactly these paths
//! (`xf86drm.c`):
//!
//! | Path | Operation | Read by |
//! |---|---|---|
//! | `/sys/dev/char/<major>:<minor>/device/drm` | `stat` | `drmNodeIsDRM` — a node that fails this is not considered a DRM device at all |
//! | `/sys/dev/char/<major>:<minor>/device/subsystem` | `readlink` | `drmParseSubsystemType` — the last component of the target selects the bus |
//! | `/sys/dev/char/<major>:<minor>/device/uevent` | `open`/`read` | `drmParseOFBusInfo` and `drmParseOFDeviceInfo` |
//! | `/sys/dev/char/<major>:<minor>/device/drm/` | `opendir` | `drmGetMinorNameForFD`, which looks for an entry named `card*` or `renderD*` |
//! | `/sys/dev/char/<major>:<minor>/uevent` | `open`/`read` | `drmGetDeviceNameFromFd2`, for `DEVNAME` |
//!
//! and needs these `uevent` keys:
//!
//! - on the parent device: `OF_FULLNAME`, `OF_COMPATIBLE_N` and
//!   `OF_COMPATIBLE_0`..`OF_COMPATIBLE_<N-1>` (`MODALIAS` is the documented
//!   fallback for a device without device tree data, and is emitted too);
//! - on the character device itself: `DEVNAME`.
//!
//! The bus is decided by string matching on the `subsystem` link target: the last
//! `/`-separated component must be `platform` (or `spi`) for
//! `DRM_BUS_PLATFORM`. K1's GPU and DPU are both platform devices, so that is the
//! branch that matters here.
//!
//! # The shape of the tree
//!
//! The layout mirrors Linux exactly, because the relative link targets are what
//! user space resolves and getting them wrong is invisible from the kernel side:
//!
//! ```text
//! /sys/devices/platform/<device>/          the platform device
//!     uevent                               OF_* keys and MODALIAS
//!     subsystem -> ../../../bus/platform   what makes it a platform device
//!     <class>/<name>/                      one character device of that class
//!         uevent                           MAJOR, MINOR, DEVNAME, DEVTYPE
//!         dev                              <major>:<minor>
//!         device -> ../..                  back to the platform device
//!         subsystem -> ../../../../../class/<class>
//! /sys/bus/platform/devices/<device> -> ../../../devices/platform/<device>
//! /sys/class/<class>/<name> -> ../../devices/platform/<device>/<class>/<name>
//! /sys/dev/char/<major>:<minor> -> ../../devices/platform/<device>/<class>/<name>
//!
//! /sys/devices/virtual/<class>/<device>/    the device on no bus
//!     uevent                               class-specific keys
//!     <attr>                               class-specific attribute files
//!     <group>/<attr>                       class-specific attribute groups
//!     subsystem -> ../../../../class/<class>
//!     <name>/                              one character device, no class level
//!         uevent                           MAJOR, MINOR, DEVNAME, DEVTYPE
//!         dev                              <major>:<minor>
//!         device -> ../../<device>          back to the parent device
//!         subsystem -> ../../../../../class/<class>
//! /sys/class/<class>/<device> -> ../../devices/virtual/<class>/<device>
//! /sys/class/<class>/<name> -> ../../devices/virtual/<class>/<device>/<name>
//! /sys/dev/char/<major>:<minor> -> ../../devices/virtual/<class>/<device>/<name>
//! ```
//!
//! The `<class>` level inside the device directory is not decoration: Linux places
//! a device that has both a parent and a class under `<parent>/<class name>/`, and
//! that is precisely why `libdrm` can probe for `device/drm` to recognize a DRM
//! node. See `get_device_parent` in `drivers/base/core.c`.
//!
//! # Registering a device
//!
//! A driver publishes its device and then each of its character device nodes:
//!
//! ```ignore
//! let device = sysfs::register_platform_device(PlatformDeviceInfo {
//!     name: "gpu@d8100000".into(),
//!     of_fullname: "/soc/gpu@d8100000".to_string(),
//!     of_compatible: vec!["img,img-bxe-2-32".to_string(), "img,img-rogue".to_string()],
//!     driver: Some("powervr".to_string()),
//! })?;
//! device.register_char_device(CharDeviceInfo {
//!     class: "drm".into(),
//!     name: "renderD128".into(),
//!     dev_name: "dri/renderD128".to_string(),
//!     dev_type: Some("drm_minor".to_string()),
//!     id: DeviceId::new(MajorId::new(226), MinorId::new(128)),
//! })?;
//! ```
//!
//! The major/minor pair passed here must be the one the character device actually
//! carries, since that pair is the only thing tying the two together as far as
//! user space is concerned.

// Platform and CPU policy helpers are used by the RISC-V board and by ktests;
// some of that surface is unused in builds for other architectures.
#![expect(dead_code)]

use alloc::format;
use core::fmt::Write;

use aster_device::{auxiliary, common::DevNum};
use aster_systree::{
    BranchNodeFields, DirNode, Error as SysTreeError, Result as SysTreeResult, SymlinkNode,
    SysAttrSet, SysAttrSetBuilder, SysObj, SysPerms, SysStr, inherit_sys_branch_node,
};
use aster_util::printer::VmPrinter;
use device_id::DeviceId;
use spin::Once;

use crate::prelude::*;

/// The name of the platform bus, as it appears in `/sys/bus`.
///
/// This string is also what `libdrm` matches the `subsystem` link target against
/// to pick its `DRM_BUS_PLATFORM` branch, so it is ABI.
const PLATFORM_BUS: &str = "platform";

/// The directory under `/sys/devices` that holds the devices on no bus, which
/// Linux calls virtual devices.
const VIRTUAL_DEVICES: &str = "virtual";

/// The name of the `uevent` attribute, which every device directory has.
const UEVENT_ATTR: &str = "uevent";

/// The name of the `dev` attribute, which every device with a device node has.
const DEV_ATTR: &str = "dev";

/// The name of the link that names a device's subsystem.
const SUBSYSTEM_LINK: &str = "subsystem";

/// The number of path components between the sysfs root and a platform device's
/// own directory, `devices/platform/<device>`.
const PLATFORM_DEVICE_DEPTH: usize = 3;

/// The number of path components between the sysfs root and a virtual device's
/// own directory, `devices/virtual/<class>/<name>`.
const VIRTUAL_DEVICE_DEPTH: usize = 4;

/// The number of path components between the sysfs root and a class device's own
/// directory, `devices/platform/<device>/<class>/<name>`.
const CLASS_DEVICE_DEPTH: usize = 5;

/// The number of path components between the sysfs root and `class/<class>`.
const CLASS_INDEX_DEPTH: usize = 2;

/// The number of path components between the sysfs root and `bus/<bus>/devices`.
const BUS_DEVICES_DEPTH: usize = 3;

/// The directories that make up the device model.
struct DeviceModel {
    /// `/sys/devices/platform`.
    platform_devices: Arc<DirNode>,
    /// `/sys/bus/platform/devices`.
    platform_bus_devices: Arc<DirNode>,
}

static MODEL: Once<DeviceModel> = Once::new();

/// Registers the K1 subtrees beneath the initialized device-model roots.
///
/// `aster-device` owns the shared roots; this adapter owns only its platform,
/// virtual-class and class-index subdirectories.
pub(super) fn init() {
    MODEL.call_once(|| {
        let platform_devices = auxiliary::device_directory(PLATFORM_BUS.into())
            .expect("cannot register the platform device directory");
        let platform_bus = auxiliary::bus_directory(PLATFORM_BUS.into())
            .expect("cannot register the platform bus directory");
        let platform_bus_devices = get_or_create_dir(&platform_bus, "devices");

        DeviceModel {
            platform_devices,
            platform_bus_devices,
        }
    });
}

fn model() -> &'static DeviceModel {
    MODEL
        .get()
        .expect("the sysfs device model is not initialized")
}

/// Returns the auxiliary `/sys/devices/system` directory for CPU policies.
pub(crate) fn system_devices_dir() -> Result<Arc<DirNode>> {
    Ok(auxiliary::device_directory("system".into())?)
}

/// Returns `/sys/class/<class>`, creating the class index if it does not exist.
///
/// A cpufreq policy lives under `/sys/devices/system/cpu`, not
/// `/sys/devices/virtual`, so it cannot use [`register_virtual_device`] to
/// get its `/sys/class/<class>/<name>` symlink. Callers add that symlink
/// themselves with [`SymlinkNode`]. This does not change how virtual or
/// platform devices populate the same index.
pub(crate) fn class_index(class: SysStr) -> Result<Arc<DirNode>> {
    Ok(auxiliary::class_directory(class)?)
}

/// Returns a subdirectory of `parent`, creating it if it does not exist.
///
/// # Panics
///
/// Panics if a different node already occupies the requested directory name.
fn get_or_create_dir(parent: &Arc<DirNode>, name: &'static str) -> Arc<DirNode> {
    parent
        .get_or_create_dir(SysStr::from(name))
        .unwrap_or_else(|err| panic!("failed to create the sysfs directory {}: {:?}", name, err))
}

/// Returns the relative path from a directory that is `depth` components below
/// the sysfs root back to the sysfs root.
fn to_sys_root(depth: usize) -> String {
    let mut path = String::with_capacity(depth * 3);
    for index in 0..depth {
        if index > 0 {
            path.push('/');
        }
        path.push_str("..");
    }
    path
}

/// The device tree node name behind a full path: `/soc/gpu@d8100000` is `gpu`.
///
/// Linux reports this as the `OF_NAME` key and builds `MODALIAS` from it. The unit
/// address is not part of the name.
fn of_name(of_fullname: &str) -> &str {
    let last_component = of_fullname.rsplit('/').next().unwrap_or(of_fullname);
    last_component.split('@').next().unwrap_or(last_component)
}

/// The name of a device's entry under `/sys/dev/char`.
fn dev_char_name(id: DeviceId) -> String {
    format!("{}:{}", id.major().get(), id.minor().get())
}

/// What a driver has to say about a platform device to publish it in sysfs.
#[derive(Debug, Clone)]
pub(crate) struct PlatformDeviceInfo {
    /// The directory name under `/sys/devices/platform`.
    ///
    /// Linux names a platform device created from a device tree node after that
    /// node, unit address included: `gpu@d8100000`.
    pub name: SysStr,
    /// The full device tree path of the device, reported as `OF_FULLNAME`.
    ///
    /// This is the key `libdrm` uses as the platform device's unique name, and
    /// the one it looks for first.
    pub of_fullname: String,
    /// The device tree `compatible` strings, most specific first.
    ///
    /// Reported as `OF_COMPATIBLE_0`..`OF_COMPATIBLE_<N-1>` plus the count in
    /// `OF_COMPATIBLE_N`. Mesa matches on these to pick a user space driver.
    pub of_compatible: Vec<String>,
    /// The name of the driver bound to the device, reported as `DRIVER`.
    pub driver: Option<String>,
}

/// The name and the full contents of one read-only sysfs attribute file.
///
/// Use this for values that do not change after registration — an input
/// device's `name`, a DRM node's `dev`. Values that move belong in
/// [`VirtualDeviceInfo::live_attrs`].
pub(crate) type AttrFile = (SysStr, String);

/// A read-only attribute that is recomputed on every read.
///
/// The function returns the file contents (value plus newline), or
/// `None` to fail the read. Battery `capacity` is this: SoC changes,
/// and a string captured at registration would freeze the first sample.
pub(crate) type LiveAttr = (SysStr, fn() -> Option<String>);

/// A live attribute that also accepts writes (`brightness`, `bl_power`).
///
/// The write function consumes the `VmReader` and returns the number of
/// bytes taken, or a [`SysTreeError`] (`InvalidOperation` → `EINVAL`).
pub(crate) type LiveRwAttr = (
    SysStr,
    fn() -> Option<String>,
    fn(&mut VmReader) -> SysTreeResult<usize>,
);

/// How a virtual-device attribute produces its contents.
#[derive(Clone, Debug)]
enum AttrSource {
    Fixed(String),
    Live(fn() -> Option<String>),
    LiveRw {
        read: fn() -> Option<String>,
        write: fn(&mut VmReader) -> SysTreeResult<usize>,
    },
}

/// What a driver has to say about a device that sits on no bus to publish it in
/// sysfs.
///
/// Linux gives such a device no bus directory and parents it on
/// `/sys/devices/virtual/<class>/` instead; see `virtual_device_parent` in
/// `drivers/base/core.c`. Input devices are the case that matters here: an
/// `input_dev` with no physical parent, as well as every `input_dev` this kernel
/// creates, ends up there.
#[derive(Debug, Clone)]
pub(crate) struct VirtualDeviceInfo {
    /// The Linux class the device belongs to, such as `input`.
    ///
    /// This names the directory under `/sys/devices/virtual`, the index under
    /// `/sys/class`, and the target of the device's `subsystem` link — which is
    /// the only thing that tells `libudev` what subsystem the device is in.
    pub class: SysStr,
    /// The name of the device, such as `input0`.
    pub name: SysStr,
    /// The `uevent` file's contents, which are entirely class specific.
    ///
    /// The keys a class emits are ABI: see `input_dev_uevent` in
    /// `drivers/input/input.c` for the input class. The caller renders them
    /// because this module knows nothing about any particular class.
    pub uevent: String,
    /// The device's own read-only attribute files, such as `name` and `phys`.
    pub attrs: Vec<AttrFile>,
    /// The device's named attribute groups, such as `id` and `capabilities`.
    ///
    /// Each group is a subdirectory holding read-only attribute files, which is
    /// what an `attribute_group` with a `name` renders as in Linux.
    pub attr_groups: Vec<(SysStr, Vec<AttrFile>)>,
    /// Attributes recomputed on every read.
    ///
    /// A name listed here replaces the same name in [`Self::attrs`] or
    /// `uevent`. Battery properties use this because voltage and
    /// current move; input capabilities stay in [`Self::attrs`] because
    /// they do not.
    pub live_attrs: Vec<LiveAttr>,
    /// Live attributes with owner-write permission (Linux `0644`).
    ///
    /// A name listed here replaces the same name in [`Self::attrs`] or
    /// [`Self::live_attrs`]. Backlight `brightness` / `bl_power` use this.
    pub live_rw_attrs: Vec<LiveRwAttr>,
}

/// What a driver has to say about one of its character devices.
#[derive(Debug, Clone)]
pub(crate) struct CharDeviceInfo {
    /// The Linux class the device belongs to, such as `drm`.
    ///
    /// This names both the directory inside the parent device and the index
    /// under `/sys/class`.
    pub class: SysStr,
    /// The name of the device, such as `card0`.
    pub name: SysStr,
    /// The path of the device node relative to `/dev`, such as `dri/card0`.
    ///
    /// Reported as `DEVNAME`.
    pub dev_name: String,
    /// The class-specific device type, such as `drm_minor`, reported as `DEVTYPE`.
    pub dev_type: Option<String>,
    /// The major and minor numbers the device node carries.
    ///
    /// This pair is the only link between the device node and its sysfs entry, so
    /// it must equal the one the character device is actually registered with.
    pub id: DeviceId,
}

/// Publishes a platform device in sysfs.
///
/// This creates `/sys/devices/platform/<name>` with its `uevent` file and
/// `subsystem` link, and the customary back reference from
/// `/sys/bus/platform/devices`. The device carries no character device nodes yet;
/// see [`PlatformDevice::register_char_device`].
pub(crate) fn register_platform_device(info: PlatformDeviceInfo) -> Result<Arc<PlatformDevice>> {
    let model = model();

    let device = PlatformDevice::new(info);
    let name = device.fields.name().clone();
    let bus_link = SymlinkNode::new(
        name.clone(),
        format!(
            "{}/devices/{}/{}",
            to_sys_root(BUS_DEVICES_DEPTH),
            PLATFORM_BUS,
            name
        ),
    );

    model.platform_devices.add_child(device.clone())?;
    if let Err(err) = model.platform_bus_devices.add_child(bus_link) {
        // Leaving a device that failed to register half-built would give user
        // space a device it cannot resolve, which is worse than no device.
        let _ = model.platform_devices.remove_child(&name);
        return Err(err.into());
    }

    Ok(device)
}

/// A device on the platform bus, as `/sys/devices/platform/<name>`.
#[derive(Debug)]
pub(crate) struct PlatformDevice {
    fields: BranchNodeFields<dyn SysObj, Self>,
    /// The `uevent` file's contents.
    ///
    /// The keys are fixed once the device is registered, so the text is built
    /// once here rather than on every read.
    uevent: String,
}

impl PlatformDevice {
    fn new(info: PlatformDeviceInfo) -> Arc<Self> {
        let mut builder = SysAttrSetBuilder::new();
        builder.add(SysStr::from(UEVENT_ATTR), SysPerms::DEFAULT_RO_ATTR_PERMS);
        let attrs = builder
            .build()
            .expect("failed to build the attribute set of a platform device");

        let uevent = platform_uevent(&info);

        let device = Arc::new_cyclic(|weak_self| Self {
            fields: BranchNodeFields::new(info.name.clone(), attrs, weak_self.clone()),
            uevent,
        });

        // What tells `libdrm` that this is a platform device, and thus the whole
        // point of the entry.
        let subsystem = SymlinkNode::new(
            SysStr::from(SUBSYSTEM_LINK),
            format!(
                "{}/bus/{}",
                to_sys_root(PLATFORM_DEVICE_DEPTH),
                PLATFORM_BUS
            ),
        );
        device
            .fields
            .add_child(subsystem)
            .expect("a fresh platform device cannot have children");

        device
    }

    /// Publishes one of this device's character devices in sysfs.
    ///
    /// This creates the device's own directory in a class-named directory inside
    /// the parent device, together with the `/sys/class` and `/sys/dev/char`
    /// entries that let user space find it by name and by major/minor pair
    /// respectively. The class-named level is what Linux's `get_device_parent`
    /// inserts for a device whose parent is in no class, and what lets `libdrm`
    /// recognize a DRM node by probing for `device/drm`.
    pub(crate) fn register_char_device(&self, info: CharDeviceInfo) -> Result<Arc<CharDevice>> {
        let class_dir = self.class_dir(&info.class)?;
        let device_path = format!(
            "devices/{}/{}/{}/{}",
            PLATFORM_BUS,
            self.fields.name(),
            info.class,
            info.name
        );

        publish_char_device(&*class_dir, device_path, String::from("../.."), info)
    }

    /// Returns the directory holding this device's character devices of a given
    /// class, creating it if it does not exist.
    fn class_dir(&self, class: &SysStr) -> Result<Arc<DirNode>> {
        if let Some(existing) = self.fields.child(class) {
            let dir = existing
                .as_any()
                .downcast_ref::<DirNode>()
                .ok_or(SysTreeError::AlreadyExists)?;
            return Ok(dir.this());
        }

        let dir = DirNode::new(class.clone());
        self.fields.add_child(dir.clone())?;
        Ok(dir)
    }
}

/// Publishes a device that sits on no bus in sysfs.
///
/// This creates `/sys/devices/virtual/<class>/<name>` with its attribute files
/// and its `subsystem` link, plus the `/sys/class/<class>/<name>` back reference.
/// The device carries no character device nodes yet; see
/// [`VirtualDevice::register_char_device`].
pub(crate) fn register_virtual_device(info: VirtualDeviceInfo) -> Result<Arc<VirtualDevice>> {
    let class_dir = auxiliary::virtual_directory(info.class.clone())?;
    let class_index = auxiliary::class_directory(info.class.clone())?;
    let class_link = SymlinkNode::new(
        info.name.clone(),
        format!(
            "{}/devices/{}/{}/{}",
            to_sys_root(CLASS_INDEX_DEPTH),
            VIRTUAL_DEVICES,
            info.class,
            info.name
        ),
    );

    let name = info.name.clone();
    let device = VirtualDevice::new(info)?;
    class_dir.add_child(device.clone())?;
    if let Err(err) = class_index.add_child(class_link) {
        // Same reasoning as in `register_platform_device`: an entry that cannot
        // be resolved is worse than a missing one.
        let _ = class_dir.remove_child(&name);
        return Err(err.into());
    }

    Ok(device)
}

/// A device on no bus, as `/sys/devices/virtual/<class>/<name>`.
#[derive(Debug)]
pub(crate) struct VirtualDevice {
    fields: BranchNodeFields<dyn SysObj, Self>,
    /// The class the device belongs to, kept because its character devices need
    /// it to build their own `subsystem` links.
    class: SysStr,
    /// The contents of every attribute file, `uevent` included.
    ///
    /// [`AttrSource::Fixed`] was rendered at registration. [`AttrSource::Live`]
    /// is called on every read so a moving value — battery SoC — is not
    /// frozen at the first sample.
    attrs: BTreeMap<SysStr, AttrSource>,
}

/// Builds the attribute map of a virtual device or an [`AttrDir`].
///
/// Fixed files first, then `uevent`, then live files. Live wins on a name
/// clash so a class can keep the `uevent` field as a dummy string and still
/// refresh it on every read.
fn attr_sources(
    uevent: String,
    attrs: Vec<AttrFile>,
    live_attrs: Vec<LiveAttr>,
    live_rw_attrs: Vec<LiveRwAttr>,
) -> BTreeMap<SysStr, AttrSource> {
    let mut map: BTreeMap<SysStr, AttrSource> = attrs
        .into_iter()
        .map(|(name, text)| (name, AttrSource::Fixed(text)))
        .collect();
    map.insert(SysStr::from(UEVENT_ATTR), AttrSource::Fixed(uevent));
    for (name, read) in live_attrs {
        map.insert(name, AttrSource::Live(read));
    }
    for (name, read, write) in live_rw_attrs {
        map.insert(name, AttrSource::LiveRw { read, write });
    }
    map
}

impl VirtualDevice {
    fn new(info: VirtualDeviceInfo) -> SysTreeResult<Arc<Self>> {
        let attrs = attr_sources(info.uevent, info.attrs, info.live_attrs, info.live_rw_attrs);

        let device = Arc::new_cyclic(|weak_self| Self {
            fields: BranchNodeFields::new(
                info.name.clone(),
                virtual_attr_set(&attrs),
                weak_self.clone(),
            ),
            class: info.class.clone(),
            attrs,
        });

        // What tells `libudev` which subsystem the device is in: it reads this
        // link and falls back to no subsystem at all when it is missing, and a
        // device with no subsystem matches no `match_subsystem` filter.
        let subsystem = SymlinkNode::new(
            SysStr::from(SUBSYSTEM_LINK),
            format!("{}/class/{}", to_sys_root(VIRTUAL_DEVICE_DEPTH), info.class),
        );
        device.fields.add_child(subsystem)?;
        for (name, files) in info.attr_groups {
            device.fields.add_child(AttrGroup::new(name, files))?;
        }

        Ok(device)
    }

    /// Publishes one of this device's character devices in sysfs.
    ///
    /// The character device lands directly inside this device's directory, with
    /// no class-named level in between: Linux's `get_device_parent` returns the
    /// parent's own directory when the parent is itself in a class, which is why
    /// `/sys/class/input/event0` resolves to `…/input/input0/event0`.
    pub(crate) fn register_char_device(&self, info: CharDeviceInfo) -> Result<Arc<CharDevice>> {
        let parent_name = self.fields.name();
        let device_path = format!(
            "devices/{}/{}/{}/{}",
            VIRTUAL_DEVICES, self.class, parent_name, info.name
        );

        publish_char_device(
            &self.fields,
            device_path,
            format!("../../{}", parent_name),
            info,
        )
    }
}

inherit_sys_branch_node!(VirtualDevice, fields, {
    fn read_attr_at(
        &self,
        name: &str,
        offset: usize,
        writer: &mut VmWriter,
    ) -> SysTreeResult<usize> {
        read_attr_source(&self.attrs, name, offset, writer)
    }

    fn write_attr(&self, name: &str, reader: &mut VmReader) -> SysTreeResult<usize> {
        match self.attrs.get(name) {
            Some(AttrSource::LiveRw { write, .. }) => write(reader),
            _ => Err(SysTreeError::AttributeError),
        }
    }

    fn perms(&self) -> SysPerms {
        SysPerms::DEFAULT_RW_PERMS
    }
});

/// What a driver has to say about a directory of live attributes that is
/// not a virtual or platform device.
///
/// The motivating case is a cpufreq policy. Linux parents that kobject on
/// `cpu_subsys` as `/sys/devices/system/cpu/cpufreq/policyN`, not on
/// `/sys/devices/virtual`. [`register_virtual_device`] cannot express that
/// parent; this is the same [`AttrSource`] machinery with an arbitrary
/// [`DirNode`] parent. Existing virtual/platform registration is unchanged.
#[derive(Debug, Clone)]
pub(crate) struct AttrDirInfo {
    /// Directory name, such as `policy0`.
    pub name: SysStr,
    /// The `uevent` file's contents. A live `uevent` in [`Self::live_attrs`]
    /// replaces this, same as on a virtual device.
    pub uevent: String,
    /// Read-only files whose contents are fixed at registration.
    pub attrs: Vec<AttrFile>,
    /// Attributes recomputed on every read.
    pub live_attrs: Vec<LiveAttr>,
    /// Live attributes with owner-write permission (Linux `0644`).
    pub live_rw_attrs: Vec<LiveRwAttr>,
}

/// Publishes `info` as a child of `parent`.
///
/// The caller owns any `/sys/class` or per-CPU symlink; see [`class_index`].
pub(crate) fn register_attr_dir(parent: &Arc<DirNode>, info: AttrDirInfo) -> Result<Arc<AttrDir>> {
    let dir = AttrDir::new(info);
    parent.add_child(dir.clone())?;
    Ok(dir)
}

/// A directory of live attributes sitting on an arbitrary [`DirNode`] parent.
#[derive(Debug)]
pub(crate) struct AttrDir {
    fields: BranchNodeFields<dyn SysObj, Self>,
    attrs: BTreeMap<SysStr, AttrSource>,
}

impl AttrDir {
    fn new(info: AttrDirInfo) -> Arc<Self> {
        let attrs = attr_sources(info.uevent, info.attrs, info.live_attrs, info.live_rw_attrs);

        Arc::new_cyclic(|weak_self| Self {
            fields: BranchNodeFields::new(info.name, virtual_attr_set(&attrs), weak_self.clone()),
            attrs,
        })
    }
}

inherit_sys_branch_node!(AttrDir, fields, {
    fn read_attr_at(
        &self,
        name: &str,
        offset: usize,
        writer: &mut VmWriter,
    ) -> SysTreeResult<usize> {
        read_attr_source(&self.attrs, name, offset, writer)
    }

    fn write_attr(&self, name: &str, reader: &mut VmReader) -> SysTreeResult<usize> {
        match self.attrs.get(name) {
            Some(AttrSource::LiveRw { write, .. }) => write(reader),
            _ => Err(SysTreeError::AttributeError),
        }
    }

    fn perms(&self) -> SysPerms {
        SysPerms::DEFAULT_RW_PERMS
    }
});

/// A directory of read-only attribute files, as a named `attribute_group` such as
/// an input device's `id/` and `capabilities/` appears in Linux sysfs.
#[derive(Debug)]
pub(crate) struct AttrGroup {
    fields: BranchNodeFields<dyn SysObj, Self>,
    /// The contents of every attribute file in the group.
    attrs: BTreeMap<SysStr, String>,
}

impl AttrGroup {
    fn new(name: SysStr, files: Vec<AttrFile>) -> Arc<Self> {
        let attrs: BTreeMap<SysStr, String> = files.into_iter().collect();

        Arc::new_cyclic(|weak_self| Self {
            fields: BranchNodeFields::new(name, read_only_attr_set(&attrs), weak_self.clone()),
            attrs,
        })
    }
}

inherit_sys_branch_node!(AttrGroup, fields, {
    fn read_attr_at(
        &self,
        name: &str,
        offset: usize,
        writer: &mut VmWriter,
    ) -> SysTreeResult<usize> {
        read_fixed_attr(&self.attrs, name, offset, writer)
    }

    fn write_attr(&self, _name: &str, _reader: &mut VmReader) -> SysTreeResult<usize> {
        Err(SysTreeError::AttributeError)
    }

    fn perms(&self) -> SysPerms {
        SysPerms::DEFAULT_RO_PERMS
    }
});

/// The part of a parent device's directory that publishing a character device in
/// it needs, which the two kinds of parent implement in different types.
trait ParentDir {
    fn add_child(&self, child: Arc<dyn SysObj>) -> SysTreeResult<()>;
    fn remove_child(&self, name: &str) -> SysTreeResult<Arc<dyn SysObj>>;
}

impl ParentDir for DirNode {
    fn add_child(&self, child: Arc<dyn SysObj>) -> SysTreeResult<()> {
        DirNode::add_child(self, child)
    }

    fn remove_child(&self, name: &str) -> SysTreeResult<Arc<dyn SysObj>> {
        DirNode::remove_child(self, name)
    }
}

impl ParentDir for BranchNodeFields<dyn SysObj, VirtualDevice> {
    fn add_child(&self, child: Arc<dyn SysObj>) -> SysTreeResult<()> {
        BranchNodeFields::add_child(self, child)
    }

    fn remove_child(&self, name: &str) -> SysTreeResult<Arc<dyn SysObj>> {
        BranchNodeFields::remove_child(self, name)
    }
}

/// Publishes a character device inside `parent`, along with the `/sys/class` and
/// `/sys/dev/char` entries that let user space find it by name and by
/// major/minor pair respectively.
///
/// `device_path` is the device's own directory relative to the sysfs root, which
/// is what those two entries point at, and `parent_link` is the target of the
/// device's `device` link.
fn publish_char_device(
    parent: &dyn ParentDir,
    device_path: String,
    parent_link: String,
    info: CharDeviceInfo,
) -> Result<Arc<CharDevice>> {
    let class_index = auxiliary::class_directory(info.class.clone())?;
    let char_device = CharDevice::new(&info, parent_link);
    let class_link = SymlinkNode::new(
        info.name.clone(),
        format!("{}/{}", to_sys_root(CLASS_INDEX_DEPTH), device_path),
    );

    parent.add_child(char_device.clone())?;
    // Roll back only entries this registration added. A class-name collision
    // belongs to another device and must remain reachable.
    if let Err(err) = class_index.add_child(class_link) {
        let _ = parent.remove_child(&info.name);
        return Err(err.into());
    }
    if let Err(err) =
        auxiliary::register_dev_link(DevNum::char(info.id), &format!("/{}", device_path))
    {
        let _ = parent.remove_child(&info.name);
        let _ = class_index.remove_child(&info.name);
        return Err(err.into());
    }

    Ok(char_device)
}

/// Builds an attribute set naming every attribute in `attrs`, all read only.
///
/// # Panics
///
/// Panics if there are more attributes than one sysfs node can hold, which is a
/// bug in the caller rather than a runtime condition.
fn read_only_attr_set<V>(attrs: &BTreeMap<SysStr, V>) -> SysAttrSet {
    let mut builder = SysAttrSetBuilder::new();
    for name in attrs.keys() {
        builder.add(name.clone(), SysPerms::DEFAULT_RO_ATTR_PERMS);
    }
    builder
        .build()
        .expect("a device has more attributes than a sysfs node can hold")
}

/// Same as [`read_only_attr_set`], except [`AttrSource::LiveRw`] files
/// get owner-write permission so `echo` to `brightness` is not `EACCES`.
fn virtual_attr_set(attrs: &BTreeMap<SysStr, AttrSource>) -> SysAttrSet {
    let mut builder = SysAttrSetBuilder::new();
    for (name, source) in attrs {
        let perms = match source {
            AttrSource::LiveRw { .. } => SysPerms::DEFAULT_RW_ATTR_PERMS,
            _ => SysPerms::DEFAULT_RO_ATTR_PERMS,
        };
        builder.add(name.clone(), perms);
    }
    builder
        .build()
        .expect("a device has more attributes than a sysfs node can hold")
}

/// Serves a read of an attribute whose contents were rendered at registration.
fn read_fixed_attr(
    attrs: &BTreeMap<SysStr, String>,
    name: &str,
    offset: usize,
    writer: &mut VmWriter,
) -> SysTreeResult<usize> {
    let value = attrs.get(name).ok_or(SysTreeError::AttributeError)?;

    let mut printer = VmPrinter::new_skip(writer, offset);
    write!(printer, "{}", value)?;
    Ok(printer.bytes_written())
}

/// Serves a virtual-device attribute, calling a live source when the
/// value is not the string stored at registration.
fn read_attr_source(
    attrs: &BTreeMap<SysStr, AttrSource>,
    name: &str,
    offset: usize,
    writer: &mut VmWriter,
) -> SysTreeResult<usize> {
    let source = attrs.get(name).ok_or(SysTreeError::AttributeError)?;
    let owned;
    let text: &str = match source {
        AttrSource::Fixed(text) => text,
        AttrSource::Live(read) | AttrSource::LiveRw { read, .. } => {
            owned = read().ok_or(SysTreeError::AttributeError)?;
            &owned
        }
    };

    let mut printer = VmPrinter::new_skip(writer, offset);
    write!(printer, "{}", text)?;
    Ok(printer.bytes_written())
}

inherit_sys_branch_node!(PlatformDevice, fields, {
    fn read_attr_at(
        &self,
        name: &str,
        offset: usize,
        writer: &mut VmWriter,
    ) -> SysTreeResult<usize> {
        if name != UEVENT_ATTR {
            return Err(SysTreeError::AttributeError);
        }

        let mut printer = VmPrinter::new_skip(writer, offset);
        write!(printer, "{}", self.uevent)?;
        Ok(printer.bytes_written())
    }

    fn write_attr(&self, _name: &str, _reader: &mut VmReader) -> SysTreeResult<usize> {
        // Divergence from Linux, which accepts writes to `uevent` to re-emit a
        // hotplug event. There is nothing listening for those here.
        Err(SysTreeError::AttributeError)
    }

    fn perms(&self) -> SysPerms {
        SysPerms::DEFAULT_RO_PERMS
    }
});

/// A character device belonging to a parent device and a class, as
/// `/sys/devices/platform/<device>/<class>/<name>` or
/// `/sys/devices/virtual/<class>/<device>/<name>`.
#[derive(Debug)]
pub(crate) struct CharDevice {
    fields: BranchNodeFields<dyn SysObj, Self>,
    /// The contents of the `uevent` and `dev` files, built once for the same
    /// reason as [`VirtualDevice::attrs`].
    attrs: BTreeMap<SysStr, String>,
}

impl CharDevice {
    /// Creates a character device entry whose `device` link points at
    /// `parent_link`.
    ///
    /// The target is the caller's business because it depends on how deep the
    /// device sits inside its parent, which differs between the two kinds of
    /// parent.
    fn new(info: &CharDeviceInfo, parent_link: String) -> Arc<Self> {
        let attrs = BTreeMap::from([
            (SysStr::from(UEVENT_ATTR), char_device_uevent(info)),
            (
                SysStr::from(DEV_ATTR),
                format!("{}\n", dev_char_name(info.id)),
            ),
        ]);

        let device = Arc::new_cyclic(|weak_self| Self {
            fields: BranchNodeFields::new(
                info.name.clone(),
                read_only_attr_set(&attrs),
                weak_self.clone(),
            ),
            attrs,
        });

        // The link `libdrm` follows to get from a major/minor pair to the device
        // that owns it. Linux creates it for every device that has both a parent
        // and a class, in `device_add_class_symlinks`.
        let parent = SymlinkNode::new(SysStr::from("device"), parent_link);
        let subsystem = SymlinkNode::new(
            SysStr::from(SUBSYSTEM_LINK),
            format!("{}/class/{}", to_sys_root(CLASS_DEVICE_DEPTH), info.class),
        );
        for link in [parent, subsystem] {
            device
                .fields
                .add_child(link)
                .expect("a fresh character device cannot have children");
        }

        device
    }
}

inherit_sys_branch_node!(CharDevice, fields, {
    fn read_attr_at(
        &self,
        name: &str,
        offset: usize,
        writer: &mut VmWriter,
    ) -> SysTreeResult<usize> {
        read_fixed_attr(&self.attrs, name, offset, writer)
    }

    fn write_attr(&self, _name: &str, _reader: &mut VmReader) -> SysTreeResult<usize> {
        Err(SysTreeError::AttributeError)
    }

    fn perms(&self) -> SysPerms {
        SysPerms::DEFAULT_RO_PERMS
    }
});

/// Builds the `uevent` contents of a platform device.
///
/// The keys and their order follow Linux's `platform_uevent` and
/// `of_device_uevent`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/of/device.c#L317>.
fn platform_uevent(info: &PlatformDeviceInfo) -> String {
    // Writing into a `String` cannot fail, hence the discarded results.
    let mut text = String::new();

    if let Some(driver) = &info.driver {
        let _ = writeln!(text, "DRIVER={}", driver);
    }
    let _ = writeln!(text, "OF_NAME={}", of_name(&info.of_fullname));
    let _ = writeln!(text, "OF_FULLNAME={}", info.of_fullname);
    for (index, compatible) in info.of_compatible.iter().enumerate() {
        let _ = writeln!(text, "OF_COMPATIBLE_{}={}", index, compatible);
    }
    let _ = writeln!(text, "OF_COMPATIBLE_N={}", info.of_compatible.len());
    let _ = writeln!(text, "MODALIAS={}", of_modalias(info));

    text
}

/// Builds the `MODALIAS` value of a device tree device: `of:N<name>T<type>` with
/// one `C<compatible>` per compatible string.
///
/// `libdrm` only falls back to this key when the `OF_*` keys are absent, but a
/// platform device without a `MODALIAS` would be unlike anything Linux produces.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/of/device.c#L248>.
fn of_modalias(info: &PlatformDeviceInfo) -> String {
    // A device tree node without a `device_type` property renders as `(null)`,
    // which is what most nodes, including K1's GPU, look like.
    let mut alias = format!("of:N{}T(null)", of_name(&info.of_fullname));
    for compatible in &info.of_compatible {
        let _ = write!(alias, "C{}", compatible);
    }
    alias
}

/// Builds the `uevent` contents of a character device.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/base/core.c#L2453>.
fn char_device_uevent(info: &CharDeviceInfo) -> String {
    let mut text = String::new();

    let _ = writeln!(text, "MAJOR={}", info.id.major().get());
    let _ = writeln!(text, "MINOR={}", info.id.minor().get());
    if let Some(dev_type) = &info.dev_type {
        let _ = writeln!(text, "DEVTYPE={}", dev_type);
    }
    let _ = writeln!(text, "DEVNAME={}", info.dev_name);

    text
}

#[cfg(ktest)]
mod test {
    use device_id::{MajorId, MinorId};
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        fs::{
            file::{InodeType, StatusFlags},
            sysfs::fs::SysFs,
            vfs::{
                file_system::FileSystem,
                inode::{Inode, SymbolicLink},
            },
        },
        time::clocks::init_for_ktest as time_init_for_ktest,
    };

    /// The DRM major number, which is what makes these tests about `libdrm`
    /// rather than about sysfs in general.
    const DRM_MAJOR: u16 = 226;

    fn sysfs_root() -> Arc<dyn Inode> {
        time_init_for_ktest();
        crate::device::init_memory_for_ktest();
        init();
        SysFs::new_for_ktest().root_inode()
    }

    /// Exercise the actual mem registration before the adapter, then enumerate
    /// two DRM minors through the same sysfs inode layer used by libdrm.
    #[ktest]
    fn managed_mem_and_auxiliary_drm_share_device_roots() {
        let root = sysfs_root();
        // The setup is idempotent even if another sysfs test has already run.
        crate::device::init_memory_for_ktest();
        init();
        for (name, minor) in [
            ("null", 3),
            ("zero", 5),
            ("full", 7),
            ("random", 8),
            ("urandom", 9),
        ] {
            let direct = resolve(&root, &format!("devices/virtual/mem/{}", name)).unwrap();
            let class = resolve(&root, &format!("class/mem/{}", name)).unwrap();
            let index = resolve(&root, &format!("dev/char/1:{}", minor)).unwrap();
            assert_eq!(direct.ino(), class.ino());
            assert_eq!(direct.ino(), index.ino());
            assert_eq!(
                read_to_string(&resolve(&root, &format!("dev/char/1:{}/dev", minor)).unwrap()),
                format!("1:{}\n", minor)
            );
        }

        let platform = register_platform_device(PlatformDeviceInfo {
            name: "coexist-gpu".into(),
            of_fullname: "/soc/coexist-gpu".into(),
            of_compatible: vec!["img,img-rogue".into()],
            driver: Some("powervr".into()),
        })
        .unwrap();
        for (name, minor) in [("card-coexist", 240), ("render-coexist", 241)] {
            platform
                .register_char_device(CharDeviceInfo {
                    class: "drm".into(),
                    name: name.into(),
                    dev_name: format!("dri/{}", name),
                    dev_type: Some("drm_minor".into()),
                    id: DeviceId::new(MajorId::new(DRM_MAJOR), MinorId::new(minor)),
                })
                .unwrap();
            let by_number = resolve(&root, &format!("dev/char/226:{}", minor)).unwrap();
            let by_class = resolve(&root, &format!("class/drm/{}", name)).unwrap();
            assert_eq!(by_number.ino(), by_class.ino());
            let drm = resolve(&root, &format!("dev/char/226:{}/device/drm", minor)).unwrap();
            assert!(drm.lookup(name).is_ok());
            assert_eq!(
                read_link(
                    &resolve_no_follow(&root, &format!("dev/char/226:{}/device/subsystem", minor))
                        .unwrap()
                ),
                "../../../bus/platform"
            );
        }
        // A collision with a managed mem index rolls back only the new K1
        // publication; the existing mem node and its index must survive.
        let before = resolve(&root, "dev/char/1:3").unwrap();
        assert!(
            platform
                .register_char_device(CharDeviceInfo {
                    class: "drm".into(),
                    name: "mem-collision".into(),
                    dev_name: "dri/mem-collision".into(),
                    dev_type: None,
                    id: DeviceId::new(MajorId::new(1), MinorId::new(3)),
                })
                .is_err()
        );
        assert_eq!(before.ino(), resolve(&root, "dev/char/1:3").unwrap().ino());
        assert!(resolve(&root, "class/drm/mem-collision").is_err());
        assert!(resolve(&root, "devices/platform/coexist-gpu/drm/mem-collision").is_err());
        assert!(class_index("mem".into()).is_err());
        assert!(auxiliary::virtual_directory("mem".into()).is_err());
    }

    /// Resolves a sysfs path the way the kernel resolves it for user space,
    /// following symbolic links in intermediate components.
    ///
    /// The point of walking the tree this way in a test is that a relative link
    /// target with the wrong number of `..` components resolves to nothing, and
    /// nothing in the kernel would ever notice.
    fn resolve(dir: &Arc<dyn Inode>, path: &str) -> Result<Arc<dyn Inode>> {
        // Every inode met on the way is kept alive: a sysfs inode reaches its
        // parent through a weak reference, so letting an ancestor go would turn a
        // `..` component into a no-op and quietly change what a relative link
        // means. The real path resolver holds the chain through its dentries.
        let mut visited = Vec::new();
        resolve_with_depth(dir, path, 0, &mut visited)
    }

    /// Resolves a sysfs path without following a symbolic link in its last
    /// component, which is what `readlink` does.
    fn resolve_no_follow(dir: &Arc<dyn Inode>, path: &str) -> Result<Arc<dyn Inode>> {
        let (parent, name) = path.rsplit_once('/').unwrap_or((".", path));
        resolve(dir, parent)?.lookup(name)
    }

    fn resolve_with_depth(
        dir: &Arc<dyn Inode>,
        path: &str,
        depth: usize,
        visited: &mut Vec<Arc<dyn Inode>>,
    ) -> Result<Arc<dyn Inode>> {
        assert!(depth < 8, "too many levels of symbolic links");

        let mut current = dir.clone();
        visited.push(current.clone());
        for component in path.split('/').filter(|name| !name.is_empty()) {
            let next = current.lookup(component)?;
            visited.push(next.clone());
            current = if next.type_() == InodeType::SymLink {
                let SymbolicLink::Plain(target) = next.read_link()? else {
                    panic!("a sysfs symbolic link is not a plain one");
                };
                resolve_with_depth(&current, &target, depth + 1, visited)?
            } else {
                next
            };
            visited.push(current.clone());
        }

        Ok(current)
    }

    fn read_to_string(inode: &Arc<dyn Inode>) -> String {
        let mut buf = [0u8; 512];
        let mut writer = VmWriter::from(&mut buf[..]).to_fallible();
        let len = inode
            .read_at(0, &mut writer, StatusFlags::empty())
            .expect("failed to read a sysfs attribute");
        core::str::from_utf8(&buf[..len]).unwrap().to_string()
    }

    fn read_link(inode: &Arc<dyn Inode>) -> String {
        let SymbolicLink::Plain(target) = inode.read_link().expect("not a symbolic link") else {
            panic!("a sysfs symbolic link is not a plain one");
        };
        target
    }

    /// Registers a platform device with one character device, as a K1 GPU would.
    fn register_gpu(suffix: &str, minor: u32) -> (String, DeviceId) {
        let device_name = format!("gpu@{}", suffix);
        let device = register_platform_device(PlatformDeviceInfo {
            name: SysStr::from(device_name.clone()),
            of_fullname: format!("/soc/gpu@{}", suffix),
            of_compatible: vec!["img,img-bxe-2-32".to_string(), "img,img-rogue".to_string()],
            driver: Some("powervr".to_string()),
        })
        .expect("failed to register a platform device");

        let id = DeviceId::new(MajorId::new(DRM_MAJOR), MinorId::new(minor));
        device
            .register_char_device(CharDeviceInfo {
                class: SysStr::from("drm"),
                name: SysStr::from(format!("renderD{}", minor)),
                dev_name: format!("dri/renderD{}", minor),
                dev_type: Some("drm_minor".to_string()),
                id,
            })
            .expect("failed to register a character device");

        (device_name, id)
    }

    /// `drmParseOFBusInfo` and `drmParseOFDeviceInfo` read these keys, and a
    /// device missing any of them is dropped from the enumeration.
    #[ktest]
    fn platform_uevent_has_the_keys_libdrm_needs() {
        let root = sysfs_root();
        let (device_name, _) = register_gpu("d8100000", 128);

        let uevent = resolve(&root, &format!("devices/platform/{}/uevent", device_name))
            .expect("the platform device has no uevent file");
        let text = read_to_string(&uevent);

        assert!(text.contains("OF_FULLNAME=/soc/gpu@d8100000\n"));
        assert!(text.contains("OF_COMPATIBLE_0=img,img-bxe-2-32\n"));
        assert!(text.contains("OF_COMPATIBLE_1=img,img-rogue\n"));
        assert!(text.contains("OF_COMPATIBLE_N=2\n"));
        assert!(text.contains("OF_NAME=gpu\n"));
        assert!(text.contains("DRIVER=powervr\n"));
        assert!(text.contains("MODALIAS=of:NgpuT(null)Cimg,img-bxe-2-32Cimg,img-rogue\n"));
    }

    /// `drmParseSubsystemType` takes the last `/`-separated component of the link
    /// target and compares it against `/platform`. This test does the same, since
    /// any other target silently means "not a platform device".
    #[ktest]
    fn subsystem_link_names_the_platform_bus() {
        let root = sysfs_root();
        let (_, id) = register_gpu("d8100001", 129);

        let path = format!("dev/char/{}/device/subsystem", dev_char_name(id));
        let subsystem = resolve_no_follow(&root, &path).expect("the device has no subsystem link");
        let target = read_link(&subsystem);

        assert_eq!(target, "../../../bus/platform");
        let last_component = &target[target.rfind('/').unwrap()..];
        assert_eq!(last_component, "/platform");

        // The target has to resolve, too: `libdrm` only reads the link, but one
        // that points into nowhere would mean the model contradicts itself.
        let bus = resolve(&root, &path).expect("the subsystem link does not resolve");
        assert_eq!(bus.type_(), InodeType::Dir);
    }

    /// Everything `drmGetDevices2` does with a `/dev/dri` node, in order.
    #[ktest]
    fn char_device_entry_answers_every_libdrm_probe() {
        let root = sysfs_root();
        let (device_name, id) = register_gpu("d8100002", 130);
        let entry = format!("dev/char/{}", dev_char_name(id));

        // `drmNodeIsDRM`: `stat` of `device/drm`.
        let drm_dir =
            resolve(&root, &format!("{}/device/drm", entry)).expect("device/drm does not exist");
        assert_eq!(drm_dir.type_(), InodeType::Dir);

        // `drmGetMinorNameForFD`: the node's name appears in that directory.
        let minor_name = format!("renderD{}", 130);
        assert!(drm_dir.lookup(&minor_name).is_ok());

        // `drmParseOFBusInfo`: the parent's `uevent` is reachable through the
        // `device` link.
        let parent_uevent = resolve(&root, &format!("{}/device/uevent", entry))
            .expect("device/uevent does not exist");
        assert!(read_to_string(&parent_uevent).contains("OF_FULLNAME=/soc/gpu@d8100002\n"));

        // `drmGetDeviceNameFromFd2`: the node's own `uevent` carries `DEVNAME`.
        let uevent = resolve(&root, &format!("{}/uevent", entry))
            .expect("the character device has no uevent file");
        let text = read_to_string(&uevent);
        assert!(text.contains(&format!("DEVNAME=dri/{}\n", minor_name)));
        assert!(text.contains(&format!("MAJOR={}\n", DRM_MAJOR)));
        assert!(text.contains("MINOR=130\n"));
        assert!(text.contains("DEVTYPE=drm_minor\n"));

        // The `device` link resolves to the platform device itself, rather than
        // to something that merely happens to have the right files.
        let parent = resolve(&root, &format!("{}/device", entry)).unwrap();
        let device_dir = resolve(&root, &format!("devices/platform/{}", device_name)).unwrap();
        assert_eq!(parent.ino(), device_dir.ino());
    }

    /// The entries that are not `libdrm`'s business but that Linux always has,
    /// and that udev-based enumeration walks.
    #[ktest]
    fn class_and_bus_entries_point_back_at_the_device() {
        let root = sysfs_root();
        let (device_name, id) = register_gpu("d8100003", 131);

        let device_dir = resolve(&root, &format!("devices/platform/{}", device_name)).unwrap();
        let bus_entry = resolve(&root, &format!("bus/platform/devices/{}", device_name))
            .expect("the platform bus has no entry for the device");
        assert_eq!(bus_entry.ino(), device_dir.ino());

        let minor_dir = resolve(&root, &format!("dev/char/{}", dev_char_name(id))).unwrap();
        let class_entry = resolve(&root, &format!("class/drm/renderD{}", 131))
            .expect("the drm class has no entry for the device");
        assert_eq!(class_entry.ino(), minor_dir.ino());
    }

    /// The evdev major number, which is what makes the tests below about the
    /// input class rather than about sysfs in general.
    const EVDEV_MAJOR: u16 = 13;

    /// Registers an input device with one evdev node under it, the way the input
    /// class does.
    ///
    /// Every test registers its own device because the `SysTree` singleton is
    /// shared by every test in the binary.
    fn register_input(index: u32, minor: u32) -> (String, DeviceId) {
        let device_name = format!("input{}", index);
        let device = register_virtual_device(VirtualDeviceInfo {
            class: SysStr::from("input"),
            name: SysStr::from(device_name.clone()),
            uevent: "PRODUCT=3/1234/5678/111\nNAME=\"Fake Keyboard\"\nEV=3\nKEY=50000002\n"
                .to_string(),
            attrs: vec![(SysStr::from("name"), "Fake Keyboard\n".to_string())],
            attr_groups: vec![
                (
                    SysStr::from("id"),
                    vec![(SysStr::from("bustype"), "0003\n".to_string())],
                ),
                (
                    SysStr::from("capabilities"),
                    vec![(SysStr::from("ev"), "3\n".to_string())],
                ),
            ],
            live_attrs: Vec::new(),
            live_rw_attrs: Vec::new(),
        })
        .expect("failed to register a virtual device");

        let id = DeviceId::new(MajorId::new(EVDEV_MAJOR), MinorId::new(minor));
        device
            .register_char_device(CharDeviceInfo {
                class: SysStr::from("input"),
                name: SysStr::from(format!("event{}", minor)),
                dev_name: format!("input/event{}", minor),
                dev_type: None,
                id,
            })
            .expect("failed to register a character device");

        (device_name, id)
    }

    /// Everything `libudev` does when it enumerates the `input` subsystem and
    /// hands a device to `libinput`, in order: find the entry under
    /// `/sys/class/input`, read its `uevent` for the device node, and read the
    /// `subsystem` link to decide the subsystem.
    #[ktest]
    fn an_input_node_answers_every_libudev_probe() {
        let root = sysfs_root();
        let (device_name, id) = register_input(0, 0);
        let entry = "class/input/event0";

        // `sd_device_new_from_syspath` refuses a device under `/sys/devices` that
        // has no `uevent` file, whatever else it has.
        let uevent = resolve(&root, &format!("{}/uevent", entry))
            .expect("the evdev node has no uevent file");
        let text = read_to_string(&uevent);
        assert!(text.contains(&format!("MAJOR={}\n", EVDEV_MAJOR)));
        assert!(text.contains("MINOR=0\n"));
        assert!(text.contains("DEVNAME=input/event0\n"));

        // `sd_device_get_subsystem` reads this link and takes its last component;
        // a device with no subsystem matches no subsystem filter.
        let subsystem = resolve_no_follow(&root, &format!("{}/subsystem", entry))
            .expect("the evdev node has no subsystem link");
        let target = read_link(&subsystem);
        assert_eq!(&target[target.rfind('/').unwrap()..], "/input");
        assert_eq!(
            resolve(&root, &format!("{}/subsystem", entry))
                .expect("the subsystem link does not resolve")
                .type_(),
            InodeType::Dir
        );

        // The bitmaps `input_id` computes `ID_INPUT*` from live on the parent, and
        // it walks up to it from the evdev node.
        let parent = resolve(&root, &format!("{}/device", entry))
            .expect("the evdev node has no device link");
        let device_dir = resolve(&root, &format!("devices/virtual/input/{}", device_name)).unwrap();
        assert_eq!(parent.ino(), device_dir.ino());
        let capability = resolve(&root, &format!("{}/device/capabilities/ev", entry))
            .expect("the input device has no capabilities group");
        assert_eq!(read_to_string(&capability), "3\n");

        // Both entries of the class index, and the major/minor lookup.
        let by_number = resolve(&root, &format!("dev/char/{}", dev_char_name(id)))
            .expect("the evdev node has no /sys/dev/char entry");
        let by_name = resolve(&root, entry).unwrap();
        assert_eq!(by_number.ino(), by_name.ino());
        assert_eq!(
            resolve(&root, &format!("class/input/{}", device_name))
                .expect("the input class has no entry for the input device")
                .ino(),
            device_dir.ino()
        );
    }

    /// A device whose parent is itself in a class goes directly into the parent's
    /// directory, with no class-named level in between. Adding one would move
    /// every relative link target by a component.
    #[ktest]
    fn a_virtual_device_holds_its_character_devices_directly() {
        let root = sysfs_root();
        let (device_name, _) = register_input(1, 1);

        let path = format!("devices/virtual/input/{}/event1", device_name);
        assert!(resolve(&root, &path).is_ok());
        assert!(
            resolve(
                &root,
                &format!("devices/virtual/input/{}/input/event1", device_name)
            )
            .is_err()
        );
    }

    /// Linux gives every device with a device node a `dev` attribute holding its
    /// numbers, and the shell assertions this port carries read it to check that
    /// the sysfs entry and the device node agree.
    #[ktest]
    fn a_character_device_reports_its_numbers_in_the_dev_attribute() {
        let root = sysfs_root();
        let (_, id) = register_input(2, 2);

        let dev = resolve(&root, "class/input/event2/dev").expect("the evdev node has no dev file");
        assert_eq!(read_to_string(&dev), format!("{}\n", dev_char_name(id)));
    }

    /// A second registration of the same major/minor pair must fail rather than
    /// overwrite, because the pair is what identifies the device to user space.
    #[ktest]
    fn duplicate_registrations_are_rejected() {
        let _root = sysfs_root();
        register_gpu("d8100004", 132);

        let duplicate = register_platform_device(PlatformDeviceInfo {
            name: SysStr::from("gpu@d8100004"),
            of_fullname: "/soc/gpu@d8100004".to_string(),
            of_compatible: vec!["img,img-rogue".to_string()],
            driver: None,
        });
        assert!(duplicate.is_err());

        register_input(3, 3);
        let duplicate = register_virtual_device(VirtualDeviceInfo {
            class: SysStr::from("input"),
            name: SysStr::from("input3"),
            uevent: String::new(),
            attrs: Vec::new(),
            attr_groups: Vec::new(),
            live_attrs: Vec::new(),
            live_rw_attrs: Vec::new(),
        });
        assert!(duplicate.is_err());
    }

    #[ktest]
    fn a_device_without_device_tree_data_still_has_a_modalias() {
        let info = PlatformDeviceInfo {
            name: SysStr::from("soft"),
            of_fullname: "/soft".to_string(),
            of_compatible: Vec::new(),
            driver: None,
        };

        let text = platform_uevent(&info);
        assert!(text.contains("OF_COMPATIBLE_N=0\n"));
        assert!(text.contains("MODALIAS=of:NsoftT(null)\n"));
    }

    /// A failed publication must not remove the class link of an earlier device.
    #[ktest]
    fn char_device_collision_preserves_existing_links() {
        let root = sysfs_root();
        let first = register_platform_device(PlatformDeviceInfo {
            name: SysStr::from("collision-first"),
            of_fullname: "/collision-first".to_string(),
            of_compatible: Vec::new(),
            driver: None,
        })
        .unwrap();
        let second = register_platform_device(PlatformDeviceInfo {
            name: SysStr::from("collision-second"),
            of_fullname: "/collision-second".to_string(),
            of_compatible: Vec::new(),
            driver: None,
        })
        .unwrap();
        let first_id = DeviceId::new(MajorId::new(DRM_MAJOR), MinorId::new(4000));
        let info = CharDeviceInfo {
            class: SysStr::from("collision-test"),
            name: SysStr::from("node0"),
            dev_name: "collision-test/node0".to_string(),
            dev_type: None,
            id: first_id,
        };
        first.register_char_device(info.clone()).unwrap();
        let original = resolve(&root, "class/collision-test/node0").unwrap();
        let mut duplicate_name = info.clone();
        duplicate_name.id = DeviceId::new(MajorId::new(DRM_MAJOR), MinorId::new(4001));
        assert!(second.register_char_device(duplicate_name).is_err());
        assert_eq!(
            resolve(&root, "class/collision-test/node0").unwrap().ino(),
            original.ino()
        );
        assert!(
            resolve(
                &root,
                "devices/platform/collision-second/collision-test/node0"
            )
            .is_err()
        );
        let mut duplicate_number = info;
        duplicate_number.name = SysStr::from("node1");
        assert!(second.register_char_device(duplicate_number).is_err());
        assert!(resolve(&root, "class/collision-test/node1").is_err());
        assert_eq!(
            resolve(&root, &format!("dev/char/{}", dev_char_name(first_id)))
                .unwrap()
                .ino(),
            original.ino()
        );
    }

    fn attr_dir_read_cur() -> Option<String> {
        Some(String::from("1228800\n"))
    }

    fn attr_dir_write_gov(reader: &mut VmReader) -> aster_systree::Result<usize> {
        let (_cstr, n) = reader
            .read_cstring_until_end(32)
            .map_err(|_| aster_systree::Error::PageFault)?;
        Ok(n)
    }

    /// A cpufreq policy is not a virtual device: it sits under
    /// `/sys/devices/system/cpu/cpufreq` and is reached from both
    /// `/sys/class/cpufreq` and a per-CPU symlink.
    #[ktest]
    fn attr_dir_can_sit_under_devices_system() {
        let root = sysfs_root();

        let system = system_devices_dir().expect("system");
        let cpu = system.get_or_create_dir(SysStr::from("cpu")).expect("cpu");
        let cpufreq = cpu
            .get_or_create_dir(SysStr::from("cpufreq"))
            .expect("cpufreq");

        register_attr_dir(
            &cpufreq,
            AttrDirInfo {
                name: SysStr::from("policy_b2b"),
                uevent: String::new(),
                attrs: vec![(SysStr::from("scaling_driver"), "k1-cpufreq\n".to_string())],
                live_attrs: vec![(SysStr::from("scaling_cur_freq"), attr_dir_read_cur)],
                live_rw_attrs: vec![(
                    SysStr::from("scaling_governor"),
                    attr_dir_read_cur,
                    attr_dir_write_gov,
                )],
            },
        )
        .expect("failed to register an attr dir");

        let class = class_index(SysStr::from("cpufreq")).expect("class");
        class
            .add_child(SymlinkNode::new(
                SysStr::from("policy_b2b"),
                String::from("../../devices/system/cpu/cpufreq/policy_b2b"),
            ))
            .expect("class symlink");

        let cpu0 = cpu.get_or_create_dir(SysStr::from("cpu0")).expect("cpu0");
        // Distinct name so this test does not collide with a later `cpu0/cpufreq`.
        cpu0.add_child(SymlinkNode::new(
            SysStr::from("cpufreq_b2b"),
            String::from("../cpufreq/policy_b2b"),
        ))
        .expect("cpu0 symlink");

        let policy =
            resolve(&root, "devices/system/cpu/cpufreq/policy_b2b").expect("policy dir missing");
        assert_eq!(policy.type_(), InodeType::Dir);

        let cur = resolve(
            &root,
            "devices/system/cpu/cpufreq/policy_b2b/scaling_cur_freq",
        )
        .expect("live attr missing");
        assert_eq!(read_to_string(&cur), "1228800\n");

        let via_class =
            resolve(&root, "class/cpufreq/policy_b2b").expect("class symlink does not resolve");
        assert_eq!(via_class.ino(), policy.ino());

        let via_cpu = resolve(&root, "devices/system/cpu/cpu0/cpufreq_b2b")
            .expect("cpu symlink does not resolve");
        assert_eq!(via_cpu.ino(), policy.ino());
    }
}
