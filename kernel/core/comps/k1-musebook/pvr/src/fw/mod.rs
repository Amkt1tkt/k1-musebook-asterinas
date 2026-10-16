// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR firmware allocation, boot, and teardown.

//! Firmware loading and the firmware address space.
//!
//! Ports `pvr_fw.c`, `pvr_fw.h`, `pvr_fw_util.c` (2,080 lines).
//!
//! The GPU does not execute commands directly; a firmware processor on the GPU
//! does, and the driver's real job is to set up that processor's memory, hand it
//! a set of shared structures, and start it. [`crate::rogue::fwif`]'s
//! compile-time assertions verify the shared layouts, not hardware behavior.
//!
//! # Address spaces
//!
//! Confusing these is the classic bug here, so they are distinct types:
//!
//! | space | seen by | type |
//! |---|---|---|
//! | CPU physical | the host | [`ostd::mm::Paddr`] |
//! | bus / DMA | the GPU's bus masters and the MIPS TLB | [`ostd::mm::Daddr`] |
//! | GPU virtual | rendering units, via the Rogue MMU | [`crate::mm::vm::DeviceAddr`] |
//! | firmware | the MIPS core, via its own TLB | [`crate::rogue::fwif::FwAddr`] |
//!
//! # Bring-up order
//!
//! [`FwDevice::load`] does everything that can be done without the GPU running:
//!
//! 1. [`validate_and_init_device_info`] — the image is for this silicon and this
//!    driver;
//! 2. [`heap::FwHeap`] — the firmware's address space;
//! 3. [`mm::vm_mips::MipsPageTable`] — the processor's page table, empty;
//! 4. allocations for the code and data sections at the addresses the layout
//!    table dictates, plus one per shared structure;
//! 5. [`mips::process`] — load the ELF segments and build the boot arguments;
//! 6. copy the sections into their allocations and sync;
//! 7. fill in the shared structures;
//! 8. map every allocation into the page table.
//!
//! Then [`boot`] runs [`startstop::start`] and waits. If the wait fails,
//! [`BootDiagnosis::collect`] says what happened.
//!
//! [`mm::vm_mips::MipsPageTable`]: crate::mm::vm_mips::MipsPageTable

