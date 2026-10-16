// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::SyscallReturn;
use crate::{
    prelude::*,
    // K1 MUSE Book --- (timezone_t is now returned by the shared state accessor)
    // time::{SystemTime, timeval_t, timezone_t},
    time::{SystemTime, timeval_t},
    // K1 MUSE Book ---
};

pub(super) fn sys_gettimeofday(
    timeval_addr: Vaddr,
    timezone_addr: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    if timeval_addr != 0 {
        let time_val = {
            let now = SystemTime::now();
            let time_duration = now.duration_since(&SystemTime::UNIX_EPOCH)?;
            timeval_t::from(time_duration)
        };
        ctx.user_space().write_val(timeval_addr, &time_val)?;
    }

    if timezone_addr != 0 {
        // K1 MUSE Book --- (read the metadata installed by settimeofday)
        // // TODO: Return the actual system timezone when available.
        // let tz = timezone_t::default();
        let tz = crate::time::timezone();
        // K1 MUSE Book ---
        ctx.user_space().write_val(timezone_addr, &tz)?;
    }

    Ok(SyscallReturn::Return(0))
}
