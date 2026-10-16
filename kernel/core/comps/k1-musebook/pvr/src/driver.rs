// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: safe PowerVR DRM file ownership and ioctl dispatch.

//! The `k1-musebook-drm` driver implementation and fourteen ioctl handlers.
//!
//! Ports `pvr_drv.c`.
//!
//! This is a render-only device. The K1 DPU exposes scanout through a separate DRM
//! device; Mesa matches this driver's `powervr` name on `/dev/dri/renderD*`.
//!
//! Handlers receive a `DrmDevice` and a `DrmFile`. [`driver_of`] downcasts the
//! device's driver object; per-open state lives in the file's driver-private slot.
//! This keeps ownership local to each device and file.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};

use k1_musebook_drm::{
    DrmError, DrmResult, PlatformIdentity,
    device::{DriverFeatures, DriverInfo, DrmDevice, DrmDriver},
    file::DrmFile,
    gem::{GemHandle, GemObject},
    ioctl::{CallerContext, IoctlDesc, IoctlFlags, payload_read, payload_write},
    prime::DmaBuf,
};
use ostd::sync::SpinLock;
use ostd_pod::{IntoBytes, Pod};

use crate::{
    device::PvrDevice,
    device_info::DeviceInfo,
    error::{PvrError, PvrResult},
    mm::{
        gem::PvrObject,
        mmu::DEVICE_PAGE_SHIFT,
        vm::{DeviceAddr, VmContext, VmContextKind},
    },
    render::{free_list::FreeList, hwrt::HwrtDataset},
    rogue::heap,
    sched::context::{Context, ContextType, Priority},
    uapi,
};

/// The name and version reported by `DRM_IOCTL_VERSION`.
///
/// These are ABI: Mesa's loader matches on the name, so it must be `powervr`
/// exactly, matching upstream's `pvr_drm_driver`.
pub const DRIVER_INFO: DriverInfo = DriverInfo {
    name: "powervr",
    desc: "Imagination PowerVR (Series 6 and later) Graphics",
    date: "20230904",
    major: 1,
    minor: 0,
    patchlevel: 0,
};

/// The feature bits reported through [`k1_musebook_drm::device::DrmDriver::features`].
///
/// No `MODESET` and no `ATOMIC`: see the module documentation.
pub const DRIVER_FEATURES: DriverFeatures = DriverFeatures::from_bits_truncate(
    DriverFeatures::RENDER.bits()
        | DriverFeatures::GEM.bits()
        | DriverFeatures::SYNCOBJ.bits()
        | DriverFeatures::SYNCOBJ_TIMELINE.bits(),
);

/// The driver-private ioctl dispatch table.
///
/// Every entry is `RENDER_ALLOW` and none is `MASTER`: this is a render-only
/// driver, so a client never needs to be DRM master. Indexed by
/// [`uapi::PvrIoctl`], offset by [`uapi::DRM_COMMAND_BASE`].
///
/// A command number that is not listed is reported as `EINVAL`, which is what
/// Linux does for a driver table's empty slots. All fourteen commands are listed.
pub static PVR_IOCTLS: &[IoctlDesc] = &[
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::DevQuery as u32,
        name: "PVR_DEV_QUERY",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_dev_query,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::CreateBo as u32,
        name: "PVR_CREATE_BO",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_create_bo,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::GetBoMmapOffset as u32,
        name: "PVR_GET_BO_MMAP_OFFSET",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_get_bo_mmap_offset,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::CreateVmContext as u32,
        name: "PVR_CREATE_VM_CONTEXT",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_create_vm_context,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::DestroyVmContext as u32,
        name: "PVR_DESTROY_VM_CONTEXT",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_destroy_vm_context,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::VmMap as u32,
        name: "PVR_VM_MAP",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_vm_map,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::VmUnmap as u32,
        name: "PVR_VM_UNMAP",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_vm_unmap,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::CreateContext as u32,
        name: "PVR_CREATE_CONTEXT",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_create_context,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::DestroyContext as u32,
        name: "PVR_DESTROY_CONTEXT",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_destroy_context,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::SubmitJobs as u32,
        name: "PVR_SUBMIT_JOBS",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_submit_jobs,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::CreateFreeList as u32,
        name: "PVR_CREATE_FREE_LIST",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_create_free_list,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::DestroyFreeList as u32,
        name: "PVR_DESTROY_FREE_LIST",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_destroy_free_list,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::CreateHwrtDataset as u32,
        name: "PVR_CREATE_HWRT_DATASET",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_create_hwrt_dataset,
    },
    IoctlDesc {
        nr: uapi::DRM_COMMAND_BASE + uapi::PvrIoctl::DestroyHwrtDataset as u32,
        name: "PVR_DESTROY_HWRT_DATASET",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: ioctl_destroy_hwrt_dataset,
    },
];

/// A file's table of one kind of driver object.
///
/// Upstream uses an `xarray` per kind, initialized with `XA_FLAGS_ALLOC1`, so
/// handles start at one and zero is never valid. Freed handles are reused, which
/// this reproduces by handing out the lowest free number — user space
/// occasionally assumes small handle values, and a monotonic counter would also
/// eventually wrap.
#[derive(Debug)]
pub struct HandleTable<T> {
    objects: SpinLock<BTreeMap<u32, Arc<T>>>,
}

impl<T> HandleTable<T> {
    /// Creates an empty table.
    pub fn new() -> Self {
        Self {
            objects: SpinLock::new(BTreeMap::new()),
        }
    }

    /// Installs an object and returns its new handle.
    pub fn insert(&self, object: Arc<T>) -> PvrResult<u32> {
        let mut objects = self.objects.lock();

        // The lowest unused handle at or above one. Keys are visited in order, so
        // the first gap is the answer.
        let mut handle = 1u32;
        for key in objects.keys() {
            if *key > handle {
                break;
            }
            handle = key.checked_add(1).ok_or(PvrError::NoMemory)?;
        }

        objects.insert(handle, object);
        Ok(handle)
    }

    /// Resolves a handle.
    ///
    /// Upstream's `xa_load()` miss becomes `-EINVAL` at most call sites, which is
    /// what [`PvrError::NoSuchHandle`] maps to.
    pub fn get(&self, handle: u32) -> PvrResult<Arc<T>> {
        self.objects
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(PvrError::NoSuchHandle)
    }

    /// Removes a handle and returns what it referred to.
    pub fn remove(&self, handle: u32) -> PvrResult<Arc<T>> {
        self.objects
            .lock()
            .remove(&handle)
            .ok_or(PvrError::NoSuchHandle)
    }

    /// Empties the table, returning everything it held.
    ///
    /// Used by [`PvrFile`]'s teardown, which must drop the kinds in a defined
    /// order.
    pub fn drain(&self) -> Vec<Arc<T>> {
        let objects = {
            let mut handles = self.objects.lock();
            core::mem::take(&mut *handles)
        };
        objects.into_values().collect()
    }

    /// Returns how many handles are live.
    pub fn len(&self) -> usize {
        self.objects.lock().len()
    }

    /// Returns whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Default for HandleTable<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-open state.
///
/// Handles are per-file, so closing a file must tear down every context, free
/// list and dataset it created. Upstream relies on `xarray` destruction order
/// for this; we make it explicit in [`Drop`] because getting it wrong leaks GPU
/// address space rather than failing loudly.
pub struct PvrFile {
    /// The device this file was opened on.
    pub device: Arc<PvrDevice>,
    /// GPU address spaces created through `CREATE_VM_CONTEXT`.
    pub vm_contexts: HandleTable<VmContext>,
    /// Render, compute, and transfer contexts.
    pub render_contexts: HandleTable<Context>,
    /// Parameter-manager free lists.
    pub free_lists: HandleTable<FreeList>,
    /// Hardware render target datasets.
    pub hwrt_datasets: HandleTable<HwrtDataset>,
}

impl PvrFile {
    /// Gives up this file's objects in upstream's order.
    ///
    /// Idempotent: both `postclose` and the destructor may call this method.
    /// Firmware cleanup requires a power reference and an acknowledgement wait, so
    /// it cannot run directly from a destructor that may execute in any context.
    ///
    /// Objects are killed first to reject new work, then handed to [`crate::service`]
    /// while still owned. Contexts precede free lists and datasets because their jobs
    /// may name those resources. The service keeps each object alive until cleanup
    /// returns. Address-space handles are dropped here; dependent objects retain the
    /// references needed by their own teardown.
    pub(crate) fn teardown(&self) {
        use crate::service::Doomed;

        for context in self.render_contexts.drain() {
            context.kill();
            crate::service::enqueue(&self.device, Doomed::Context(context));
        }
        for free_list in self.free_lists.drain() {
            crate::service::enqueue(&self.device, Doomed::FreeList(free_list));
        }
        for hwrt in self.hwrt_datasets.drain() {
            crate::service::enqueue(&self.device, Doomed::Hwrt(hwrt));
        }
        drop(self.vm_contexts.drain());
    }

    /// Creates the per-open state for a file on `device`.
    pub fn new(device: Arc<PvrDevice>) -> Arc<Self> {
        Arc::new(Self {
            device,
            vm_contexts: HandleTable::new(),
            render_contexts: HandleTable::new(),
            free_lists: HandleTable::new(),
            hwrt_datasets: HandleTable::new(),
        })
    }

    /// Resolves a GEM handle to its buffer object.
    ///
    /// Ports upstream `pvr_gem_object_from_handle()`. An unknown handle or a handle
    /// whose object is not a [`PvrObject`] returns [`DrmError::NotFound`] (`ENOENT`).
    /// [`PvrDriver::import_buffer`] wraps imported dma-buf frames in a `PvrObject`,
    /// so PRIME buffers use this same path. A wrong object type is reported once to
    /// distinguish it from an absent handle without changing the ioctl errno.
    fn object_from_handle(drm_file: &DrmFile, handle: u32) -> DrmResult<Arc<PvrObject>> {
        let gem = drm_file.handles().get(GemHandle(handle))?;
        Arc::downcast(gem.into_any()).map_err(|_| {
            Self::report_foreign_object();
            DrmError::NotFound
        })
    }

    /// Reports a handle whose object type cannot be mapped by this driver.
    ///
    /// The warning is emitted once to bound output from clients retrying per frame.
    fn report_foreign_object() {
        static REPORTED: AtomicBool = AtomicBool::new(false);

        if REPORTED.swap(true, Ordering::Relaxed) {
            return;
        }
        ostd::warn!(
            "[k1-gpu] a GEM handle resolved to a non-PowerVR object; refusing GPU VM mapping"
        );
    }
}

impl Drop for PvrFile {
    /// Tears the file's objects down in upstream's order.
    ///
    /// `pvr_drm_driver_postclose()` kills contexts, then free lists, then HWRT
    /// datasets, then VM contexts, and the order is not arbitrary: a context can
    /// still be running work that refers to a free list, and every one of them
    /// holds mappings in a VM context. Dropping the address space first would
    /// unmap memory the firmware is still reading.
    ///
    /// Buffer objects need no explicit teardown here: their handles live in
    /// `k1-musebook-drm`'s per-file table, which drops with the [`DrmFile`], and any
    /// still-mapped object is kept alive by the VM context that holds it.
    fn drop(&mut self) {
        self.teardown();
    }
}

/// The `k1-musebook-drm` driver backed by one [`PvrDevice`].
pub struct PvrDriver {
    device: Arc<PvrDevice>,
}

impl core::fmt::Debug for PvrDriver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PvrDriver")
            .field("gpu_id", &self.device.gpu_id)
            .finish_non_exhaustive()
    }
}

impl PvrDriver {
    /// Wraps a probed device.
    pub fn new(device: Arc<PvrDevice>) -> Arc<Self> {
        Arc::new(Self { device })
    }

    /// Returns the GPU this driver drives.
    pub fn device(&self) -> &Arc<PvrDevice> {
        &self.device
    }

