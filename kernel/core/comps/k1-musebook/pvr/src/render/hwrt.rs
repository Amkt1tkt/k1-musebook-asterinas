// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: hardware render-target lifetime and firmware cleanup.

//! Hardware render target datasets.
//!
//! Ports `pvr_hwrt.c` and `pvr_hwrt.h` (712 lines).
//!
//! A dataset describes a render target to the firmware: its dimensions, its
//! layers, the free lists that feed it, and the geometry state carried between
//! the geometry and fragment passes. Each dataset holds two render-target data
//! structures so that one frame's fragment pass can overlap the next frame's
//! geometry pass.
//!
//! # What is computed here and why it cannot be left to Mesa
//!
//! Most of this file turns a width, a height and a sample count into control
//! register values: the macrotile grid, the multisample positions, the screen
//! limits. Those are hardware layout decisions, and upstream makes them in the
//! kernel rather than accepting them from the client for a reason worth keeping
//! in view — they are derived from *feature values the firmware reports*
//! (`tile_size_x`, `isp_samples_per_pixel`, `simple_parameter_format_version`),
//! which user space has no authoritative copy of. A client that computed them
//! itself would be guessing at the part it is running on.
//!
//! # Lifetime
//!
//! Three layers, and the middle one is where the subtlety is:
//!
//! - **The dataset** lives as long as its handle or any job naming it. Dropping
//!   it releases everything below.
//! - **Its two [`HwrtData`]** are `Arc`s because the *local* free list keeps a
//!   weak reference to each, so that a reconstruction can reset them; see
//!   [`super`]. A job holds the dataset, not the data, which is what makes
//!   `data_index` meaningful for the whole life of the job.
//! - **The firmware objects** are heap allocations released in [`Drop`], in the
//!   reverse of the order they were made. [`HwrtDataset::destroy`] is separate
//!   because it waits for the firmware, which a destructor must not do.

use alloc::{sync::Arc, vec::Vec};
use core::mem::offset_of;

use ostd_pod::FromZeros;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwObject},
    render::{
        abi,
        free_list::{self, FreeList},
    },
    rogue::{
        cr,
        fwif::{FwAddr, RogueFwifHwrtdata, RogueFwifHwrtdataCommon, RogueFwifKccbCmd},
    },
    uapi,
};

/// Bytes per Shadow Render Target Cache entry. Upstream `SRTC_ENTRY_SIZE`.
const SRTC_ENTRY_SIZE: u64 = size_of::<u32>() as u64;
/// Bytes per Renders Accumulation Array entry. Upstream `RAA_ENTRY_SIZE`.
const RAA_ENTRY_SIZE: u64 = size_of::<u32>() as u64;

/// The uapi's array of render-target arguments is the firmware's count.
///
/// Upstream asserts `ROGUE_FWIF_NUM_RTDATAS == 2` at the top of `pvr_hwrt.c` and
/// separately declares `rt_data_args[2]` in the uapi. Tying them together here
/// means a regenerated constant cannot quietly disagree with the ABI.
const _: () = {
    // `rt_data_args` is immediately followed by `free_list_handles`, so the gap
    // between them is the array's length in bytes.
    assert!(
        offset_of!(uapi::CreateHwrtDatasetArgs, free_list_handles)
            - offset_of!(uapi::CreateHwrtDatasetArgs, rt_data_args)
            == abi::NUM_RTDATAS * size_of::<uapi::CreateHwrtRtDataArgs>()
    );
    assert!(abi::NUM_RTDATA_FREELISTS == 2);
    assert!(uapi::HWRT_FREE_LIST_LOCAL == 0);
    assert!(uapi::HWRT_FREE_LIST_GLOBAL == 1);
};

