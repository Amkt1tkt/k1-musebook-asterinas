// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR ioctl ABI and checked object-array copying.

//! The PowerVR user-space ABI.
//!
//! A transcription of `include/uapi/drm/pvr_drm.h` (1,295 lines at the revision
//! pinned in [`crate`]'s `UPSTREAM`). This is the whole contract with Mesa:
//! fourteen ioctls, and it is a stable upstream uapi, so nothing here may be
//! invented or "improved". Field order, sizes and padding must match byte for
//! byte, and every structure carries a [`sizes`] entry proving it does.
//!
//! Reference: <https://elixir.bootlin.com/linux/latest/source/include/uapi/drm/pvr_drm.h>.

use alloc::vec::Vec;
use core::mem::{align_of, offset_of, size_of};

use bitflags::bitflags;

use crate::error::{PvrError, PvrResult};

/// Upstream `offsetofend($ty, $field)`: byte offset of `$field` plus that
/// field's size. `$field_ty` must be the declared type of `$field`.
///
/// Used for every [`MIN_STRIDE`](Job::MIN_STRIDE) constant. The result must
/// never be replaced with a hand-written integer — that is the class of bug
/// this module's `const` asserts exist to catch.
macro_rules! offsetofend {
    ($ty:ty, $field:ident, $field_ty:ty) => {
        offset_of!($ty, $field) + size_of::<$field_ty>()
    };
}

/// The DRM driver-private ioctl range base.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/include/drm/drm_ioctl.h>.
pub const DRM_COMMAND_BASE: u32 = 0x40;

/// Driver-private ioctl numbers, as offsets from [`DRM_COMMAND_BASE`].
///
/// The numbers are ABI. Mesa calls by number.
///
/// Corresponds to the `DRM_IOCTL_PVR_*` macros in `pvr_drm.h`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PvrIoctl {
    /// Query device properties. `DRM_IOWR`. `DRM_IOCTL_PVR_DEV_QUERY`.
    DevQuery = 0x00,
    /// Allocate a buffer object. `DRM_IOWR`. `DRM_IOCTL_PVR_CREATE_BO`.
    CreateBo = 0x01,
    /// Get the fake mmap offset for a buffer object. `DRM_IOWR`.
    /// `DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET`.
    GetBoMmapOffset = 0x02,
    /// Create a GPU address space. `DRM_IOWR`. `DRM_IOCTL_PVR_CREATE_VM_CONTEXT`.
    CreateVmContext = 0x03,
    /// Destroy a GPU address space. `DRM_IOW`. `DRM_IOCTL_PVR_DESTROY_VM_CONTEXT`.
    DestroyVmContext = 0x04,
    /// Map a buffer object into a GPU address space. `DRM_IOW`. `DRM_IOCTL_PVR_VM_MAP`.
    VmMap = 0x05,
    /// Remove a mapping. `DRM_IOW`. `DRM_IOCTL_PVR_VM_UNMAP`.
    VmUnmap = 0x06,
    /// Create a rendering context. `DRM_IOWR`. `DRM_IOCTL_PVR_CREATE_CONTEXT`.
    CreateContext = 0x07,
    /// Destroy a rendering context. `DRM_IOW`. `DRM_IOCTL_PVR_DESTROY_CONTEXT`.
    DestroyContext = 0x08,
    /// Create a free list for the parameter manager. `DRM_IOWR`.
    /// `DRM_IOCTL_PVR_CREATE_FREE_LIST`.
    CreateFreeList = 0x09,
    /// Destroy a free list. `DRM_IOW`. `DRM_IOCTL_PVR_DESTROY_FREE_LIST`.
    DestroyFreeList = 0x0a,
    /// Create a hardware render target dataset. `DRM_IOWR`.
    /// `DRM_IOCTL_PVR_CREATE_HWRT_DATASET`.
    CreateHwrtDataset = 0x0b,
    /// Destroy a hardware render target dataset. `DRM_IOW`.
    /// `DRM_IOCTL_PVR_DESTROY_HWRT_DATASET`.
    DestroyHwrtDataset = 0x0c,
    /// Submit work to the GPU. `DRM_IOW`. `DRM_IOCTL_PVR_SUBMIT_JOBS`.
    SubmitJobs = 0x0d,
}

impl PvrIoctl {
    /// Resolves a driver-private ioctl number, as it appears in the encoded
    /// command after [`DRM_COMMAND_BASE`] has been subtracted.
    pub fn from_nr(nr: u32) -> Option<Self> {
        match nr {
            0x00 => Some(Self::DevQuery),
            0x01 => Some(Self::CreateBo),
            0x02 => Some(Self::GetBoMmapOffset),
            0x03 => Some(Self::CreateVmContext),
            0x04 => Some(Self::DestroyVmContext),
            0x05 => Some(Self::VmMap),
            0x06 => Some(Self::VmUnmap),
            0x07 => Some(Self::CreateContext),
            0x08 => Some(Self::DestroyContext),
            0x09 => Some(Self::CreateFreeList),
            0x0a => Some(Self::DestroyFreeList),
            0x0b => Some(Self::CreateHwrtDataset),
            0x0c => Some(Self::DestroyHwrtDataset),
            0x0d => Some(Self::SubmitJobs),
            _ => None,
        }
    }
}

/// A user pointer to an array of objects, with an explicit element stride.
///
/// Upstream extends indirect objects by growing the structure and relying on the
/// stride to tell old callers from new ones: a caller-supplied stride smaller
/// than ours means the tail must be zero-filled, larger means the excess must be
/// verified zero and ignored. See [`obj_array`] for the shared implementation of
/// that rule — do not open-code it per ioctl.
///
/// Corresponds to `struct drm_pvr_obj_array`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct ObjArray {
    /// Size of one element as the caller understands it.
    pub stride: u32,
    /// Number of elements.
    pub count: u32,
    /// User-space address of the first element.
    pub array: u64,
}

/// Which `DEV_QUERY` sub-query is being issued.
///
/// Corresponds to `enum drm_pvr_dev_query`. Append-only; do not reorder.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevQuery {
    /// `struct drm_pvr_dev_query_gpu_info`.
    GpuInfoGet = 0,
    /// `struct drm_pvr_dev_query_runtime_info`.
    RuntimeInfoGet = 1,
    /// `struct drm_pvr_dev_query_quirks`.
    QuirksGet = 2,
    /// `struct drm_pvr_dev_query_enhancements`.
    EnhancementsGet = 3,
    /// `struct drm_pvr_dev_query_heap_info`.
    HeapInfoGet = 4,
    /// `struct drm_pvr_dev_query_static_data_areas`.
    StaticDataAreasGet = 5,
}

/// Arguments for [`PvrIoctl::DevQuery`].
///
/// Corresponds to `struct drm_pvr_ioctl_dev_query_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryArgs {
    /// Which query, from [`DevQuery`].
    pub r#type: u32,
    /// Size of the buffer at [`Self::pointer`].
    ///
    /// A caller may pass a null pointer with a zero size to learn the required
    /// size, which the driver writes back here.
    pub size: u32,
    /// User-space destination buffer.
    pub pointer: u64,
}