    /// Returns the per-open state belonging to a DRM file.
    fn file_of(&self, file: &DrmFile) -> DrmResult<Arc<PvrFile>> {
        // A file the driver never accepted cannot reach a driver ioctl, so a
        // miss here means `open` did not run rather than that user space did
        // something wrong.
        file.driver_priv::<PvrFile>().ok_or(DrmError::NoDevice)
    }
}

impl DrmDriver for PvrDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DRIVER_INFO.clone()
    }

    fn features(&self) -> DriverFeatures {
        DRIVER_FEATURES
    }

    /// Reports the platform's device identity as its bus address.
    ///
    /// The platform name remains stable for the device's lifetime. Modern `libdrm`
    /// enumerates platform devices through the corresponding sysfs identity.
    fn unique(&self) -> String {
        "platform:imggpu:00".to_string()
    }

    fn private_ioctls(&self) -> &[IoctlDesc] {
        PVR_IOCTLS
    }

    /// Publishes the platform identity needed by `libdrm` sysfs enumeration.
    ///
    /// The values come from [`crate::platform`]. `of_fullname` must differ from the
    /// DPU's: `drmDevicesEqual` compares this string and `drmFoldDuplicatedDevices`
    /// merges matching entries, even when their render and display drivers differ.
    fn platform_identity(&self) -> Option<PlatformIdentity> {
        crate::platform::identity()
    }

    /// Makes a buffer another device exported usable by the GPU.
    ///
    /// The answer is an ordinary [`PvrObject`], which is what makes `VM_MAP`, the
    /// MMU and the submission path need no notion of imports; see
    /// [`PvrObject::import`] for why that is upstream's shape as well as the
    /// convenient one.
    ///
    /// This is the direction a render-only GPU needs in order to put anything on
    /// a screen: the display controller allocates the scanout buffer, and the GPU
    /// has to be able to draw into it. Answering `Ok(None)` here — the default —
    /// would leave the generic wrapper in the handle table, and every `VM_MAP` of
    /// it would report `ENOENT` when `PvrFile::object_from_handle` failed to
    /// recognise it, which is what this driver did before.
    fn import_buffer(&self, buffer: &Arc<DmaBuf>) -> DrmResult<Option<Arc<dyn GemObject>>> {
        // A failure to import is reported rather than silently downgraded to the
        // generic wrapper: the wrapper cannot be mapped into the GPU, so
        // downgrading would turn a diagnosable refusal at `PRIME_FD_TO_HANDLE`
        // into an `ENOENT` from a `VM_MAP` much later.
        let object = PvrObject::import(buffer).map_err(DrmError::from)?;
        Ok(Some(object))
    }

    /// Allocates this file's driver-private state.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
    /// (`pvr_drm_driver_open`).
    fn open(&self, file: &DrmFile) -> DrmResult<()> {
        file.set_driver_priv(PvrFile::new(self.device.clone()));
        Ok(())
    }

    /// Releases this file's driver-private state.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
    /// (`pvr_drm_driver_postclose`).
    fn postclose(&self, file: &DrmFile) {
        // The state itself is dropped with the `DrmFile`; what has to happen
        // *here* is the ordered teardown, because upstream's postclose is the
        // point at which contexts stop referring to free lists and address
        // spaces. Leaving it to the destructor would still get the order right
        // but would move it to whenever the last `Arc` goes away.
        if let Some(state) = file.driver_priv::<PvrFile>() {
            state.teardown();
        }
    }
}

/// Registers a probed GPU with `k1-musebook-drm`, creating `/dev/dri/renderD*`.
///
/// The platform maps its register window before registration. Each GPU's state
/// belongs to its own `DrmDevice` and is reached through [`driver_of`].
pub fn register(device: Arc<PvrDevice>) -> DrmResult<Arc<DrmDevice>> {
    k1_musebook_drm::register_device(PvrDriver::new(device))
}

/// Resolves the driver behind a DRM device.
///
/// Upstream's `to_pvr_device(drm_dev)`, which is a `container_of`. Here it is a
/// downcast of the device's own driver object, so an ioctl issued against some
/// other DRM device cannot reach this one's state — the type check *is* the
/// identity check.
/// A borrow rather than an `Arc`: every caller is a handler, and the device it
/// was handed outlives the call, so there is nothing to keep alive.
fn driver_of(device: &DrmDevice) -> DrmResult<&PvrDriver> {
    device
        .driver()
        .as_any()
        .downcast_ref::<PvrDriver>()
        .ok_or(DrmError::NoDevice)
}

/// The largest `size` or `stride` a `DEV_QUERY` may name.
///
/// Divergence: upstream accepts any `size`, reading the first `sizeof(struct)`
/// bytes and merely checking that the rest is zero, so a caller may legally pass
/// a gigabyte. We copy the caller's whole object in before negotiating strides,
/// so an unbounded `size` would be an unbounded kernel allocation. Every
/// container in the interface is at most 32 bytes and every array element at most
/// 24, so a page is orders of magnitude more room than any real caller needs;
/// beyond it the request is refused with `EINVAL`. This is the same reasoning as
/// the array cap `k1-musebook-drm` applies to the variable-length core ioctls.
const MAX_QUERY_BYTES: u32 = 4096;

/// Handles `DRM_IOCTL_PVR_DEV_QUERY`.
///
/// The first ioctl Mesa issues and the one that decides whether it will talk to
/// us at all: it fetches GPU info, runtime info, quirks, enhancements, heap
/// layout and static data areas. Each sub-query follows the two-pass convention —
/// a null pointer with zero size asks for the required size.
///
/// Because this is the gate for everything else, it is also the first thing that
/// can be checked without hardware: Mesa's `drm-shim` with
/// `PVR_SHIM_DEVICE_BVNC=36.29.52.182` produces the expected answers to compare
/// against.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_dev_query`).
pub fn dev_query(
    file: &PvrFile,
    args: &mut uapi::DevQueryArgs,
    caller: &CallerContext,
) -> DrmResult<()> {
    // Upstream opens with `drm_dev_enter()`, which reports `-EIO` for a device
    // that has gone away. A GPU declared lost is the same situation from user
    // space's point of view and reports the same errno.
    file.device.check_alive()?;
    let info = &file.device.info;

    // Upstream's `switch` has no `default`, and `ret` is pre-set to `-EINVAL`, so
    // an unknown query type is `EINVAL` rather than a panic.
    let result = match args.r#type {
        t if t == uapi::DevQuery::GpuInfoGet as u32 => gpu_info_get(file, args, caller),
        t if t == uapi::DevQuery::RuntimeInfoGet as u32 => runtime_info_get(info, args, caller),
        t if t == uapi::DevQuery::QuirksGet as u32 => quirks_get(info, args, caller),
        t if t == uapi::DevQuery::EnhancementsGet as u32 => enhancements_get(info, args, caller),
        t if t == uapi::DevQuery::HeapInfoGet as u32 => heap_info_get(info, args, caller),
        t if t == uapi::DevQuery::StaticDataAreasGet as u32 => static_data_areas_get(args, caller),
        _ => Err(PvrError::InvalidArgument),
    };

    result.map_err(DrmError::from)
}

/// Answers `DRM_PVR_DEV_QUERY_GPU_INFO_GET`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_dev_query_gpu_info_get`).
fn gpu_info_get(
    file: &PvrFile,
    args: &mut uapi::DevQueryArgs,
    caller: &CallerContext,
) -> PvrResult<()> {
    if args.pointer == 0 {
        args.size = size_of::<uapi::DevQueryGpuInfo>() as u32;
        return Ok(());
    }

    let gpu_info = uapi::DevQueryGpuInfo {
        gpu_id: file.device.gpu_id.to_packed(),
        num_phantoms: num_phantoms(&file.device.info),
        _padding_c: 0,
    };

    write_uobj(
        caller,
        args,
        uapi::DevQueryGpuInfo::MIN_STRIDE,
        gpu_info.as_bytes(),
    )
}

/// Answers `DRM_PVR_DEV_QUERY_RUNTIME_INFO_GET`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_dev_query_runtime_info_get`).
fn runtime_info_get(
    info: &DeviceInfo,
    args: &mut uapi::DevQueryArgs,
    caller: &CallerContext,
) -> PvrResult<()> {
    if args.pointer == 0 {
        args.size = size_of::<uapi::DevQueryRuntimeInfo>() as u32;
        return Ok(());
    }

    let runtime_info = uapi::DevQueryRuntimeInfo {
        free_list_min_pages: crate::render::free_list::min_pages(info) as u64,
        free_list_max_pages: ROGUE_PM_MAX_FREELIST_SIZE / ROGUE_PM_PAGE_SIZE,
        common_store_alloc_region_size: common_store_alloc_region_size(info),
        common_store_partition_space_size: common_store_partition_space_size(info),
        max_coeffs: max_coeffs(info),
        cdm_max_local_mem_size_regs: cdm_max_local_mem_size_regs(info),
    };

    write_uobj(
        caller,
        args,
        uapi::DevQueryRuntimeInfo::MIN_STRIDE,
        runtime_info.as_bytes(),
    )
}

/// The quirks reported to user space, in the order upstream lists them.
///
/// The must-haves come first, because `musthave_count` describes a prefix of the
/// array rather than a separate list, and Mesa refuses the device if it does not
/// recognise every one of them.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_dev_query_quirks_get`).
const UAPI_QUIRKS_MUSTHAVE: [u32; 3] = [47217, 49927, 62269];
/// The optional quirks reported to user space, in upstream's order.
const UAPI_QUIRKS_OPTIONAL: [u32; 2] = [48545, 51764];
/// The enhancements reported to user space, in upstream's order.
const UAPI_ENHANCEMENTS: [u32; 2] = [35421, 42064];

/// Whether a quirk that appears in the uapi lists is present.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
/// (`pvr_device_has_uapi_quirk`).
pub(crate) fn has_uapi_quirk(info: &DeviceInfo, quirk: u32) -> bool {
    match quirk {
        47217 => info.quirks.brn47217,
        48545 => info.quirks.brn48545,
        49927 => info.quirks.brn49927,
        51764 => info.quirks.brn51764,
        62269 => info.quirks.brn62269,
        _ => false,
    }
}

/// Whether an enhancement that appears in the uapi list is present.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
/// (`pvr_device_has_uapi_enhancement`).
fn has_uapi_enhancement(info: &DeviceInfo, enhancement: u32) -> bool {
    match enhancement {
        35421 => info.enhancements.ern35421,
        42064 => info.enhancements.ern42064,
        _ => false,
    }
}

/// Answers `DRM_PVR_DEV_QUERY_QUIRKS_GET`.
fn quirks_get(
    info: &DeviceInfo,
    args: &mut uapi::DevQueryArgs,
    caller: &CallerContext,
) -> PvrResult<()> {
    if args.pointer == 0 {
        args.size = size_of::<uapi::DevQueryQuirks>() as u32;
        return Ok(());
    }

    let mut query: uapi::DevQueryQuirks =
        read_uobj(caller, args, uapi::DevQueryQuirks::MIN_STRIDE)?;
    if query._padding_c != 0 {
        return Err(PvrError::InvalidArgument);
    }

    let musthave: Vec<u32> = UAPI_QUIRKS_MUSTHAVE
        .into_iter()
        .filter(|quirk| has_uapi_quirk(info, *quirk))
        .collect();
    let mut out = musthave.clone();
    out.extend(
        UAPI_QUIRKS_OPTIONAL
            .into_iter()
            .filter(|quirk| has_uapi_quirk(info, *quirk)),
    );

    if query.quirks != 0 {
        if (query.count as usize) < out.len() {
            return Err(PvrError::TooBig);
        }
        write_u32_array(caller, query.quirks, &out)?;
        // Upstream only updates this on the copying path, leaving whatever the
        // caller passed in when it asked for the count alone.
        query.musthave_count = musthave.len() as u16;
    }

    query.count = out.len() as u16;
    write_uobj(
        caller,
        args,
        uapi::DevQueryQuirks::MIN_STRIDE,
        query.as_bytes(),
    )
}

