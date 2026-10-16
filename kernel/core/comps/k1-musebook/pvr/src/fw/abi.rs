// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: firmware container constants and checked binary layouts.

//! Firmware-interface constants that `pvr-regen` does not emit.
//!
//! Transcribes the `#define`s and `enum`s of `pvr_rogue_fwif.h`,
//! `pvr_rogue_fwif_common.h` and `pvr_rogue_fwif_sf.h` (the scalar parts only —
//! the structures come from [`crate::rogue::fwif`], which is generated).
//!
//! # Why these are hand-written when the structures are not
//!
//! [`crate::rogue::fwif`]'s generator is driven by the `*_check.h` layout-assert
//! headers: it emits exactly the structures upstream asserts about. Those
//! headers say nothing about scalar `#define`s, so a constant like
//! `ROGUE_FWIF_LOG_TYPE_TRACE` has no generator input to key off. The required scalars are
//! transcribed here with their upstream spelling preserved so they grep against
//! the header, and every one carries its upstream file.
//!
//! Anything that *is* generated must not be duplicated here. In particular the
//! MIPS constants live in [`crate::rogue::mips`] and the heap bases in
//! [`crate::rogue::heap`].
//!
//! Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif.h>.

/// Log type: tracing disabled.
///
/// Upstream `ROGUE_FWIF_LOG_TYPE_NONE`.
pub const LOG_TYPE_NONE: u32 = 0x0000_0000;

/// Log type: write entries to the trace buffer.
///
/// Upstream `ROGUE_FWIF_LOG_TYPE_TRACE`. The firmware only writes anything at
/// all when this bit is set *and* at least one group bit is set.
pub const LOG_TYPE_TRACE: u32 = 0x0000_0001;

/// The group bits of a log type.
///
/// Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_MASK`. A group mask with a bit outside
/// this is rejected rather than silently truncated, mirroring upstream's
/// `validate_group_mask()`.
pub const LOG_TYPE_GROUP_MASK: u32 = 0x8000_7ffe;

/// Log group: the main firmware thread. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_MAIN`.
pub const LOG_TYPE_GROUP_MAIN: u32 = 0x0000_0002;
/// Log group: MTS scheduling. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_MTS`.
pub const LOG_TYPE_GROUP_MTS: u32 = 0x0000_0004;
/// Log group: cleanup requests. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_CLEANUP`.
pub const LOG_TYPE_GROUP_CLEANUP: u32 = 0x0000_0008;
/// Log group: context switch. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_CSW`.
pub const LOG_TYPE_GROUP_CSW: u32 = 0x0000_0010;
/// Log group: bus interface. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_BIF`.
pub const LOG_TYPE_GROUP_BIF: u32 = 0x0000_0020;
/// Log group: parameter management. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_PM`.
pub const LOG_TYPE_GROUP_PM: u32 = 0x0000_0040;
/// Log group: render target data. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_RTD`.
pub const LOG_TYPE_GROUP_RTD: u32 = 0x0000_0080;
/// Log group: partial renders. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_SPM`.
pub const LOG_TYPE_GROUP_SPM: u32 = 0x0000_0100;
/// Log group: power management. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_POW`.
pub const LOG_TYPE_GROUP_POW: u32 = 0x0000_0200;
/// Log group: hardware recovery. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_HWR`.
pub const LOG_TYPE_GROUP_HWR: u32 = 0x0000_0400;
/// Log group: hardware performance. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_HWP`.
pub const LOG_TYPE_GROUP_HWP: u32 = 0x0000_0800;
/// Log group: RPM. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_RPM`.
pub const LOG_TYPE_GROUP_RPM: u32 = 0x0000_1000;
/// Log group: DMA. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_DMA`.
pub const LOG_TYPE_GROUP_DMA: u32 = 0x0000_2000;
/// Log group: everything else. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_MISC`.
pub const LOG_TYPE_GROUP_MISC: u32 = 0x0000_4000;
/// Log group: debug. Upstream `ROGUE_FWIF_LOG_TYPE_GROUP_DEBUG`.
pub const LOG_TYPE_GROUP_DEBUG: u32 = 0x8000_0000;

/// Size of one thread's trace buffer, in 32-bit words.
///
/// Upstream `ROGUE_FW_TRACE_BUF_DEFAULT_SIZE_IN_DWORDS`. 12,000 words is
/// 48,000 bytes, so the allocation rounds up to twelve 4 KiB pages.
pub const TRACE_BUF_SIZE_IN_DWORDS: usize = 12_000;

/// Number of firmware threads, and so of trace buffers.
///
/// Upstream `ROGUE_FW_THREAD_MAX`. Pinned to the generated
/// [`crate::rogue::fwif::RogueFwifTracebuf::tracebuf`] array length by a `const`
/// assertion in [`crate::fw::trace`], which is upstream's `BUILD_BUG_ON` in
/// `pvr_fw_trace_init()`.
pub const THREAD_MAX: usize = 2;