/// Container used to fetch information about the graphics processor.
///
/// Corresponds to `struct drm_pvr_dev_query_gpu_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryGpuInfo {
    /// GPU identifier. Packed BVNC for all currently supported GPUs:
    /// `B` in bits 63..48, `V` in 47..32, `N` in 31..16, `C` in 15..0.
    pub gpu_id: u64,
    /// Number of Phantoms present.
    pub num_phantoms: u32,
    /// Reserved. Must be zeroed by the caller.
    pub _padding_c: u32,
}

impl DevQueryGpuInfo {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., num_phantoms)`.
    ///
    /// Trailing [`Self::_padding_c`] is optional for versioning.
    pub const MIN_STRIDE: usize = offsetofend!(Self, num_phantoms, u32);
}

/// Container used to fetch information about the graphics runtime.
///
/// Corresponds to `struct drm_pvr_dev_query_runtime_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryRuntimeInfo {
    /// Minimum allowed free list size, in PM physical pages.
    pub free_list_min_pages: u64,
    /// Maximum allowed free list size, in PM physical pages.
    pub free_list_max_pages: u64,
    /// Size of the Allocation Region within the Common Store, in dwords.
    pub common_store_alloc_region_size: u32,
    /// Size of the Partition Space within the Common Store, in dwords.
    pub common_store_partition_space_size: u32,
    /// Maximum coefficients, in dwords.
    pub max_coeffs: u32,
    /// Maximum local memory available to a compute kernel, in dwords.
    pub cdm_max_local_mem_size_regs: u32,
}

impl DevQueryRuntimeInfo {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., cdm_max_local_mem_size_regs)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, cdm_max_local_mem_size_regs, u32);
}

/// Container used to fetch hardware quirks (BRNs).
///
/// Corresponds to `struct drm_pvr_dev_query_quirks`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryQuirks {
    /// User pointer to a `__u32` quirks array.
    pub quirks: u64,
    /// Length of [`Self::quirks`] (number of `__u32`).
    pub count: u16,
    /// Number of mandatory quirks at the start of the list.
    pub musthave_count: u16,
    /// Reserved. Must be zeroed by the caller.
    pub _padding_c: u32,
}

impl DevQueryQuirks {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., _padding_c)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, _padding_c, u32);
}

/// Container used to fetch optional enhancements (ERNs).
///
/// Corresponds to `struct drm_pvr_dev_query_enhancements`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryEnhancements {
    /// User pointer to a `__u32` enhancements array.
    pub enhancements: u64,
    /// Length of [`Self::enhancements`] (number of `__u32`).
    pub count: u16,
    /// Reserved. Must be zeroed by the caller.
    pub _padding_a: u16,
    /// Reserved. Must be zeroed by the caller.
    pub _padding_c: u32,
}

impl DevQueryEnhancements {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., _padding_c)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, _padding_c, u32);
}

/// GPU virtual address heaps.
///
/// All indices are always reported, even for heaps this GPU does not have; an
/// absent heap is signalled by a zero [`Heap::size`] rather than by being
/// omitted.
///
/// Corresponds to `enum drm_pvr_heap_id`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapId {
    /// General purpose.
    General = 0,
    /// PDS code and data.
    PdsCodeData = 1,
    /// USC code.
    UscCode = 2,
    /// Region header. Present only when the GPU has `BRN63142`.
    RgnHdr = 3,
    /// Visibility test.
    VisTest = 4,
    /// Transfer fragment.
    TransferFrag = 5,
}

/// The number of heaps reported by `DRM_PVR_DEV_QUERY_HEAP_INFO_GET`.
///
/// Corresponds to `DRM_PVR_HEAP_COUNT`.
pub const HEAP_COUNT: u32 = 6;

/// Description of one GPU virtual address heap.
///
/// Corresponds to `struct drm_pvr_heap`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct Heap {
    /// Base GPU virtual address.
    pub base: u64,
    /// Size in bytes, or zero if this GPU does not have the heap.
    pub size: u64,
    /// Always zero at present.
    pub flags: u32,
    /// Log2 of the heap's page size.
    pub page_size_log2: u32,
}

impl Heap {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., page_size_log2)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, page_size_log2, u32);
}

/// Container used to fetch heap information.
///
/// Corresponds to `struct drm_pvr_dev_query_heap_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryHeapInfo {
    /// Array of [`Heap`]. A null pointer updates count/stride only.
    pub heaps: ObjArray,
}

impl DevQueryHeapInfo {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., heaps)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, heaps, ObjArray);
}

/// Usage of a static data area within a heap.
///
/// Corresponds to `enum drm_pvr_static_data_area_usage`.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticDataAreaUsage {
    /// End of Tile PDS program code segment.
    Eot = 0,
    /// MCU fence area, used during cache flush and invalidation.
    Fence = 1,
    /// VDM sync program.
    VdmSync = 2,
    /// YUV coefficients.
    YuvCsc = 3,
}

/// One static data area.
///
/// Corresponds to `struct drm_pvr_static_data_area`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct StaticDataArea {
    /// Usage. A [`StaticDataAreaUsage`] discriminant.
    pub area_usage: u16,
    /// Heap index from [`DevQuery::HeapInfoGet`].
    pub location_heap_id: u16,
    /// Size of the area. Not present if zero.
    pub size: u32,
    /// Offset of the area from the start of the heap.
    pub offset: u64,
}

impl StaticDataArea {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., offset)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, offset, u64);
}

/// Container used to fetch static data area information.
///
/// Corresponds to `struct drm_pvr_dev_query_static_data_areas`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DevQueryStaticDataAreas {
    /// Array of [`StaticDataArea`]. A null pointer updates count/stride only.
    pub static_data_areas: ObjArray,
}

impl DevQueryStaticDataAreas {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., static_data_areas)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, static_data_areas, ObjArray);
}

bitflags! {
    /// Buffer object creation flags (`DRM_PVR_BO_*`).
    ///
    /// Corresponds to the `DRM_PVR_BO_*` macros in `pvr_drm.h`.
    pub struct BoFlags: u64 {
        /// Map uncached on the GPU.
        const BYPASS_DEVICE_CACHE = 1 << 0;
        /// Reserve the object for firmware use; not CPU mappable.
        const PM_FW_PROTECT = 1 << 1;
        /// Permit user-space CPU mapping.
        const ALLOW_CPU_USERSPACE_ACCESS = 1 << 2;
    }
}

impl BoFlags {
    /// The set of flags the driver accepts. Anything else is `EINVAL`.
    ///
    /// Corresponds to `DRM_PVR_BO_FLAGS_MASK`.
    pub const MASK: u64 = Self::BYPASS_DEVICE_CACHE.bits()
        | Self::PM_FW_PROTECT.bits()
        | Self::ALLOW_CPU_USERSPACE_ACCESS.bits();
}

/// Flat constants for call sites that want a `u64` rather than [`BoFlags`].
///
/// Prefer [`BoFlags`] in new code. This module exists so the rest of the crate's
/// existing doc links (`uapi::bo_flags::*`) keep resolving.
pub mod bo_flags {
    use super::BoFlags;

