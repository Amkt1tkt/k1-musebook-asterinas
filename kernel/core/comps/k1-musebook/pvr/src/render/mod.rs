// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR render resources and limits.

//! Tile-based rendering resources.
//!
//! Ports `pvr_free_list.c`, `pvr_hwrt.c` and their headers (1,531 lines).
//!
//! PowerVR is a tile-based deferred renderer, which means geometry results are
//! written into a parameter buffer and consumed by the fragment pass. Two
//! resources exist only to serve that: a [`free_list`] the firmware grows the
//! parameter buffer from, and a [`hwrt`] dataset describing a render target.
//!
//! Both are created by explicit ioctls, so Mesa cannot render a single triangle
//! without them — they are on the critical path for a working GPU, not optional
//! extras.
//!
//! # The two of them together
//!
//! A dataset names two free lists — a *local* one and a *global* one — and holds
//! two render-target data structures so that one frame's fragment pass can
//! overlap the next frame's geometry pass. The coupling runs in both directions
//! and both directions matter:
//!
//! - a dataset holds a strong reference to its free lists, because the firmware
//!   will read them for as long as the dataset exists;
//! - the *local* free list holds weak references back to the dataset's render
//!   target data, because a reconstruction has to reset every render target that
//!   was drawing from it. Weak, because a strong reference either way round
//!   would be a cycle and neither object would ever be freed.
//!
//! [`free_list`]: self::free_list
//! [`hwrt`]: self::hwrt

pub mod free_list;
pub mod hwrt;

#[cfg(ktest)]
mod test;

/// Firmware ABI used only by render-resource management.
///
/// The layout-header generator does not parse enum bodies, so these constants
/// are transcribed with their upstream names.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_fwif.h>.
pub mod abi {
    use crate::sched::abi::CMD_MAGIC_DWORD_SHIFTED;

    /// Cleanup request: release a hardware render target's data.
    ///
    /// Upstream `ROGUE_FWIF_CLEANUP_HWRTDATA`, the second enumerator of
    /// `enum rogue_fwif_cleanup_type`.
    pub const CLEANUP_HWRTDATA: u32 = 1;
    /// Cleanup request: release a free list.
    ///
    /// Upstream `ROGUE_FWIF_CLEANUP_FREELIST`, the third enumerator of the same
    /// enumeration. [`crate::sched::abi::CLEANUP_FWCOMMONCONTEXT`] is the first.
    pub const CLEANUP_FREELIST: u32 = 2;

    /// Kernel CCB command: a free-list grow request has been answered.
    ///
    /// Upstream `ROGUE_FWIF_KCCB_CMD_FREELIST_GROW_UPDATE`.
    pub const KCCB_CMD_FREELIST_GROW_UPDATE: u32 = 110 | CMD_MAGIC_DWORD_SHIFTED;
    /// Kernel CCB command: a free-list reconstruction request has been answered.
    ///
    /// Upstream `ROGUE_FWIF_KCCB_CMD_FREELISTS_RECONSTRUCTION_UPDATE`.
    pub const KCCB_CMD_FREELISTS_RECONSTRUCTION_UPDATE: u32 = 112 | CMD_MAGIC_DWORD_SHIFTED;

    /// Render-target data state: the firmware lost the parameter buffer.
    ///
    /// Upstream `ROGUE_FWIF_RTDATA_STATE_HWR`, the tenth enumerator of
    /// `enum rogue_fwif_rtdata_state`. Set rather than `..._NONE` after a
    /// reconstruction, because `NONE` would make the next geometry job look like
    /// the first one for this target and the firmware would not skip the
    /// abandoned ones.
    pub const RTDATA_STATE_HWR: u32 = 9;

    /// HWRT data flag: a geometry pass has run against this target.
    ///
    /// Upstream `HWRTDATA_HAS_LAST_GEOM`, `BIT(2)`.
    pub const HWRTDATA_HAS_LAST_GEOM: u32 = 1 << 2;

    /// Number of render-target data structures a dataset holds.
    ///
    /// Upstream `ROGUE_FWIF_NUM_RTDATAS`, which it also `static_assert`s equal
    /// to two in `pvr_hwrt.c`; the uapi's `rt_data_args` array is sized by the
    /// same number, so a change would be an ABI break rather than a tuning knob.
    pub const NUM_RTDATAS: usize = 2;

    /// Number of free lists a render-target data structure names.
    ///
    /// Upstream `ROGUE_FWIF_NUM_RTDATA_FREELISTS`. Index
    /// [`crate::uapi::HWRT_FREE_LIST_LOCAL`] is upstream's
    /// `ROGUE_FW_LOCAL_FREELIST` and [`crate::uapi::HWRT_FREE_LIST_GLOBAL`] its
    /// `ROGUE_FW_GLOBAL_FREELIST`.
    pub const NUM_RTDATA_FREELISTS: usize = 2;
}
