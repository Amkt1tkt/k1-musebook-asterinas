// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: firmware command layouts with compile-time checks.

//! Firmware ABI the submission path needs and `pvr-regen` does not emit.
//!
//! # Why these are hand-written
//!
//! Same reason as [`crate::fw::abi`], which is the precedent this file follows.
//! [`crate::rogue::fwif`]'s generator is driven by the `*_check.h` layout-assert
//! headers, so it emits exactly the structures upstream asserts about and
//! nothing else. Two kinds of thing therefore have no generator input:
//!
//! - **Command-type and data-master enumerators.** Upstream spells the CCB
//!   command types as `#define`s and the KCCB/FWCCB ones as `enum` constants
//!   whose initialisers are `#define` arithmetic. `pvr-regen`'s
//!   `FWIF_DEFINE_ALLOW_LIST` handles object-like `#define`s only, so an `enum`
//!   body is out of its reach.
//! - **The three per-context firmware structures.** `rogue_fwif_fwrendercontext`
//!   and its compute and transfer siblings appear in no `*_check.h` file, so
//!   upstream itself has no layout assertions for them. They are written out
//!   below with assertions derived by hand from the C definitions —
//!   [`CONVENTIONS.md`](../../../CONVENTIONS.md) layout contract applies to the port
//!   whether or not upstream got there first, and these three are exactly the
//!   objects the firmware scheduler reads for every kick.
//!
//! Every name keeps its upstream spelling so a value greps straight back to the
//! header. Nothing here duplicates [`crate::fw::abi`] or
//! [`crate::rogue::fwif::defines`].
//!
//! Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif.h>,
//! <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif_common.h>,
//! <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif_shared.h>
//! and <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_fw.h>.

use core::mem::offset_of;

use crate::rogue::fwif::{
    RogueFwifFwcommoncontext, RogueFwifStaticComputecontextState, RogueFwifStaticRendercontextState,
};

/// Magic value every CCB command type carries in its top sixteen bits.
///
/// Upstream `ROGUE_CMD_MAGIC_DWORD`, shifted by `ROGUE_CMD_MAGIC_DWORD_SHIFT`
/// into `ROGUE_CMD_MAGIC_DWORD_SHIFTED`. A command word that does not carry it
/// is corruption rather than an unknown command, which is why
/// [`crate::sched::ccb::process_fwccb`] separates the two cases.
pub const CMD_MAGIC_DWORD_SHIFTED: u32 = 0x2abc << 16;

/// Mask selecting the bits [`CMD_MAGIC_DWORD_SHIFTED`] occupies.
///
/// Upstream `ROGUE_CMD_MAGIC_DWORD_MASK`.
pub const CMD_MAGIC_DWORD_MASK: u32 = 0xffff_0000;

/// Marks a CCB command as a task the firmware's scheduler runs.
///
/// Upstream `ROGUE_CCB_TYPE_TASK`, `BIT(15)`. Set on the per-data-master
/// commands and deliberately clear on the fence, update and padding commands.
pub const CCB_TYPE_TASK: u32 = 1 << 15;

/// Builds a CCB command type the way the upstream `#define`s do.
const fn ccb_cmd(number: u32, task: bool) -> u32 {
    number | CMD_MAGIC_DWORD_SHIFTED | if task { CCB_TYPE_TASK } else { 0 }
}

/// Client CCB command: run a geometry job. Upstream `ROGUE_FWIF_CCB_CMD_TYPE_GEOM`.
pub const CCB_CMD_TYPE_GEOM: u32 = ccb_cmd(201, true);
/// Client CCB command: run a 3D transfer job. Upstream `ROGUE_FWIF_CCB_CMD_TYPE_TQ_3D`.
pub const CCB_CMD_TYPE_TQ_3D: u32 = ccb_cmd(202, true);
/// Client CCB command: run a fragment job. Upstream `ROGUE_FWIF_CCB_CMD_TYPE_FRAG`.
pub const CCB_CMD_TYPE_FRAG: u32 = ccb_cmd(203, true);
/// Client CCB command: run a partial render. Upstream `ROGUE_FWIF_CCB_CMD_TYPE_FRAG_PR`.
pub const CCB_CMD_TYPE_FRAG_PR: u32 = ccb_cmd(204, true);
/// Client CCB command: run a compute job. Upstream `ROGUE_FWIF_CCB_CMD_TYPE_CDM`.
pub const CCB_CMD_TYPE_CDM: u32 = ccb_cmd(205, true);