    /// [`BoFlags::BYPASS_DEVICE_CACHE`].
    pub const BYPASS_DEVICE_CACHE: u64 = BoFlags::BYPASS_DEVICE_CACHE.bits();
    /// [`BoFlags::PM_FW_PROTECT`].
    pub const PM_FW_PROTECT: u64 = BoFlags::PM_FW_PROTECT.bits();
    /// [`BoFlags::ALLOW_CPU_USERSPACE_ACCESS`].
    pub const ALLOW_CPU_USERSPACE_ACCESS: u64 = BoFlags::ALLOW_CPU_USERSPACE_ACCESS.bits();
    /// [`BoFlags::MASK`].
    pub const MASK: u64 = BoFlags::MASK;
}

/// Arguments for [`PvrIoctl::CreateBo`].
///
/// Corresponds to `struct drm_pvr_ioctl_create_bo_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateBoArgs {
    /// Requested size in bytes; rounded up to the device page size.
    pub size: u64,
    /// Handle of the created object. Written by the driver.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_c: u32,
    /// See [`BoFlags`].
    pub flags: u64,
}

/// Arguments for [`PvrIoctl::GetBoMmapOffset`].
///
/// Corresponds to `struct drm_pvr_ioctl_get_bo_mmap_offset_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct GetBoMmapOffsetArgs {
    /// Handle of the object to map.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
    /// The fake offset to pass to `mmap`. Written by the driver.
    pub offset: u64,
}

/// Arguments for [`PvrIoctl::CreateVmContext`].
///
/// Corresponds to `struct drm_pvr_ioctl_create_vm_context_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateVmContextArgs {
    /// Handle of the created context. Written by the driver.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
}

/// Arguments for [`PvrIoctl::DestroyVmContext`].
///
/// Corresponds to `struct drm_pvr_ioctl_destroy_vm_context_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DestroyVmContextArgs {
    /// Handle of the context to destroy.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
}

/// Arguments for [`PvrIoctl::VmMap`].
///
/// Corresponds to `struct drm_pvr_ioctl_vm_map_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct VmMapArgs {
    /// Target VM context.
    pub vm_context_handle: u32,
    /// Always zero at present.
    pub flags: u32,
    /// GPU virtual address to map at. Must be device-page aligned.
    pub device_addr: u64,
    /// Buffer object to map.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_14: u32,
    /// Offset into the buffer object.
    pub offset: u64,
    /// Length to map. Zero means the whole object.
    pub size: u64,
}

/// Arguments for [`PvrIoctl::VmUnmap`].
///
/// Corresponds to `struct drm_pvr_ioctl_vm_unmap_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct VmUnmapArgs {
    /// Target VM context.
    pub vm_context_handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
    /// GPU virtual address of the mapping to remove.
    pub device_addr: u64,
    /// Length of the mapping. Zero means the whole mapping.
    pub size: u64,
}

/// Context scheduling priority.
///
/// Corresponds to `enum drm_pvr_ctx_priority`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtxPriority {
    /// Priority below normal.
    Low = -512,
    /// Normal priority.
    Normal = 0,
    /// Priority above normal. Requires `CAP_SYS_NICE` or `DRM_MASTER`.
    High = 512,
}

/// Type of rendering context to create.
///
/// Corresponds to `enum drm_pvr_ctx_type`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtxType {
    /// Render context.
    Render = 0,
    /// Compute context.
    Compute = 1,
    /// Transfer context for fragment data master.
    TransferFrag = 2,
}

/// Arguments for [`PvrIoctl::CreateContext`].
///
/// Corresponds to `struct drm_pvr_ioctl_create_context_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateContextArgs {
    /// Type of context. A [`CtxType`] discriminant.
    pub r#type: u32,
    /// Flags for context.
    pub flags: u32,
    /// Priority. A [`CtxPriority`] discriminant.
    pub priority: i32,
    /// Handle for the new context. Written by the driver.
    pub handle: u32,
    /// Pointer to the static context state stream.
    pub static_context_state: u64,
    /// Length of static context state, in bytes.
    pub static_context_state_len: u32,
    /// VM context this context is associated with.
    pub vm_context_handle: u32,
    /// Initial call stack pointer. Only valid for [`CtxType::Render`].
    pub callstack_addr: u64,
}

/// Arguments for [`PvrIoctl::DestroyContext`].
///
/// Corresponds to `struct drm_pvr_ioctl_destroy_context_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DestroyContextArgs {
    /// Handle of the context to destroy.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
}

/// Arguments for [`PvrIoctl::CreateFreeList`].
///
/// Corresponds to `struct drm_pvr_ioctl_create_free_list_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateFreeListArgs {
    /// GPU virtual address of the free-list buffer object.
    pub free_list_gpu_addr: u64,
    /// Pages initially allocated to the free list.
    pub initial_num_pages: u32,
    /// Maximum number of pages in the free list.
    pub max_num_pages: u32,
    /// Pages to grow the free list by per request.
    pub grow_num_pages: u32,
    /// Percentage of FL memory used that should trigger a grow.
    pub grow_threshold: u32,
    /// VM context the free-list buffer is mapped in.
    pub vm_context_handle: u32,
    /// Handle for the created free list. Written by the driver.
    pub handle: u32,
}

/// Arguments for [`PvrIoctl::DestroyFreeList`].
///
/// Corresponds to `struct drm_pvr_ioctl_destroy_free_list_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DestroyFreeListArgs {
    /// Handle of the free list to destroy.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
}

/// Geometry data arguments for HWRT dataset creation.
///
/// Corresponds to `struct drm_pvr_create_hwrt_geom_data_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateHwrtGeomDataArgs {
    /// Tail pointer cache GPU virtual address.
    pub tpc_dev_addr: u64,
    /// Size of TPC, in bytes.
    pub tpc_size: u32,
    /// Stride between layers in TPC, in pages.
    pub tpc_stride: u32,
    /// VHEAP table GPU virtual address.
    pub vheap_table_dev_addr: u64,
    /// Render Target Cache virtual address.
    pub rtc_dev_addr: u64,
}

/// Render target arguments for HWRT dataset creation.
///
/// Corresponds to `struct drm_pvr_create_hwrt_rt_data_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateHwrtRtDataArgs {
    /// PM MLIST GPU virtual address.
    pub pm_mlist_dev_addr: u64,
    /// Macrotile array GPU virtual address.
    pub macrotile_array_dev_addr: u64,
    /// Region header array GPU virtual address.
    pub region_header_dev_addr: u64,
}

/// Index of the local free list in [`CreateHwrtDatasetArgs::free_list_handles`].
///
/// Corresponds to `PVR_DRM_HWRT_FREE_LIST_LOCAL`.
pub const HWRT_FREE_LIST_LOCAL: usize = 0;
/// Index of the global free list in [`CreateHwrtDatasetArgs::free_list_handles`].
///
/// Corresponds to `PVR_DRM_HWRT_FREE_LIST_GLOBAL`.
pub const HWRT_FREE_LIST_GLOBAL: usize = 1;