/// Macrotile geometry derived from a render target's size.
///
/// Upstream `struct pvr_rt_mtile_info`.
struct MacrotileInfo {
    /// Macrotile X boundaries, upstream `mtile_x`.
    mtile_x: [u32; 3],
    /// Macrotile Y boundaries, upstream `mtile_y`.
    mtile_y: [u32; 3],
    /// Index of the last tile in X, upstream `tile_max_x`.
    tile_max_x: u32,
    /// Index of the last tile in Y, upstream `tile_max_y`.
    tile_max_y: u32,
}

/// One of a dataset's two render-target data structures.
///
/// Upstream `struct pvr_hwrt_data`.
pub struct HwrtData {
    /// The device whose heap the allocations came from.
    device: Arc<PvrDevice>,
    /// The structure the firmware reads.
    fw_object: Option<FwObject>,
    /// Mapping ownership in order: firmware object, SRTC, RAA.
    mapped: [bool; 3],
    /// Shadow Render Target Cache. Only present when the target has layers.
    srtc: Option<FwObject>,
    /// Renders Accumulation Array. Only present when the target has layers.
    raa: Option<FwObject>,
}

impl HwrtData {
    /// Only fully initialized entries are published or exposed to other modules.
    fn fw_object(&self) -> &FwObject {
        self.fw_object
            .as_ref()
            .expect("published render target has firmware storage")
    }

    /// Firmware address of the structure.
    ///
    /// What a geometry or fragment job writes into
    /// `rogue_fwif_cmd_geom_frag_shared::hwrt_data_fw_addr`; upstream
    /// `pvr_fw_object_get_fw_addr(job->hwrt->fw_obj, ...)`.
    pub fn fw_addr(&self) -> FwAddr {
        self.fw_object().fw_addr()
    }

    /// Firmware address of the cleanup-control block inside the structure.
    ///
    /// What a `KICK` command lists in `cleanup_ctl_fw_addr`; upstream
    /// `fill_cmd_kick_data()` takes `offsetof(struct rogue_fwif_hwrtdata,
    /// cleanup_state)` for exactly this. Without it the firmware has no way to
    /// tell the host when it has finished with the render target, and a dataset
    /// destroyed while a render is in flight would be freed underneath it.
    pub fn cleanup_state_fw_addr(&self) -> PvrResult<FwAddr> {
        let offset = u32::try_from(offset_of!(RogueFwifHwrtdata, cleanup_state))
            .map_err(|_| PvrError::InvalidArgument)?;
        self.fw_object().fw_addr_at(offset)
    }

    /// The structure the firmware reads, for the tests that read it back.
    ///
    /// Not part of the driver's interface: these fields are the firmware's to
    /// update, and a caller with a reason to look at them is checking this
    /// module's own work. The same seam [`crate::sched::context::Context`] has.
    #[cfg(ktest)]
    pub(crate) fn fw_data_for_test(&self) -> PvrResult<RogueFwifHwrtdata> {
        self.fw_object().read_val(0)
    }

    /// Resets the target after its free list was reconstructed.
    ///
    /// Ports the `list_for_each_entry` tail of upstream
    /// `pvr_free_list_reconstruct()`. The parameter buffer this target's
    /// geometry pass filled no longer exists, so the firmware is told to treat
    /// the pass as never having happened: `HWR` rather than `NONE`, because
    /// `NONE` would make the next geometry job look like this target's first and
    /// the firmware would run the abandoned ones instead of skipping them.
    pub(super) fn mark_reconstructed(&self) -> PvrResult<()> {
        self.fw_object()
            .write_val(offset_of!(RogueFwifHwrtdata, state), &abi::RTDATA_STATE_HWR)?;

        let at = offset_of!(RogueFwifHwrtdata, hwrt_data_flags);
        let flags: u32 = self.fw_object().read_val(at)?;
        self.fw_object()
            .write_val(at, &(flags & !abi::HWRTDATA_HAS_LAST_GEOM))
    }
}