/// Client CCB command: wait for UFOs, allowing a partial render meanwhile.
///
/// Upstream `ROGUE_FWIF_CCB_CMD_TYPE_FENCE_PR`. This is the command that makes a
/// job-to-job dependency the firmware's problem rather than the driver's: the
/// payload is an array of [`crate::rogue::fwif::RogueFwifUfo`], and the firmware
/// does not start the following command until every one of them has reached its
/// value.
pub const CCB_CMD_TYPE_FENCE_PR: u32 = ccb_cmd(215, false);

/// Client CCB command: write a UFO value. Upstream `ROGUE_FWIF_CCB_CMD_TYPE_UPDATE`.
///
/// The firmware runs this after the job ahead of it completes, which is how a
/// queue's timeline UFO advances and therefore how [`crate::sched::queue::Queue`]
/// learns that a job is done.
pub const CCB_CMD_TYPE_UPDATE: u32 = ccb_cmd(213, false);

/// Client CCB command: ignore this many bytes and wrap.
///
/// Upstream `ROGUE_FWIF_CCB_CMD_TYPE_PADDING`. Written when the next command
/// sequence would not fit in the tail of the ring; see
/// [`crate::sched::cccb`].
pub const CCB_CMD_TYPE_PADDING: u32 = ccb_cmd(221, false);

/// Largest number of UFOs one fence or update command may carry.
///
/// Upstream `ROGUE_FWIF_CCB_CMD_MAX_UFOS`, `(32U + 32U)`, in
/// `pvr_rogue_fwif_shared.h`. A dependency set longer than this is split across
/// several commands rather than truncated.
pub const CCB_CMD_MAX_UFOS: usize = 32 + 32;

/// Kernel CCB command: a client CCB has work in it. Upstream `ROGUE_FWIF_KCCB_CMD_KICK`.
pub const KCCB_CMD_KICK: u32 = 101 | CMD_MAGIC_DWORD_SHIFTED;
/// Kernel CCB command: invalidate MMU caches. Upstream `ROGUE_FWIF_KCCB_CMD_MMUCACHE`.
pub const KCCB_CMD_MMUCACHE: u32 = 102 | CMD_MAGIC_DWORD_SHIFTED;
/// Kernel CCB command: release a firmware structure. Upstream `ROGUE_FWIF_KCCB_CMD_CLEANUP`.
pub const KCCB_CMD_CLEANUP: u32 = 106 | CMD_MAGIC_DWORD_SHIFTED;
/// Kernel CCB command requesting that firmware reread the trace group mask.
///
/// Upstream `ROGUE_FWIF_KCCB_CMD_LOGTYPE_UPDATE`. A shared-memory mask write
/// alone does not notify a running firmware.
pub const KCCB_CMD_LOGTYPE_UPDATE: u32 = 206 | CMD_MAGIC_DWORD_SHIFTED;
/// Kernel CCB command: kick a paired geometry and fragment job together.
///
/// Upstream `ROGUE_FWIF_KCCB_CMD_COMBINED_GEOM_FRAG_KICK`. One command carrying
/// two rings, which is what leaves the firmware holding a fragment job it can run
/// when the Parameter Manager runs out of memory part-way through the geometry
/// job. Sent by [`crate::sched::queue::Queue::submit_pair`]; which jobs qualify is
/// [`crate::sched::job`]'s decision, and what it does to the queues is FIFO contract in
/// [`crate::sched::queue`].
pub const KCCB_CMD_COMBINED_GEOM_FRAG_KICK: u32 = 117 | CMD_MAGIC_DWORD_SHIFTED;
/// Kernel CCB command: a power request. Upstream `ROGUE_FWIF_KCCB_CMD_POW`.
///
/// Carries [`crate::rogue::fwif::RogueFwifPowerRequest`]. Sent by
/// [`crate::power`] on the way down, before the firmware core is stopped.
pub const KCCB_CMD_POW: u32 = 107 | CMD_MAGIC_DWORD_SHIFTED;