/// Arguments for [`PvrIoctl::CreateHwrtDataset`].
///
/// Corresponds to `struct drm_pvr_ioctl_create_hwrt_dataset_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct CreateHwrtDatasetArgs {
    /// Geometry data arguments.
    pub geom_data_args: CreateHwrtGeomDataArgs,
    /// Render target arguments (double-buffered).
    pub rt_data_args: [CreateHwrtRtDataArgs; 2],
    /// Free list handles: local then global.
    pub free_list_handles: [u32; 2],
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Number of samples.
    pub samples: u32,
    /// Number of layers.
    pub layers: u32,
    /// Lower X coefficient for triangle merging.
    pub isp_merge_lower_x: u32,
    /// Lower Y coefficient for triangle merging.
    pub isp_merge_lower_y: u32,
    /// Scale X coefficient for triangle merging.
    pub isp_merge_scale_x: u32,
    /// Scale Y coefficient for triangle merging.
    pub isp_merge_scale_y: u32,
    /// Upper X coefficient for triangle merging.
    pub isp_merge_upper_x: u32,
    /// Upper Y coefficient for triangle merging.
    pub isp_merge_upper_y: u32,
    /// Size of the region header array (format-dependent units).
    pub region_header_size: u32,
    /// Handle for the created HWRT dataset. Written by the driver.
    pub handle: u32,
}

/// Arguments for [`PvrIoctl::DestroyHwrtDataset`].
///
/// Corresponds to `struct drm_pvr_ioctl_destroy_hwrt_dataset_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct DestroyHwrtDatasetArgs {
    /// Handle of the HWRT dataset to destroy.
    pub handle: u32,
    /// Padding; must be zero.
    pub _padding_4: u32,
}

bitflags! {
    /// Flags for [`SyncOp`] (`DRM_PVR_SYNC_OP_FLAG_*`).
    ///
    /// Corresponds to the `DRM_PVR_SYNC_OP_FLAG_*` macros in `pvr_drm.h`.
    ///
    /// `DRM_PVR_SYNC_OP_FLAG_HANDLE_TYPE_SYNCOBJ` and
    /// `DRM_PVR_SYNC_OP_FLAG_WAIT` are both zero (the defaults) and so are not
    /// represented as bits here; see [`SyncOpFlags::WAIT`] and the handle-type
    /// constants below.
    pub struct SyncOpFlags: u32 {
        /// Handle-type field mask.
        const HANDLE_TYPE_MASK = 0xf;
        /// Handle is a timeline syncobj.
        const HANDLE_TYPE_TIMELINE_SYNCOBJ = 1;
        /// Signal operation requested.
        const SIGNAL = 1 << 31;
    }
}

/// Handle is a binary syncobj (the default).
///
/// Corresponds to `DRM_PVR_SYNC_OP_FLAG_HANDLE_TYPE_SYNCOBJ` (= 0).
pub const SYNC_OP_HANDLE_TYPE_SYNCOBJ: u32 = 0;

impl SyncOpFlags {
    /// Wait is the default operation (no bits set beyond the handle type).
    ///
    /// Corresponds to `DRM_PVR_SYNC_OP_FLAG_WAIT` (= 0).
    pub const WAIT: u32 = 0;

    /// Mask of all recognised flag bits.
    ///
    /// Corresponds to `DRM_PVR_SYNC_OP_FLAGS_MASK`.
    pub const FLAGS_MASK: u32 = Self::HANDLE_TYPE_MASK.bits() | Self::SIGNAL.bits();
}

/// One sync operation attached to a job.
///
/// Corresponds to `struct drm_pvr_sync_op`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct SyncOp {
    /// Handle of the sync object.
    pub handle: u32,
    /// Combination of [`SyncOpFlags`].
    pub flags: u32,
    /// Timeline value. Must be zero for a binary syncobj.
    pub value: u64,
}

impl SyncOp {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., value)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, value, u64);
}

bitflags! {
    /// Flags for a geometry command (`DRM_PVR_SUBMIT_JOB_GEOM_CMD_*`).
    pub struct GeomCmdFlags: u64 {
        /// First command issued for a render.
        const FIRST = 1 << 0;
        /// Last command issued for a render.
        const LAST = 1 << 1;
        /// Force a single core on a multi-core device.
        const SINGLE_CORE = 1 << 2;
    }
}

impl GeomCmdFlags {
    /// Logical OR of all geometry cmd flags.
    ///
    /// Corresponds to `DRM_PVR_SUBMIT_JOB_GEOM_CMD_FLAGS_MASK`.
    pub const FLAGS_MASK: u64 = Self::FIRST.bits() | Self::LAST.bits() | Self::SINGLE_CORE.bits();
}

bitflags! {
    /// Flags for a fragment command (`DRM_PVR_SUBMIT_JOB_FRAG_CMD_*`).
    pub struct FragCmdFlags: u64 {
        /// Use a single core in a multi-core setup.
        const SINGLE_CORE = 1 << 0;
        /// A depth buffer is present.
        const DEPTHBUFFER = 1 << 1;
        /// A stencil buffer is present.
        const STENCILBUFFER = 1 << 2;
        /// Disallow compute overlapped with this render.
        const PREVENT_CDM_OVERLAP = 1 << 3;
        /// Partial renders write to a scratch buffer.
        const SCRATCHBUFFER = 1 << 4;
        /// This render produces visibility results.
        const GET_VIS_RESULTS = 1 << 5;
        /// Partial render.
        const PARTIAL_RENDER = 1 << 6;
        /// Disable pixel merging for this render.
        const DISABLE_PIXELMERGE = 1 << 7;
    }
}

impl FragCmdFlags {
    /// Logical OR of all fragment cmd flags.
    ///
    /// Corresponds to `DRM_PVR_SUBMIT_JOB_FRAG_CMD_FLAGS_MASK`.
    pub const FLAGS_MASK: u64 = Self::SINGLE_CORE.bits()
        | Self::DEPTHBUFFER.bits()
        | Self::STENCILBUFFER.bits()
        | Self::PREVENT_CDM_OVERLAP.bits()
        | Self::SCRATCHBUFFER.bits()
        | Self::GET_VIS_RESULTS.bits()
        | Self::PARTIAL_RENDER.bits()
        | Self::DISABLE_PIXELMERGE.bits();
}

bitflags! {
    /// Flags for a compute command (`DRM_PVR_SUBMIT_JOB_COMPUTE_CMD_*`).
    pub struct ComputeCmdFlags: u64 {
        /// Disallow other jobs overlapped with this compute.
        const PREVENT_ALL_OVERLAP = 1 << 0;
        /// Force a single core on a multi-core device.
        const SINGLE_CORE = 1 << 1;
    }
}

impl ComputeCmdFlags {
    /// Logical OR of all compute cmd flags.
    ///
    /// Corresponds to `DRM_PVR_SUBMIT_JOB_COMPUTE_CMD_FLAGS_MASK`.
    pub const FLAGS_MASK: u64 = Self::PREVENT_ALL_OVERLAP.bits() | Self::SINGLE_CORE.bits();
}

bitflags! {
    /// Flags for a transfer command (`DRM_PVR_SUBMIT_JOB_TRANSFER_CMD_*`).
    pub struct TransferCmdFlags: u64 {
        /// Force the job to use a single core on a multi-core device.
        const SINGLE_CORE = 1 << 0;
    }
}

impl TransferCmdFlags {
    /// Logical OR of all transfer cmd flags.
    ///
    /// Corresponds to `DRM_PVR_SUBMIT_JOB_TRANSFER_CMD_FLAGS_MASK`.
    pub const FLAGS_MASK: u64 = Self::SINGLE_CORE.bits();
}

