// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: host-side PowerVR register and firmware ABI generation.

//! Register allow-list for `cr.rs`.
//!
//! Derived by grepping every `ROGUE_CR_*` token referenced from upstream
//! `drivers/gpu/drm/imagination/*.c` at the commit in `../UPSTREAM`, then
//! mapping each token to its register base (the `#define` whose value is the
//! MMIO offset). `ROGUE_CR_CORE_ID__PBVNC` is always included because it is the
//! hand-written seed in `src/rogue/cr.rs` and the first register the port reads.
//!
//! META wrapper registers stay on the list even though BXE-2-32 boots MIPS: the
//! upstream `.c` files still reference them on the META path, and silently
//! dropping an allow-listed name would violate the totality rule.

/// One allow-listed control register.
pub struct AllowEntry {
    /// Exact upstream `#define` name for the register offset (e.g. `ROGUE_CR_SOFT_RESET`).
    pub name: &'static str,
    /// Why this register is on the list (which upstream call sites need it).
    pub why: &'static str,
}

/// Registers emitted into `src/rogue/cr.rs`.
pub const REGISTER_ALLOW_LIST: &[AllowEntry] = &[
    AllowEntry {
        name: "ROGUE_CR_CORE_ID__PBVNC",
        why: "seed + PvrDevice::read_gpu_id / GpuId::from_packed (packed BVNC at 0x20)",
    },
    AllowEntry {
        name: "ROGUE_CR_CORE_ID",
        why: "legacy non-packed CORE_ID layout; pvr_device.c reads both forms",
    },
    AllowEntry {
        name: "ROGUE_CR_CORE_REVISION",
        why: "pvr_device.c identity / revision reads",
    },
    AllowEntry {
        name: "ROGUE_CR_SOFT_RESET",
        why: "pvr_power.c / pvr_fw_startstop.c reset sequences",
    },
    AllowEntry {
        name: "ROGUE_CR_SOFT_RESET2",
        why: "pvr_fw_startstop.c Jones/Dust reset step two",
    },
    AllowEntry {
        name: "ROGUE_CR_EVENT_STATUS",
        why: "IRQ / event polling in pvr_irq.c and friends",
    },
    AllowEntry {
        name: "ROGUE_CR_EVENT_CLEAR",
        why: "acknowledge events after STATUS",
    },
    AllowEntry {
        name: "ROGUE_CR_IRQ_OS0_EVENT_STATUS",
        why: "OS0 IRQ status (host-visible)",
    },
    AllowEntry {
        name: "ROGUE_CR_IRQ_OS0_EVENT_CLEAR",
        why: "OS0 IRQ clear",
    },
    AllowEntry {
        name: "ROGUE_CR_SIDEKICK_IDLE",
        why: "idle wait before reset / power down",
    },
    AllowEntry {
        name: "ROGUE_CR_MARS_IDLE",
        why: "MARS idle bits alongside SIDEKICK",
    },
    AllowEntry {
        name: "ROGUE_CR_SLC_IDLE",
        why: "SLC idle wait in start/stop",
    },
    AllowEntry {
        name: "ROGUE_CR_SLC_STATUS1",
        why: "SLC status during bring-up / debug",
    },
    AllowEntry {
        name: "ROGUE_CR_SLC_CTRL_MISC",
        why: "SLC hash / bypass / scramble programming",
    },
    AllowEntry {
        name: "ROGUE_CR_BIF_STATUS_MMU",
        why: "BIF MMU fault status",
    },
    AllowEntry {
        name: "ROGUE_CR_BIF_READS_EXT_STATUS",
        why: "BIF outstanding reads (hang detection)",
    },
    AllowEntry {
        name: "ROGUE_CR_BIFPM_STATUS_MMU",
        why: "PM BIF MMU status",
    },
    AllowEntry {
        name: "ROGUE_CR_BIFPM_READS_EXT_STATUS",
        why: "PM BIF outstanding reads",
    },
    AllowEntry {
        name: "ROGUE_CR_BIF_CAT_BASE0",
        why: "page-catalogue base; ADDR_* fields used when programming contexts",
    },
    AllowEntry {
        name: "ROGUE_CR_SYS_BUS_SECURE",
        why: "sys-bus secure bit during boot",
    },
    AllowEntry {
        name: "ROGUE_CR_AXI_ACE_LITE_CONFIGURATION",
        why: "coherency / cache domain setup on probe",
    },
    AllowEntry {
        name: "ROGUE_CR_MTS_SCHEDULE",
        why: "kick firmware DMs via MTS",
    },
    AllowEntry {
        name: "ROGUE_CR_MTS_BGCTX_THREAD0_DM_ASSOC",
        why: "background-context DM association (thread 0)",
    },
    AllowEntry {
        name: "ROGUE_CR_MTS_BGCTX_THREAD1_DM_ASSOC",
        why: "background-context DM association (thread 1)",
    },
    AllowEntry {
        name: "ROGUE_CR_MTS_INTCTX_THREAD0_DM_ASSOC",
        why: "interrupt-context DM association (thread 0)",
    },
    AllowEntry {
        name: "ROGUE_CR_MTS_INTCTX_THREAD1_DM_ASSOC",
        why: "interrupt-context DM association (thread 1)",
    },
    AllowEntry {
        name: "ROGUE_CR_MTS_GARTEN_WRAPPER_CONFIG",
        why: "Garten wrapper fence / idle config at FW start",
    },
    AllowEntry {
        name: "ROGUE_CR_FWCORE_BOOT",
        why: "FWCORE boot kick",
    },
    AllowEntry {
        name: "ROGUE_CR_FWCORE_MEM_CAT_BASE0",
        why: "FWCORE MMU catalogue base",
    },
    AllowEntry {
        name: "ROGUE_CR_FWCORE_ADDR_REMAP_CONFIG0",
        why: "FWCORE remap config (size / enable bits)",
    },
    AllowEntry {
        name: "ROGUE_CR_FWCORE_ADDR_REMAP_CONFIG1",
        why: "FWCORE remap device VA",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_WRAPPER_CONFIG",
        why: "MIPS wrapper boot / ISA mode (BXE-2-32 path)",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_DEBUG_CONFIG",
        why: "MIPS debug config",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_WRAPPER_IRQ_STATUS",
        why: "MIPS wrapper IRQ status",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_WRAPPER_IRQ_CLEAR",
        why: "MIPS wrapper IRQ clear",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP1_CONFIG1",
        why: "MIPS remap region 1 config1 (boot)",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP1_CONFIG2",
        why: "MIPS remap region 1 config2",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP2_CONFIG1",
        why: "MIPS remap region 2 config1 (data)",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP2_CONFIG2",
        why: "MIPS remap region 2 config2",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP3_CONFIG1",
        why: "MIPS remap region 3 config1 (code)",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP3_CONFIG2",
        why: "MIPS remap region 3 config2",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP5_CONFIG1",
        why: "MIPS remap region 5 config1",
    },
    AllowEntry {
        name: "ROGUE_CR_MIPS_ADDR_REMAP5_CONFIG2",
        why: "MIPS remap region 5 config2",
    },
    AllowEntry {
        name: "ROGUE_CR_META_BOOT",
        why: "referenced from shared start/stop C; META path unused on BXE-2-32",
    },
    AllowEntry {
        name: "ROGUE_CR_META_SP_MSLVCTRL0",
        why: "META slave port ctrl0 (shared C references)",
    },
    AllowEntry {
        name: "ROGUE_CR_META_SP_MSLVCTRL1",
        why: "META slave port ctrl1",
    },
    AllowEntry {
        name: "ROGUE_CR_META_SP_MSLVDATAX",
        why: "META slave port data",
    },
    AllowEntry {
        name: "ROGUE_CR_META_SP_MSLVIRQSTATUS",
        why: "META slave port IRQ status",
    },
    AllowEntry {
        name: "ROGUE_CR_FAULT_FW_STATUS",
        why: "firmware fault status",
    },
    AllowEntry {
        name: "ROGUE_CR_FAULT_FW_CLEAR",
        why: "firmware fault clear",
    },
    AllowEntry {
        name: "ROGUE_CR_SAFETY_EVENT_STATUS__ROGUEXE",
        why: "RoguelXE safety event status",
    },
    AllowEntry {
        name: "ROGUE_CR_SAFETY_EVENT_CLEAR__ROGUEXE",
        why: "RoguelXE safety event clear",
    },
    AllowEntry {
        name: "ROGUE_CR_TE_AA",
        why: "pvr_rogue_cr_defs_client.h; AA mode in geom setup",
    },
    AllowEntry {
        name: "ROGUE_CR_TE_MTILE1",
        why: "client header; macrotile X boundaries",
    },
    AllowEntry {
        name: "ROGUE_CR_TE_MTILE2",
        why: "client header; macrotile Y boundaries",
    },
    AllowEntry {
        name: "ROGUE_CR_TE_SCREEN",
        why: "client header; TE screen in tiles",
    },
    AllowEntry {
        name: "ROGUE_CR_PPP_SCREEN",
        why: "client header; PPP screen in pixels",
    },
    AllowEntry {
        name: "ROGUE_CR_ISP_MTILE_SIZE",
        why: "client header; ISP macrotile size",
    },
];