/// Power request: power the GPU off. Upstream `ROGUE_FWIF_POW_OFF_REQ`.
///
/// First enumerator of `enum rogue_fwif_power_type`, explicitly `= 1`.
pub const POW_OFF_REQ: u32 = 1;
/// Power request: force the firmware idle. Upstream `ROGUE_FWIF_POW_FORCED_IDLE_REQ`.
pub const POW_FORCED_IDLE_REQ: u32 = 2;
/// Forced-idle request kind. Upstream `ROGUE_FWIF_POWER_FORCE_IDLE`.
///
/// First enumerator of `enum rogue_fwif_power_force_idle_type`, explicitly `= 1`.
pub const POWER_FORCE_IDLE: u32 = 1;

/// Firmware power state: idle and ready to be powered down completely.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif.h>
/// (`enum rogue_fwif_pow_state`, lines 105-110).
pub const POW_STATE_OFF: u32 = 0;
/// Firmware power state: running hardware commands.
pub const POW_STATE_ON: u32 = 1;
/// Firmware power state: forced idle by the host.
pub const POW_STATE_FORCED_IDLE: u32 = 2;
/// Firmware power state: idle, waiting for the host to acknowledge.
///
/// Upstream `pvr_power_is_idle()` requires this state before suspend, rather
/// than [`POW_STATE_OFF`]. Follow the protocol check, not an inference from
/// the two enum names.
pub const POW_STATE_IDLE: u32 = 3;

/// Names a firmware power state, for diagnostics.
pub fn pow_state_name(pow_state: u32) -> &'static str {
    match pow_state {
        POW_STATE_OFF => "OFF",
        POW_STATE_ON => "ON",
        POW_STATE_FORCED_IDLE => "FORCED_IDLE",
        POW_STATE_IDLE => "IDLE",
        _ => "unknown",
    }
}

/// Names a kernel CCB command type, for the hang report.
///
/// Three answers rather than two, because they point at three different faults.
/// A recognised name means the firmware is refusing a command this driver
/// legitimately sends. `"not a command"` means the word does not even carry
/// [`CMD_MAGIC_DWORD_SHIFTED`], so the slot is uninitialised or corrupt and the
/// question is how the ring got into that state — the same distinction
/// [`crate::sched::ccb::process_fwccb`] makes in the other direction. An
/// unrecognised number *with* the magic is a command this port does not know it
/// can send, which would mean the write offset moved without going through
/// [`crate::sched::ccb::send`].
pub fn kccb_cmd_name(cmd_type: u32) -> &'static str {
    match cmd_type {
        KCCB_CMD_KICK => "KICK",
        KCCB_CMD_MMUCACHE => "MMUCACHE",
        KCCB_CMD_CLEANUP => "CLEANUP",
        KCCB_CMD_POW => "POW",
        KCCB_CMD_LOGTYPE_UPDATE => "LOGTYPE_UPDATE",
        KCCB_CMD_COMBINED_GEOM_FRAG_KICK => "COMBINED_GEOM_FRAG_KICK",
        _ if cmd_type & CMD_MAGIC_DWORD_MASK != CMD_MAGIC_DWORD_SHIFTED => "not a command",
        _ => "unknown command",
    }
}