/// Type of job being submitted.
///
/// Corresponds to `enum drm_pvr_job_type`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobType {
    /// Geometry job.
    Geometry = 0,
    /// Fragment job.
    Fragment = 1,
    /// Compute job.
    Compute = 2,
    /// Fragment transfer job.
    TransferFrag = 3,
}

/// Reference to HWRT data inside a dataset.
///
/// Corresponds to `struct drm_pvr_hwrt_data_ref`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct HwrtDataRef {
    /// HWRT data set handle.
    pub set_handle: u32,
    /// Index of the HWRT data inside the data set.
    pub data_index: u32,
}

/// One job in a [`SubmitJobsArgs`] batch.
///
/// Corresponds to `struct drm_pvr_job`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct Job {
    /// Type of job. A [`JobType`] discriminant.
    pub r#type: u32,
    /// Context handle.
    pub context_handle: u32,
    /// Job-dependent flags. See the `*CmdFlags` types.
    pub flags: u32,
    /// Length of the command stream, in bytes.
    pub cmd_stream_len: u32,
    /// Pointer to the command stream. Must be u64-aligned.
    pub cmd_stream: u64,
    /// Sync operations for this job.
    pub sync_ops: ObjArray,
    /// HWRT data used by render jobs. Must be zero for non-render jobs.
    pub hwrt: HwrtDataRef,
}

impl Job {
    /// Upstream `PVR_UOBJ_MIN_SIZE` — `offsetofend(..., hwrt)`.
    pub const MIN_STRIDE: usize = offsetofend!(Self, hwrt, HwrtDataRef);
}

/// Arguments for [`PvrIoctl::SubmitJobs`].
///
/// Corresponds to `struct drm_pvr_ioctl_submit_jobs_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct SubmitJobsArgs {
    /// Array of jobs to submit.
    pub jobs: ObjArray,
}

/// Shared `usr_stride < min_stride → -EINVAL` gate.
///
/// This is the hard floor from upstream `pvr_get_uobj` / `pvr_set_uobj` /
/// `pvr_get_uobj_array` / `pvr_set_uobj_array`. Every path that accepts a
/// versioned userspace object must go through here — do not open-code the
/// comparison.
#[inline]
pub fn check_min_stride(usr_stride: usize, min_stride: usize) -> PvrResult<()> {
    if usr_stride < min_stride {
        Err(PvrError::InvalidArgument)
    } else {
        Ok(())
    }
}

/// Copies one userspace object under the shared stride rule (`pvr_get_uobj`).
///
/// `usr` is the caller's object image; its length is the caller's stride.
/// `min_stride` is the type's [`MIN_STRIDE`](Job::MIN_STRIDE);
/// `our_size` is `size_of::<T>()`.
///
/// Order matches upstream: reject `len < min_stride` first, then
/// zero-extend / verify-excess via [`negotiate_element`].
pub fn copy_uobj(usr: &[u8], min_stride: usize, our_size: usize) -> PvrResult<Vec<u8>> {
    if our_size == 0 {
        return Err(PvrError::InvalidArgument);
    }
    check_min_stride(usr.len(), min_stride)?;
    negotiate_element(usr, our_size)
}

/// Applies the shared object-array stride negotiation to a contiguous image of
/// caller elements (`pvr_get_uobj_array`).
///
/// This is the **only** array implementation of the compatibility rule. Order
/// matches upstream exactly:
///
/// 1. `array.stride < min_stride` → [`PvrError::InvalidArgument`] (`-EINVAL`);
/// 2. `array.count == 0` → empty `Ok` (even when `raw` is empty);
/// 3. otherwise copy each element: short → zero-fill tail, long → excess must
///    be zero, equal → copy as-is.
///
/// `raw` must be exactly `array.count * array.stride` bytes when `count > 0`.
/// Returns `count * our_stride` bytes — one kernel-sized element per entry,
/// tightly packed.
///
/// `min_stride` is the element type's [`MIN_STRIDE`](Job::MIN_STRIDE);
/// `our_stride` is `size_of::<T>()`.
pub fn obj_array(
    array: &ObjArray,
    min_stride: usize,
    our_stride: usize,
    raw: &[u8],
) -> PvrResult<Vec<u8>> {
    if our_stride == 0 {
        return Err(PvrError::InvalidArgument);
    }
    // Upstream checks min_stride before the count==0 short-circuit. An empty
    // array with an illegal stride must still be rejected.
    check_min_stride(array.stride as usize, min_stride)?;
    if array.count == 0 {
        return Ok(Vec::new());
    }
    let stride = array.stride as usize;
    let expected = stride
        .checked_mul(array.count as usize)
        .ok_or(PvrError::InvalidArgument)?;
    if raw.len() != expected {
        return Err(PvrError::InvalidArgument);
    }

    let mut out = Vec::with_capacity(our_stride.saturating_mul(array.count as usize));
    for i in 0..array.count as usize {
        let start = i * stride;
        let elem = &raw[start..start + stride];
        out.extend(negotiate_element(elem, our_stride)?);
    }
    Ok(out)
}

/// Renders one kernel object into a caller-sized element (`pvr_set_uobj`).
///
/// The mirror image of [`copy_uobj`], for the direction where the kernel is the
/// producer: the caller's `usr_stride` decides how many bytes it expects, so a
/// short caller gets the leading prefix and a long one gets the object followed
/// by zeroes. Nothing is verified about the caller's existing contents — the
/// kernel is overwriting them — which is the one asymmetry with the read
/// direction.
///
/// Returns exactly `usr_stride` bytes.
pub fn set_uobj(usr_stride: usize, min_stride: usize, object: &[u8]) -> PvrResult<Vec<u8>> {
    check_min_stride(usr_stride, min_stride)?;
    Ok(expand_element(object, usr_stride))
}

/// Renders kernel objects into a caller's object array (`pvr_set_uobj_array`).
///
/// The output-direction counterpart of [`obj_array`], and the only one: writing
/// the compatibility rule out again per ioctl is how the two directions drift
/// apart.
///
/// `objects` is `array.count * our_stride` bytes of tightly packed kernel
/// objects. Returns `array.count * array.stride` bytes, ready to be written at
/// `array.array`.
///
/// Callers must bound `array.stride` themselves; this allocates `count * stride`
/// bytes and has no view of what a sensible bound is.
pub fn set_obj_array(
    array: &ObjArray,
    min_stride: usize,
    our_stride: usize,
    objects: &[u8],
) -> PvrResult<Vec<u8>> {
    if our_stride == 0 {
        return Err(PvrError::InvalidArgument);
    }
    check_min_stride(array.stride as usize, min_stride)?;
    if array.count == 0 {
        return Ok(Vec::new());
    }

    let count = array.count as usize;
    let stride = array.stride as usize;
    if objects.len()
        != count
            .checked_mul(our_stride)
            .ok_or(PvrError::InvalidArgument)?
    {
        return Err(PvrError::InvalidArgument);
    }

    let mut out = Vec::with_capacity(count.checked_mul(stride).ok_or(PvrError::InvalidArgument)?);
    for object in objects.chunks_exact(our_stride) {
        out.extend(expand_element(object, stride));
    }
    Ok(out)
}