/// Answers `DRM_PVR_DEV_QUERY_ENHANCEMENTS_GET`.
fn enhancements_get(
    info: &DeviceInfo,
    args: &mut uapi::DevQueryArgs,
    caller: &CallerContext,
) -> PvrResult<()> {
    if args.pointer == 0 {
        args.size = size_of::<uapi::DevQueryEnhancements>() as u32;
        return Ok(());
    }

    let mut query: uapi::DevQueryEnhancements =
        read_uobj(caller, args, uapi::DevQueryEnhancements::MIN_STRIDE)?;
    if query._padding_a != 0 || query._padding_c != 0 {
        return Err(PvrError::InvalidArgument);
    }

    let out: Vec<u32> = UAPI_ENHANCEMENTS
        .into_iter()
        .filter(|ern| has_uapi_enhancement(info, *ern))
        .collect();

    if query.enhancements != 0 {
        if (query.count as usize) < out.len() {
            return Err(PvrError::TooBig);
        }
        write_u32_array(caller, query.enhancements, &out)?;
    }

    query.count = out.len() as u16;
    write_uobj(
        caller,
        args,
        uapi::DevQueryEnhancements::MIN_STRIDE,
        query.as_bytes(),
    )
}

/// The GPU virtual address space layout, as reported to user space.
///
/// The addresses are a firmware and Mesa contract rather than a policy, which is
/// why they come from the generated [`heap`] module. All six entries are always
/// present; a heap this GPU does not have is reported with a zero size, never
/// omitted, because the index is the heap's identity.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_vm.c>
/// (`pvr_heaps`).
fn heaps() -> [uapi::Heap; uapi::HEAP_COUNT as usize] {
    let heap_of = |base: u64, size: u64| uapi::Heap {
        base,
        size,
        flags: 0,
        page_size_log2: DEVICE_PAGE_SHIFT,
    };

    // Indexed by `HeapId`; the `heap_ids_match_the_table_order` test pins the
    // correspondence, which upstream gets from designated initializers.
    [
        heap_of(heap::ROGUE_GENERAL_HEAP_BASE, heap::ROGUE_GENERAL_HEAP_SIZE),
        heap_of(
            heap::ROGUE_PDSCODEDATA_HEAP_BASE,
            heap::ROGUE_PDSCODEDATA_HEAP_SIZE,
        ),
        heap_of(heap::ROGUE_USCCODE_HEAP_BASE, heap::ROGUE_USCCODE_HEAP_SIZE),
        heap_of(heap::ROGUE_RGNHDR_HEAP_BASE, heap::ROGUE_RGNHDR_HEAP_SIZE),
        heap_of(heap::ROGUE_VISTEST_HEAP_BASE, heap::ROGUE_VISTEST_HEAP_SIZE),
        heap_of(
            heap::ROGUE_TRANSFER_FRAG_HEAP_BASE,
            heap::ROGUE_TRANSFER_FRAG_HEAP_SIZE,
        ),
    ]
}

/// Finds the heap containing a device-virtual address range.
///
/// Ports upstream `pvr_find_heap_containing()`. `VM_MAP` uses it to refuse a
/// mapping that would fall outside every heap or straddle two of them, which is
/// the check that keeps user space inside the layout it was told about through
/// `DRM_PVR_DEV_QUERY_HEAP_INFO_GET`. It reads the same [`heaps`] table as that
/// query so that the two can never describe different address spaces — including
/// the region header heap, which is only present on a GPU with BRN63142 and is
/// filtered out here exactly as upstream filters it and as `HEAP_INFO_GET`
/// reports it with a zero size.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_vm.c>
/// (`pvr_find_heap_containing`).
pub fn find_heap_containing(info: &DeviceInfo, start: u64, size: u64) -> Option<uapi::Heap> {
    let end = start.checked_add(size.checked_sub(1)?)?;

    heaps()
        .into_iter()
        .enumerate()
        .filter(|(index, _)| *index != uapi::HeapId::RgnHdr as usize || info.quirks.brn63142)
        .map(|(_, heap)| heap)
        .find(|heap| heap.base <= start && end < heap.base + heap.size)
}

/// Answers `DRM_PVR_DEV_QUERY_HEAP_INFO_GET`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_vm.c>
/// (`pvr_heap_info_get`).
fn heap_info_get(
    info: &DeviceInfo,
    args: &mut uapi::DevQueryArgs,
    caller: &CallerContext,
) -> PvrResult<()> {
    if args.pointer == 0 {
        args.size = size_of::<uapi::DevQueryHeapInfo>() as u32;
        return Ok(());
    }

    let mut query: uapi::DevQueryHeapInfo =
        read_uobj(caller, args, uapi::DevQueryHeapInfo::MIN_STRIDE)?;

    if query.heaps.array == 0 {
        query.heaps.count = uapi::HEAP_COUNT;
        query.heaps.stride = size_of::<uapi::Heap>() as u32;
    } else {
        query.heaps.count = query.heaps.count.min(uapi::HEAP_COUNT);

        let mut heaps = heaps();
        // The region header heap only exists on GPUs that need it. Reporting a
        // zero size rather than dropping the entry keeps every other heap at its
        // documented index.
        if !info.quirks.brn63142 {
            heaps[uapi::HeapId::RgnHdr as usize].size = 0;
        }

        let objects: Vec<u8> = heaps[..query.heaps.count as usize]
            .iter()
            .flat_map(|heap| heap.as_bytes().to_vec())
            .collect();
        write_obj_array(
            caller,
            &query.heaps,
            uapi::Heap::MIN_STRIDE,
            size_of::<uapi::Heap>(),
            &objects,
        )?;
    }

    write_uobj(
        caller,
        args,
        uapi::DevQueryHeapInfo::MIN_STRIDE,
        query.as_bytes(),
    )
}

/// The static data areas, as reported to user space.
///
/// "Determined by firmware" in upstream's words: these offsets are where the
/// firmware expects to find the end-of-tile program, the MCU fence and the VDM
/// sync programs, so they are not ours to choose. Note that `VDM_SYNC` appears
/// twice, in two different heaps, and that the list is not sorted by heap.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_vm.c>
/// (`static_data_areas`).
const STATIC_DATA_AREAS: [uapi::StaticDataArea; 5] = [
    uapi::StaticDataArea {
        area_usage: uapi::StaticDataAreaUsage::Fence as u16,
        location_heap_id: uapi::HeapId::General as u16,
        size: 128,
        offset: 0,
    },
    uapi::StaticDataArea {
        area_usage: uapi::StaticDataAreaUsage::YuvCsc as u16,
        location_heap_id: uapi::HeapId::General as u16,
        size: 1024,
        offset: 128,
    },
    uapi::StaticDataArea {
        area_usage: uapi::StaticDataAreaUsage::VdmSync as u16,
        location_heap_id: uapi::HeapId::PdsCodeData as u16,
        size: 128,
        offset: 0,
    },
    uapi::StaticDataArea {
        area_usage: uapi::StaticDataAreaUsage::Eot as u16,
        location_heap_id: uapi::HeapId::PdsCodeData as u16,
        size: 128,
        offset: 128,
    },
    uapi::StaticDataArea {
        area_usage: uapi::StaticDataAreaUsage::VdmSync as u16,
        location_heap_id: uapi::HeapId::UscCode as u16,
        size: 128,
        offset: 0,
    },
];

/// Answers `DRM_PVR_DEV_QUERY_STATIC_DATA_AREAS_GET`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_vm.c>
/// (`pvr_static_data_areas_get`).
fn static_data_areas_get(args: &mut uapi::DevQueryArgs, caller: &CallerContext) -> PvrResult<()> {
    if args.pointer == 0 {
        args.size = size_of::<uapi::DevQueryStaticDataAreas>() as u32;
        return Ok(());
    }

    let mut query: uapi::DevQueryStaticDataAreas =
        read_uobj(caller, args, uapi::DevQueryStaticDataAreas::MIN_STRIDE)?;
    let total = STATIC_DATA_AREAS.len() as u32;

    if query.static_data_areas.array == 0 {
        query.static_data_areas.count = total;
        query.static_data_areas.stride = size_of::<uapi::StaticDataArea>() as u32;
    } else {
        query.static_data_areas.count = query.static_data_areas.count.min(total);

        let objects: Vec<u8> = STATIC_DATA_AREAS[..query.static_data_areas.count as usize]
            .iter()
            .flat_map(|area| area.as_bytes().to_vec())
            .collect();
        write_obj_array(
            caller,
            &query.static_data_areas,
            uapi::StaticDataArea::MIN_STRIDE,
            size_of::<uapi::StaticDataArea>(),
            &objects,
        )?;
    }

    write_uobj(
        caller,
        args,
        uapi::DevQueryStaticDataAreas::MIN_STRIDE,
        query.as_bytes(),
    )
}

/// Reads a versioned container out of the caller's `DEV_QUERY` buffer.
///
/// `args.size` is the caller's stride, so this is `PVR_UOBJ_GET` with the stride
/// rule delegated to [`uapi::copy_uobj`].
fn read_uobj<T: Pod>(
    caller: &CallerContext,
    args: &uapi::DevQueryArgs,
    min_stride: usize,
) -> PvrResult<T> {
    if args.size > MAX_QUERY_BYTES {
        return Err(PvrError::InvalidArgument);
    }

    let mut raw = alloc::vec![0u8; args.size as usize];
    caller
        .read_bytes(args.pointer, &mut raw)
        .map_err(|_| PvrError::BadAddress)?;

    let object = uapi::copy_uobj(&raw, min_stride, size_of::<T>())?;
    Ok(T::from_bytes(&object))
}

/// Writes a versioned container back into the caller's `DEV_QUERY` buffer.
///
/// `PVR_UOBJ_SET` followed by upstream's `args->size` clamp: the caller learns
/// how much of its buffer the kernel actually understood, which is how a newer
/// user space discovers it is talking to an older driver.
fn write_uobj(
    caller: &CallerContext,
    args: &mut uapi::DevQueryArgs,
    min_stride: usize,
    object: &[u8],
) -> PvrResult<()> {
    if args.size > MAX_QUERY_BYTES {
        return Err(PvrError::InvalidArgument);
    }

    let raw = uapi::set_uobj(args.size as usize, min_stride, object)?;
    caller
        .write_bytes(args.pointer, &raw)
        .map_err(|_| PvrError::BadAddress)?;

    args.size = args.size.min(object.len() as u32);
    Ok(())
}

/// Writes kernel objects into the caller's object array.
fn write_obj_array(
    caller: &CallerContext,
    array: &uapi::ObjArray,
    min_stride: usize,
    our_stride: usize,
    objects: &[u8],
) -> PvrResult<()> {
    if array.stride > MAX_QUERY_BYTES {
        return Err(PvrError::InvalidArgument);
    }

    let raw = uapi::set_obj_array(array, min_stride, our_stride, objects)?;
    if raw.is_empty() {
        return Ok(());
    }

    caller
        .write_bytes(array.array, &raw)
        .map_err(|_| PvrError::BadAddress)
}

/// Writes a plain `__u32` array, as the quirks and enhancements queries use.
fn write_u32_array(caller: &CallerContext, pointer: u64, values: &[u32]) -> PvrResult<()> {
    if values.is_empty() {
        return Ok(());
    }

    let mut raw = Vec::with_capacity(size_of_val(values));
    for value in values {
        raw.extend_from_slice(&value.to_le_bytes());
    }
    caller
        .write_bytes(pointer, &raw)
        .map_err(|_| PvrError::BadAddress)
}

/// The PM physical page size, in bytes.
///
/// Upstream `ROGUE_PM_PAGE_SIZE` (`SZ_4K`) from `pvr_rogue_fwif_client.h`.
/// The layout-header generator does not emit this scalar definition.
const ROGUE_PM_PAGE_SIZE: u64 = 4096;
/// The largest free list the parameter manager can address, in bytes.
///
/// Upstream `ROGUE_PM_MAX_FREELIST_SIZE` (`SZ_2G`). See [`ROGUE_PM_PAGE_SIZE`].
const ROGUE_PM_MAX_FREELIST_SIZE: u64 = 2 * 1024 * 1024 * 1024;