impl Drop for HwrtData {
    /// Returns the three possible allocations to the firmware heap.
    ///
    /// Reverse of the order [`HwrtDataset::build_data`] made them, which is also
    /// the order upstream's `hwrt_data_fini_fw_structure()` uses.
    fn drop(&mut self) {
        let Some(fw) = self.device.fw.get() else {
            return;
        };
        let mut heap = fw.heap.lock();
        // Distinct heap allocations; reverse initialization order. Unmap only
        // entries installed by this owner, including partial construction.
        for (object, mapped) in [
            (self.fw_object.as_ref(), self.mapped[0]),
            (self.raa.as_ref(), self.mapped[2]),
            (self.srtc.as_ref(), self.mapped[1]),
        ] {
            if let Some(object) = object {
                if mapped && let Err(err) = fw.mips.page_table.unmap(object) {
                    ostd::error!("could not unmap render target storage: {err:?}");
                }
                heap.free(object);
            }
        }
    }
}

impl core::fmt::Debug for HwrtData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HwrtData")
            .field("fw_addr", &self.fw_object().fw_addr())
            .field("layered", &self.srtc.is_some())
            .finish_non_exhaustive()
    }
}

/// A render target dataset.
///
/// Upstream `struct pvr_hwrt_dataset`.
pub struct HwrtDataset {
    /// The device whose heap the allocations came from.
    device: Arc<PvrDevice>,
    /// The free lists this dataset draws parameter pages from.
    ///
    /// Index [`uapi::HWRT_FREE_LIST_LOCAL`] first, then
    /// [`uapi::HWRT_FREE_LIST_GLOBAL`]. Strong references: the firmware reads
    /// both for as long as the dataset exists.
    pub free_lists: Vec<Arc<FreeList>>,
    /// The state both render-target data structures share.
    common_fw_object: FwObject,
    /// Whether this dataset installed its common object's MIPS mapping.
    common_mapped: bool,
    /// The two render-target data structures. See [`abi::NUM_RTDATAS`].
    data: Vec<Arc<HwrtData>>,
    /// Layers, upstream `max_rts`.
    max_rts: u32,
}

impl HwrtDataset {
    /// Creates a dataset.
    ///
    /// Ports upstream `pvr_hwrt_dataset_create()` and the three helpers it
    /// calls. The geometry and render-target argument structures are part of the
    /// ioctl's fixed payload rather than stride-negotiated arrays, so unlike a
    /// job's sync operations they arrive already decoded.
    ///
    /// # Errors
    ///
    /// - [`PvrError::NoSuchHandle`] for a free-list handle this file does not
    ///   hold, which upstream reports as `-EINVAL` from `pvr_free_list_lookup()`
    ///   failing.
    /// - [`PvrError::InvalidArgument`] when the local free list is smaller than
    ///   [`free_list::min_pages`], when the sample count is not one the
    ///   multisample tables cover, or for a zero width, height or layer count.
    /// - [`PvrError::Unsupported`] when the firmware does not report a feature
    ///   value the layout depends on; upstream `WARN_ON`s and propagates the
    ///   same failure.
    pub fn new(
        device: &Arc<PvrDevice>,
        file: &crate::driver::PvrFile,
        args: &uapi::CreateHwrtDatasetArgs,
    ) -> PvrResult<Arc<Self>> {
        device.check_alive()?;
        let fw = device.fw.get().ok_or(PvrError::Unsupported)?;

        // Divergence: upstream validates none of these three. `width` and
        // `height` reach `args->width - 1` in `screen_pixel_max`, so a zero
        // wraps to `0xffffffff` and the field mask turns it into the largest
        // representable screen rather than an empty one; `layers` reaches
        // `rta_ctl->max_rts` and a zero would describe a target with nothing to
        // render into. Neither is reachable from Mesa, which is presumably why
        // upstream never needed the check. Recorded in `ABI.md`
        // compatibility limits.
        if args.width == 0 || args.height == 0 || args.layers == 0 {
            return Err(PvrError::InvalidArgument);
        }

        // Upstream `hwrt_init_kernel_structure()`.
        let mut free_lists = Vec::with_capacity(abi::NUM_RTDATA_FREELISTS);
        for handle in args.free_list_handles {
            free_lists.push(file.free_lists.get(handle)?);
        }
        // The local list is the one the Parameter Manager allocates from, so a
        // render started against one that is too small runs out immediately and
        // the firmware has nowhere to put the geometry.
        if free_lists[uapi::HWRT_FREE_LIST_LOCAL].current_pages()
            < free_list::min_pages(&device.info)
        {
            return Err(PvrError::InvalidArgument);
        }

        let info = macrotile_info(device, args)?;
        let common = common_fw_structure(device, args, &info)?;

        let common_fw_object = fw.heap.lock().alloc(
            size_of::<RogueFwifHwrtdataCommon>(),
            FwCache::DEVICE_UNCACHED,
        )?;

        // Built before the `Arc` because every step below can fail, and this
        // type's `Drop` returns whatever has been allocated so far: an early
        // return drops a half-built dataset and leaks nothing.
        let mut dataset = Self {
            device: device.clone(),
            free_lists,
            common_fw_object,
            common_mapped: false,
            data: Vec::with_capacity(abi::NUM_RTDATAS),
            max_rts: args.layers,
        };
        dataset.common_fw_object.write_val(0, &common)?;
        fw.mips.page_table.map(&dataset.common_fw_object)?;
        dataset.common_mapped = true;

        for rt_args in &args.rt_data_args {
            let entry = dataset.build_data(args, rt_args)?;
            dataset.data.push(entry);
        }

        // Only the local free list gets the back-reference, matching upstream's
        // `pvr_free_list_add_hwrt(hwrt->free_lists[0], hwrt_data)`: the global
        // list is shared between datasets and is not reconstructed per target.
        let local = &dataset.free_lists[uapi::HWRT_FREE_LIST_LOCAL];
        for entry in &dataset.data {
            local.attach_hwrt(entry);
        }

        Ok(Arc::new(dataset))
    }