/// Negotiates one caller's element into a kernel-sized buffer.
///
/// Assumes the caller has already enforced [`check_min_stride`].
fn negotiate_element(caller: &[u8], our_stride: usize) -> PvrResult<Vec<u8>> {
    let mut out = alloc::vec![0u8; our_stride];
    let copy_len = core::cmp::min(caller.len(), our_stride);
    out[..copy_len].copy_from_slice(&caller[..copy_len]);
    if caller.len() > our_stride && caller[our_stride..].iter().any(|&b| b != 0) {
        return Err(PvrError::InvalidArgument);
    }
    Ok(out)
}

/// Renders one kernel object into `usr_stride` bytes.
///
/// Assumes the caller has already enforced [`check_min_stride`].
fn expand_element(object: &[u8], usr_stride: usize) -> Vec<u8> {
    let mut out = alloc::vec![0u8; usr_stride];
    let copy_len = core::cmp::min(object.len(), usr_stride);
    out[..copy_len].copy_from_slice(&object[..copy_len]);
    out
}

/// Compile-time size checks for every ioctl argument structure.
///
/// Ioctl numbers encode argument size. Driver-private dispatch copies the caller's
/// encoded length, so a larger kernel type fails the payload-size check while a
/// smaller type can omit caller fields. These constants are transcribed from the
/// UAPI header independently of the definitions under test.
///
/// The C regressions also encode their own argument sizes. Agreement between a
/// fixture and the implementation alone does not establish ABI compatibility.
///
/// The matching userspace header is Mesa 26.1.6's `include/drm-uapi/pvr_drm.h`.
/// The Linux 6.6 BSP uses the incompatible pre-upstream srvkm interface; this
/// fourteen-ioctl interface entered Linux 6.8. [`layout_checks`] additionally
/// checks alignments and `MIN_STRIDE` prefixes.
mod sizes {
    use super::*;

    macro_rules! assert_size {
        ($($type:ty => $size:expr,)*) => {
            $(const _: () = assert!(size_of::<$type>() == $size);)*
        };
    }

    // Numbers from Mesa 26.1.6 `include/drm-uapi/pvr_drm.h`, measured with
    // `sizeof` against that header, not inferred from the Rust types above.
    assert_size! {
        ObjArray => 16,
        DevQueryArgs => 16,
        DevQueryGpuInfo => 16,
        DevQueryRuntimeInfo => 32,
        DevQueryQuirks => 16,
        DevQueryEnhancements => 16,
        Heap => 24,
        DevQueryHeapInfo => 16,
        StaticDataArea => 16,
        DevQueryStaticDataAreas => 16,
        CreateBoArgs => 24,
        GetBoMmapOffsetArgs => 16,
        CreateVmContextArgs => 8,
        DestroyVmContextArgs => 8,
        VmMapArgs => 40,
        VmUnmapArgs => 24,
        CreateContextArgs => 40,
        DestroyContextArgs => 8,
        CreateFreeListArgs => 32,
        DestroyFreeListArgs => 8,
        CreateHwrtGeomDataArgs => 32,
        CreateHwrtRtDataArgs => 24,
        CreateHwrtDatasetArgs => 136,
        DestroyHwrtDatasetArgs => 8,
        SyncOp => 16,
        HwrtDataRef => 8,
        Job => 48,
        SubmitJobsArgs => 16,
    }

    // Encoded `DRM_IOCTL_PVR_*` values from the same header. Linux `_IOC`
    // layout: dir in 31..30 (WRITE=1, READ|WRITE=3), size in 29..16, type
    // `'d'` in 15..8, `DRM_COMMAND_BASE + offset` in 7..0. `DriHandle::call`
    // consumes the size and direction from this word for private commands; a
    // mismatch here is the same class of bug as a wrong struct size.
    const IOC_WRITE: u32 = 1;
    const IOC_READ_WRITE: u32 = 3;