/// Number of Phantoms, derived from the cluster count.
///
/// Upstream `ROGUE_REQ_NUM_PHANTOMS(CLUSTERS)` is `((CLUSTERS) + 3) / 4`.
///
/// Divergence: the result is clamped to at least one. Upstream would divide by
/// zero in [`max_coeffs`] if the firmware ever reported no clusters; a value of
/// zero is nonsense either way, and a panic is not an acceptable way to report
/// it.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_defs.h>.
fn num_phantoms(info: &DeviceInfo) -> u32 {
    info.values.num_clusters.div_ceil(4).max(1)
}

/// Size of the Partition Space within the Common Store, in dwords.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`rogue_get_common_store_partition_space_size`).
fn common_store_partition_space_size(info: &DeviceInfo) -> u32 {
    let values = &info.values;

    if values.tile_size_x == 16 && values.tile_size_y == 16 {
        return values.tile_size_x
            * values.tile_size_y
            * values.max_partitions
            * values.usc_min_output_registers_per_pix;
    }

    values.max_partitions * 1024
}

/// Size of the Allocation Region within the Common Store, in dwords.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`rogue_get_common_store_alloc_region_size`).
fn common_store_alloc_region_size(info: &DeviceInfo) -> u32 {
    let alloc_region_size = info
        .values
        .common_store_size_in_dwords
        .saturating_sub(256 * 4)
        .saturating_sub(common_store_partition_space_size(info));

    if info.quirks.brn44079 {
        // The Common Store is split, so the region cannot run past the split
        // point however large the store is.
        let common_store_split_point = 768 * 4 * 4;
        return (common_store_split_point - 256 * 4).min(alloc_region_size);
    }

    alloc_region_size
}

/// Maximum coefficient registers available, in dwords.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`rogue_get_max_coeffs`).
fn max_coeffs(info: &DeviceInfo) -> u32 {
    use crate::rogue::defs::{
        ROGUE_MAX_COMPUTE_SHARED_REGISTERS, ROGUE_MAX_PIXEL_SHARED_REGISTERS,
        ROGUE_MAX_VERTEX_SHARED_REGISTERS,
    };

    let mut max_coeff_additional_portion = ROGUE_MAX_VERTEX_SHARED_REGISTERS as u32;
    let mut pending_allocation_shared_regs = 2 * 1024;
    let mut pending_allocation_coeff_regs = 0;

    let max_coeff_pixel_portion = info
        .values
        .isp_max_tiles_in_flight
        .div_ceil(num_phantoms(info))
        * ROGUE_MAX_PIXEL_SHARED_REGISTERS as u32;

    // Compute tasks on cores with BRN48492 and without compute overlap may lock
    // up without two additional lines of coeffs.
    if info.quirks.brn48492 && !info.features.compute_overlap {
        pending_allocation_coeff_regs = 2 * 1024;
    }

    if info.enhancements.ern38748 {
        pending_allocation_shared_regs = 0;
    }

    if info.enhancements.ern38020 {
        max_coeff_additional_portion += ROGUE_MAX_COMPUTE_SHARED_REGISTERS as u32;
    }

    // Saturating rather than wrapping: a firmware whose Common Store is smaller
    // than the reservations would otherwise report a max near `u32::MAX`, and
    // Mesa would size allocations from it.
    (common_store_alloc_region_size(info) + pending_allocation_coeff_regs).saturating_sub(
        max_coeff_pixel_portion + max_coeff_additional_portion + pending_allocation_shared_regs,
    )
}

/// Maximum local memory available to a compute kernel, in dwords.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`rogue_get_cdm_max_local_mem_size_regs`).
fn cdm_max_local_mem_size_regs(info: &DeviceInfo) -> u32 {
    use crate::rogue::defs::{
        ROGUE_CSRM_LINE_SIZE_IN_DWORDS, ROGUE_MAX_PER_KERNEL_LOCAL_MEM_SIZE_REGS,
    };

    let mut available_coeffs_in_dwords = max_coeffs(info);

    if info.quirks.brn48492 && info.features.roguexe && !info.features.compute_overlap {
        // Two lines are reserved and must not be handed out.
        available_coeffs_in_dwords =
            available_coeffs_in_dwords.saturating_sub(ROGUE_CSRM_LINE_SIZE_IN_DWORDS as u32 * 2);
    }

    available_coeffs_in_dwords.min(ROGUE_MAX_PER_KERNEL_LOCAL_MEM_SIZE_REGS as u32)
}

/// The `DRM_IOCTL_PVR_DEV_QUERY` dispatch-table entry.
///
/// The argument structure is copied back even when the handler fails, matching
/// `drm_ioctl()`: the two-pass size protocol reports how much room a buffer needs
/// through the very field an error would otherwise discard.
fn ioctl_dev_query(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::DevQueryArgs>(payload)?;
    let result = dev_query(&pvr_file, &mut args, caller);
    payload_write(payload, &args)?;
    result
}

/// The `DRM_IOCTL_PVR_CREATE_BO` dispatch-table entry.
///
/// Unlike `DEV_QUERY`, the argument structure is copied back only on success.
/// That is `drm_ioctl()`'s rule (`if (!ret && ...)`), and `DEV_QUERY` is the
/// exception rather than this.
fn ioctl_create_bo(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::CreateBoArgs>(payload)?;
    create_bo(&pvr_file, file, &mut args)?;
    payload_write(payload, &args)
}

/// The `DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET` dispatch-table entry.
fn ioctl_get_bo_mmap_offset(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::GetBoMmapOffsetArgs>(payload)?;
    get_bo_mmap_offset(&pvr_file, file, &mut args)?;
    payload_write(payload, &args)
}

/// The `DRM_IOCTL_PVR_CREATE_VM_CONTEXT` dispatch-table entry.
fn ioctl_create_vm_context(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::CreateVmContextArgs>(payload)?;
    create_vm_context(&pvr_file, &mut args)?;
    payload_write(payload, &args)
}

/// The `DRM_IOCTL_PVR_DESTROY_VM_CONTEXT` dispatch-table entry.
fn ioctl_destroy_vm_context(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::DestroyVmContextArgs>(payload)?;
    destroy_vm_context(&pvr_file, &args)
}

/// The `DRM_IOCTL_PVR_VM_MAP` dispatch-table entry.
fn ioctl_vm_map(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::VmMapArgs>(payload)?;
    vm_map(&pvr_file, file, &args)
}

/// The `DRM_IOCTL_PVR_VM_UNMAP` dispatch-table entry.
fn ioctl_vm_unmap(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::VmUnmapArgs>(payload)?;
    vm_unmap(&pvr_file, &args)
}

/// The `DRM_IOCTL_PVR_CREATE_CONTEXT` dispatch-table entry.
fn ioctl_create_context(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::CreateContextArgs>(payload)?;
    create_context(&pvr_file, caller, &mut args)?;
    payload_write(payload, &args)
}

/// The `DRM_IOCTL_PVR_DESTROY_CONTEXT` dispatch-table entry.
fn ioctl_destroy_context(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::DestroyContextArgs>(payload)?;
    destroy_context(&pvr_file, &args)
}

/// The `DRM_IOCTL_PVR_SUBMIT_JOBS` dispatch-table entry.
fn ioctl_submit_jobs(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::SubmitJobsArgs>(payload)?;
    submit_jobs(&pvr_file, file, caller, &args.jobs)
}

/// The `DRM_IOCTL_PVR_CREATE_FREE_LIST` dispatch-table entry.
fn ioctl_create_free_list(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::CreateFreeListArgs>(payload)?;
    create_free_list(&pvr_file, &mut args)?;
    payload_write(payload, &args)
}

/// The `DRM_IOCTL_PVR_DESTROY_FREE_LIST` dispatch-table entry.
fn ioctl_destroy_free_list(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::DestroyFreeListArgs>(payload)?;
    destroy_free_list(&pvr_file, &args)
}

/// The `DRM_IOCTL_PVR_CREATE_HWRT_DATASET` dispatch-table entry.
fn ioctl_create_hwrt_dataset(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let mut args = payload_read::<uapi::CreateHwrtDatasetArgs>(payload)?;
    create_hwrt_dataset(&pvr_file, &mut args)?;
    payload_write(payload, &args)
}

/// The `DRM_IOCTL_PVR_DESTROY_HWRT_DATASET` dispatch-table entry.
fn ioctl_destroy_hwrt_dataset(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let driver = driver_of(device)?;
    let pvr_file = driver.file_of(file)?;

    let args = payload_read::<uapi::DestroyHwrtDatasetArgs>(payload)?;
    destroy_hwrt_dataset(&pvr_file, &args)
}

/// Handles `DRM_IOCTL_PVR_CREATE_BO`.
///
/// Allocates a buffer object and hands user space a GEM handle to it. The
/// handle is an ordinary DRM one, so `GEM_CLOSE` releases it and PRIME can
/// export it, which is also how upstream does it — `pvr_gem_object_into_handle()`
/// is `drm_gem_handle_create()`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_create_bo`).
pub fn create_bo(
    file: &PvrFile,
    drm_file: &DrmFile,
    args: &mut uapi::CreateBoArgs,
) -> DrmResult<()> {
    file.device.check_alive()?;
    if args._padding_c != 0 {
        return Err(DrmError::InvalidArgument);
    }

    // Size and flag validation lives with the object, because it is the same
    // validation the kernel's own allocations need.
    let object = PvrObject::new(args.size, args.flags)?;

    let handle = drm_file.handles().insert(object as Arc<dyn GemObject>)?;
    args.handle = handle.0;
    crate::live::note_create_bo();

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET`.
///
/// Returns the fake offset to pass to `mmap` on the render node. Idempotent, as
/// upstream's `drm_gem_create_mmap_offset()` is: a second call for one handle
/// must return the same offset or the client's second mapping would be of a
/// different buffer.
///
/// Divergence: upstream hands out an offset for any object and refuses the
/// `mmap` itself in `pvr_gem_mmap()` when
/// `DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS` is absent. This port has no per-object
/// `mmap` hook, so the check moves here. The errno is the same `EINVAL`,
/// and it arrives one call earlier.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_get_bo_mmap_offset`).
pub fn get_bo_mmap_offset(
    file: &PvrFile,
    drm_file: &DrmFile,
    args: &mut uapi::GetBoMmapOffsetArgs,
) -> DrmResult<()> {
    file.device.check_alive()?;
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let object = drm_file.handles().get(GemHandle(args.handle))?;
    if !object.is_mappable() {
        return Err(DrmError::InvalidArgument);
    }

    let drm_device = drm_file.device().ok_or(DrmError::NoDevice)?;
    args.offset = drm_device.gem().offsets().offset_of(&object)?;

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_CREATE_VM_CONTEXT`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_create_vm_context`).
pub fn create_vm_context(file: &PvrFile, args: &mut uapi::CreateVmContextArgs) -> DrmResult<()> {
    file.device.check_alive()?;
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let context = VmContext::new(file.device.clone(), VmContextKind::User)?;
    args.handle = file.vm_contexts.insert(context)?;

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_DESTROY_VM_CONTEXT`.
///
/// Dropping the last reference tears down every mapping the context still holds;
/// see [`VmContext`]'s `Drop`. Upstream gets there through
/// `pvr_vm_context_put()` and `pvr_vm_unmap_all()`.
///
/// An unknown handle is `EINVAL`, not `ENOENT`: upstream's `xa_erase()` miss
/// returns `-EINVAL` here, unlike the buffer-object lookups.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_destroy_vm_context`).
pub fn destroy_vm_context(file: &PvrFile, args: &uapi::DestroyVmContextArgs) -> DrmResult<()> {
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    // Removed from the table first and dropped afterwards, outside its lock:
    // the destructor unmaps, and unmapping writes page tables.
    let context = file
        .vm_contexts
        .remove(args.handle)
        .map_err(|_| DrmError::InvalidArgument)?;
    drop(context);

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_VM_MAP`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_vm_map`).
pub fn vm_map(file: &PvrFile, drm_file: &DrmFile, args: &uapi::VmMapArgs) -> DrmResult<()> {
    file.device.check_alive()?;
    if args._padding_14 != 0 {
        return Err(DrmError::InvalidArgument);
    }
    // No flag has been defined yet, so a non-zero value is a newer client
    // asking for something this driver does not implement.
    if args.flags != 0 {
        return Err(DrmError::InvalidArgument);
    }

    // An unknown VM context is `EINVAL` and an unknown buffer object is
    // `ENOENT`; upstream distinguishes the two in exactly this way, and Mesa
    // sees the difference.
    let context = file
        .vm_contexts
        .get(args.vm_context_handle)
        .map_err(|_| DrmError::InvalidArgument)?;
    let object = PvrFile::object_from_handle(drm_file, args.handle)?;

    context.map(
        DeviceAddr(args.device_addr),
        &object,
        args.offset,
        args.size,
    )?;
    crate::live::note_vm_map_ioctl();

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_VM_UNMAP`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_vm_unmap`).
pub fn vm_unmap(file: &PvrFile, args: &uapi::VmUnmapArgs) -> DrmResult<()> {
    file.device.check_alive()?;
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let context = file
        .vm_contexts
        .get(args.vm_context_handle)
        .map_err(|_| DrmError::InvalidArgument)?;
    context.unmap(DeviceAddr(args.device_addr), args.size)?;
    crate::live::note_vm_unmap_ioctl();

    Ok(())
}