pub mod abi;
pub mod elf;
pub mod heap;
pub mod info;
pub mod mips;
pub mod startstop;
pub mod trace;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{
    mem::offset_of,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use ostd::{
    arch::{read_tsc, tsc_freq},
    mm::Daddr,
    sync::SpinLock,
};

use crate::{
    device::{FwVersion, GpuId, PvrDevice},
    device_info::DeviceInfo,
    error::{PvrError, PvrResult},
    mm::vm_mips::MipsPageTable,
    rogue::{
        fwif::{
            RogueFwifConnectionCtl, RogueFwifGpuUtilFwcb, RogueFwifOsdata, RogueFwifOsinit,
            RogueFwifRuntimeCfg, RogueFwifSysdata, RogueFwifSysinit, RogueMipsState,
        },
        heap::{ROGUE_PDSCODEDATA_HEAP_BASE, ROGUE_USCCODE_HEAP_BASE},
        mips as rmips,
    },
};

/// Platform facts [`FwDevice::load`] cannot discover for itself.
///
/// Upstream reads all three out of `struct pvr_device`, which by then has been
/// through `platform_probe`. Here the platform layer owns the platform and passes them
/// down, so that nothing under it has to know about device trees.
#[derive(Clone, Copy, Debug)]
pub struct FwInitParams {
    /// Current GPU clock in Hz, used by firmware timer and power-management setup.
    ///
    /// The platform must supply the rate selected by its clock registers.
    pub core_clock_hz: u32,
    /// Bus address of the GPU's register window.
    ///
    /// The firmware needs it to reach its own register bank; upstream uses
    /// `pvr_dev->regs_resource->start`.
    pub regs_daddr: Daddr,
    /// Which firmware trace groups to enable. See [`trace::validate_group_mask`].
    ///
    /// Upstream's equivalent is the `init_fw_trace_mask` module parameter, which
    /// defaults to zero. On a GPU nobody has run this driver on, defaulting to
    /// silence is the wrong trade — see [`DEFAULT_TRACE_GROUP_MASK`].
    pub trace_group_mask: u32,
}

/// Default firmware trace groups.
///
/// MAIN records initialization, POW power transitions, MTS scheduling, CLEANUP
/// resource release, and HWR firmware recovery. Other groups remain disabled by
/// default to limit ring traffic. Unlike upstream's zero mask, these groups retain
/// context for failure diagnosis; host-side dumping remains separately gated.
pub const DEFAULT_TRACE_GROUP_MASK: u32 = abi::LOG_TYPE_GROUP_MAIN
    | abi::LOG_TYPE_GROUP_POW
    | abi::LOG_TYPE_GROUP_MTS
    | abi::LOG_TYPE_GROUP_CLEANUP
    | abi::LOG_TYPE_GROUP_HWR;

impl Default for FwInitParams {
    fn default() -> Self {
        Self {
            core_clock_hz: 0,
            regs_daddr: 0,
            trace_group_mask: DEFAULT_TRACE_GROUP_MASK,
        }
    }
}

/// The firmware-heap allocations the driver makes.
///
/// Ports upstream `struct pvr_fw_mem`. Every one of these is firmware-visible, so
/// every one is a [`heap::FwObject`] and therefore a [`ostd::mm::DmaStream`].
/// Most use write-back plus explicit syncs; the ones the firmware writes and
/// the CPU polls are [`heap::FwCache::POLLED`]. See `CONVENTIONS.md` DMA contract.
pub struct FwMem {
    /// The image's `FW_CODE` sections, at heap offset 0.
    pub code: heap::FwObject,
    /// The image's `FW_DATA` sections, at the offset `MIPS_PRIVATE_DATA` names.
    pub data: heap::FwObject,
    /// `rogue_fwif_connection_ctl`, at a fixed offset in the configuration area.
    pub connection_ctl: heap::FwObject,
    /// The host/firmware initialization handshake.
    ///
    /// Both halves must be allocated before boot. Submission initialization publishes
    /// the CCB addresses before the firmware reads `osinit`; zero addresses are not
    /// valid substitutes for those objects.
    pub osinit: heap::FwObject,
    /// `rogue_fwif_sysinit`, at a fixed offset in the configuration area.
    pub sysinit: heap::FwObject,
    /// `rogue_fwif_osdata`.
    pub osdata: heap::FwObject,
    /// `rogue_fwif_sysdata`.
    pub sysdata: heap::FwObject,
    /// `rogue_fwif_hwrinfobuf`.
    pub hwrinfobuf: heap::FwObject,
    /// The power-transition sync object.
    pub power_sync: heap::FwObject,
    /// The page every refused GPU access lands on.
    pub fault_page: heap::FwObject,
    /// `rogue_fwif_gpu_util_fwcb`.
    pub gpu_util_fwcb: heap::FwObject,
    /// `rogue_fwif_runtime_cfg`.
    pub runtime_cfg: heap::FwObject,
    /// The sync object an MMU cache operation writes to.
    pub mmucache_sync: heap::FwObject,
    /// Driver-side copy of the code allocation, for [`FwDevice::hard_reset`].
    pub code_copy: Vec<u8>,
    /// Driver-side copy of the data allocation, for [`FwDevice::hard_reset`].
    pub data_copy: Vec<u8>,
}

impl FwMem {
    /// Every allocation, for the mapping and teardown loops.
    pub(crate) fn objects(&self) -> [&heap::FwObject; 13] {
        [
            &self.code,
            &self.data,
            &self.connection_ctl,
            &self.osinit,
            &self.sysinit,
            &self.osdata,
            &self.sysdata,
            &self.hwrinfobuf,
            &self.power_sync,
            &self.fault_page,
            &self.gpu_util_fwcb,
            &self.runtime_cfg,
            &self.mmucache_sync,
        ]
    }
}

/// Firmware state owned by a [`PvrDevice`].
///
/// Ports upstream `struct pvr_fw_device`.
pub struct FwDevice {
    /// The validated image, still in its on-disk form.
    ///
    /// Kept because [`Self::hard_reset`] reloads the sections from it, and
    /// because the device-info block is the only record of what the hardware
    /// claims to be.
    pub image: Vec<u8>,
    /// The parsed trailing header and layout table.
    pub info: info::ParsedInfo,
    /// The firmware address space allocator, for submission and rendering.
    pub heap: SpinLock<heap::FwHeap>,
    /// The firmware-heap allocations.
    pub mem: FwMem,
    /// The firmware processor's page tables. MIPS only, for this GPU.
    pub mips: mips::MipsData,
    /// The firmware's trace buffer, for post-mortem diagnosis.
    pub trace: trace::FwTrace,
    /// The platform facts [`FwDevice::load`] was given.
    ///
    /// Kept because [`Self::hard_reset`] re-runs the initialisers that consume
    /// them, and a reset that invented different values — a different core clock
    /// or a different register-window address — would hand the firmware a
    /// different world from the one it booted into the first time. Upstream
    /// re-reads them from `struct pvr_device` for the same reason.
    pub params: FwInitParams,
    /// Whether the firmware has reported itself started.
    pub booted: AtomicBool,
    /// How many times the MIPS core has been started, counting from one.
    ///
    /// # Why a counter and not just [`Self::booted`]
    ///
    /// The GPU is powered down whenever the reference count reaches zero and the
    /// autosuspend delay has passed, and coming back up restarts the firmware.
    /// So a queue can outlive the firmware that was running when its context was
    /// created, and a job can be written into a client CCB under one firmware
    /// and be waited on under another. Nothing recorded which, and that is
    /// exactly the distinction a stalled job needs: a job whose generation is
    /// behind the current one was interrupted by a power cycle, while one whose
    /// generation matches was seen by the firmware now running and not executed.
    /// Those two have entirely different causes and the console could not tell
    /// them apart.
    ///
    /// Not upstream, which has no equivalent because it does not need one: its
    /// runtime PM keeps a reference for every unfinished job
    /// (`pvr_job_get_pm_ref`), so the first case is supposed to be impossible.
    /// This counter is how that claim gets checked rather than assumed.
    pub generation: AtomicU32,
}

/// What [`validate_and_init_device_info`] extracts from an image.
pub struct ValidatedFirmware {
    /// BVNC the image was built for. Must match the hardware.
    pub gpu_id: GpuId,
    /// Version from the info header.
    pub version: FwVersion,
    /// Features, quirks and enhancements from the device-info block.
    pub info: DeviceInfo,
    /// Section layout.
    pub layout: Vec<info::LayoutEntry>,
}

/// The firmware file name for a GPU.
///
/// Upstream's naming scheme, which is also how the open firmware repository is
/// organised. For the K1 this resolves to
/// `powervr/rogue_36.29.52.182_v1.fw`, whose header has been verified offline —
/// see the crate documentation.
pub fn firmware_name(gpu_id: &GpuId) -> String {
    alloc::format!(
        "powervr/rogue_{}.{}.{}.{}_v1.fw",
        gpu_id.b,
        gpu_id.v,
        gpu_id.n,
        gpu_id.c
    )
}

/// Validates an image and extracts the device description from it.
///
/// This must run before anything queries [`DeviceInfo`], because the device
/// description *comes from here* rather than from a table in the driver. The
/// checks, in upstream's order:
///
/// 1. `info_version == 3`, else the format is one we do not understand.
/// 2. `header_len`, `layout_entry_size` and `layout_entry_num` agree with our
///    structure definitions and the 8-entry maximum.
/// 3. `PVR_FW_FLAGS_OPEN_SOURCE` is set. Closed DDK firmware is rejected here,
///    deliberately: it does not match this driver's protocol.
/// 4. `fw_version_major` is non-zero and no greater than 1.
/// 5. The BVNC matches what the hardware reported.
///
/// Steps 1 to 4 are [`info::parse`]'s, which also bounds the layout table and
/// every section within it. Step 5 belongs to [`PvrDevice::probe`], which is the
/// only place that has read `RGX_CR_CORE_ID`; this function returns the BVNC the
/// image claims and leaves the comparison there.
///
/// There is a sixth check upstream does not make, because upstream supports all
/// three firmware processors and this port supports one:
///
/// 6. Every section in the layout table belongs to the same firmware processor,
///    and that processor is MIPS.
///
/// That is what makes `pvr/UPSTREAM`'s exclusion of `pvr_fw_meta.c` and
/// `pvr_fw_riscv.c` an enforced property rather than a note. An image for a META
/// or RISC-V core is [`PvrError::Unsupported`] with a message saying so, so that
/// the next person knows the exclusion was wrong rather than that the driver is
/// broken.
pub fn validate_and_init_device_info(image: &[u8]) -> PvrResult<ValidatedFirmware> {
    let parsed = info::parse(image)?;
    let (b, v, n, c) = parsed.bvnc_parts();
    let gpu_id = GpuId { b, v, n, c };

    let coprocessor = parsed.coprocessor()?;
    if coprocessor != info::Coprocessor::Mips {
        ostd::error!(
            "firmware for BVNC {} is built for a {} core; only MIPS is ported. \
             If this is correct for the hardware, `pvr/UPSTREAM`'s exclusion of the \
             {} path must be revisited rather than worked around.",
            gpu_id,
            coprocessor,
            coprocessor
        );
        return Err(PvrError::Unsupported);
    }

    let block = parsed.device_info_block(image)?;
    let decoded = DeviceInfo::from_device_info_block(block).map_err(|err| {
        ostd::error!("firmware device-info block is malformed: {err:?}");
        PvrError::BadFirmware
    })?;

    // Bits we do not recognise are reported, never dropped: on a BVNC upstream
    // has never tested, an unimplemented quirk is exactly the thing that would
    // otherwise be debugged from scratch on hardware.
    if !decoded.unknown_features.is_empty() {
        ostd::warn!(
            "firmware reports {} feature bits this driver does not know: {:?}",
            decoded.unknown_features.len(),
            decoded.unknown_features
        );
    }
    if !decoded.unknown_quirks.is_empty() {
        ostd::warn!(
            "firmware reports {} quirk bits this driver does not know: {:?}",
            decoded.unknown_quirks.len(),
            decoded.unknown_quirks
        );
    }
    if !decoded.unknown_enhancements.is_empty() {
        ostd::warn!(
            "firmware reports {} enhancement bits this driver does not know: {:?}",
            decoded.unknown_enhancements.len(),
            decoded.unknown_enhancements
        );
    }

    ostd::info!(
        "firmware v{}.{} (build {}) for BVNC {}, {} MIPS sections",
        parsed.header.fw_version_major,
        parsed.header.fw_version_minor,
        parsed.header.fw_version_build,
        gpu_id,
        parsed.layout.len()
    );

    Ok(ValidatedFirmware {
        gpu_id,
        version: FwVersion {
            major: parsed.header.fw_version_major,
            minor: parsed.header.fw_version_minor,
        },
        info: decoded.info,
        layout: parsed.layout,
    })
}

/// Installs the firmware state used by runtime power management.
///
/// The state is published once. A second installation returns `Busy`.
pub fn init(device: &Arc<PvrDevice>, image: Vec<u8>, params: FwInitParams) -> PvrResult<()> {
    if device.fw.is_completed() {
        return Err(PvrError::Busy);
    }
    device
        .fw
        .try_call_once(|| FwDevice::load(device, image, params))?;
    Ok(())
}

impl FwDevice {
    /// Loads an image into GPU-visible memory and builds the firmware's world.
    ///
    /// Ports upstream `pvr_fw_init()` up to but excluding `pvr_fw_start()`: the
    /// processor-specific init, the address-space allocator, `pvr_fw_process()`
    /// and `pvr_fw_create_structures()`.
    ///
    /// Does not touch a register. Everything here is memory setup, which is why
    /// it can be exercised without the silicon.
    pub fn load(device: &Arc<PvrDevice>, image: Vec<u8>, params: FwInitParams) -> PvrResult<Self> {
        let parsed = info::parse(&image)?;
        if parsed.coprocessor()? != info::Coprocessor::Mips {
            return Err(PvrError::Unsupported);
        }

        // Upstream `pvr_mips_init()`: size the heap, then build the page table.
        let heap_info = heap::FwHeapInfo::new(
            mips::FW_HEAP_MIPS_LOG2_SIZE,
            mips::FW_HEAP_MIPS_RESERVED_SIZE,
        )?;
        if !device.info.features.phys_bus_width {
            ostd::error!("firmware image does not report a physical bus width");
            return Err(PvrError::Unsupported);
        }
        // Weak: this table ends up inside `device.fw`, so an `Arc` would cycle.
        let page_table = MipsPageTable::new(
            Arc::downgrade(device),
            &heap_info,
            device.info.values.phys_bus_width,
        )?;
        let mut fw_heap = heap::FwHeap::new(heap_info, mips::FW_HEAP_MIPS_BASE);

        let sizes = parsed.section_sizes();
        let private_data = parsed.find_private_data().ok_or_else(|| {
            ostd::error!("firmware image has no private-data section");
            PvrError::BadFirmware
        })?;

        // Code must be at heap offset 0 or the firmware processor cannot boot.
        // Data must be where MIPS_PRIVATE_DATA says, because
        // `mips::HAS_FIXED_DATA_ADDR`.
        let code = fw_heap.alloc_at(0, sizes.code as usize, heap::FwCache::DEVICE_UNCACHED)?;
        let data_offset = private_data.base_addr & heap_info.offset_mask;
        let data = fw_heap.alloc_at(
            data_offset,
            sizes.data as usize,
            heap::FwCache::DEVICE_UNCACHED,
        )?;

        let mem = Self::allocate_structures(&mut fw_heap, code, data)?;
        let fw_trace = trace::FwTrace::new(&mut fw_heap, params.trace_group_mask)?;

        // Upstream `pvr_mips_fw_process()`, then the memcpy in
        // `pvr_fw_process()`.
        let processed = mips::process(
            &parsed,
            &image,
            &mem.code,
            &mem.data,
            page_table,
            params.regs_daddr,
        )?;
        mem.code.write_bytes(0, &processed.code)?;
        mem.data.write_bytes(0, &processed.data)?;

        let mut mem = mem;
        mem.code_copy = processed.code;
        mem.data_copy = processed.data;

        let fw = Self {
            image,
            info: parsed,
            heap: SpinLock::new(fw_heap),
            mem,
            mips: processed.mips,
            trace: fw_trace,
            params,
            booted: AtomicBool::new(false),
            generation: AtomicU32::new(0),
        };

        fw.init_structures(device, &params)?;
        fw.map_all()?;

        ostd::info!(
            "firmware loaded: {} B code at heap offset 0, {} B data at {:#x}, \
             page table {} entries",
            sizes.code,
            sizes.data,
            data_offset,
            fw.mips.page_table.entry_count()
        );
        Ok(fw)
    }