    const fn drm_ioc(dir: u32, nr: u32, size: usize) -> u32 {
        (dir << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | nr
    }

    macro_rules! assert_ioctl {
        ($($dir:expr, $off:expr, $size:expr => $encoded:expr;)*) => {
            $(const _: () = assert!(
                drm_ioc($dir, DRM_COMMAND_BASE + $off, $size) == $encoded
            );)*
        };
    }

    assert_ioctl! {
        IOC_READ_WRITE, 0x00,  16 => 0xc0106440; // DEV_QUERY
        IOC_READ_WRITE, 0x01,  24 => 0xc0186441; // CREATE_BO
        IOC_READ_WRITE, 0x02,  16 => 0xc0106442; // GET_BO_MMAP_OFFSET
        IOC_READ_WRITE, 0x03,   8 => 0xc0086443; // CREATE_VM_CONTEXT
        IOC_WRITE,      0x04,   8 => 0x40086444; // DESTROY_VM_CONTEXT
        IOC_WRITE,      0x05,  40 => 0x40286445; // VM_MAP
        IOC_WRITE,      0x06,  24 => 0x40186446; // VM_UNMAP
        IOC_READ_WRITE, 0x07,  40 => 0xc0286447; // CREATE_CONTEXT
        IOC_WRITE,      0x08,   8 => 0x40086448; // DESTROY_CONTEXT
        IOC_READ_WRITE, 0x09,  32 => 0xc0206449; // CREATE_FREE_LIST
        IOC_WRITE,      0x0a,   8 => 0x4008644a; // DESTROY_FREE_LIST
        IOC_READ_WRITE, 0x0b, 136 => 0xc088644b; // CREATE_HWRT_DATASET
        IOC_WRITE,      0x0c,   8 => 0x4008644c; // DESTROY_HWRT_DATASET
        IOC_WRITE,      0x0d,  16 => 0x4010644d; // SUBMIT_JOBS
    }

    // Enum discriminants and flag bits. A wrong value here is not `EINVAL`:
    // the command is accepted and then silently does the wrong thing.
    const _: () = assert!(DRM_COMMAND_BASE == 0x40);
    const _: () = assert!(PvrIoctl::DevQuery as u32 == 0x00);
    const _: () = assert!(PvrIoctl::CreateBo as u32 == 0x01);
    const _: () = assert!(PvrIoctl::GetBoMmapOffset as u32 == 0x02);
    const _: () = assert!(PvrIoctl::CreateVmContext as u32 == 0x03);
    const _: () = assert!(PvrIoctl::DestroyVmContext as u32 == 0x04);
    const _: () = assert!(PvrIoctl::VmMap as u32 == 0x05);
    const _: () = assert!(PvrIoctl::VmUnmap as u32 == 0x06);
    const _: () = assert!(PvrIoctl::CreateContext as u32 == 0x07);
    const _: () = assert!(PvrIoctl::DestroyContext as u32 == 0x08);
    const _: () = assert!(PvrIoctl::CreateFreeList as u32 == 0x09);
    const _: () = assert!(PvrIoctl::DestroyFreeList as u32 == 0x0a);
    const _: () = assert!(PvrIoctl::CreateHwrtDataset as u32 == 0x0b);
    const _: () = assert!(PvrIoctl::DestroyHwrtDataset as u32 == 0x0c);
    const _: () = assert!(PvrIoctl::SubmitJobs as u32 == 0x0d);

    const _: () = assert!(DevQuery::GpuInfoGet as u32 == 0);
    const _: () = assert!(DevQuery::RuntimeInfoGet as u32 == 1);
    const _: () = assert!(DevQuery::QuirksGet as u32 == 2);
    const _: () = assert!(DevQuery::EnhancementsGet as u32 == 3);
    const _: () = assert!(DevQuery::HeapInfoGet as u32 == 4);
    const _: () = assert!(DevQuery::StaticDataAreasGet as u32 == 5);

    const _: () = assert!(HeapId::General as u32 == 0);
    const _: () = assert!(HeapId::PdsCodeData as u32 == 1);
    const _: () = assert!(HeapId::UscCode as u32 == 2);
    const _: () = assert!(HeapId::RgnHdr as u32 == 3);
    const _: () = assert!(HeapId::VisTest as u32 == 4);
    const _: () = assert!(HeapId::TransferFrag as u32 == 5);
    const _: () = assert!(HEAP_COUNT == 6);

    const _: () = assert!(StaticDataAreaUsage::Eot as u16 == 0);
    const _: () = assert!(StaticDataAreaUsage::Fence as u16 == 1);
    const _: () = assert!(StaticDataAreaUsage::VdmSync as u16 == 2);
    const _: () = assert!(StaticDataAreaUsage::YuvCsc as u16 == 3);

    const _: () = assert!(BoFlags::BYPASS_DEVICE_CACHE.bits() == 0x1);
    const _: () = assert!(BoFlags::PM_FW_PROTECT.bits() == 0x2);
    const _: () = assert!(BoFlags::ALLOW_CPU_USERSPACE_ACCESS.bits() == 0x4);
    const _: () = assert!(BoFlags::MASK == 0x7);

    const _: () = assert!(CtxPriority::Low as i32 == -512);
    const _: () = assert!(CtxPriority::Normal as i32 == 0);
    const _: () = assert!(CtxPriority::High as i32 == 512);
    const _: () = assert!(CtxType::Render as u32 == 0);
    const _: () = assert!(CtxType::Compute as u32 == 1);
    const _: () = assert!(CtxType::TransferFrag as u32 == 2);

    const _: () = assert!(HWRT_FREE_LIST_LOCAL == 0);
    const _: () = assert!(HWRT_FREE_LIST_GLOBAL == 1);

    const _: () = assert!(SyncOpFlags::HANDLE_TYPE_MASK.bits() == 0xf);
    const _: () = assert!(SYNC_OP_HANDLE_TYPE_SYNCOBJ == 0);
    const _: () = assert!(SyncOpFlags::HANDLE_TYPE_TIMELINE_SYNCOBJ.bits() == 1);
    const _: () = assert!(SyncOpFlags::SIGNAL.bits() == 0x8000_0000);
    const _: () = assert!(SyncOpFlags::WAIT == 0);
    const _: () = assert!(SyncOpFlags::FLAGS_MASK == 0x8000_000f);

    const _: () = assert!(GeomCmdFlags::FIRST.bits() == 0x1);
    const _: () = assert!(GeomCmdFlags::LAST.bits() == 0x2);
    const _: () = assert!(GeomCmdFlags::SINGLE_CORE.bits() == 0x4);
    const _: () = assert!(GeomCmdFlags::FLAGS_MASK == 0x7);

    const _: () = assert!(FragCmdFlags::SINGLE_CORE.bits() == 0x1);
    const _: () = assert!(FragCmdFlags::DEPTHBUFFER.bits() == 0x2);
    const _: () = assert!(FragCmdFlags::STENCILBUFFER.bits() == 0x4);
    const _: () = assert!(FragCmdFlags::PREVENT_CDM_OVERLAP.bits() == 0x8);
    const _: () = assert!(FragCmdFlags::SCRATCHBUFFER.bits() == 0x10);
    const _: () = assert!(FragCmdFlags::GET_VIS_RESULTS.bits() == 0x20);
    const _: () = assert!(FragCmdFlags::PARTIAL_RENDER.bits() == 0x40);
    const _: () = assert!(FragCmdFlags::DISABLE_PIXELMERGE.bits() == 0x80);
    const _: () = assert!(FragCmdFlags::FLAGS_MASK == 0xff);

    const _: () = assert!(ComputeCmdFlags::PREVENT_ALL_OVERLAP.bits() == 0x1);
    const _: () = assert!(ComputeCmdFlags::SINGLE_CORE.bits() == 0x2);
    const _: () = assert!(ComputeCmdFlags::FLAGS_MASK == 0x3);

    const _: () = assert!(TransferCmdFlags::SINGLE_CORE.bits() == 0x1);
    const _: () = assert!(TransferCmdFlags::FLAGS_MASK == 0x1);

    const _: () = assert!(JobType::Geometry as u32 == 0);
    const _: () = assert!(JobType::Fragment as u32 == 1);
    const _: () = assert!(JobType::Compute as u32 == 2);
    const _: () = assert!(JobType::TransferFrag as u32 == 3);
}

/// Alignments and `MIN_STRIDE` prefixes. Sizes live in [`sizes`]: a wrong
/// alignment on this ABI is almost always a wrong size as well, but the
/// stride floor is independent and a stale one silently accepts a truncated
/// object.
mod layout_checks {
    use super::*;

    macro_rules! assert_align {
        ($($ty:ty => $align:expr;)*) => {
            $(const _: () = assert!(align_of::<$ty>() == $align);)*
        };
    }

    assert_align! {
        ObjArray => 8;
        DevQueryArgs => 8;
        DevQueryGpuInfo => 8;
        DevQueryRuntimeInfo => 8;
        DevQueryQuirks => 8;
        DevQueryEnhancements => 8;
        Heap => 8;
        DevQueryHeapInfo => 8;
        StaticDataArea => 8;
        DevQueryStaticDataAreas => 8;
        CreateBoArgs => 8;
        GetBoMmapOffsetArgs => 8;
        CreateVmContextArgs => 4;
        DestroyVmContextArgs => 4;
        VmMapArgs => 8;
        VmUnmapArgs => 8;
        CreateContextArgs => 8;
        DestroyContextArgs => 4;
        CreateFreeListArgs => 8;
        DestroyFreeListArgs => 4;
        CreateHwrtGeomDataArgs => 8;
        CreateHwrtRtDataArgs => 8;
        CreateHwrtDatasetArgs => 8;
        DestroyHwrtDatasetArgs => 4;
        SyncOp => 8;
        HwrtDataRef => 4;
        Job => 8;
        SubmitJobsArgs => 8;
    }

    // min_stride is always a positive prefix of the kernel object.
    macro_rules! assert_min_stride {
        ($($ty:ty;)*) => {
            $(
                const _: () = assert!(<$ty>::MIN_STRIDE > 0);
                const _: () = assert!(<$ty>::MIN_STRIDE <= size_of::<$ty>());
            )*
        };
    }

    assert_min_stride! {
        Job;
        SyncOp;
        DevQueryGpuInfo;
        DevQueryRuntimeInfo;
        DevQueryQuirks;
        DevQueryEnhancements;
        Heap;
        DevQueryHeapInfo;
        StaticDataArea;
        DevQueryStaticDataAreas;
    }