/// Marker bits every trace entry's leading word carries.
///
/// Upstream `ROGUE_FW_LOG_IDMARKER`.
pub const LOG_IDMARKER: u32 = 0x7000_0000;

/// Mask selecting the bits [`LOG_IDMARKER`] occupies.
///
/// Upstream `ROGUE_FW_LOG_IDMASK`.
pub const LOG_IDMASK: u32 = 0xfff0_0000;

/// Whether a trace word starts a valid entry.
///
/// Upstream `ROGUE_FW_LOG_VALIDID()`. Zeroed and partially-overwritten words
/// fail this, which is how the reader skips the parts of the ring the firmware
/// has not filled.
pub const fn log_valid_id(word: u32) -> bool {
    word & LOG_IDMASK == LOG_IDMARKER
}

/// Log group of a trace entry id.
///
/// Upstream `ROGUE_FW_SF_GID()`.
pub const fn sf_group_id(id: u32) -> u32 {
    (id >> 12) & 0xf
}

/// Number of parameter words following a trace entry's timestamp.
///
/// Upstream `ROGUE_FW_SF_PARAMNUM()`. Four bits, so at most 15.
pub const fn sf_param_num(id: u32) -> u32 {
    (id >> 16) & 0xf
}

/// Message number within its group.
///
/// The low twelve bits of the id. Not a named upstream macro, but it is the `a`
/// argument of `ROGUE_FW_LOG_CREATESFID(a, b, e)` and therefore the number to
/// look up in `pvr_rogue_fwif_sf.h`.
pub const fn sf_message_id(id: u32) -> u32 {
    id & 0xfff
}

/// Builds a trace entry id the way the firmware does.
///
/// Upstream `ROGUE_FW_LOG_CREATESFID(a, b, e)`.
pub const fn create_sfid(message: u32, group: u32, params: u32) -> u32 {
    message | (group << 12) | (params << 16) | LOG_IDMARKER
}

/// The id the firmware writes when an assertion fires.
///
/// Upstream `ROGUE_FW_SF_MAIN_ASSERT_FAILED`, i.e.
/// `ROGUE_FW_LOG_CREATESFID(24, ROGUE_FW_GROUP_MAIN, 1)`. It terminates the
/// trace: everything after it is stale, and the failing assertion is described
/// by [`crate::rogue::fwif::RogueFwifTracebufSpace::assert_buf`] instead of by
/// parameter words.
pub const SF_MAIN_ASSERT_FAILED: u32 = create_sfid(24, LOG_GROUP_MAIN, 1);

/// The placeholder first entry of upstream's format-string table.
///
/// Upstream `ROGUE_FW_SF_FIRST`, i.e. `ROGUE_FW_LOG_CREATESFID(0, NULL, 0)`.
/// Skipped rather than reported.
pub const SF_FIRST: u32 = create_sfid(0, LOG_GROUP_NULL, 0);

/// Group ordinal `ROGUE_FW_GROUP_NULL`.
pub const LOG_GROUP_NULL: u32 = 0;
/// Group ordinal `ROGUE_FW_GROUP_MAIN`.
pub const LOG_GROUP_MAIN: u32 = 1;
/// Group ordinal `ROGUE_FW_GROUP_CLEANUP`.
pub const LOG_GROUP_CLEANUP: u32 = 2;
/// Group ordinal `ROGUE_FW_GROUP_CSW`.
pub const LOG_GROUP_CSW: u32 = 3;
/// Group ordinal `ROGUE_FW_GROUP_PM`.
pub const LOG_GROUP_PM: u32 = 4;
/// Group ordinal `ROGUE_FW_GROUP_RTD`.
pub const LOG_GROUP_RTD: u32 = 5;
/// Group ordinal `ROGUE_FW_GROUP_SPM`.
pub const LOG_GROUP_SPM: u32 = 6;
/// Group ordinal `ROGUE_FW_GROUP_MTS`.
pub const LOG_GROUP_MTS: u32 = 7;
/// Group ordinal `ROGUE_FW_GROUP_BIF`.
pub const LOG_GROUP_BIF: u32 = 8;
/// Group ordinal `ROGUE_FW_GROUP_MISC`.
pub const LOG_GROUP_MISC: u32 = 9;
/// Group ordinal `ROGUE_FW_GROUP_POW`.
pub const LOG_GROUP_POW: u32 = 10;
/// Group ordinal `ROGUE_FW_GROUP_HWR`.
pub const LOG_GROUP_HWR: u32 = 11;
/// Group ordinal `ROGUE_FW_GROUP_HWP`.
pub const LOG_GROUP_HWP: u32 = 12;
/// Group ordinal `ROGUE_FW_GROUP_RPM`.
pub const LOG_GROUP_RPM: u32 = 13;
/// Group ordinal `ROGUE_FW_GROUP_DMA`.
pub const LOG_GROUP_DMA: u32 = 14;
/// Group ordinal `ROGUE_FW_GROUP_DBG`.
pub const LOG_GROUP_DBG: u32 = 15;