    /// Allocates every shared structure.
    ///
    /// Ports upstream `pvr_fw_create_structures()` and the three fixed-offset
    /// allocations in `pvr_fw_create_fwif_connection_ctl()` and
    /// `pvr_fw_create_structures()`.
    ///
    /// All are uncached from the firmware's point of view — upstream passes
    /// `PVR_BO_FW_FLAGS_DEVICE_UNCACHED` to every one of them. The CPU mapping
    /// is a separate bit: words the firmware writes and the host polls take
    /// [`heap::FwCache::POLLED`]; the rest stay write-back. See
    /// `CONVENTIONS.md` DMA contract.
    fn allocate_structures(
        fw_heap: &mut heap::FwHeap,
        code: heap::FwObject,
        data: heap::FwObject,
    ) -> PvrResult<FwMem> {
        let config = fw_heap.info().config_offset;
        let mem = FwMem {
            code,
            data,
            connection_ctl: fw_heap.alloc_at(
                config + heap::CONNECTION_CTL_OFFSET,
                size_of::<RogueFwifConnectionCtl>(),
                heap::FwCache::POLLED,
            )?,
            osinit: fw_heap.alloc_at(
                config + heap::OSINIT_OFFSET,
                size_of::<RogueFwifOsinit>(),
                heap::FwCache::DEVICE_UNCACHED,
            )?,
            sysinit: fw_heap.alloc_at(
                config + heap::SYSINIT_OFFSET,
                size_of::<RogueFwifSysinit>(),
                heap::FwCache::POLLED,
            )?,
            osdata: fw_heap.alloc(size_of::<RogueFwifOsdata>(), heap::FwCache::POLLED)?,
            sysdata: fw_heap.alloc(size_of::<RogueFwifSysdata>(), heap::FwCache::POLLED)?,
            hwrinfobuf: fw_heap.alloc(
                size_of::<crate::rogue::fwif::RogueFwifHwrinfobuf>(),
                heap::FwCache::DEVICE_UNCACHED,
            )?,
            power_sync: fw_heap.alloc(abi::SYNC_OBJ_SIZE, heap::FwCache::POLLED)?,
            fault_page: fw_heap.alloc(abi::FAULT_PAGE_SIZE, heap::FwCache::DEVICE_UNCACHED)?,
            gpu_util_fwcb: fw_heap.alloc(
                size_of::<RogueFwifGpuUtilFwcb>(),
                heap::FwCache::DEVICE_UNCACHED,
            )?,
            runtime_cfg: fw_heap.alloc(
                size_of::<RogueFwifRuntimeCfg>(),
                heap::FwCache::DEVICE_UNCACHED,
            )?,
            mmucache_sync: fw_heap.alloc(abi::SYNC_OBJ_SIZE, heap::FwCache::POLLED)?,
            code_copy: Vec::new(),
            data_copy: Vec::new(),
        };

        // Say once whether the words the firmware writes and this driver polls
        // really are uncacheable to the CPU. `FwCache::POLLED` only *asks*: on
        // RISC-V without Svpbmt the page-table entry cannot encode anything but
        // write-back, and the request is dropped with no error. Retirement
        // depends on seeing those writes, and when it does not the symptom is a
        // completed job declared hung several hundred milliseconds later, which
        // is a very long way from the cause. Keep the fallback at warning level.
        if mem.sysinit.is_cpu_uncached() {
            ostd::debug!("[k1-gpu] polled firmware words are CPU-uncacheable (Svpbmt PBMT_NC)");
        } else {
            ostd::warn!(
                "[k1-gpu] polled firmware words asked for an uncacheable CPU mapping and did \
                 not get one -- they are write-back, and this driver is relying on cache \
                 maintenance to see firmware writes. Expect completed jobs to be declared \
                 hung. Check for the `[riscv] Svpbmt is absent` line above"
            );
        }

        Ok(mem)
    }