    // Spot-check the `offsetofend` results against the C layout. These are
    // consequences of the definitions above, not substitutes for them: if a
    // field moves, both the `offsetofend!` expression and this assert must be
    // reconsidered together.
    const _: () = assert!(Job::MIN_STRIDE == 48);
    const _: () = assert!(SyncOp::MIN_STRIDE == 16);
    const _: () = assert!(DevQueryGpuInfo::MIN_STRIDE == 12);
    const _: () = assert!(DevQueryRuntimeInfo::MIN_STRIDE == 32);
    const _: () = assert!(DevQueryQuirks::MIN_STRIDE == 16);
    const _: () = assert!(DevQueryEnhancements::MIN_STRIDE == 16);
    const _: () = assert!(Heap::MIN_STRIDE == 24);
    const _: () = assert!(DevQueryHeapInfo::MIN_STRIDE == 16);
    const _: () = assert!(StaticDataArea::MIN_STRIDE == 16);
    const _: () = assert!(DevQueryStaticDataAreas::MIN_STRIDE == 16);
}

/// Host-side unit tests for the stride rule.
///
/// These are ordinary `#[test]`s so they can run under `cargo test` on a host
/// target once the crate's dependencies allow it. They are also compiled under
/// `ktest` for `cargo osdk test`.
#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// A stand-in kernel element: 8 bytes, two `u32`s.
    const OUR: usize = 8;
    /// Synthetic min_stride that permits a 4-byte caller prefix.
    const MIN: usize = 4;

    fn array(stride: u32, count: u32) -> ObjArray {
        ObjArray {
            stride,
            count,
            array: 0,
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn obj_array_stride_shorter_zero_fills_tail() {
        // Caller only has the first u32; the second must become zero.
        // min_stride == 4 so this short-but-legal prefix is accepted.
        let raw = [0x11u8, 0x22, 0x33, 0x44];
        let out = obj_array(&array(4, 1), MIN, OUR, &raw).unwrap();
        assert_eq!(out, [0x11, 0x22, 0x33, 0x44, 0, 0, 0, 0]);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn obj_array_stride_equal_copies() {
        let raw = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let out = obj_array(&array(8, 1), OUR, OUR, &raw).unwrap();
        assert_eq!(out, raw);
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn obj_array_stride_longer_accepts_zero_excess() {
        let mut raw = [1u8, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0];
        let out = obj_array(&array(12, 1), OUR, OUR, &raw).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);

        raw[9] = 1;
        assert_eq!(
            obj_array(&array(12, 1), OUR, OUR, &raw),
            Err(PvrError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn obj_array_rejects_stride_below_min() {
        let raw = [0u8; 23];
        assert_eq!(
            obj_array(&array(23, 1), Heap::MIN_STRIDE, size_of::<Heap>(), &raw),
            Err(PvrError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn obj_array_accepts_exact_min_stride_zero_fills() {
        // GpuInfo: MIN_STRIDE ends at `num_phantoms`; trailing `_padding_c`
        // must be zero-filled when the caller omits it.
        assert!(DevQueryGpuInfo::MIN_STRIDE < size_of::<DevQueryGpuInfo>());
        let mut raw = [0u8; DevQueryGpuInfo::MIN_STRIDE];
        raw[0] = 0xaa;
        let out = obj_array(
            &array(DevQueryGpuInfo::MIN_STRIDE as u32, 1),
            DevQueryGpuInfo::MIN_STRIDE,
            size_of::<DevQueryGpuInfo>(),
            &raw,
        )
        .unwrap();
        assert_eq!(out.len(), size_of::<DevQueryGpuInfo>());
        assert_eq!(out[0], 0xaa);
        assert!(out[DevQueryGpuInfo::MIN_STRIDE..].iter().all(|&b| b == 0));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn obj_array_zero_count_returns_empty_after_min_check() {
        let out = obj_array(
            &array(Heap::MIN_STRIDE as u32, 0),
            Heap::MIN_STRIDE,
            size_of::<Heap>(),
            &[],
        )
        .unwrap();
        assert!(out.is_empty());

        // count == 0 must not bypass the min_stride gate.
        assert_eq!(
            obj_array(&array(1, 0), Heap::MIN_STRIDE, size_of::<Heap>(), &[]),
            Err(PvrError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn copy_uobj_shares_min_stride_gate() {
        let short = [0u8; 11];
        assert_eq!(
            copy_uobj(
                &short,
                DevQueryGpuInfo::MIN_STRIDE,
                size_of::<DevQueryGpuInfo>()
            ),
            Err(PvrError::InvalidArgument)
        );
        let ok = [0u8; DevQueryGpuInfo::MIN_STRIDE];
        let out = copy_uobj(
            &ok,
            DevQueryGpuInfo::MIN_STRIDE,
            size_of::<DevQueryGpuInfo>(),
        )
        .unwrap();
        assert_eq!(out.len(), size_of::<DevQueryGpuInfo>());
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn set_obj_array_matches_the_callers_stride() {
        let objects = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        // Equal: copied through unchanged.
        let out = set_obj_array(&array(8, 2), MIN, OUR, &objects).unwrap();
        assert_eq!(out, objects);

        // Short: the caller only gets the mandatory prefix.
        let out = set_obj_array(&array(4, 2), MIN, OUR, &objects).unwrap();
        assert_eq!(out, [1, 2, 3, 4, 9, 10, 11, 12]);

        // Long: the object followed by zeroes, per element.
        let out = set_obj_array(&array(12, 2), MIN, OUR, &objects).unwrap();
        assert_eq!(
            out,
            [
                1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 9, 10, 11, 12, 13, 14, 15, 16, 0, 0, 0, 0
            ]
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn set_obj_array_shares_the_min_stride_gate() {
        let objects = [0u8; OUR];
        assert_eq!(
            set_obj_array(&array(MIN as u32 - 1, 1), MIN, OUR, &objects),
            Err(PvrError::InvalidArgument)
        );
        // A zero count still has to pass the gate, as in the read direction.
        assert_eq!(
            set_obj_array(&array(MIN as u32 - 1, 0), MIN, OUR, &[]),
            Err(PvrError::InvalidArgument)
        );
        // The kernel-side buffer must describe exactly `count` objects.
        assert_eq!(
            set_obj_array(&array(OUR as u32, 2), MIN, OUR, &objects),
            Err(PvrError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn set_uobj_truncates_and_zero_extends() {
        let object = [1u8, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(set_uobj(OUR, MIN, &object).unwrap(), object);
        assert_eq!(set_uobj(MIN, MIN, &object).unwrap(), [1, 2, 3, 4]);
        assert_eq!(
            set_uobj(OUR + 2, MIN, &object).unwrap(),
            [1, 2, 3, 4, 5, 6, 7, 8, 0, 0]
        );
        assert_eq!(
            set_uobj(MIN - 1, MIN, &object),
            Err(PvrError::InvalidArgument)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn pvr_ioctl_from_nr_covers_all_fourteen() {
        for nr in 0u32..=0x0d {
            assert!(PvrIoctl::from_nr(nr).is_some());
        }
        assert!(PvrIoctl::from_nr(0x0e).is_none());
    }
}