/// Firmware-to-host command numbers from `enum rogue_fwif_fwccb_cmd_type`.
///
/// The enum has gaps; command numbers must not be inferred from handler order.
/// [`fwccb_numbers`] checks them against independently transcribed header values.
/// Misdecoding a notification can request an unrelated reset or lose a fault report.
///
/// Reference: `pvr_rogue_fwif.h`, `enum rogue_fwif_fwccb_cmd_type`.
pub const FWCCB_CMD_ZSBUFFER_BACKING: u32 = 101 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: a Z/S buffer may be unbacked.
pub const FWCCB_CMD_ZSBUFFER_UNBACKING: u32 = 102 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: a free list needs growing.
pub const FWCCB_CMD_FREELIST_GROW: u32 = 103 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: free lists must be reconstructed.
pub const FWCCB_CMD_FREELISTS_RECONSTRUCTION: u32 = 104 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: a context was reset after a hardware-recovery event.
pub const FWCCB_CMD_CONTEXT_RESET_NOTIFICATION: u32 = 105 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: dump firmware state.
pub const FWCCB_CMD_DEBUG_DUMP: u32 = 106 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: process statistics.
pub const FWCCB_CMD_UPDATE_STATS: u32 = 107 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: the core clock rate changed.
pub const FWCCB_CMD_CORE_CLK_RATE_CHANGE: u32 = 108 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: the firmware wants the GPU reset.
pub const FWCCB_CMD_REQUEST_GPU_RESTART: u32 = 109 | CMD_MAGIC_DWORD_SHIFTED;
/// Firmware CCB command: the firmware processor took a page fault.
///
/// The payload is [`crate::rogue::fwif::RogueFwifFwccbCmdFwPagefaultData`], one
/// `u64` naming the firmware virtual address it faulted on — which is the
/// address to look up in the MIPS page table.
pub const FWCCB_CMD_CONTEXT_FW_PF_NOTIFICATION: u32 = 112 | CMD_MAGIC_DWORD_SHIFTED;