/// Plain object-like `#define`s lifted out of `pvr_rogue_fwif.h`.
///
/// The struct emitter is driven by the `*_check.h` layout asserts, so a bare
/// `#define` in the same header has no way of reaching `fwif.rs` — the same
/// class of gap that `emit_sf` closes for `stid_fmts[]`. These are firmware ABI
/// constants generated from their pinned source, as required by the component's
/// maintenance conventions. Keep them out of handwritten ABI tables.
///
/// Names are emitted verbatim into `rogue::fwif::defines`, with the
/// `ROGUE_FWIF_` prefix kept so that a value can be grepped straight back to the
/// upstream header. A name that does not parse is an error, not a skip.
pub const FWIF_DEFINE_ALLOW_LIST: &[AllowEntry] = &[
    AllowEntry {
        name: "ROGUE_FWIF_MMUCACHEDATA_FLAGS_PT",
        why: "pvr_mmu.c PVR_MMU_SYNC_LEVEL_0_FLAGS; invalidate page tables",
    },
    AllowEntry {
        name: "ROGUE_FWIF_MMUCACHEDATA_FLAGS_PD",
        why: "pvr_mmu.c PVR_MMU_SYNC_LEVEL_1_FLAGS; invalidate page directories",
    },
    AllowEntry {
        name: "ROGUE_FWIF_MMUCACHEDATA_FLAGS_PC",
        why: "pvr_mmu.c PVR_MMU_SYNC_LEVEL_2_FLAGS; invalidate the page catalogue",
    },
    AllowEntry {
        name: "ROGUE_FWIF_MMUCACHEDATA_FLAGS_PMTLB",
        why: "component of ROGUE_FWIF_MMUCACHEDATA_FLAGS_TLB",
    },
    AllowEntry {
        name: "ROGUE_FWIF_MMUCACHEDATA_FLAGS_TLB",
        why: "pvr_mmu.c PVR_MMU_SYNC_LEVEL_0_FLAGS; invalidate the MMU TLB",
    },
    AllowEntry {
        name: "ROGUE_FWIF_MMUCACHEDATA_FLAGS_INTERRUPT",
        why: "pvr_mmu.c PVR_MMU_SYNC_LEVEL_0_FLAGS; interrupt the host on completion",
    },
];
