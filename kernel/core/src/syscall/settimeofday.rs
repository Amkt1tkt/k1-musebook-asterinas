// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `settimeofday` for date setting and UTC hwclock initialization.

//! `settimeofday` for date setting and UTC hwclock initialization.

use core::time::Duration;

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    prelude::*,
    time::{set_realtime, set_timezone, timeval_t, timezone_t, validate_realtime_input},
};

/// Validates supplied arguments before checking global time-setting authority.
/// Timezone metadata supports UTC RTC initialization; first-call local warp is unsupported.
pub(super) fn sys_settimeofday(
    timeval_addr: Vaddr,
    timezone_addr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let timeval = if timeval_addr == 0 {
        None
    } else {
        let value = ctx.user_space().read_val::<timeval_t>(timeval_addr)?;
        // Linux checks the raw microsecond field before copying a timezone.
        // Exactly 1,000,000 is rejected by normalized conversion below.
        if value.usec < 0 || value.usec > 1_000_000 {
            return_errno_with_message!(Errno::EINVAL, "invalid timeval microseconds");
        }
        Some(value)
    };
    let timezone = if timezone_addr == 0 {
        None
    } else {
        Some(ctx.user_space().read_val::<timezone_t>(timezone_addr)?)
    };
    let new_unix = timeval.map(Duration::try_from).transpose()?;
    if let Some(value) = new_unix {
        validate_realtime_input(value)?;
    }
    crate::time::require_sys_time()?;
    if let Some(value) = timezone {
        set_timezone(value, new_unix.is_some())?;
    }
    if let Some(value) = new_unix {
        set_realtime(value)?;
    }
    Ok(SyscallReturn::Return(0))
}