    /// Fills in the shared structures.
    ///
    /// Ports upstream's five `*_init()` callbacks. Fields upstream sets to zero
    /// are left to the allocation, which is zeroed — except where the zero means
    /// something, in which case it is written so that the code says so.
    ///
    /// `rogue_fwif_osinit` is filled only as far as its BVNC compatibility blocks:
    /// its KCCB and FWCCB addresses do not exist until [`crate::sched::init`] has
    /// created the rings, and this function also runs from [`Self::hard_reset`],
    /// where they already do. See [`Self::publish_ccb_addresses`].
    fn init_structures(&self, device: &PvrDevice, params: &FwInitParams) -> PvrResult<()> {
        // Firmware timers and active power management depend on a nonzero GPU clock.
        // Report the supplied rate so invalid platform configuration can be diagnosed.
        if params.core_clock_hz == 0 {
            ostd::error!(
                "[k1-gpu] core clock rate is unknown: the firmware will be told 0 Hz, will \
                 idle itself at once, and will not run submitted work"
            );
        } else {
            ostd::debug!("[k1-gpu] core clock: {} Hz", params.core_clock_hz);
        }

        // rogue_fwif_sysinit. Upstream `fw_sysinit_init()`.
        let sysinit = &self.mem.sysinit;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, fault_phys_addr),
            &(self.mem.fault_page.daddr() as u64),
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, pds_exec_base),
            &ROGUE_PDSCODEDATA_HEAP_BASE,
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, usc_exec_base),
            &ROGUE_USCCODE_HEAP_BASE,
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, runtime_cfg_fw_addr),
            &self.mem.runtime_cfg.fw_addr(),
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, trace_buf_ctl_fw_addr),
            &self.trace.ctrl().fw_addr(),
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, fw_sys_data_fw_addr),
            &self.mem.sysdata.fw_addr(),
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, gpu_util_fw_cb_ctl_fw_addr),
            &self.mem.gpu_util_fwcb.fw_addr(),
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, initial_core_clock_speed),
            &params.core_clock_hz,
        )?;
        // The firmware writes `firmware_started` when it is up; the host must see
        // it start out false, and `wait_for_boot` polls it.
        sysinit.write_val(offset_of!(RogueFwifSysinit, firmware_started), &0u32)?;
        sysinit.write_val(offset_of!(RogueFwifSysinit, marker_val), &1u32)?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, gpio_validation_mode),
            &abi::GPIO_VAL_OFF,
        )?;
        sysinit.write_val(
            offset_of!(RogueFwifSysinit, firmware_perf),
            &abi::FW_PERF_CONF_NONE,
        )?;

        // rogue_fwif_sysdata. Upstream `fw_sysdata_init()`.
        let slc_kb = if device.info.features.slc_size_in_kilobytes {
            device.info.values.slc_size_in_kilobytes
        } else {
            // Upstream `WARN_ON`s the lookup failure and leaves its local zero,
            // which selects the same branch as a tiny SLC.
            ostd::warn!("firmware image does not report the SLC size");
            0
        };
        let config_flags = if slc_kb < abi::SLC_MIN_SIZE_FOR_DM_OVERLAP_KB {
            abi::INICFG_DISABLE_DM_OVERLAP
        } else {
            0
        };
        self.mem
            .sysdata
            .write_val(offset_of!(RogueFwifSysdata, config_flags), &config_flags)?;

        // rogue_fwif_runtime_cfg. Upstream `fw_runtime_cfg_init()`.
        let runtime_cfg = &self.mem.runtime_cfg;
        runtime_cfg.write_val(
            offset_of!(RogueFwifRuntimeCfg, core_clock_speed),
            &params.core_clock_hz,
        )?;
        runtime_cfg.write_val(
            offset_of!(RogueFwifRuntimeCfg, active_pm_latency_persistant),
            &1u32,
        )?;
        runtime_cfg.write_val(
            offset_of!(RogueFwifRuntimeCfg, default_dusts_num_init),
            &device.info.values.num_clusters,
        )?;
        // Keep the firmware watchdog disabled, as upstream does.
        runtime_cfg.write_val(offset_of!(RogueFwifRuntimeCfg, wdg_period_us), &0u32)?;

        // rogue_fwif_gpu_util_fwcb. Upstream `fw_gpu_util_fwcb_init()`.
        self.mem.gpu_util_fwcb.write_val(
            offset_of!(RogueFwifGpuUtilFwcb, last_word),
            &abi::GPU_UTIL_STATE_IDLE,
        )?;

        // rogue_fwif_osdata. Upstream `fw_osdata_init()`.
        self.mem.osdata.write_val(
            offset_of!(RogueFwifOsdata, power_sync_fw_addr),
            &self.mem.power_sync.fw_addr(),
        )?;

        // The fault page. Upstream `fw_fault_page_init()`.
        self.mem.fault_page.fill_u32(abi::FAULT_PAGE_FILL)?;

        // `rogue_fwif_osinit`'s two BVNC compatibility blocks. Upstream
        // `rogue_fwif_compchecks_bvnc_init()`, called from `fw_osinit_init()`.
        //
        // The rest of `osinit` — the KCCB, FWCCB and return-slot addresses — is
        // written by [`Self::publish_ccb_addresses`] rather than here, because
        // those rings do not exist until `crate::sched::init` creates them and
        // this function also runs from `hard_reset()`, where they already do.
        // Splitting it that way is what lets a reset re-run this half without
        // destroying the other.
        self.init_bvnc_checks()?;

        Ok(())
    }

    /// Writes the two BVNC compatibility blocks the firmware validates.
    ///
    /// Ports upstream `rogue_fwif_compchecks_bvnc_init()`, which sets the layout
    /// version and zeroes the BVNC itself. The *firmware* fills in the BVNCs and
    /// sets `updated`; the host's job is only to say which layout it is using, and
    /// a firmware that finds a version it does not know stops rather than
    /// misreading the block.
    fn init_bvnc_checks(&self) -> PvrResult<()> {
        use crate::rogue::fwif::{RogueFwifCompchecks, RogueFwifCompchecksBvnc};

        let base = offset_of!(RogueFwifOsinit, rogue_comp_checks);
        for field in [
            offset_of!(RogueFwifCompchecks, hw_bvnc),
            offset_of!(RogueFwifCompchecks, fw_bvnc),
        ] {
            let at = base + field;
            self.mem.osinit.write_val(
                at + offset_of!(RogueFwifCompchecksBvnc, layout_version),
                &crate::sched::abi::COMPCHECKS_LAYOUT_VERSION,
            )?;
            self.mem
                .osinit
                .write_val(at + offset_of!(RogueFwifCompchecksBvnc, bvnc), &0u64)?;
        }
        Ok(())
    }

    /// Publishes CCB addresses and BVNC compatibility blocks before firmware boot.
    ///
    /// [`crate::sched::init`] creates the kernel and firmware rings, maps them, then
    /// calls this method. Reinitialization after reset republishes the same owned
    /// objects; none of their firmware addresses may be zero.
    pub fn publish_ccb_addresses(&self, device: &PvrDevice) -> PvrResult<()> {
        let kccb = device.kccb.get().ok_or(PvrError::Unsupported)?;
        let fwccb = device.fwccb.get().ok_or(PvrError::Unsupported)?;
        let osinit = &self.mem.osinit;

        {
            let kccb = kccb.lock();
            osinit.write_val(
                offset_of!(RogueFwifOsinit, kernel_ccbctl_fw_addr),
                &kccb.ccb().ctrl_fw_addr(),
            )?;
            osinit.write_val(
                offset_of!(RogueFwifOsinit, kernel_ccb_fw_addr),
                &kccb.ccb().ccb_fw_addr(),
            )?;
            osinit.write_val(
                offset_of!(RogueFwifOsinit, kernel_ccb_rtn_slots_fw_addr),
                &kccb.rtn_fw_addr(),
            )?;
        }
        {
            let fwccb = fwccb.lock();
            osinit.write_val(
                offset_of!(RogueFwifOsinit, firmware_ccbctl_fw_addr),
                &fwccb.ctrl_fw_addr(),
            )?;
            osinit.write_val(
                offset_of!(RogueFwifOsinit, firmware_ccb_fw_addr),
                &fwccb.ccb_fw_addr(),
            )?;
        }

        // Upstream writes these two as zero explicitly. Workload estimation is
        // not implemented — there is no `work_est` CCB — and a zero address is
        // how the firmware is told so, which is worth writing rather than
        // leaving to the allocation.
        let zero = crate::rogue::fwif::FwAddr(0);
        osinit.write_val(
            offset_of!(RogueFwifOsinit, work_est_firmware_ccbctl_fw_addr),
            &zero,
        )?;
        osinit.write_val(
            offset_of!(RogueFwifOsinit, work_est_firmware_ccb_fw_addr),
            &zero,
        )?;

        osinit.write_val(
            offset_of!(RogueFwifOsinit, rogue_fwif_hwr_info_buf_ctl_fw_addr),
            &self.mem.hwrinfobuf.fw_addr(),
        )?;
        osinit.write_val(
            offset_of!(RogueFwifOsinit, fw_os_data_fw_addr),
            &self.mem.osdata.fw_addr(),
        )?;
        // Upstream `fwif_osinit->hwr_debug_dump_limit = 0`: no limit on how many
        // hardware-recovery dumps the firmware may produce.
        osinit.write_val(offset_of!(RogueFwifOsinit, hwr_debug_dump_limit), &0u32)?;

        ostd::info!("firmware osinit populated: KCCB, FWCCB and return slots published");
        Ok(())
    }

    /// Maps every allocation into the firmware processor's page table.
    ///
    /// Upstream does this per object inside `pvr_fw_object_fw_map()`, as each is
    /// created. Doing it in one pass at the end is the same set of entries and
    /// makes the "every page of firmware-visible memory is accounted for" claim
    /// checkable in one place.
    fn map_all(&self) -> PvrResult<()> {
        let table = &self.mips.page_table;
        for object in self.mem.objects() {
            table.map(object)?;
        }
        for buffer in self.trace.buffers() {
            table.map(buffer)?;
        }
        table.map(self.trace.ctrl())?;
        Ok(())
    }

    /// Re-loads the code and data sections and re-runs every initialiser.
    ///
    /// Ports upstream `pvr_fw_hard_reset()`. The firmware writes to its own data
    /// section while it runs, so a reset has to put the original bytes back;
    /// upstream keeps the same driver-side copies for the same reason.
    ///
    /// The trace buffers are deliberately *not* cleared — upstream marks them
    /// `PVR_BO_FW_NO_CLEAR_ON_RESET` — because the reset is usually a response to
    /// the failure they describe.
    ///
    /// The rings are not this function's to restore: upstream reaches them by
    /// walking every firmware object, while here they belong to
    /// [`crate::sched`], which puts them back in [`crate::sched::reinit`]. The
    /// caller — [`crate::power::run_reset`] — does the two in that order.
    pub fn hard_reset(&self, device: &PvrDevice) -> PvrResult<()> {
        let params = &self.params;
        ostd::info!("firmware hard reset: restoring code and data, re-initialising structures");
        for object in self.mem.objects() {
            // Code and data are restored from the driver-side copies below.
            if core::ptr::eq(object, &self.mem.code) || core::ptr::eq(object, &self.mem.data) {
                continue;
            }
            object.zero()?;
        }
        self.mem.code.write_bytes(0, &self.mem.code_copy)?;
        self.mem.data.write_bytes(0, &self.mem.data_copy)?;
        self.init_structures(device, params)?;
        // The trace control structure is deliberately *not* re-initialised:
        // upstream skips the initialiser for `PVR_BO_FW_NO_CLEAR_ON_RESET`
        // objects, and resetting `trace_pointer` here would throw away where the
        // firmware had got to — which is the account of the failure that caused
        // this reset.
        self.booted.store(false, Ordering::Release);
        Ok(())
    }

    /// The bootloader's progress counter, from the boot data page.
    ///
    /// Upstream never reads this; the DDK does, and it is the only evidence that
    /// distinguishes "the core never fetched an instruction" from "the core ran
    /// the bootloader and stopped". Zero means the core has not reached the
    /// counter's first update.
    pub fn boot_stage(&self) -> PvrResult<u32> {
        let offset = self.mips.boot_data_offset + rmips::ROGUE_MIPSFW_BOOT_STAGE_OFFSET;
        self.mem.data.read_val::<u32>(offset as usize)
    }

    /// Reads the MIPS exception state from the boot data page's NMI block.
    ///
    /// The shared block has one 32-bit synchronization word followed by
    /// `RogueMipsState`. The ABI constants are word offsets; the size assertion
    /// below verifies the containing layout. Boot-failure classification consumes
    /// this state, so an incorrect offset changes the diagnosis.
    pub fn mips_state(&self) -> PvrResult<RogueMipsState> {
        const _: () = assert!(
            rmips::ROGUE_MIPSFW_NMI_SHARED_SIZE as usize == 4 + size_of::<RogueMipsState>()
        );
        let offset = self.mips.boot_data_offset as usize
            + rmips::ROGUE_MIPSFW_NMI_SHARED_DATA_BASE as usize
            + rmips::ROGUE_MIPSFW_NMI_STATE_OFFSET as usize * 4;
        self.mem.data.read_val::<RogueMipsState>(offset)
    }

    /// Whether the firmware has written `firmware_started`.
    ///
    /// Ports the loop body of upstream `pvr_wait_for_fw_boot()`.
    pub fn firmware_started(&self) -> PvrResult<bool> {
        let started = self
            .mem
            .sysinit
            .read_val::<u32>(offset_of!(RogueFwifSysinit, firmware_started))?;
        Ok(started != 0)
    }
}