    /// One render-target data structure. Ports upstream
    /// `hwrt_data_init_fw_structure()`.
    fn build_data(
        &self,
        args: &uapi::CreateHwrtDatasetArgs,
        rt_args: &uapi::CreateHwrtRtDataArgs,
    ) -> PvrResult<Arc<HwrtData>> {
        let fw = self.device.fw.get().ok_or(PvrError::Unsupported)?;

        let mut data = RogueFwifHwrtdata::new_zeroed();
        data.hwrt_data_common_fw_addr = self.common_fw_object.fw_addr();
        for (index, free_list) in self.free_lists.iter().enumerate() {
            data.freelists_fw_addr[index] = free_list.fw_addr();
        }

        data.tail_ptrs_dev_addr = args.geom_data_args.tpc_dev_addr;
        data.vheap_table_dev_addr = args.geom_data_args.vheap_table_dev_addr;
        data.rtc_dev_addr = args.geom_data_args.rtc_dev_addr;

        data.pm_mlist_dev_addr = rt_args.pm_mlist_dev_addr;
        data.macrotile_array_dev_addr = rt_args.macrotile_array_dev_addr;
        data.rgn_header_dev_addr = rt_args.region_header_dev_addr;

        data.rta_ctl.render_target_index = 0;
        data.rta_ctl.active_render_targets = 0;
        data.rta_ctl.valid_render_targets_fw_addr = FwAddr(0);
        data.rta_ctl.rta_num_partial_renders_fw_addr = FwAddr(0);
        data.rta_ctl.max_rts = self.max_rts;

        // The owner exists before the first fallible allocation. None denotes
        // storage not allocated yet; mapping flags are set only after success.
        let mut entry = HwrtData {
            device: self.device.clone(),
            fw_object: None,
            mapped: [false; 3],
            srtc: None,
            raa: None,
        };
        // Keep the original SRTC/RAA allocation, zero, and mapping order.
        if self.max_rts > 1 {
            let mut heap = fw.heap.lock();
            entry.srtc = Some(heap.alloc(
                (u64::from(self.max_rts) * SRTC_ENTRY_SIZE) as usize,
                FwCache::DEVICE_UNCACHED,
            )?);
            entry.raa = Some(heap.alloc(
                (u64::from(self.max_rts) * RAA_ENTRY_SIZE) as usize,
                FwCache::DEVICE_UNCACHED,
            )?);
            drop(heap);
            let srtc = entry.srtc.as_ref().expect("SRTC just allocated");
            let raa = entry.raa.as_ref().expect("RAA just allocated");
            srtc.zero()?;
            raa.zero()?;
            fw.mips.page_table.map(srtc)?;
            entry.mapped[1] = true;
            fw.mips.page_table.map(raa)?;
            entry.mapped[2] = true;
            data.rta_ctl.valid_render_targets_fw_addr = srtc.fw_addr();
            data.rta_ctl.rta_num_partial_renders_fw_addr = raa.fw_addr();
        }

        entry.fw_object = Some(
            fw.heap
                .lock()
                .alloc(size_of::<RogueFwifHwrtdata>(), FwCache::DEVICE_UNCACHED)?,
        );
        entry.fw_object().write_val(0, &data)?;
        fw.mips.page_table.map(entry.fw_object())?;
        entry.mapped[0] = true;
        Ok(Arc::new(entry))
    }