/// Short names for the sixteen log groups, indexed by [`sf_group_id`].
///
/// Transcribed from upstream `enum rogue_fw_log_sfgroups`, whose order is ABI:
/// the header warns that rearranging it breaks compatibility with existing
/// firmware.
pub const LOG_GROUP_NAMES: [&str; 16] = [
    "NULL", "MAIN", "CLEANUP", "CSW", "PM", "RTD", "SPM", "MTS", "BIF", "MISC", "POW", "HWR",
    "HWP", "RPM", "DMA", "DBG",
];

/// Shift of the time field within a trace entry's 64-bit timestamp.
///
/// Upstream `ROGUE_FWT_TIMESTAMP_TIME_SHIFT`.
pub const FWT_TIMESTAMP_TIME_SHIFT: u32 = 0;

/// Clear-mask of the time field within a trace entry's 64-bit timestamp.
///
/// Upstream `ROGUE_FWT_TIMESTAMP_TIME_CLRMSK`. The top sixteen bits carry debug
/// info rather than time.
pub const FWT_TIMESTAMP_TIME_CLRMSK: u64 = 0xffff_0000_0000_0000;

/// Disable overlapping work across data masters.
///
/// Upstream `ROGUE_FWIF_INICFG_DISABLE_DM_OVERLAP`, `BIT(19)`. Set when the SLC
/// is too small for two data masters to share it.
pub const INICFG_DISABLE_DM_OVERLAP: u32 = 1 << 19;

/// SLC size below which overlapping data masters are disabled, in kilobytes.
///
/// Upstream `ROGUE_FWIF_SLC_MIN_SIZE_FOR_DM_OVERLAP_KB`, in `pvr_fw.c`.
pub const SLC_MIN_SIZE_FOR_DM_OVERLAP_KB: u32 = 4;

/// No GPIO validation.
///
/// Upstream `ROGUE_FWIF_GPIO_VAL_OFF`. The only mode a production driver uses.
pub const GPIO_VAL_OFF: u32 = 0;

/// No firmware performance counters.
///
/// Upstream `FW_PERF_CONF_NONE`.
pub const FW_PERF_CONF_NONE: u32 = 0;

/// The GPU is idle.
///
/// Upstream `PVR_FWIF_GPU_UTIL_STATE_IDLE`, in `pvr_rogue_fwif_common.h`.
pub const GPU_UTIL_STATE_IDLE: u64 = 0;

/// The host's side of the connection is offline.
///
/// Upstream `ROGUE_FW_CONNECTION_OS_OFFLINE`.
pub const CONNECTION_OS_OFFLINE: u32 = 0;

/// The host's KM driver is set up and waiting.
///
/// Upstream `ROGUE_FW_CONNECTION_OS_READY`.
pub const CONNECTION_OS_READY: u32 = 1;

/// The firmware is offline.
///
/// Upstream `ROGUE_FW_CONNECTION_FW_OFFLINE`.
pub const CONNECTION_FW_OFFLINE: u32 = 0;

/// The firmware has finished initialising.
///
/// Upstream `ROGUE_FW_CONNECTION_FW_READY`. Written by the firmware, read by the
/// host; a firmware that boots but never reaches this is the "died during init"
/// case [`crate::fw::BootDiagnosis`] names.
pub const CONNECTION_FW_READY: u32 = 1;

/// Size of the firmware's fault page.
///
/// Upstream `PVR_ROGUE_FAULT_PAGE_SIZE`, in `pvr_fw.c`.
pub const FAULT_PAGE_SIZE: usize = 4096;

/// Fill word of the fault page.
///
/// Upstream `fw_fault_page_init()` writes this to every word. Any GPU access
/// that lands on the fault page therefore reads a recognisable pattern rather
/// than plausible data, which is the point.
pub const FAULT_PAGE_FILL: u32 = 0xdead_bee0;

/// How long to wait for the firmware to report that it has booted.
///
/// Upstream `FW_BOOT_TIMEOUT_USEC`, in `pvr_fw.c`. Five seconds.
pub const FW_BOOT_TIMEOUT_USEC: u64 = 5_000_000;

/// How long to wait for any single register poll in the start/stop sequences.
///
/// Upstream `POLL_TIMEOUT_USEC`, in `pvr_fw_startstop.c`. One second.
pub const POLL_TIMEOUT_USEC: u64 = 1_000_000;

/// Size of one KCCB return slot, and of the power sync object.
///
/// Upstream `PVR_SYNC_OBJ_SIZE`, in `pvr_fw.c`.
pub const SYNC_OBJ_SIZE: usize = size_of::<u32>();