/// Logs the bounded firmware trace at debug level before recovery clears it.
pub fn log_trace(device: &PvrDevice, label: &str) {
    if !aster_cmdline::diag_enabled("gpu") {
        return;
    }
    let Some(fw) = device.fw.get() else {
        ostd::debug!("[k1-gpu] {}: no firmware to read a trace from", label);
        return;
    };
    match fw.trace.dump() {
        Ok(summary) if !summary.was_enabled() => ostd::debug!(
            "[k1-gpu] {}: firmware tracing is disabled (log_type {:#010x}); an empty trace \
             says nothing",
            label,
            summary.log_type
        ),
        Ok(summary) => {
            ostd::debug!(
                "[k1-gpu] {}: firmware trace (log_type {:#010x}):",
                label,
                summary.log_type
            );
            for thread in &summary.threads {
                thread.log_early();
            }
        }
        Err(err) => {
            ostd::debug!("[k1-gpu] {}: firmware trace unreadable ({:?})", label, err)
        }
    }
}

/// Starts the MIPS firmware; boot failure is reported before reset.
pub fn boot(device: &PvrDevice) -> PvrResult<()> {
    ostd::debug!("[k1-gpu] firmware boot: starting the MIPS core");
    startstop::start(device)?;

    match wait_for_boot(device) {
        Ok(()) => {
            let fw = device.fw.get().ok_or(PvrError::InvalidArgument)?;
            // Bumped before `booted`, so that anything which observes a booted
            // firmware also observes the generation it booted as. The reverse
            // order would let a job be stamped with the previous generation and
            // then look like it had been interrupted by a power cycle.
            let generation = fw.generation.fetch_add(1, Ordering::AcqRel) + 1;
            fw.booted.store(true, Ordering::Release);
            ostd::debug!("firmware is up, boot generation {}", generation);
            Ok(())
        }
        Err(err) => {
            ostd::debug!(
                "[k1-gpu] firmware boot: wait_for_boot failed ({:?}); collecting diagnosis",
                err
            );
            ostd::error!("firmware failed to boot: {err:?}");
            BootDiagnosis::collect(device).log();
            // Upstream's `err_fw_stop`. Ignore a stop failure: the boot error is
            // the one to report.
            let _ = startstop::stop(device);
            Err(err)
        }
    }
}

/// Waits for the firmware to set `firmware_started`.
///
/// Ports upstream `pvr_wait_for_fw_boot()`, including its five-second budget.
/// Upstream spins on a plain read. `sysinit` is [`heap::FwCache::POLLED`], so
/// the CPU mapping is uncacheable and [`heap::FwObject::read_val`]'s `sync_from_device`
/// is a no-op — the word is visible without `cbo.flush`. A write-back mapping
/// plus that flush was fourteen updates late on this board.
pub fn wait_for_boot(device: &PvrDevice) -> PvrResult<()> {
    let fw = device.fw.get().ok_or(PvrError::InvalidArgument)?;
    let deadline = ticks_from_now(abi::FW_BOOT_TIMEOUT_USEC);
    let start = read_tsc();

    loop {
        if fw.firmware_started()? {
            return Ok(());
        }
        if read_tsc().wrapping_sub(start) >= deadline {
            return Err(PvrError::Timeout);
        }
        core::hint::spin_loop();
    }
}

/// Busy-waits for `us` microseconds.
///
/// The two three-microsecond waits in [`startstop::start`] are upstream
/// `udelay()`s and happen with the GPU half-configured, so they cannot sleep.
/// Asterinas has no `udelay`, so this is the same spin on the timestamp counter
/// that [`PvrDevice::poll32`] uses.
pub fn delay_us(us: u64) {
    let deadline = ticks_from_now(us);
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < deadline {
        core::hint::spin_loop();
    }
}

/// Timestamp-counter ticks in `us` microseconds, at least one.
fn ticks_from_now(us: u64) -> u64 {
    ((tsc_freq() as u128 * us as u128) / 1_000_000).max(1) as u64
}

/// Why the firmware did not come up.
///
/// The distinction is the point. Each of these sends the next hour somewhere
/// different, and on a GPU that has never run this driver, guessing wrong is
/// expensive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootFailure {
    /// The MIPS core took an exception. `error_state` in the NMI block is set,
    /// and the accompanying `epc` and `cause` say where and why.
    ///
    /// Look at: the exception PC against the code section's layout entry, and
    /// whether the page table covers the address in `bad_register`.
    Exception,
    /// A firmware assertion fired. The trace buffer carries its text.
    ///
    /// Look at: the assertion message. The firmware is telling you which of its
    /// own invariants the host broke.
    Assertion,
    /// The firmware reported itself started and then stopped responding.
    ///
    /// Look at: the trace's last entries. The firmware got through
    /// initialisation, so the host's structures are broadly right and the problem
    /// is in what happened next — usually a command stream or a mapping.
    HungAfterStart,
    /// Trace output before `firmware_started` means the firmware executed but did
    /// not complete initialization. Inspect the shared initialization structures and
    /// firmware report; the trace alone does not identify which input was rejected.
    DiedDuringInit,
    /// The bootloader's stage counter moved but the firmware logged nothing.
    ///
    /// Look at: the boot data block and the page table. The core is executing, so
    /// the remap registers are right; something between the bootloader and the
    /// first trace write is not.
    DiedInBootloader,
    /// Without trace output, firmware execution is unconfirmed. A readable BVNC
    /// establishes register access but does not prove firmware clocks, reset state,
    /// or MIPS mappings are correct.
    NeverStarted,
}

impl BootFailure {
    /// A one-line hint about where to look next.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Exception => "check the exception PC against the code section and the page table",
            Self::Assertion => "read the assertion text; the firmware names the broken invariant",
            Self::HungAfterStart => "initialisation succeeded; look at the last trace entries",
            Self::DiedDuringInit => {
                "firmware wrote trace but never set firmware_started; check osinit/KCCB and the last trace"
            }
            Self::DiedInBootloader => "the core is executing; check the boot data and page table",
            Self::NeverStarted => "check power, clock and reset order, then the remap registers",
        }
    }
}

/// Everything the host can find out about a firmware that did not come up.
///
/// Collected *before* [`startstop::stop`] runs, because the reset it asserts
/// clears the state this reads.
pub struct BootDiagnosis {
    /// The classification.
    pub failure: BootFailure,
    /// The bootloader's progress counter, or `None` if it could not be read.
    pub boot_stage: Option<u32>,
    /// Whether `firmware_started` was set.
    pub firmware_started: bool,
    /// The trace buffers' contents.
    pub trace: Option<trace::TraceSummary>,
    /// The MIPS core's exception state, when it reports one.
    pub mips_state: Option<RogueMipsState>,
}