    /// One of the dataset's render-target data structures.
    ///
    /// Ports upstream `pvr_hwrt_data_lookup()`'s bounds check. An index the
    /// dataset does not have is the client naming a buffer that was never
    /// created, which upstream reports as `-EINVAL`.
    pub fn data(&self, index: u32) -> Option<&Arc<HwrtData>> {
        self.data.get(index as usize)
    }

    /// Layers this target renders into.
    pub fn max_rts(&self) -> u32 {
        self.max_rts
    }

    /// Asks the firmware to stop referring to the dataset's render targets.
    ///
    /// Ports the `pvr_fw_structure_cleanup()` loop of upstream
    /// `pvr_hwrt_dataset_release()`, in the same reverse order. Waits for the
    /// firmware, so it runs from `DESTROY_HWRT_DATASET` rather than from
    /// [`Drop`].
    ///
    /// Best effort: every failure is reported and none of them stops the next,
    /// because the alternative to finishing is leaking firmware address space.
    pub fn destroy(&self) -> PvrResult<()> {
        if self.device.check_alive().is_err() {
            return Ok(());
        }

        let mut result = Ok(());
        for entry in self.data.iter().rev() {
            if let Err(err) = self.cleanup(entry) {
                ostd::error!("firmware refused to release a render target: {err:?}");
                result = Err(err);
            }
        }
        result
    }

    /// One `ROGUE_FWIF_CLEANUP_HWRTDATA` round trip.
    fn cleanup(&self, entry: &HwrtData) -> PvrResult<()> {
        let mut cmd = RogueFwifKccbCmd::new_zeroed();
        cmd.cmd_type = crate::sched::abi::KCCB_CMD_CLEANUP;
        {
            let cleanup = cmd.cmd_data.cleanup_data_mut();
            cleanup.cleanup_type = abi::CLEANUP_HWRTDATA;
            *cleanup.cleanup_data.hwrt_data_fw_addr_mut() = entry.fw_addr();
        }

        let rtn = crate::sched::ccb::send_and_wait(&self.device, &cmd)?;
        if rtn & crate::sched::abi::KCCB_RTN_SLOT_CLEANUP_BUSY != 0 {
            return Err(PvrError::Busy);
        }
        Ok(())
    }
}

impl Drop for HwrtDataset {
    /// Detaches from the local free list, then releases the shared allocation.
    ///
    /// The per-target allocations go with their [`HwrtData`], which this drops
    /// on the way out. The detach comes first because it needs those `Arc`s
    /// alive to identify them.
    fn drop(&mut self) {
        if let Some(local) = self.free_lists.get(uapi::HWRT_FREE_LIST_LOCAL) {
            for entry in &self.data {
                local.detach_hwrt(entry);
            }
        }
        if let Some(fw) = self.device.fw.get() {
            let mut heap = fw.heap.lock();
            if self.common_mapped
                && let Err(err) = fw.mips.page_table.unmap(&self.common_fw_object)
            {
                ostd::error!("could not unmap common render target storage: {err:?}");
            }
            heap.free(&self.common_fw_object);
        }
    }
}