/// The largest object array `SUBMIT_JOBS` or `CREATE_CONTEXT` will copy in.
///
/// Divergence: upstream bounds nothing here — `PVR_UOBJ_GET_ARRAY()` calls
/// `kvmalloc_array()` and lets the allocator refuse — so a client can ask for a
/// very large array and get `ENOMEM`. `k1-musebook-drm` caps its own array copies for
/// the same reason (see `MAX_ARRAY_BYTES` there), and an unbounded allocation on
/// behalf of an unprivileged caller is the kind of thing `CONVENTIONS.md` command-stream contract
/// exists to prevent. A megabyte is four orders of magnitude above any real
/// batch: Mesa submits one or two jobs per call. Recorded in `ABI.md`
/// compatibility limits.
const MAX_ARRAY_BYTES: usize = 1 << 20;

/// The largest command stream a job may carry.
///
/// Same reasoning, and the same divergence. The longest stream any definition
/// table can consume is the fragment one at a few hundred bytes, so this is
/// generous by three orders of magnitude while still being a bound.
const MAX_STREAM_BYTES: usize = 1 << 16;

/// Copies a user object array in, under [`MAX_ARRAY_BYTES`].
///
/// The stride negotiation itself belongs to [`uapi::obj_array`] and stays there;
/// this only fetches the bytes it will be given.
fn read_obj_array(caller: &CallerContext, array: &uapi::ObjArray) -> DrmResult<Vec<u8>> {
    if array.count == 0 {
        return Ok(Vec::new());
    }
    let len = (array.stride as usize)
        .checked_mul(array.count as usize)
        .ok_or(DrmError::InvalidArgument)?;
    if len > MAX_ARRAY_BYTES {
        return Err(DrmError::InvalidArgument);
    }
    let mut bytes = alloc::vec![0u8; len];
    caller.read_bytes(array.array, &mut bytes)?;
    Ok(bytes)
}

/// Copies one command stream in, under [`MAX_STREAM_BYTES`].
fn read_stream(caller: &CallerContext, pointer: u64, len: u32) -> DrmResult<Vec<u8>> {
    let len = len as usize;
    if len == 0 || len > MAX_STREAM_BYTES {
        return Err(DrmError::InvalidArgument);
    }
    let mut bytes = alloc::vec![0u8; len];
    caller.read_bytes(pointer, &mut bytes)?;
    Ok(bytes)
}

