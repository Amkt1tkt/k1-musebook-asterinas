// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: GPU scheduling interfaces and timeout clock.

//! Command submission.
//!
//! Ports `pvr_ccb.c`, `pvr_cccb.c`, `pvr_context.c`, `pvr_job.c`, `pvr_queue.c`,
//! `pvr_stream.c`, and `pvr_stream_defs.c`.
//!
//! KCCB and FWCCB are the kernel/firmware command channels. Client CCCBs are
//! per-context, per-job-type rings carrying rendering commands. A submission
//! validates its streams, resolves synchronization, writes the client rings,
//! then notifies firmware through the KCCB.
//!
//! [`queue`] supplies FIFO ordering, dependency tracking, and timeout handling
//! over `k1-musebook-drm` fences. Its module documentation defines the locking
//! and completion contracts.
//!
//! Initialization order is required by the firmware protocol:
//!
//! 1. [`crate::fw::FwDevice::load`] allocates firmware memory, including `osinit`.
//! 2. [`ccb::init`] creates and maps both command rings and the return slots.
//! 3. [`crate::fw::FwDevice::publish_ccb_addresses`] publishes their addresses and
//!    the BVNC compatibility blocks.
//! 4. `crate::fw::boot` starts firmware.
//!
//! [`init`] performs steps 2 and 3 before the firmware can consume `osinit`.

use ostd::timer::Jiffies;

pub mod abi;
pub mod ccb;
pub mod cccb;
pub mod context;
pub mod job;
pub mod queue;
pub mod stream;

#[cfg(ktest)]
mod test;

use crate::{device::PvrDevice, error::PvrResult};

/// Monotonic time since boot, in nanoseconds.
///
/// One definition for the whole submission path, so that a timeout and the
/// timestamp it is compared against cannot come from two different clocks.
///
/// Resolution is one timer tick, as exposed by Jiffies. The four-second
/// job timeout uses this clock to bound stalls, not to profile GPU execution.
/// DRM's Blocker::now_ns uses the same counter.
///
/// [`Blocker::now_ns`]: k1_musebook_drm::fence::Blocker::now_ns
pub fn now_ns() -> u64 {
    u64::try_from(Jiffies::elapsed().as_duration().as_nanos()).unwrap_or(u64::MAX)
}

/// Creates the kernel/firmware command buffers and tells the firmware about them.
///
/// Ports the `pvr_kccb_init()`, `pvr_fwccb_init()` and `fw_osinit_init()` part of
/// upstream `pvr_device_gpu_init()`. Must run after
/// [`crate::fw::FwDevice::load`] and before `crate::fw::boot`; see the module
/// documentation for why.
///
/// Installing twice is [`crate::PvrError::Busy`].
pub fn init(device: &PvrDevice) -> PvrResult<()> {
    ccb::init(device)?;

    let fw = device.fw.get().ok_or(crate::PvrError::Unsupported)?;
    fw.publish_ccb_addresses(device)?;

    ostd::info!("submission path ready: KCCB, FWCCB and osinit are populated");
    Ok(())
}

/// Puts the submission path back where [`init`] left it, after a hard reset.
///
/// The same two steps in the same order, minus the allocation: the rings still
/// exist and still have the firmware addresses they were created with, so the
/// offsets are zeroed rather than the rings rebuilt, and the addresses are
/// re-published because the hard reset zeroed `osinit`.
///
/// Called from [`crate::power::reset`]'s [`ResetStep::RebuildRings`] step, which
/// is where the module documentation's ordering rule — rings before the firmware
/// is started — has to be obeyed a second time.
///
/// [`ResetStep::RebuildRings`]: crate::power::ResetStep::RebuildRings
pub fn reinit(device: &PvrDevice) -> PvrResult<()> {
    ccb::reinit(device)?;

    let fw = device.fw.get().ok_or(crate::PvrError::Unsupported)?;
    fw.publish_ccb_addresses(device)?;

    Ok(())
}