impl core::fmt::Debug for HwrtDataset {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HwrtDataset")
            .field("max_rts", &self.max_rts)
            .field("data", &self.data.len())
            .finish_non_exhaustive()
    }
}

/// Divides `value` by `divisor`, rounding up. Upstream `DIV_ROUND_UP`.
///
/// Every caller has already established that `divisor` is non-zero — the two
/// constant ones by inspection, the tile sizes by the check in
/// [`macrotile_info`].
fn div_round_up(value: u32, divisor: u32) -> u32 {
    value.div_ceil(divisor)
}

/// Rounds `value` up to a multiple of `to`. Upstream `round_up`.
fn round_up(value: u32, to: u32) -> u32 {
    value.div_ceil(to) * to
}

/// Works out the macrotile grid for a render target.
///
/// Ports the first half of upstream `hwrt_init_common_fw_structure()`. Sixteen
/// macrotiles either way; what the parameter format changes is whether they are
/// a multiple of 2x2 tiles or of 4x4, and — because the simple format tiles in
/// groups of two — whether the last tile index is rounded up.
fn macrotile_info(
    device: &PvrDevice,
    args: &uapi::CreateHwrtDatasetArgs,
) -> PvrResult<MacrotileInfo> {
    let values = &device.info.values;
    // Upstream reads both with `PVR_FEATURE_VALUE()` and `WARN_ON`s a missing
    // one. A zero is the same failure here: [`crate::device_info`] surfaces the
    // tile dimensions as bare values with no flag, and a zero-pixel tile is not
    // a size the rest of this function could do anything with.
    if values.tile_size_x == 0 || values.tile_size_y == 0 {
        ostd::error!("firmware does not report the tile dimensions");
        return Err(PvrError::Unsupported);
    }
    let num_tiles_x = div_round_up(args.width, values.tile_size_x);
    let num_tiles_y = div_round_up(args.height, values.tile_size_y);

    if device.info.features.simple_parameter_format_version {
        // Upstream `WARN_ON(parameter_format != 2)` and carries on: the layout
        // below is version 2's, so a different version would be described
        // wrongly. Reported rather than refused, exactly as upstream does.
        if values.simple_parameter_format_version != 2 {
            ostd::warn!(
                "firmware reports simple parameter format version {}, not 2; \
                 the macrotile layout below assumes 2",
                values.simple_parameter_format_version
            );
        }

        Ok(MacrotileInfo {
            mtile_x: [div_round_up(num_tiles_x, 8) * 2, 0, 0],
            mtile_y: [div_round_up(num_tiles_y, 8) * 2, 0, 0],
            tile_max_x: round_up(num_tiles_x, 2) - 1,
            tile_max_y: round_up(num_tiles_y, 2) - 1,
        })
    } else {
        let x = round_up(div_round_up(num_tiles_x, 4), 4);
        let y = round_up(div_round_up(num_tiles_y, 4), 4);
        Ok(MacrotileInfo {
            mtile_x: [x, x * 2, x * 3],
            mtile_y: [y, y * 2, y * 3],
            tile_max_x: num_tiles_x - 1,
            tile_max_y: num_tiles_y - 1,
        })
    }
}