/// Handles `DRM_IOCTL_PVR_CREATE_CONTEXT`.
///
/// Ports upstream `pvr_ioctl_create_context()`. The static context state is a
/// command stream like any other and goes through the same table-driven validator
/// as a job's, which is why a render or compute context that supplies none is
/// refused: the alternative is a firmware that context-switches into zeroed
/// registers.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_create_context`).
pub fn create_context(
    file: &PvrFile,
    caller: &CallerContext,
    args: &mut uapi::CreateContextArgs,
) -> DrmResult<()> {
    file.device.check_alive()?;
    // Upstream: "Context creation flags are currently unused and must be zero."
    if args.flags != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let context_type = ContextType::from_uapi(args.r#type)?;
    // Divergence: upstream allows `DRM_PVR_CTX_PRIORITY_HIGH` for a caller with
    // `CAP_SYS_NICE` or DRM master. Neither is reachable from here; see
    // `Priority::from_uapi`.
    let priority = Priority::from_uapi(args.priority, false)?;

    let vm = file
        .vm_contexts
        .get(args.vm_context_handle)
        .map_err(|_| DrmError::InvalidArgument)?;

    let static_state = if args.static_context_state_len == 0 {
        // A null pointer with a zero length is how a transfer context says "I
        // have none"; a null pointer with a non-zero length is a client bug.
        if args.static_context_state != 0 {
            return Err(DrmError::InvalidArgument);
        }
        Vec::new()
    } else {
        read_stream(
            caller,
            args.static_context_state,
            args.static_context_state_len,
        )?
    };

    let context = Context::new(
        &file.device,
        vm,
        context_type,
        priority,
        &static_state,
        args.callstack_addr,
    )?;
    args.handle = file.render_contexts.insert(context)?;

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_DESTROY_CONTEXT`.
///
/// Ports upstream `pvr_ioctl_destroy_context()`, which is `xa_erase()` plus
/// `pvr_context_kill_queues()` plus dropping the handle's reference. The order
/// matters: the queues stop accepting work *before* the reference goes, so a
/// concurrent `SUBMIT_JOBS` on the same handle either resolved it before the erase
/// and is queuing onto a live context, or fails to resolve it. There is no window
/// in which it queues onto a context being torn down.
///
/// An unknown handle is `EINVAL`, not `ENOENT`, matching upstream's `xa_erase()`
/// miss.
///
/// The firmware cleanup that follows talks to the GPU over the KCCB. Upstream
/// reaches `pvr_power_get()` from `pvr_fw_structure_cleanup()` via
/// `pvr_kccb_send_cmd()`; this port's `ccb::send` deliberately does not take a
/// power reference (interrupt/fence-callback reachable — see `sched/ccb.rs`), so
/// the ioctl takes one instead. Taken after the table erase and after `kill`,
/// with no queue lock held: [`crate::power::Pm::get`] sleeps.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_destroy_context`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_fw.c>
/// (`pvr_fw_structure_cleanup`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_ccb.c>
/// (`pvr_kccb_send_cmd`).
pub fn destroy_context(file: &PvrFile, args: &uapi::DestroyContextArgs) -> DrmResult<()> {
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let context = file
        .render_contexts
        .remove(args.handle)
        .map_err(|_| DrmError::InvalidArgument)?;

    // Nothing new may be queued from this moment, even though the teardown below
    // may take as long as the in-flight work does.
    context.kill();
    let cleaned = match crate::power::PowerRef::take(&file.device) {
        Ok(_power) => {
            // The firmware cleanup below is a KCCB command, so it takes the read
            // side of the reset lock — upstream's `pvr_fw_structure_cleanup()`
            // takes `down_read(&pvr_dev->reset_sem)` around the same command.
            let _serialise_against_reset = file.device.reset_lock.read();
            match context.destroy() {
                Ok(()) => true,
                Err(err) => {
                    ostd::error!("firmware cleanup failed ({err:?}); deferring resource release");
                    false
                }
            }
        }
        Err(err) => {
            ostd::error!("could not power the GPU for cleanup: {err:?}");
            false
        }
    };
    if !cleaned {
        crate::service::enqueue(&file.device, crate::service::Doomed::Context(context));
    }

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_SUBMIT_JOBS`.
///
/// The hot path, and the only ioctl that takes nested object arrays: jobs, each
/// with its own sync operations and an opaque command stream that
/// [`crate::sched::stream`] validates. Partial success is not permitted — either
/// every job is queued or none is.
///
/// Holds a runtime-PM reference for the duration of the ioctl. Upstream takes
/// one inside `pvr_kccb_send_cmd()` for the MMU flush this path issues, and again
/// via `pvr_job_get_pm_ref()` when a job is kicked onto the firmware. This port's
/// `ccb::send` is the `*_powered` form on purpose — [`crate::sched::queue::Queue::advance`]
/// can run from a fence callback — so the ioctl is the place that may sleep in
/// [`crate::power::Pm::get`] and boot the firmware. Taken before any queue lock
/// and before any copy from user memory, so that no user-memory copy ever
/// happens with a lock held.
///
/// This reference covers the ioctl only. The one that outlives it belongs to each
/// job: [`crate::sched::queue::Queue::push`] takes a
/// [`crate::power::JobPowerRef`] that is released when the job's fence is
/// terminated, which is upstream's `pvr_job_get_pm_ref()`. Letting go of this one
/// on the way out is also what performs any power-down the retired jobs have left
/// outstanding — see [`crate::power::Pm::settle`].
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_submit_jobs`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_ccb.c>
/// (`pvr_kccb_send_cmd`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_job.h>
/// (`pvr_job_get_pm_ref`).
pub fn submit_jobs(
    file: &PvrFile,
    drm_file: &DrmFile,
    caller: &CallerContext,
    args: &uapi::ObjArray,
) -> DrmResult<()> {
    file.device.check_alive()?;
    if args.count == 0 {
        return Err(DrmError::InvalidArgument);
    }

    // Before any work that needs the GPU, and before any lock this path takes.
    // A boot failure here is what the client must see: this ioctl may be the
    // first thing that starts the firmware.
    let _power = crate::power::PowerRef::take(&file.device)?;

    // Three copies from user space, all before anything is validated: the array
    // of job descriptions, then one command stream and one sync-operation array
    // per job. Nothing touches user memory after this point, so a client that
    // mutates its own buffers cannot make the driver see one thing while
    // validating and another while submitting.
    let raw = read_obj_array(caller, args)?;
    let elements = uapi::obj_array(args, uapi::Job::MIN_STRIDE, size_of::<uapi::Job>(), &raw)?;

    let mut streams = Vec::with_capacity(args.count as usize);
    let mut sync_ops = Vec::with_capacity(args.count as usize);
    for chunk in elements.as_chunks::<{ size_of::<uapi::Job>() }>().0 {
        let job = <uapi::Job as Pod>::from_bytes(chunk);
        streams.push(read_stream(caller, job.cmd_stream, job.cmd_stream_len)?);
        sync_ops.push(read_obj_array(caller, &job.sync_ops)?);
    }

    // Upstream flushes here rather than per mapping: "this has been deferred until
    // now to avoid overuse of this expensive operation". It has to happen before
    // any job is queued, because the jobs are about to read through page tables
    // the GPU may still have cached.
    crate::mm::mmu::flush_if_pending(&file.device)?;

    // Upstream's rule for `pvr_dev->reset_sem`: anything that submits commands to
    // the firmware and is neither an IRQ handler nor the scheduler workqueue
    // holds it for reading. Upstream's submission is on that workqueue, which a
    // reset parks; this port's runs here, so this is where the read side goes.
    // *After* the flush above, which is itself one of the reset call sites and
    // would otherwise be waiting on a lock it is about to need for writing.
    let _serialise_against_reset = file.device.reset_lock.read();

    let (jobs, mut signals) = crate::sched::job::prepare_batch(
        file,
        drm_file.syncobjs(),
        args,
        &raw,
        &streams,
        &sync_ops,
    )?;
    crate::sched::job::commit_batch(jobs)?;

    // Last, and only on complete success: this is the only step user space can
    // observe, so leaving it here is what makes the whole submission all or
    // nothing. Upstream orders `pvr_sync_signal_array_push_fences()` after
    // `push_jobs()` for the same reason.
    signals.push_fences();

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_CREATE_FREE_LIST`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_create_free_list`).
pub fn create_free_list(file: &PvrFile, args: &mut uapi::CreateFreeListArgs) -> DrmResult<()> {
    file.device.check_alive()?;

    // An unknown VM context is `EINVAL`, matching every other ioctl that takes
    // one; upstream's `pvr_vm_context_lookup()` miss says the same.
    let vm = file
        .vm_contexts
        .get(args.vm_context_handle)
        .map_err(|_| DrmError::InvalidArgument)?;

    let free_list = FreeList::new(&file.device, &vm, args)?;
    args.handle = file.free_lists.insert(free_list)?;

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_DESTROY_FREE_LIST`.
///
/// Ports upstream `pvr_ioctl_destroy_free_list()`, which is `xa_erase()` plus
/// dropping the handle's reference. The firmware cleanup that
/// `pvr_free_list_release()` does from the `kref` release runs here instead,
/// before the reference goes: a destructor must not wait for the GPU, and this
/// step has to complete before the Parameter Manager's pages are freed.
///
/// An unknown handle is `EINVAL`, matching upstream's `xa_erase()` miss.
///
/// Runtime-PM as in [`destroy_context`]: cleanup is a KCCB command, and
/// `ccb::send` does not take the reference itself.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_destroy_free_list`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_fw.c>
/// (`pvr_fw_structure_cleanup`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_ccb.c>
/// (`pvr_kccb_send_cmd`).
pub fn destroy_free_list(file: &PvrFile, args: &uapi::DestroyFreeListArgs) -> DrmResult<()> {
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let free_list = file
        .free_lists
        .remove(args.handle)
        .map_err(|_| DrmError::InvalidArgument)?;

    let cleaned = match crate::power::PowerRef::take(&file.device) {
        Ok(_power) => {
            // See `destroy_context`: a cleanup command is upstream's
            // `pvr_fw_structure_cleanup()`, which holds the reset lock for
            // reading.
            let _serialise_against_reset = file.device.reset_lock.read();
            match free_list.destroy() {
                Ok(()) => true,
                Err(err) => {
                    ostd::error!("firmware cleanup failed ({err:?}); deferring resource release");
                    false
                }
            }
        }
        Err(err) => {
            ostd::error!("could not power the GPU for cleanup: {err:?}");
            false
        }
    };
    if !cleaned {
        crate::service::enqueue(&file.device, crate::service::Doomed::FreeList(free_list));
    }

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_CREATE_HWRT_DATASET`.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_create_hwrt_dataset`).
pub fn create_hwrt_dataset(
    file: &PvrFile,
    args: &mut uapi::CreateHwrtDatasetArgs,
) -> DrmResult<()> {
    file.device.check_alive()?;

    let dataset = HwrtDataset::new(&file.device, file, args)?;
    args.handle = file.hwrt_datasets.insert(dataset)?;

    Ok(())
}

/// Handles `DRM_IOCTL_PVR_DESTROY_HWRT_DATASET`.
///
/// The same shape as [`destroy_free_list`], and for the same reason: the
/// firmware has to be told before the render target's memory can go.
///
/// Runtime-PM as in [`destroy_context`]: cleanup is a KCCB command, and
/// `ccb::send` does not take the reference itself.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_drv.c>
/// (`pvr_ioctl_destroy_hwrt_dataset`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_fw.c>
/// (`pvr_fw_structure_cleanup`);
/// <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_ccb.c>
/// (`pvr_kccb_send_cmd`).
pub fn destroy_hwrt_dataset(file: &PvrFile, args: &uapi::DestroyHwrtDatasetArgs) -> DrmResult<()> {
    if args._padding_4 != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let dataset = file
        .hwrt_datasets
        .remove(args.handle)
        .map_err(|_| DrmError::InvalidArgument)?;

    let cleaned = match crate::power::PowerRef::take(&file.device) {
        Ok(_power) => {
            // See `destroy_context`: a cleanup command is upstream's
            // `pvr_fw_structure_cleanup()`, which holds the reset lock for
            // reading.
            let _serialise_against_reset = file.device.reset_lock.read();
            match dataset.destroy() {
                Ok(()) => true,
                Err(err) => {
                    ostd::error!("firmware cleanup failed ({err:?}); deferring resource release");
                    false
                }
            }
        }
        Err(err) => {
            ostd::error!("could not power the GPU for cleanup: {err:?}");
            false
        }
    };
    if !cleaned {
        crate::service::enqueue(&file.device, crate::service::Doomed::Hwrt(dataset));
    }

    Ok(())
}

/// `DEV_QUERY` and handle-table tests, none of which need hardware.
///
/// The device is built on a simulated register window and a device description
/// whose values match the fixture in [`crate::device_info`], so the expected
/// `RUNTIME_INFO` numbers here are the ones a `drm-shim` comparison should
/// reproduce once the real firmware's block is used instead.
#[cfg(any(test, ktest))]
mod tests {
    use k1_musebook_drm::ioctl::UserMem;
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;
    use crate::{
        device::{FwVersion, GpuId, RegisterWindow},
        device_info::{Enhancements, FeatureValues, Features, Quirks},
    };

    /// Where the fake user buffer starts. Non-zero, since a null pointer means
    /// "just tell me the size".
    const USER_BASE: u64 = 0x1000_0000;

    /// A stand-in for the calling process's address space.
    struct FakeUserMem {
        bytes: SpinLock<Vec<u8>>,
    }

    impl FakeUserMem {
        fn new() -> Self {
            Self {
                bytes: SpinLock::new(alloc::vec![0u8; 8192]),
            }
        }

        fn range(&self, addr: u64, len: usize) -> DrmResult<core::ops::Range<usize>> {
            let start = addr
                .checked_sub(USER_BASE)
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or(DrmError::BadAddress)?;
            let end = start.checked_add(len).ok_or(DrmError::BadAddress)?;
            if end > self.bytes.lock().len() {
                return Err(DrmError::BadAddress);
            }
            Ok(start..end)
        }

        fn put<T: Pod>(&self, addr: u64, value: &T) {
            self.write_bytes(addr, value.as_bytes()).unwrap();
        }

        fn get<T: Pod>(&self, addr: u64) -> T {
            let mut raw = alloc::vec![0u8; size_of::<T>()];
            self.read_bytes(addr, &mut raw).unwrap();
            T::from_bytes(&raw)
        }

        fn get_u32s(&self, addr: u64, count: usize) -> Vec<u32> {
            let mut raw = alloc::vec![0u8; count * size_of::<u32>()];
            self.read_bytes(addr, &mut raw).unwrap();
            raw.chunks_exact(4)
                .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
                .collect()
        }
    }

    impl FakeUserMem {
        /// The caller context a handler is given. Memory only: these tests drive
        /// handlers directly, which is exactly the in-kernel caller that has no
        /// descriptor table and no thread to block.
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

    /// A device description with the fixture's feature values.
    fn bxe_info() -> DeviceInfo {
        DeviceInfo {
            features: Features {
                compute: true,
                compute_overlap: true,
                mips: true,
                num_clusters: true,
                pbvnc_coreid_reg: true,
                phys_bus_width: true,
                roguexe: true,
                ..Features::default()
            },
            values: FeatureValues {
                common_store_size_in_dwords: 1344 * 4 * 4,
                isp_max_tiles_in_flight: 6,
                max_partitions: 16,
                num_clusters: 1,
                tile_size_x: 16,
                tile_size_y: 16,
                usc_min_output_registers_per_pix: 2,
                ..FeatureValues::default()
            },
            quirks: Quirks {
                brn44079: true,
                ..Quirks::default()
            },
            enhancements: Enhancements {
                ern35421: true,
                ern38748: true,
                ..Enhancements::default()
            },
        }
    }

    fn file_with(info: DeviceInfo) -> Arc<PvrFile> {
        PvrFile::new(PvrDevice::new(
            RegisterWindow::simulated(),
            GpuId::K1_BXE_2_32,
            info,
            FwVersion { major: 1, minor: 1 },
        ))
    }

    fn args_for(query: uapi::DevQuery, size: usize) -> uapi::DevQueryArgs {
        uapi::DevQueryArgs {
            r#type: query as u32,
            size: size as u32,
            pointer: USER_BASE,
        }
    }

    /// Asks for the required size with a null pointer, as every client does first.
    fn query_size(file: &PvrFile, caller: &CallerContext, query: uapi::DevQuery) -> u32 {
        let mut args = uapi::DevQueryArgs {
            r#type: query as u32,
            size: 0,
            pointer: 0,
        };
        dev_query(file, &mut args, caller).expect("a size request never fails");
        args.size
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_null_pointer_reports_every_containers_size() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();

        assert_eq!(
            query_size(&file, &caller, uapi::DevQuery::GpuInfoGet),
            size_of::<uapi::DevQueryGpuInfo>() as u32
        );
        assert_eq!(
            query_size(&file, &caller, uapi::DevQuery::RuntimeInfoGet),
            size_of::<uapi::DevQueryRuntimeInfo>() as u32
        );
        assert_eq!(
            query_size(&file, &caller, uapi::DevQuery::QuirksGet),
            size_of::<uapi::DevQueryQuirks>() as u32
        );
        assert_eq!(
            query_size(&file, &caller, uapi::DevQuery::EnhancementsGet),
            size_of::<uapi::DevQueryEnhancements>() as u32
        );
        assert_eq!(
            query_size(&file, &caller, uapi::DevQuery::HeapInfoGet),
            size_of::<uapi::DevQueryHeapInfo>() as u32
        );
        assert_eq!(
            query_size(&file, &caller, uapi::DevQuery::StaticDataAreasGet),
            size_of::<uapi::DevQueryStaticDataAreas>() as u32
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn gpu_info_reports_the_packed_bvnc() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();
        let mut args = args_for(
            uapi::DevQuery::GpuInfoGet,
            size_of::<uapi::DevQueryGpuInfo>(),
        );

        dev_query(&file, &mut args, &caller).unwrap();

        let info: uapi::DevQueryGpuInfo = user.get(USER_BASE);
        assert_eq!(info.gpu_id, GpuId::K1_BXE_2_32.to_packed());
        // One Phantom per four clusters, rounded up.
        assert_eq!(info.num_phantoms, 1);
        assert_eq!(args.size, size_of::<uapi::DevQueryGpuInfo>() as u32);
    }

    /// `drm_pvr_dev_query_gpu_info` is the one interface object whose mandatory
    /// prefix stops short of its end, so it is where all three container stride
    /// cases are reachable.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn gpu_info_negotiates_all_three_strides() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();
        let ours = size_of::<uapi::DevQueryGpuInfo>();
        assert_eq!(uapi::DevQueryGpuInfo::MIN_STRIDE, ours - 4);

        // Equal: the whole object, and `size` is left alone.
        let mut args = args_for(uapi::DevQuery::GpuInfoGet, ours);
        dev_query(&file, &mut args, &caller).unwrap();
        assert_eq!(args.size, ours as u32);
        assert_eq!(
            user.get::<uapi::DevQueryGpuInfo>(USER_BASE).gpu_id,
            GpuId::K1_BXE_2_32.to_packed()
        );

        // Shorter: an older caller that has no `_padding_c`. It gets the prefix
        // and nothing beyond it is touched.
        caller.write_bytes(USER_BASE, &[0xff; 32]).unwrap();
        let mut args = args_for(
            uapi::DevQuery::GpuInfoGet,
            uapi::DevQueryGpuInfo::MIN_STRIDE,
        );
        dev_query(&file, &mut args, &caller).unwrap();
        assert_eq!(args.size, uapi::DevQueryGpuInfo::MIN_STRIDE as u32);
        assert_eq!(
            user.get_u32s(USER_BASE, 4),
            alloc::vec![0x0034_00b6, 0x0024_001d, 1, 0xffff_ffff]
        );

        // Longer: a newer caller. The tail is zeroed and `size` reports how much
        // of its buffer we understood.
        caller.write_bytes(USER_BASE, &[0xff; 32]).unwrap();
        let mut args = args_for(uapi::DevQuery::GpuInfoGet, ours + 8);
        dev_query(&file, &mut args, &caller).unwrap();
        assert_eq!(args.size, ours as u32);
        assert_eq!(
            user.get_u32s(USER_BASE + ours as u64, 3),
            alloc::vec![0, 0, 0xffff_ffff]
        );

        // Below the mandatory prefix.
        let mut args = args_for(
            uapi::DevQuery::GpuInfoGet,
            uapi::DevQueryGpuInfo::MIN_STRIDE - 1,
        );
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );
    }

    /// The numbers Mesa sizes its allocations from. Every one is a pure function
    /// of the firmware's device-info block, so they can be checked exactly.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn runtime_info_derives_its_numbers_from_the_device_info() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();
        let mut args = args_for(
            uapi::DevQuery::RuntimeInfoGet,
            size_of::<uapi::DevQueryRuntimeInfo>(),
        );

        dev_query(&file, &mut args, &caller).unwrap();

        let info: uapi::DevQueryRuntimeInfo = user.get(USER_BASE);
        // RogueXE without BRN66011.
        assert_eq!(info.free_list_min_pages, 25);
        // 2 GiB of PM pages.
        assert_eq!(info.free_list_max_pages, 524_288);
        // 16x16 tiles: tile_x * tile_y * max_partitions * usc_min_output_regs.
        assert_eq!(info.common_store_partition_space_size, 8192);
        // 21504 - 1024 - 8192 = 12288, capped by BRN44079's split point.
        assert_eq!(info.common_store_alloc_region_size, 11264);
        // 11264 - (6 * 1024 pixel + 1024 vertex + 0 pending) = 4096.
        assert_eq!(info.max_coeffs, 4096);
        assert_eq!(info.cdm_max_local_mem_size_regs, 4096);
    }

    /// Device-info fields used by the K1 runtime-query formulas.
    ///
    /// Values are parsed from `rogue_36.29.52.182_v1.fw` by `tools/fwinfo.py`.
    /// The separate [`bxe_info`] fixture has a BXS-4-64-shaped feature set to exercise
    /// the BRN44079 split-point branch; it is not a model of this K1 GPU.
    fn k1_firmware_info() -> DeviceInfo {
        DeviceInfo {
            features: Features {
                compute: true,
                compute_overlap: true,
                mips: true,
                num_clusters: true,
                pbvnc_coreid_reg: true,
                phys_bus_width: true,
                roguexe: true,
                xe_memory_hierarchy: true,
                ..Features::default()
            },
            values: FeatureValues {
                // 832 * 4 * 4. A quarter of the fixture's store.
                common_store_size_in_dwords: 13312,
                isp_max_tiles_in_flight: 2,
                max_partitions: 8,
                num_clusters: 1,
                num_isp_ipp_pipes: 2,
                tile_size_x: 16,
                tile_size_y: 16,
                usc_min_output_registers_per_pix: 2,
                ..FeatureValues::default()
            },
            // The quirk bitmap has exactly one bit set, BRN_63553, and this port
            // does not branch on it. Notably absent: BRN44079 (so the Common
            // Store is not split), BRN66011, BRN48492, BRN47217, BRN49927.
            quirks: Quirks::default(),
            // Six are set; the two that change these numbers are ERN_38748
            // (present, so no shared-register reservation) and ERN_38020
            // (absent, so no extra compute portion).
            enhancements: Enhancements {
                ern35421: true,
                ern38748: true,
                ..Enhancements::default()
            },
        }
    }

    /// The numbers this board's Mesa will actually receive, pinned exactly.
    ///
    /// Separate from [`runtime_info_derives_its_numbers_from_the_device_info`]
    /// because that one asserts the formulas against a synthetic fixture, and
    /// four of its six numbers are wrong for this hardware. Mesa sizes its
    /// allocations straight from these values, so being wrong here is not an
    /// error anyone sees: it is a GPU that renders incorrectly.
    ///
    /// The expected values were computed by `tools/fwinfo.py` from the firmware
    /// image, independently of this code — which is the property that makes the
    /// test worth having. Recomputed here so a reader can check the arithmetic
    /// without running anything.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn runtime_info_matches_the_k1_firmware_image() {
        let file = file_with(k1_firmware_info());
        let user = FakeUserMem::new();
        let caller = user.context();
        let mut args = args_for(
            uapi::DevQuery::RuntimeInfoGet,
            size_of::<uapi::DevQueryRuntimeInfo>(),
        );

        dev_query(&file, &mut args, &caller).unwrap();

        let info: uapi::DevQueryRuntimeInfo = user.get(USER_BASE);
        // RogueXE without BRN66011.
        assert_eq!(info.free_list_min_pages, 25);
        // 2 GiB of PM pages; a constant, independent of the device info.
        assert_eq!(info.free_list_max_pages, 524_288);
        // 16 * 16 * 8 partitions * 2 output registers.
        assert_eq!(info.common_store_partition_space_size, 4096);
        // 13312 - 1024 - 4096, with no BRN44079 split point to cap it.
        assert_eq!(info.common_store_alloc_region_size, 8192);
        // 8192 - (2 tiles in flight / 1 phantom * 1024 pixel + 1024 vertex
        // + 0 pending, because ERN_38748 is present).
        assert_eq!(info.max_coeffs, 5120);
        // ...then clamped by ROGUE_MAX_PER_KERNEL_LOCAL_MEM_SIZE_REGS. Note this
        // is 4096 for a different reason than in the fixture above, where
        // `max_coeffs` happened to equal the clamp: the same number, reached two
        // ways, so it cannot be used as evidence that the fixture is right.
        assert_eq!(info.cdm_max_local_mem_size_regs, 4096);

        // Mesa reads `num_phantoms` from a different query but uses it in the
        // same arithmetic, so it belongs to the same fact.
        let mut args = args_for(
            uapi::DevQuery::GpuInfoGet,
            size_of::<uapi::DevQueryGpuInfo>(),
        );
        dev_query(&file, &mut args, &caller).unwrap();
        let gpu: uapi::DevQueryGpuInfo = user.get(USER_BASE);
        // One cluster, so one phantom.
        assert_eq!(gpu.num_phantoms, 1);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn runtime_info_without_16x16_tiles_uses_the_other_formula() {
        let mut info = bxe_info();
        info.values.tile_size_x = 32;
        info.values.tile_size_y = 32;
        let file = file_with(info);
        let user = FakeUserMem::new();
        let caller = user.context();
        let mut args = args_for(
            uapi::DevQuery::RuntimeInfoGet,
            size_of::<uapi::DevQueryRuntimeInfo>(),
        );

        dev_query(&file, &mut args, &caller).unwrap();

        let runtime: uapi::DevQueryRuntimeInfo = user.get(USER_BASE);
        // max_partitions * 1024.
        assert_eq!(runtime.common_store_partition_space_size, 16384);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn quirks_report_must_haves_first() {
        let mut info = bxe_info();
        info.quirks.brn47217 = true;
        info.quirks.brn62269 = true;
        info.quirks.brn51764 = true;
        let file = file_with(info);
        let user = FakeUserMem::new();
        let caller = user.context();

        // First pass: a null array asks how many there are.
        let quirks_at = USER_BASE + 64;
        user.put(
            USER_BASE,
            &uapi::DevQueryQuirks {
                quirks: 0,
                count: 0,
                musthave_count: 0,
                _padding_c: 0,
            },
        );
        let mut args = args_for(uapi::DevQuery::QuirksGet, size_of::<uapi::DevQueryQuirks>());
        dev_query(&file, &mut args, &caller).unwrap();
        let query: uapi::DevQueryQuirks = user.get(USER_BASE);
        assert_eq!(query.count, 3);

        // Second pass: with room for them.
        user.put(
            USER_BASE,
            &uapi::DevQueryQuirks {
                quirks: quirks_at,
                count: 3,
                musthave_count: 0,
                _padding_c: 0,
            },
        );
        let mut args = args_for(uapi::DevQuery::QuirksGet, size_of::<uapi::DevQueryQuirks>());
        dev_query(&file, &mut args, &caller).unwrap();

        let query: uapi::DevQueryQuirks = user.get(USER_BASE);
        assert_eq!(query.count, 3);
        // The must-haves are a prefix, so their count describes the first two.
        assert_eq!(query.musthave_count, 2);
        assert_eq!(
            user.get_u32s(quirks_at, 3),
            alloc::vec![47217, 62269, 51764]
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn quirks_reject_a_short_array_and_dirty_padding() {
        let mut info = bxe_info();
        info.quirks.brn47217 = true;
        info.quirks.brn49927 = true;
        let file = file_with(info);
        let user = FakeUserMem::new();
        let caller = user.context();

        user.put(
            USER_BASE,
            &uapi::DevQueryQuirks {
                quirks: USER_BASE + 64,
                count: 1,
                musthave_count: 0,
                _padding_c: 0,
            },
        );
        let mut args = args_for(uapi::DevQuery::QuirksGet, size_of::<uapi::DevQueryQuirks>());
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::from(PvrError::TooBig))
        );

        user.put(
            USER_BASE,
            &uapi::DevQueryQuirks {
                quirks: 0,
                count: 0,
                musthave_count: 0,
                _padding_c: 1,
            },
        );
        let mut args = args_for(uapi::DevQuery::QuirksGet, size_of::<uapi::DevQueryQuirks>());
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn enhancements_report_the_uapi_list() {
        let mut info = bxe_info();
        info.enhancements.ern42064 = true;
        let file = file_with(info);
        let user = FakeUserMem::new();
        let caller = user.context();
        let list_at = USER_BASE + 64;

        user.put(
            USER_BASE,
            &uapi::DevQueryEnhancements {
                enhancements: list_at,
                count: 2,
                _padding_a: 0,
                _padding_c: 0,
            },
        );
        let mut args = args_for(
            uapi::DevQuery::EnhancementsGet,
            size_of::<uapi::DevQueryEnhancements>(),
        );
        dev_query(&file, &mut args, &caller).unwrap();

        let query: uapi::DevQueryEnhancements = user.get(USER_BASE);
        assert_eq!(query.count, 2);
        // ERN35421 comes from the fixture, ERN42064 was added above.
        assert_eq!(user.get_u32s(list_at, 2), alloc::vec![35421, 42064]);

        // Either padding field being set is a malformed request.
        user.put(
            USER_BASE,
            &uapi::DevQueryEnhancements {
                enhancements: 0,
                count: 0,
                _padding_a: 1,
                _padding_c: 0,
            },
        );
        let mut args = args_for(
            uapi::DevQuery::EnhancementsGet,
            size_of::<uapi::DevQueryEnhancements>(),
        );
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );
    }

    /// Fills a heap-info query and returns what the caller sees.
    fn run_heap_info(
        file: &PvrFile,
        user: &FakeUserMem,
        array: u64,
        count: u32,
        stride: u32,
    ) -> DrmResult<uapi::DevQueryHeapInfo> {
        let caller = user.context();
        user.put(
            USER_BASE,
            &uapi::DevQueryHeapInfo {
                heaps: uapi::ObjArray {
                    stride,
                    count,
                    array,
                },
            },
        );
        let mut args = args_for(
            uapi::DevQuery::HeapInfoGet,
            size_of::<uapi::DevQueryHeapInfo>(),
        );
        dev_query(file, &mut args, &caller)?;
        Ok(user.get(USER_BASE))
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn heap_info_reports_count_and_stride_for_a_null_array() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();

        let query = run_heap_info(&file, &user, 0, 0, size_of::<uapi::Heap>() as u32).unwrap();
        assert_eq!(query.heaps.count, uapi::HEAP_COUNT);
        assert_eq!(query.heaps.stride, size_of::<uapi::Heap>() as u32);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn heap_info_writes_all_six_heaps() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let heaps_at = USER_BASE + 64;
        let stride = size_of::<uapi::Heap>() as u32;

        let query = run_heap_info(&file, &user, heaps_at, uapi::HEAP_COUNT, stride).unwrap();
        assert_eq!(query.heaps.count, uapi::HEAP_COUNT);

        let general: uapi::Heap = user.get(heaps_at);
        assert_eq!(general.base, heap::ROGUE_GENERAL_HEAP_BASE);
        assert_eq!(general.size, heap::ROGUE_GENERAL_HEAP_SIZE);
        assert_eq!(general.page_size_log2, 12);
        assert_eq!(general.flags, 0);

        // Without BRN63142 the region header heap is present but empty.
        let rgn_hdr: uapi::Heap =
            user.get(heaps_at + u64::from(stride) * uapi::HeapId::RgnHdr as u64);
        assert_eq!(rgn_hdr.base, heap::ROGUE_RGNHDR_HEAP_BASE);
        assert_eq!(rgn_hdr.size, 0);

        let transfer: uapi::Heap =
            user.get(heaps_at + u64::from(stride) * uapi::HeapId::TransferFrag as u64);
        assert_eq!(transfer.base, heap::ROGUE_TRANSFER_FRAG_HEAP_BASE);
        assert_eq!(transfer.size, heap::ROGUE_TRANSFER_FRAG_HEAP_SIZE);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn heap_info_gives_the_region_header_heap_a_size_with_brn63142() {
        let mut info = bxe_info();
        info.quirks.brn63142 = true;
        let file = file_with(info);
        let user = FakeUserMem::new();
        let heaps_at = USER_BASE + 64;
        let stride = size_of::<uapi::Heap>() as u32;

        run_heap_info(&file, &user, heaps_at, uapi::HEAP_COUNT, stride).unwrap();

        let rgn_hdr: uapi::Heap =
            user.get(heaps_at + u64::from(stride) * uapi::HeapId::RgnHdr as u64);
        assert_eq!(rgn_hdr.size, heap::ROGUE_RGNHDR_HEAP_SIZE);
    }

    /// The three stride cases the shared negotiation has to handle, exercised
    /// through the ioctl rather than against `uapi::set_obj_array` directly.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn heap_info_negotiates_all_three_strides() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let heaps_at = USER_BASE + 64;
        let ours = size_of::<uapi::Heap>() as u32;

        // Equal: the caller's element is exactly ours.
        run_heap_info(&file, &user, heaps_at, 1, ours).unwrap();
        assert_eq!(
            user.get::<uapi::Heap>(heaps_at).size,
            heap::ROGUE_GENERAL_HEAP_SIZE
        );

        // Longer: the object followed by zeroes, and the next element starts a
        // full stride further on.
        let longer = ours + 8;
        run_heap_info(&file, &user, heaps_at, 2, longer).unwrap();
        assert_eq!(
            user.get_u32s(heaps_at + u64::from(ours), 2),
            alloc::vec![0, 0]
        );
        assert_eq!(
            user.get::<uapi::Heap>(heaps_at + u64::from(longer)).base,
            heap::ROGUE_PDSCODEDATA_HEAP_BASE
        );

        // Shorter than our object but at or above the mandatory prefix. For
        // `drm_pvr_heap` the prefix is the whole structure, so the only legal
        // short stride is `MIN_STRIDE` itself.
        assert_eq!(uapi::Heap::MIN_STRIDE, size_of::<uapi::Heap>());
        run_heap_info(&file, &user, heaps_at, 1, uapi::Heap::MIN_STRIDE as u32).unwrap();
        assert_eq!(
            user.get::<uapi::Heap>(heaps_at).base,
            heap::ROGUE_GENERAL_HEAP_BASE
        );

        // Below the prefix is refused outright.
        assert_eq!(
            run_heap_info(&file, &user, heaps_at, 1, ours - 1).err(),
            Some(DrmError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn heap_info_clamps_an_over_long_count() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let heaps_at = USER_BASE + 64;

        let query = run_heap_info(
            &file,
            &user,
            heaps_at,
            uapi::HEAP_COUNT + 4,
            size_of::<uapi::Heap>() as u32,
        )
        .unwrap();
        assert_eq!(query.heaps.count, uapi::HEAP_COUNT);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn static_data_areas_report_the_firmwares_layout() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();
        let areas_at = USER_BASE + 64;
        let stride = size_of::<uapi::StaticDataArea>() as u32;

        // First pass: how many are there?
        user.put(
            USER_BASE,
            &uapi::DevQueryStaticDataAreas {
                static_data_areas: uapi::ObjArray {
                    stride,
                    count: 0,
                    array: 0,
                },
            },
        );
        let mut args = args_for(
            uapi::DevQuery::StaticDataAreasGet,
            size_of::<uapi::DevQueryStaticDataAreas>(),
        );
        dev_query(&file, &mut args, &caller).unwrap();
        let query: uapi::DevQueryStaticDataAreas = user.get(USER_BASE);
        assert_eq!(query.static_data_areas.count, 5);
        assert_eq!(query.static_data_areas.stride, stride);

        // Second pass: fetch them.
        user.put(
            USER_BASE,
            &uapi::DevQueryStaticDataAreas {
                static_data_areas: uapi::ObjArray {
                    stride,
                    count: 5,
                    array: areas_at,
                },
            },
        );
        let mut args = args_for(
            uapi::DevQuery::StaticDataAreasGet,
            size_of::<uapi::DevQueryStaticDataAreas>(),
        );
        dev_query(&file, &mut args, &caller).unwrap();

        let fence: uapi::StaticDataArea = user.get(areas_at);
        assert_eq!(fence.area_usage, uapi::StaticDataAreaUsage::Fence as u16);
        assert_eq!(fence.location_heap_id, uapi::HeapId::General as u16);
        assert_eq!((fence.offset, fence.size), (0, 128));

        let eot: uapi::StaticDataArea = user.get(areas_at + u64::from(stride) * 3);
        assert_eq!(eot.area_usage, uapi::StaticDataAreaUsage::Eot as u16);
        assert_eq!(eot.location_heap_id, uapi::HeapId::PdsCodeData as u16);
        assert_eq!((eot.offset, eot.size), (128, 128));

        // The last entry is a second VDM_SYNC, in the USC code heap.
        let vdm_sync: uapi::StaticDataArea = user.get(areas_at + u64::from(stride) * 4);
        assert_eq!(
            vdm_sync.area_usage,
            uapi::StaticDataAreaUsage::VdmSync as u16
        );
        assert_eq!(vdm_sync.location_heap_id, uapi::HeapId::UscCode as u16);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_unknown_query_type_is_rejected() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();
        let mut args = uapi::DevQueryArgs {
            r#type: 6,
            size: 0,
            pointer: 0,
        };
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn an_absurd_size_is_rejected_rather_than_allocated() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();

        // The write path (no container to read first).
        let mut args = args_for(uapi::DevQuery::GpuInfoGet, MAX_QUERY_BYTES as usize + 1);
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );

        // The read path.
        let mut args = args_for(uapi::DevQuery::HeapInfoGet, MAX_QUERY_BYTES as usize + 1);
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );

        // And an array element stride nobody could mean.
        let user2 = FakeUserMem::new();
        assert_eq!(
            run_heap_info(&file, &user2, USER_BASE + 64, 1, MAX_QUERY_BYTES + 1).err(),
            Some(DrmError::InvalidArgument)
        );
    }

    /// A container shorter than its mandatory prefix is `EINVAL`, and one longer
    /// than the kernel's must have a zeroed tail.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn container_strides_follow_the_shared_rule() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();

        let mut args = args_for(
            uapi::DevQuery::HeapInfoGet,
            uapi::DevQueryHeapInfo::MIN_STRIDE - 1,
        );
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );

        // A longer container with a dirty tail is refused; the same request with
        // a zeroed tail succeeds and reports how much we understood.
        let longer = size_of::<uapi::DevQueryHeapInfo>() + 8;
        user.put(
            USER_BASE,
            &uapi::DevQueryHeapInfo {
                heaps: uapi::ObjArray {
                    stride: size_of::<uapi::Heap>() as u32,
                    count: 0,
                    array: 0,
                },
            },
        );
        user.put(
            USER_BASE + size_of::<uapi::DevQueryHeapInfo>() as u64,
            &1u64,
        );
        let mut args = args_for(uapi::DevQuery::HeapInfoGet, longer);
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::InvalidArgument)
        );

        user.put(
            USER_BASE + size_of::<uapi::DevQueryHeapInfo>() as u64,
            &0u64,
        );
        let mut args = args_for(uapi::DevQuery::HeapInfoGet, longer);
        dev_query(&file, &mut args, &caller).unwrap();
        assert_eq!(args.size, size_of::<uapi::DevQueryHeapInfo>() as u32);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_lost_device_answers_nothing() {
        let file = file_with(bxe_info());
        file.device.declare_lost();
        let user = FakeUserMem::new();
        let caller = user.context();

        let mut args = args_for(
            uapi::DevQuery::GpuInfoGet,
            size_of::<uapi::DevQueryGpuInfo>(),
        );
        assert_eq!(dev_query(&file, &mut args, &caller), Err(DrmError::Io));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn a_faulting_user_pointer_is_reported() {
        let file = file_with(bxe_info());
        let user = FakeUserMem::new();
        let caller = user.context();

        // A pointer we cannot follow is `EFAULT`, not `EINVAL`: user space
        // distinguishes "your stride is wrong" from "your pointer is wrong".
        let mut args = uapi::DevQueryArgs {
            r#type: uapi::DevQuery::GpuInfoGet as u32,
            size: size_of::<uapi::DevQueryGpuInfo>() as u32,
            pointer: USER_BASE - 8,
        };
        assert_eq!(
            dev_query(&file, &mut args, &caller),
            Err(DrmError::BadAddress)
        );

        // Including on the array a query fills rather than the container.
        assert_eq!(
            run_heap_info(&file, &user, USER_BASE - 8, uapi::HEAP_COUNT, 24).err(),
            Some(DrmError::BadAddress)
        );
    }

    /// Upstream's designated initializers tie each heap to its `drm_pvr_heap_id`;
    /// a plain array does not, so the order is pinned here instead.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn heap_ids_match_the_table_order() {
        let table = heaps();
        assert_eq!(table.len(), uapi::HEAP_COUNT as usize);
        for (id, base) in [
            (uapi::HeapId::General, heap::ROGUE_GENERAL_HEAP_BASE),
            (uapi::HeapId::PdsCodeData, heap::ROGUE_PDSCODEDATA_HEAP_BASE),
            (uapi::HeapId::UscCode, heap::ROGUE_USCCODE_HEAP_BASE),
            (uapi::HeapId::RgnHdr, heap::ROGUE_RGNHDR_HEAP_BASE),
            (uapi::HeapId::VisTest, heap::ROGUE_VISTEST_HEAP_BASE),
            (
                uapi::HeapId::TransferFrag,
                heap::ROGUE_TRANSFER_FRAG_HEAP_BASE,
            ),
        ] {
            assert_eq!(table[id as usize].base, base);
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn handles_start_at_one_and_are_reused() {
        let table: HandleTable<u32> = HandleTable::new();

        assert_eq!(table.insert(Arc::new(10)).unwrap(), 1);
        assert_eq!(table.insert(Arc::new(20)).unwrap(), 2);
        assert_eq!(table.insert(Arc::new(30)).unwrap(), 3);
        assert_eq!(*table.get(2).unwrap(), 20);

        // A freed handle is handed out again, as `xa_alloc` does.
        assert_eq!(*table.remove(2).unwrap(), 20);
        assert_eq!(table.get(2), Err(PvrError::NoSuchHandle));
        assert_eq!(table.insert(Arc::new(40)).unwrap(), 2);

        assert_eq!(table.remove(9), Err(PvrError::NoSuchHandle));
        assert_eq!(table.len(), 3);
        assert_eq!(table.drain().len(), 3);
        assert!(table.is_empty());
    }
}