/// Checks command numbers against independently transcribed header values.
///
/// The right-hand sides come from `enum rogue_fwif_fwccb_cmd_type`, rather than
/// being derived from the constants being tested.
const _: () = {
    assert!(FWCCB_CMD_ZSBUFFER_BACKING == 101 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_ZSBUFFER_UNBACKING == 102 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_FREELIST_GROW == 103 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_FREELISTS_RECONSTRUCTION == 104 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_CONTEXT_RESET_NOTIFICATION == 105 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_DEBUG_DUMP == 106 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_UPDATE_STATS == 107 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_CORE_CLK_RATE_CHANGE == 108 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_REQUEST_GPU_RESTART == 109 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(FWCCB_CMD_CONTEXT_FW_PF_NOTIFICATION == 112 | CMD_MAGIC_DWORD_SHIFTED);

    // Check host-to-firmware command numbers independently of FWCCB numbers.
    assert!(KCCB_CMD_KICK == 101 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(KCCB_CMD_MMUCACHE == 102 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(KCCB_CMD_CLEANUP == 106 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(KCCB_CMD_POW == 107 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(KCCB_CMD_COMBINED_GEOM_FRAG_KICK == 117 | CMD_MAGIC_DWORD_SHIFTED);
    assert!(KCCB_CMD_LOGTYPE_UPDATE == 206 | CMD_MAGIC_DWORD_SHIFTED);

    // `enum rogue_fwif_power_type` / `enum rogue_fwif_power_force_idle_type`.
    // Right-hand values are transcribed from the header independently of the constants.
    assert!(POW_OFF_REQ == 1);
    assert!(POW_FORCED_IDLE_REQ == 2);
    assert!(POWER_FORCE_IDLE == 1);
};

/// Hardware-recovery state: the firmware processor itself faulted.
///
/// Upstream `ROGUE_FWIF_HWR_FW_FAULT`, `BIT(6)`, one of the two bits a soft
/// reset clears in `rogue_fwif_sysdata::hwr_state_flags`.
pub const HWR_FW_FAULT: u32 = 1 << 6;

/// Hardware-recovery state: the firmware has asked for a restart.
///
/// Upstream `ROGUE_FWIF_HWR_RESTART_REQUESTED`, `BIT(7)`. Set by the firmware
/// alongside the `REQUEST_GPU_RESTART` command in the FWCCB, and cleared by the
/// soft reset that answers it — see [`crate::power::run_reset`].
pub const HWR_RESTART_REQUESTED: u32 = 1 << 7;

/// Cleanup request: release a firmware common context.
///
/// Upstream `ROGUE_FWIF_CLEANUP_FWCOMMONCONTEXT`, the first enumerator of
/// `enum rogue_fwif_cleanup_type`.
pub const CLEANUP_FWCOMMONCONTEXT: u32 = 0;

/// KCCB return slot: the firmware ran the command. Upstream
/// `ROGUE_FWIF_KCCB_RTN_SLOT_CMD_EXECUTED`.
pub const KCCB_RTN_SLOT_CMD_EXECUTED: u32 = 1 << 0;
/// KCCB return slot: the object was still in use. Upstream
/// `ROGUE_FWIF_KCCB_RTN_SLOT_CLEANUP_BUSY`.
pub const KCCB_RTN_SLOT_CLEANUP_BUSY: u32 = 1 << 1;
/// KCCB return slot: the firmware's own poll timed out. Upstream
/// `ROGUE_FWIF_KCCB_RTN_SLOT_POLL_FAILURE`.
pub const KCCB_RTN_SLOT_POLL_FAILURE: u32 = 1 << 2;
/// KCCB return slot: nothing written yet. Upstream
/// `ROGUE_FWIF_KCCB_RTN_SLOT_NO_RESPONSE`.
pub const KCCB_RTN_SLOT_NO_RESPONSE: u32 = 0;

/// Log base two of the number of KCCB slots.
///
/// Upstream `ROGUE_FWIF_KCCB_NUMCMDS_LOG2_DEFAULT`, in `pvr_fw.h`.
pub const KCCB_NUMCMDS_LOG2: u32 = 7;

/// Log base two of the number of FWCCB slots.
///
/// Upstream `ROGUE_FWIF_FWCCB_NUMCMDS_LOG2`, in `pvr_fw.h`.
pub const FWCCB_NUMCMDS_LOG2: u32 = 5;

/// Data master: the general-purpose one, which runs KCCB commands.
///
/// Upstream `PVR_FWIF_DM_GP`, in `pvr_rogue_fwif_common.h`.
pub const FWIF_DM_GP: u32 = 0;
/// Data master: geometry. Upstream `PVR_FWIF_DM_GEOM`.
pub const FWIF_DM_GEOM: u32 = 2;
/// Data master: fragment. Upstream `PVR_FWIF_DM_FRAG`.
pub const FWIF_DM_FRAG: u32 = 3;
/// Data master: compute. Upstream `PVR_FWIF_DM_CDM`.
pub const FWIF_DM_CDM: u32 = 4;

/// Version of the BVNC compatibility block's layout.
///
/// Upstream `ROGUE_FWIF_COMPCHECKS_LAYOUT_VERSION`. The header warns that any
/// change to `struct rogue_fwif_compchecks_bvnc` must bump it, and the firmware
/// refuses a block whose version it does not know — which is the whole point of
/// `rogue_fwif_compchecks_bvnc_init()` writing it before the firmware boots.
pub const COMPCHECKS_LAYOUT_VERSION: u32 = 3;

/// Firmware hardware-recovery deadline for a context, in milliseconds.
///
/// Upstream `MAX_DEADLINE_MS`, in `pvr_queue.c`. This is the firmware's own
/// watchdog for the context, independent of the host-side timeout in
/// [`crate::sched::queue`].
pub const MAX_DEADLINE_MS: u32 = 30_000;

/// Log base two of a client CCB's size in bytes.
///
/// Upstream uses one value for all four job types —
/// `CTX_{GEOM,FRAG,COMPUTE,TRANSFER}_CCCB_SIZE_LOG2`, all 15 — so this port
/// keeps one constant rather than four identical ones.
pub const CCCB_SIZE_LOG2: u32 = 15;

/// The firmware render context, upstream `struct rogue_fwif_fwrendercontext`.
///
/// One geometry and one fragment common context, so a render context feeds two
/// queues. Padding is explicit because `CONVENTIONS.md` layout contract forbids letting
/// Rust choose it.
#[repr(C, align(8))]
#[derive(Clone, Copy, Pod)]
pub struct RogueFwifFwrendercontext {
    /// Geometry firmware context.
    pub geom_context: RogueFwifFwcommoncontext,
    /// Fragment firmware context.
    pub frag_context: RogueFwifFwcommoncontext,
    /// Context-switch register state, from the client's static state stream.
    pub static_render_context_state: RogueFwifStaticRendercontextState,
    /// Commands submitted to the workload-estimation CCB. Unused by this port.
    pub work_est_ccb_submitted: u32,
    /// Compatibility and other flags.
    pub fw_render_ctx_flags: u32,
}

/// The firmware compute context, upstream `struct rogue_fwif_fwcomputecontext`.
#[repr(C, align(8))]
#[derive(Clone, Copy, Pod)]
pub struct RogueFwifFwcomputecontext {
    /// Firmware context for the CDM.
    pub cdm_context: RogueFwifFwcommoncontext,
    /// Context-switch register state, from the client's static state stream.
    pub static_compute_context_state: RogueFwifStaticComputecontextState,
    /// Commands submitted to the workload-estimation CCB. Unused by this port.
    pub work_est_ccb_submitted: u32,
    /// Compatibility and other flags.
    pub compute_ctx_flags: u32,
    /// Workgroup protection state.
    pub wgp_state: u32,
    /// Workgroup protection checksum.
    pub wgp_checksum: u32,
    /// First half of the core mask, for multicore GPUs.
    pub core_mask_a: u32,
    /// Second half of the core mask, for multicore GPUs.
    pub core_mask_b: u32,
}

/// The firmware transfer context, upstream `struct rogue_fwif_fwtransfercontext`.
///
/// Upstream's `rogue_fwif_fwtdmcontext` is the TDM sibling of this structure and
/// is deliberately absent: `DRM_PVR_CTX_TYPE_TRANSFER_FRAG` is the only transfer
/// context type the uapi exposes, and it runs on the fragment data master.
#[repr(C, align(8))]
#[derive(Clone, Copy, Pod)]
pub struct RogueFwifFwtransfercontext {
    /// Firmware context for TQ3D.
    pub tq_context: RogueFwifFwcommoncontext,
}

/// Layout assertions for the three structures above.
///
/// Upstream has no `*_check.h` entries for these, so unlike every other
/// firmware-shared structure in this port the numbers are derived from the C
/// definitions rather than transcribed from upstream's own assertions. They are
/// still assertions rather than comments: a field inserted into
/// [`RogueFwifFwcommoncontext`] by a regeneration would otherwise move
/// `frag_context` silently, and the symptom would be a firmware that accepts
/// kicks and renders nothing.
const _: () = {
    assert!(size_of::<RogueFwifFwcommoncontext>() == 152);

    assert!(offset_of!(RogueFwifFwrendercontext, geom_context) == 0);
    assert!(offset_of!(RogueFwifFwrendercontext, frag_context) == 152);
    assert!(offset_of!(RogueFwifFwrendercontext, static_render_context_state) == 304);
    assert!(offset_of!(RogueFwifFwrendercontext, work_est_ccb_submitted) == 672);
    assert!(offset_of!(RogueFwifFwrendercontext, fw_render_ctx_flags) == 676);
    assert!(size_of::<RogueFwifFwrendercontext>() == 680);

    assert!(offset_of!(RogueFwifFwcomputecontext, cdm_context) == 0);
    assert!(offset_of!(RogueFwifFwcomputecontext, static_compute_context_state) == 152);
    assert!(offset_of!(RogueFwifFwcomputecontext, work_est_ccb_submitted) == 208);
    assert!(offset_of!(RogueFwifFwcomputecontext, core_mask_b) == 228);
    assert!(size_of::<RogueFwifFwcomputecontext>() == 232);

    assert!(offset_of!(RogueFwifFwtransfercontext, tq_context) == 0);
    assert!(size_of::<RogueFwifFwtransfercontext>() == 152);
};