/// Builds the state both of a dataset's render targets share.
///
/// Ports the second half of upstream `hwrt_init_common_fw_structure()`.
fn common_fw_structure(
    device: &PvrDevice,
    args: &uapi::CreateHwrtDatasetArgs,
    info: &MacrotileInfo,
) -> PvrResult<RogueFwifHwrtdataCommon> {
    let mut common = RogueFwifHwrtdataCommon::new_zeroed();

    common.geom_caches_need_zeroing = 0;

    common.isp_merge_lower_x = args.isp_merge_lower_x;
    common.isp_merge_lower_y = args.isp_merge_lower_y;
    common.isp_merge_upper_x = args.isp_merge_upper_x;
    common.isp_merge_upper_y = args.isp_merge_upper_y;
    common.isp_merge_scale_x = args.isp_merge_scale_x;
    common.isp_merge_scale_y = args.isp_merge_scale_y;

    common.multi_sample_ctl = multisample_ctl(args.samples, false)?;
    common.flipped_multi_sample_ctl = multisample_ctl(args.samples, true)?;

    common.mtile_stride = info.mtile_x[0].wrapping_mul(info.mtile_y[0]);
    common.teaa = te_aa(device, args.samples)?;

    common.screen_pixel_max = (cr::ppp_screen::PIXXMAX.set(0, u64::from(args.width - 1))
        | cr::ppp_screen::PIXYMAX.set(0, u64::from(args.height - 1)))
        as u32;
    common.te_screen = (cr::te_screen::XMAX.set(0, u64::from(info.tile_max_x))
        | cr::te_screen::YMAX.set(0, u64::from(info.tile_max_y))) as u32;
    common.te_mtile1 = (cr::te_mtile1::X1.set(0, u64::from(info.mtile_x[0]))
        | cr::te_mtile1::X2.set(0, u64::from(info.mtile_x[1]))
        | cr::te_mtile1::X3.set(0, u64::from(info.mtile_x[2]))) as u32;
    common.te_mtile2 = (cr::te_mtile2::Y1.set(0, u64::from(info.mtile_y[0]))
        | cr::te_mtile2::Y2.set(0, u64::from(info.mtile_y[1]))
        | cr::te_mtile2::Y3.set(0, u64::from(info.mtile_y[2]))) as u32;

    common.isp_mtile_size = isp_mtile_size(device, args.samples, info)?;

    common.tpc_stride = args.geom_data_args.tpc_stride;
    common.tpc_size = args.geom_data_args.tpc_size;
    common.rgn_header_size = args.region_header_size;

    Ok(common)
}

/// Sample positions within a pixel, by `log2(samples)`.
///
/// Upstream's `sample_positions[4]` table in `get_cr_multisamplectl_val()`,
/// transcribed including the trailing zeroes: the C arrays are `u8 x[8]` with
/// only the first `samples` entries initialised, and the loop reads all eight,
/// so the zeroes are part of the value the register gets.
const SAMPLE_POSITIONS: [([u8; 8], [u8; 8]); 4] = [
    // 1 sample.
    ([8, 0, 0, 0, 0, 0, 0, 0], [8, 0, 0, 0, 0, 0, 0, 0]),
    // 2 samples.
    ([12, 4, 0, 0, 0, 0, 0, 0], [12, 4, 0, 0, 0, 0, 0, 0]),
    // 4 samples.
    ([6, 14, 2, 10, 0, 0, 0, 0], [2, 6, 10, 14, 0, 0, 0, 0]),
    // 8 samples.
    ([9, 7, 13, 5, 3, 1, 11, 15], [5, 11, 9, 3, 13, 7, 15, 1]),
];

/// `ROGUE_CR_MULTISAMPLECTL` for a sample count.
///
/// Ports upstream `get_cr_multisamplectl_val()`. `y_flip` produces the second
/// copy the firmware keeps for targets rendered bottom-up.
///
/// The table is indexed by `fls(samples) - 1`, so a count that is not a power of
/// two selects the table for the next power of two *down*. Reproduced rather
/// than tightened: it is what upstream does, and rejecting three samples here
/// while upstream accepts it would be a difference user space could see.
fn multisample_ctl(samples: u32, y_flip: bool) -> PvrResult<u64> {
    if samples == 0 {
        // Upstream's `idx < 0`.
        return Err(PvrError::InvalidArgument);
    }
    let index = (u32::BITS - 1 - samples.leading_zeros()) as usize;
    let (x, y) = *SAMPLE_POSITIONS
        .get(index)
        .ok_or(PvrError::InvalidArgument)?;

    let mut value = 0u64;
    for i in 0..8 {
        value |= u64::from(x[i]) << (i * 8);
        let y = if y_flip {
            u64::from(16 - u32::from(y[i])) & 0xf
        } else {
            u64::from(y[i])
        };
        value |= y << (i * 8 + 4);
    }
    Ok(value)
}

