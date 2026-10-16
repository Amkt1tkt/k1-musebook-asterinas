// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `clock_settime` — landing point for `date -s` / glibc.

//! `clock_settime` — the landing point for `date -s` / glibc `clock_settime`.

use core::time::Duration;

use ostd::mm::VmIo;

use super::{SyscallReturn, clock_gettime::ClockId};
use crate::{
    prelude::*,
    time::{clockid_t, set_realtime, timespec_t, validate_realtime_input},
};

pub(super) fn sys_clock_settime(
    clockid: clockid_t,
    timespec_addr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    let clock_id = ClockId::try_from(clockid)?;
    if clock_id != ClockId::CLOCK_REALTIME {
        return_errno_with_message!(Errno::EINVAL, "only CLOCK_REALTIME can be set");
    }

    let timespec = ctx.user_space().read_val::<timespec_t>(timespec_addr)?;
    let new_unix = Duration::try_from(timespec)?;
    validate_realtime_input(new_unix)?;
    crate::time::require_sys_time()?;
    set_realtime(new_unix)?;
    Ok(SyscallReturn::Return(0))
}