impl BootDiagnosis {
    /// Gathers every piece of evidence and classifies the failure.
    ///
    /// Every read is fallible and every failure is tolerated: this runs when
    /// something has already gone wrong, and a diagnosis that gives up on the
    /// first unreadable field is no diagnosis.
    pub fn collect(device: &PvrDevice) -> Self {
        let Some(fw) = device.fw.get() else {
            return Self {
                failure: BootFailure::NeverStarted,
                boot_stage: None,
                firmware_started: false,
                trace: None,
                mips_state: None,
            };
        };

        let boot_stage = fw.boot_stage().ok();
        let firmware_started = fw.firmware_started().unwrap_or(false);
        let trace = fw.trace.dump().ok();
        let mips_state = fw
            .mips_state()
            .ok()
            .filter(|state| state.error_state & rmips::ROGUE_MIPSFW_NMI_ERROR_STATE_SET != 0);

        let has_trace_entries = trace.as_ref().is_some_and(trace::TraceSummary::has_entries);
        let has_assertion = trace
            .as_ref()
            .is_some_and(|summary| summary.assertion().is_some());

        let failure = if mips_state.is_some() {
            BootFailure::Exception
        } else if has_assertion {
            BootFailure::Assertion
        } else if firmware_started {
            BootFailure::HungAfterStart
        } else if has_trace_entries {
            BootFailure::DiedDuringInit
        } else if boot_stage.is_some_and(|stage| stage != 0) {
            BootFailure::DiedInBootloader
        } else {
            BootFailure::NeverStarted
        };

        Self {
            failure,
            boot_stage,
            firmware_started,
            trace,
            mips_state,
        }
    }

    /// Reports the failure class, with bounded detailed state at debug level.
    pub fn log(&self) {
        ostd::error!(
            "[k1-gpu] firmware boot diagnosis: {:?} — {}",
            self.failure,
            self.failure.hint()
        );
        ostd::debug!(
            "[k1-gpu]   boot_stage={:?} firmware_started={} trace_entries={} has_assertion={} \
             mips_exception={}",
            self.boot_stage,
            self.firmware_started,
            self.trace
                .as_ref()
                .is_some_and(trace::TraceSummary::has_entries),
            self.trace
                .as_ref()
                .is_some_and(|summary| summary.assertion().is_some()),
            self.mips_state.is_some()
        );
        match &self.trace {
            Some(summary) if !summary.was_enabled() => {
                ostd::debug!(
                    "[k1-gpu]   firmware tracing was disabled (log_type {:#010x}); an empty \
                     trace says nothing",
                    summary.log_type
                );
            }
            Some(summary) => {
                ostd::debug!(
                    "[k1-gpu]   firmware tracing enabled (log_type {:#010x}), entries present: {}",
                    summary.log_type,
                    summary.has_entries()
                );
                // The entries themselves, not just their existence. This used
                // to say only "entries present: true" and throw the decoded
                // ring away, leaving the one question a stalled `SUBMIT_JOBS`
                // turns on — did the firmware ever see the command the driver
                // posted — answerable only by guessing from the driver's side
                // of the ring.
                for thread in &summary.threads {
                    thread.log_early();
                }
            }
            None => {
                ostd::debug!("[k1-gpu]   trace buffers could not be read");
            }
        }
        if let Some(state) = &self.mips_state {
            // Capture every exception-state field, including zero values, so a firmware
            // address fault can be distinguished from missing execution evidence.
            ostd::debug!(
                "[k1-gpu]   MIPS exception: error_state {:#x}, epc {:#010x}, error_epc {:#010x}, \
                 cause {:#010x}, status {:#010x}, bad_vaddr {:#010x}, bad_instr {:#010x}, \
                 sp {:#010x}, unmapped {:#010x}",
                state.error_state,
                state.epc,
                state.error_epc,
                state.cause_register,
                state.status_register,
                state.bad_register,
                state.bad_instr,
                state.sp,
                state.unmapped_address
            );
            // An empty record with the flag set is a different finding from an
            // exception, and the hint above ("check the exception PC") is a
            // dead end for it: `Exception` is chosen from
            // `ROGUE_MIPSFW_NMI_ERROR_STATE_SET` alone, so a firmware that
            // raised the flag without filling the block in classifies the same
            // way as one that faulted. Say which happened rather than leaving
            // the reader to notice the zeros.
            if state.epc == 0 && state.error_epc == 0 && state.cause_register == 0 {
                ostd::debug!(
                    "[k1-gpu]   the NMI block carries no exception record: the error-state flag \
                     is set but every register is zero, so this is not a fault at a PC. Look at \
                     the trace above, not at the code section."
                );
            }
        }
    }
}

/// A synthetic firmware image, complete enough for [`FwDevice::load`]'s
/// non-allocating half.
///
/// Combines [`info::fixture`]'s trailer with a synthetic ELF32 body and the
/// deterministic device-info block from [`crate::device_info::fixture`]. Tests
/// exercise image validation and loading independently of the licensed firmware
/// supplied by the initramfs; they do not execute firmware or GPU work.
#[cfg(any(test, ktest))]
pub(crate) mod fixture {
    use alloc::vec::Vec;

    use ostd_pod::IntoBytes;

    use super::{
        elf::{Elf32Ehdr, Elf32Phdr, PT_LOAD},
        info::{FW_BLOCK_SIZE, fixture::*},
    };

    /// Byte offset of the first ELF segment's payload.
    pub(crate) const FIRST_SEGMENT_OFFSET: usize = FW_BLOCK_SIZE;

    /// Fill byte written into segment `n`, so a test can tell the sections apart
    /// after they have been copied.
    pub(crate) fn segment_fill(index: usize) -> u8 {
        0xa0 + index as u8
    }

    /// Bytes present in the file for each segment; the rest of `p_memsz` is
    /// zero-filled by the loader.
    ///
    /// `MIPS_BOOT_DATA` is deliberately short. In a real image most of that page
    /// is the firmware's own scratch — the bootloader configuration block the
    /// driver fills, then the NMI state block and the boot-stage counter the
    /// firmware writes — so it arrives as zeroes rather than as loaded bytes. The
    /// diagnosis path depends on that: a boot-stage counter that came out of the
    /// image would read non-zero before anything had run.
    pub(crate) const SECTION_FILE_SIZES: [u32; 6] = [
        SECTION_SIZE,
        SECTION_SIZE,
        SECTION_SIZE,
        SECTION_SIZE,
        BOOT_DATA_FILE_SIZE,
        SECTION_SIZE,
    ];

    /// Bytes of `MIPS_BOOT_DATA` the fixture's image actually carries.
    ///
    /// Past `size_of::<mips::MipsBootData>()` (64), so a test can tell a loaded
    /// byte from one the boot-argument writer overwrote.
    pub(crate) const BOOT_DATA_FILE_SIZE: u32 = 0x80;