/// `ROGUE_CR_TE_AA` for a sample count.
///
/// Ports upstream `get_cr_te_aa_val()`. Which bits a sample count sets depends
/// on how many samples the ISP already processes per pixel, because the
/// tiling engine only has to supersample what the ISP does not.
fn te_aa(device: &PvrDevice, samples: u32) -> PvrResult<u32> {
    let per_pixel = isp_samples_per_pixel(device)?;

    let mut value = 0u64;
    match per_pixel {
        1 => {
            if samples >= 2 {
                value |= cr::te_aa::Y_EN;
            }
            if samples >= 4 {
                value |= cr::te_aa::X_EN;
            }
        }
        2 => {
            if samples >= 2 {
                value |= cr::te_aa::X2_EN;
            }
            if samples >= 4 {
                value |= cr::te_aa::Y_EN;
            }
            if samples >= 8 {
                value |= cr::te_aa::X_EN;
            }
        }
        4 => {
            if samples >= 2 {
                value |= cr::te_aa::X2_EN;
            }
            if samples >= 4 {
                value |= cr::te_aa::Y2_EN;
            }
            if samples >= 8 {
                value |= cr::te_aa::Y_EN;
            }
        }
        other => {
            // Upstream `WARN(true, "Unsupported ISP samples per pixel value")`
            // and `-EINVAL`.
            ostd::error!("firmware reports {other} ISP samples per pixel, which is not 1, 2 or 4");
            return Err(PvrError::InvalidArgument);
        }
    }
    Ok(value as u32)
}

/// `ROGUE_CR_ISP_MTILE_SIZE` for a sample count.
///
/// Ports upstream `get_cr_isp_mtile_size_val()`: a macrotile has to grow to hold
/// the extra samples the ISP does not absorb, and which axis it grows along
/// depends on both counts.
fn isp_mtile_size(device: &PvrDevice, samples: u32, info: &MacrotileInfo) -> PvrResult<u32> {
    let mut x = info.mtile_x[0];
    let mut y = info.mtile_y[0];

    match isp_samples_per_pixel(device)? {
        1 => {
            if samples >= 4 {
                x <<= 1;
            }
            if samples >= 2 {
                y <<= 1;
            }
        }
        2 => {
            if samples >= 8 {
                x <<= 1;
            }
            if samples >= 4 {
                y <<= 1;
            }
        }
        4 => {
            if samples >= 8 {
                y <<= 1;
            }
        }
        other => {
            ostd::error!("firmware reports {other} ISP samples per pixel, which is not 1, 2 or 4");
            return Err(PvrError::InvalidArgument);
        }
    }

    Ok(
        (cr::isp_mtile_size::X.set(0, u64::from(x)) | cr::isp_mtile_size::Y.set(0, u64::from(y)))
            as u32,
    )
}

/// The ISP's samples-per-pixel count, or an error if the firmware did not say.
///
/// Upstream's `PVR_FEATURE_VALUE()` returns `-EINVAL` for an absent feature and
/// both callers propagate it. [`PvrError::Unsupported`] rather than
/// `InvalidArgument` here because the caller did nothing wrong: this is the
/// firmware image failing to describe a part this path needs described.
fn isp_samples_per_pixel(device: &PvrDevice) -> PvrResult<u32> {
    if !device.info.features.isp_samples_per_pixel {
        ostd::error!("firmware does not report the ISP samples-per-pixel count");
        return Err(PvrError::Unsupported);
    }
    Ok(device.info.values.isp_samples_per_pixel)
}