    /// Builds a complete image: ELF header, six `PT_LOAD` segments matching the
    /// layout table, a device-info block, and the trailing container.
    pub(crate) fn image() -> Vec<u8> {
        let header = default_header();
        let layout = default_layout();
        let section_size = SECTION_SIZE as usize;

        // body | device_info (4 KiB) | header region (4 KiB)
        let body = FIRST_SEGMENT_OFFSET + layout.len() * section_size;
        let total = body + header.device_info_size as usize + FW_BLOCK_SIZE;
        let mut image = alloc::vec![0u8; total];

        let ehdr = Elf32Ehdr {
            e_ident: [0x7f, b'E', b'L', b'F', 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            e_type: 2,    // ET_EXEC
            e_machine: 8, // EM_MIPS
            e_version: 1,
            e_entry: SECTION_BASE_ADDRS[2],
            e_phoff: size_of::<Elf32Ehdr>() as u32,
            e_shoff: 0,
            e_flags: 0,
            e_ehsize: size_of::<Elf32Ehdr>() as u16,
            e_phentsize: size_of::<Elf32Phdr>() as u16,
            e_phnum: layout.len() as u16,
            e_shentsize: 0,
            e_shnum: 0,
            e_shstrndx: 0,
        };
        image[..size_of::<Elf32Ehdr>()].copy_from_slice(ehdr.as_bytes());

        for (index, base) in SECTION_BASE_ADDRS.iter().enumerate() {
            let payload = FIRST_SEGMENT_OFFSET + index * section_size;
            let file_size = SECTION_FILE_SIZES[index];
            let phdr = Elf32Phdr {
                p_type: PT_LOAD,
                p_offset: payload as u32,
                p_vaddr: *base,
                p_paddr: *base,
                p_filesz: file_size,
                p_memsz: SECTION_SIZE,
                p_flags: 0,
                p_align: SECTION_SIZE,
            };
            let at = size_of::<Elf32Ehdr>() + index * size_of::<Elf32Phdr>();
            image[at..at + size_of::<Elf32Phdr>()].copy_from_slice(phdr.as_bytes());
            image[payload..payload + file_size as usize].fill(segment_fill(index));
        }

        // The device-info block, padded to the `device_info_size` the header
        // declares. Its contents are the same fixture `DeviceInfo`'s own tests
        // use, so a feature this port reads is a feature the image reports.
        let (features, params, quirks, enhancements) = crate::device_info::fixture::bxe_masks();
        let block = crate::device_info::fixture::block(&features, &params, &quirks, &enhancements);
        let block_start = total - FW_BLOCK_SIZE - header.device_info_size as usize;
        image[block_start..block_start + block.len()].copy_from_slice(&block);

        write_trailer(&mut image, &header, &layout);
        image
    }
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::{fixture::*, *};

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn firmware_name_matches_the_open_firmware_repository() {
        assert_eq!(
            firmware_name(&GpuId::K1_BXE_2_32),
            "powervr/rogue_36.29.52.182_v1.fw"
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn validate_accepts_the_fixture_and_reports_bxe_2_32() {
        let image = image();
        let validated = validate_and_init_device_info(&image).expect("fixture must validate");

        assert_eq!(validated.gpu_id, GpuId::K1_BXE_2_32);
        assert_eq!(validated.version, FwVersion { major: 1, minor: 1 });
        assert_eq!(validated.layout.len(), 6);
        // The device description comes from the image, not from a table.
        assert!(validated.info.features.mips);
        assert!(!validated.info.features.meta);
        assert!(validated.info.features.phys_bus_width);
        assert_eq!(validated.info.values.phys_bus_width, 40);
    }

    /// Step 6: the layout table's section ids are the only evidence of which
    /// firmware processor an image is for, and `pvr/UPSTREAM` stakes the META and
    /// RISC-V exclusions on them.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn validate_rejects_an_image_for_a_non_mips_coprocessor() {
        use info::{SectionId, fixture::*};

        for (id, ty) in [
            (SectionId::MetaCode, info::SectionType::FwCode),
            (SectionId::RiscvCachedCode, info::SectionType::FwCode),
        ] {
            let mut header = default_header();
            header.layout_entry_num = 1;
            let layout = [info::LayoutEntry {
                id: id as u32,
                r#type: ty as u32,
                base_addr: 0x8000_0000,
                max_size: SECTION_SIZE,
                alloc_size: SECTION_SIZE,
                alloc_offset: 0,
            }];
            let image = build_fixture(&header, &layout);
            assert_eq!(
                validate_and_init_device_info(&image).err(),
                Some(PvrError::Unsupported),
                "a {id:?} image must be refused, not silently accepted"
            );
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn validate_rejects_the_four_container_failures() {
        use info::fixture::*;

        // info_version != 3.
        let mut header = default_header();
        header.info_version = 2;
        assert_eq!(
            validate_and_init_device_info(&build_fixture(&header, &default_layout())).err(),
            Some(PvrError::BadFirmware)
        );

        // PVR_FW_FLAGS_OPEN_SOURCE missing: DDK firmware for the same silicon.
        let mut header = default_header();
        header.flags = 0;
        assert_eq!(
            validate_and_init_device_info(&build_fixture(&header, &default_layout())).err(),
            Some(PvrError::BadFirmware)
        );

        // A firmware major version this driver does not speak.
        let mut header = default_header();
        header.fw_version_major = 2;
        assert_eq!(
            validate_and_init_device_info(&build_fixture(&header, &default_layout())).err(),
            Some(PvrError::BadFirmware)
        );

        // More layout entries than the format allows.
        let mut header = default_header();
        header.layout_entry_num = (info::MAX_LAYOUT_ENTRIES + 1) as u32;
        let mut layout = Vec::new();
        for index in 0..info::MAX_LAYOUT_ENTRIES + 1 {
            layout.push(info::LayoutEntry {
                id: info::SectionId::MipsCode as u32,
                r#type: info::SectionType::FwCode as u32,
                base_addr: 0xc000_0000 + index as u32 * SECTION_SIZE,
                max_size: SECTION_SIZE,
                alloc_size: SECTION_SIZE,
                alloc_offset: index as u32 * SECTION_SIZE,
            });
        }
        assert_eq!(
            validate_and_init_device_info(&build_fixture(&header, &layout)).err(),
            Some(PvrError::BadFirmware)
        );
    }

    /// The ELF loader must put each segment where the layout table says, in the
    /// right buffer, at the right offset. Getting this wrong loads the firmware's
    /// exception vectors over its boot code, which looks exactly like a hardware
    /// fault.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn elf_segments_land_in_the_right_section_at_the_right_offset() {
        use info::fixture::*;

        let image = image();
        let parsed = info::parse(&image).expect("fixture");
        let sizes = parsed.section_sizes();

        let mut code = alloc::vec![0u8; sizes.code as usize];
        let mut data = alloc::vec![0u8; sizes.data as usize];
        elf::process_command_stream(
            &image,
            &parsed.layout,
            &mut elf::Destination {
                code: &mut code,
                data: &mut data,
                core_code: None,
                core_data: None,
            },
        )
        .expect("every segment must be placed");

        let block = SECTION_SIZE as usize;
        // Code: MIPS_CODE, MIPS_EXCEPTIONS_CODE, MIPS_BOOT_CODE, in layout order.
        for (slot, source) in [(0usize, 0usize), (1, 1), (2, 2)] {
            assert_eq!(
                code[slot * block],
                segment_fill(source),
                "code block {slot} should hold segment {source}"
            );
            assert_eq!(code[slot * block + block - 1], segment_fill(source));
        }
        // Data: MIPS_PRIVATE_DATA, MIPS_BOOT_DATA, MIPS_STACK.
        for (slot, source) in [(0usize, 3usize), (1, 4), (2, 5)] {
            assert_eq!(
                data[slot * block],
                segment_fill(source),
                "data block {slot} should hold segment {source}"
            );
        }
        // The whole of each fully-loaded section was written.
        assert!(code.iter().all(|byte| *byte != 0));

        // `MIPS_BOOT_DATA` carries fewer file bytes than it occupies, and the
        // tail must be zeroed rather than left holding the neighbouring segment.
        // The boot-stage counter lives there, and a non-zero one would make
        // `BootDiagnosis` report a firmware that had run.
        let boot_data = block + BOOT_DATA_FILE_SIZE as usize;
        assert!(data[boot_data..2 * block].iter().all(|byte| *byte == 0));
        assert_eq!(
            data[block + crate::rogue::mips::ROGUE_MIPSFW_BOOT_STAGE_OFFSET as usize],
            0
        );
    }

    /// A hint per failure mode, and no two the same: the whole value of the
    /// classification is that it points somewhere different each time.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn every_boot_failure_points_somewhere_different() {
        let failures = [
            BootFailure::Exception,
            BootFailure::Assertion,
            BootFailure::HungAfterStart,
            BootFailure::DiedDuringInit,
            BootFailure::DiedInBootloader,
            BootFailure::NeverStarted,
        ];
        for (index, failure) in failures.iter().enumerate() {
            assert!(!failure.hint().is_empty());
            for other in &failures[index + 1..] {
                assert_ne!(failure.hint(), other.hint());
            }
        }
    }

    /// Tracing must be on by default, or the first failure on this untested GPU
    /// produces an empty trace buffer and no second chance.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn tracing_is_enabled_by_default() {
        assert_ne!(DEFAULT_TRACE_GROUP_MASK, 0);
        assert_eq!(trace::validate_group_mask(DEFAULT_TRACE_GROUP_MASK), Ok(()));
        assert_eq!(
            FwInitParams::default().trace_group_mask,
            DEFAULT_TRACE_GROUP_MASK
        );
        assert_ne!(
            trace::build_log_type(DEFAULT_TRACE_GROUP_MASK) & abi::LOG_TYPE_TRACE,
            0
        );
    }

    /// A device whose registers are simulated and whose description comes from
    /// the same fixture the image carries.
    #[cfg(ktest)]
    fn bxe_device() -> Arc<PvrDevice> {
        use crate::device::RegisterWindow;

        let image = image();
        let validated = validate_and_init_device_info(&image).expect("fixture");
        let regs = RegisterWindow::simulated();
        regs.write64(
            crate::rogue::cr::core_id_pbvnc::OFFSET,
            GpuId::K1_BXE_2_32.to_packed(),
        )
        .unwrap();
        PvrDevice::probe(regs, &validated).expect("BVNCs agree and the core is MIPS")
    }

    /// The whole of [`FwDevice::load`], which is everything firmware loading can do
    /// without the silicon: validate, allocate, load the ELF, build the boot
    /// arguments, fill the structures and map every page.
    #[ktest]
    fn load_places_every_section_and_maps_every_page() {
        let device = bxe_device();
        let image = image();
        let params = FwInitParams {
            core_clock_hz: 614_400_000,
            regs_daddr: 0xd800_0000,
            ..FwInitParams::default()
        };
        let fw = FwDevice::load(&device, image, params).expect("the fixture must load");

        // Code at heap offset 0, data where MIPS_PRIVATE_DATA said.
        assert_eq!(fw.mem.code.fw_offset(), 0);
        assert_eq!(fw.mem.code.fw_addr().0, mips::FW_HEAP_MIPS_BASE);
        assert_eq!(fw.mem.data.fw_offset(), 0x0040_0000);

        // The ELF segments reached firmware-visible memory, in layout order.
        // `MIPS_BOOT_DATA` — data block 1 — is probed past the 64 bytes
        // `mips::process` writes the bootloader configuration into.
        let block = info::fixture::SECTION_SIZE as usize;
        let data_probe = [0usize, size_of::<mips::MipsBootData>(), 0];
        for (slot, probe) in data_probe.iter().enumerate() {
            let mut byte = [0u8; 1];
            fw.mem.code.read_bytes(slot * block, &mut byte).unwrap();
            assert_eq!(byte[0], segment_fill(slot), "code block {slot}");
            fw.mem
                .data
                .read_bytes(slot * block + probe, &mut byte)
                .unwrap();
            assert_eq!(byte[0], segment_fill(slot + 3), "data block {slot}");
        }

        // Every code page has a page table entry, and the entry names the page.
        let table = &fw.mips.page_table;
        for page in 0..3u32 {
            let pte = table.entry(page).expect("entry");
            assert_ne!(pte, 0, "code page {page} must be mapped");
        }
        // So does the sysinit structure, up in the configuration area.
        let sysinit_pfn = fw.mem.sysinit.fw_offset() / 4096;
        assert_ne!(table.entry(sysinit_pfn).unwrap(), 0);

        // The structures the firmware reads before anything else.
        let sysinit_clock: u32 = fw
            .mem
            .sysinit
            .read_val(offset_of!(RogueFwifSysinit, initial_core_clock_speed))
            .unwrap();
        assert_eq!(sysinit_clock, 614_400_000);
        let trace_ctl: crate::rogue::fwif::FwAddr = fw
            .mem
            .sysinit
            .read_val(offset_of!(RogueFwifSysinit, trace_buf_ctl_fw_addr))
            .unwrap();
        assert_eq!(trace_ctl, fw.trace.ctrl().fw_addr());
        assert!(!fw.firmware_started().unwrap());

        // The fault page is recognisable rather than plausible.
        assert_eq!(
            fw.mem.fault_page.read_val::<u32>(0).unwrap(),
            abi::FAULT_PAGE_FILL
        );

        // Tracing is on, so a failure would leave evidence.
        let summary = fw.trace.dump().expect("trace readable");
        assert!(summary.was_enabled());
        assert!(!summary.has_entries());

        // Nothing ran, so the diagnosis must say so rather than guess.
        assert_eq!(fw.boot_stage().unwrap(), 0);
    }

    /// Objects the firmware writes and the CPU polls must be CPU-uncached.
    /// Everything else from [`FwDevice::load`] must stay write-back.
    ///
    /// The expected column is the policy, not a copy of the flag
    /// `allocate_structures` passed. Flipping `sysinit` to write-back, or
    /// dragging the code image onto `PBMT_NC`, turns this red. Adding a
    /// field to [`FwMem`] without updating the table fails the length check.
    #[ktest]
    fn firmware_heap_cpu_mapping_matches_the_poll_table() {
        let device = bxe_device();
        let fw = FwDevice::load(&device, image(), FwInitParams::default()).expect("load");

        let rows: &[(&str, &heap::FwObject, bool)] = &[
            ("code", &fw.mem.code, false),
            ("data", &fw.mem.data, false),
            ("connection_ctl", &fw.mem.connection_ctl, true),
            ("osinit", &fw.mem.osinit, false),
            ("sysinit", &fw.mem.sysinit, true),
            ("osdata", &fw.mem.osdata, true),
            ("sysdata", &fw.mem.sysdata, true),
            ("hwrinfobuf", &fw.mem.hwrinfobuf, false),
            ("power_sync", &fw.mem.power_sync, true),
            ("fault_page", &fw.mem.fault_page, false),
            ("gpu_util_fwcb", &fw.mem.gpu_util_fwcb, false),
            ("runtime_cfg", &fw.mem.runtime_cfg, false),
            ("mmucache_sync", &fw.mem.mmucache_sync, true),
        ];
        assert_eq!(
            fw.mem.objects().len(),
            rows.len(),
            "FwMem gained a field; add it to this table"
        );
        for (what, object, expected) in rows {
            assert_eq!(
                object.is_cpu_uncached(),
                *expected,
                "{what}: firmware writes + CPU polls ⇒ uncacheable, else write-back"
            );
        }

        assert!(fw.trace.ctrl().is_cpu_uncached(), "trace control is polled");
        for (i, buffer) in fw.trace.buffers().iter().enumerate() {
            assert!(
                !buffer.is_cpu_uncached(),
                "trace ring {i} is dumped, not polled"
            );
        }
    }

    /// The boot arguments are the first thing the firmware reads, and every field
    /// wrong there looks the same from the host.
    #[ktest]
    fn boot_data_names_the_stack_registers_and_page_table() {
        let device = bxe_device();
        let params = FwInitParams {
            regs_daddr: 0xd800_0000,
            ..FwInitParams::default()
        };
        let fw = FwDevice::load(&device, image(), params).expect("load");

        let boot: mips::MipsBootData = fw
            .mem
            .data
            .read_val(fw.mips.boot_data_offset as usize)
            .unwrap();
        assert_eq!(boot.reg_base, 0xd800_0000);
        assert_eq!(
            boot.pt_log2_page_size,
            crate::rogue::mips::ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K
        );
        assert_eq!(boot.pt_num_pages, 4);
        // The four page-table pages are consecutive and non-zero.
        for page in 0..4usize {
            assert_eq!(
                boot.pt_phys_addr[page],
                fw.mips.page_table.page_daddr(page).unwrap() as u64
            );
        }
        // MIPS_STACK is the third data section.
        assert_eq!(
            boot.stack_phys_addr,
            fw.mem
                .data
                .daddr_at(2 * info::fixture::SECTION_SIZE as usize)
                .unwrap() as u64
        );
    }

    /// A firmware that never ran leaves an empty trace and a zero boot stage, and
    /// the diagnosis must reach `NeverStarted` from that rather than from a
    /// timeout alone. This is the classification the `needs-hardware` boot path
    /// depends on.
    #[ktest]
    fn diagnosis_of_an_unstarted_firmware_is_never_started() {
        let device = bxe_device();
        init(&device, image(), FwInitParams::default()).expect("load and install");
        // Installing twice would give the firmware two worlds.
        assert_eq!(
            init(&device, image(), FwInitParams::default()).err(),
            Some(PvrError::Busy)
        );

        let diagnosis = BootDiagnosis::collect(&device);
        assert_eq!(diagnosis.failure, BootFailure::NeverStarted);
        assert_eq!(diagnosis.boot_stage, Some(0));
        assert!(!diagnosis.firmware_started);
        assert!(
            diagnosis
                .trace
                .as_ref()
                .is_some_and(trace::TraceSummary::was_enabled)
        );
        assert!(diagnosis.mips_state.is_none());
        diagnosis.log();
    }

    /// The heap arithmetic the firmware's boot code has compiled in.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn mips_heap_geometry_matches_upstream() {
        let heap = heap::FwHeapInfo::new(
            mips::FW_HEAP_MIPS_LOG2_SIZE,
            mips::FW_HEAP_MIPS_RESERVED_SIZE,
        )
        .expect("16 MiB heap with 1 MiB reserved");

        assert_eq!(heap.raw_size, 16 * 1024 * 1024);
        assert_eq!(heap.offset_mask, 0x00ff_ffff);
        // The configuration area is the top 192 KiB.
        assert_eq!(heap.config_offset, 16 * 1024 * 1024 - 3 * 64 * 1024);
        // The general area excludes both the configuration and reserved areas.
        assert_eq!(heap.size, 16 * 1024 * 1024 - 3 * 64 * 1024 - 1024 * 1024);
        // The three fixed-offset structures are one granule apart.
        assert_eq!(heap::CONNECTION_CTL_OFFSET, 0);
        assert_eq!(heap::OSINIT_OFFSET, 64 * 1024);
        assert_eq!(heap::SYSINIT_OFFSET, 128 * 1024);
        // A heap too small for the configuration area is rejected, not wrapped.
        assert!(heap::FwHeapInfo::new(19, 0).is_err());
        assert!(heap::FwHeapInfo::new(20, 16 * 1024 * 1024).is_err());
    }
}
