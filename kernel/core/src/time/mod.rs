// SPDX-License-Identifier: MPL-2.0

#![expect(non_camel_case_types)]

pub(crate) use core::{Clock, timer};

use ::core::time::Duration;
// K1 MUSE Book --- (share the precise wall origin with the vDSO)
// pub(crate) use system_time::{START_TIME, SystemTime};
#[cfg(not(target_arch = "loongarch64"))]
pub(crate) use system_time::wall_origin;
pub(crate) use system_time::{START_TIME, SystemTime, validate_realtime_input};
// K1 MUSE Book ---
pub(crate) use timer::{Timer, TimerManager};

use crate::prelude::*;

pub(crate) mod clocks;
mod core;
pub(crate) mod cpu_time_stats;
mod softirq;
mod system_time;
pub(crate) mod timerfd;
pub(crate) mod wait;

pub(crate) type clockid_t = i32;
pub(crate) type time_t = i64;
pub(crate) type timer_t = i32;
pub(crate) type suseconds_t = i64;

const NSEC_PER_USEC: i64 = 1_000;
const USEC_PER_SEC: i64 = 1_000_000;
pub(crate) const NSEC_PER_SEC: i64 = 1_000_000_000;

pub(super) fn init() {
    system_time::init();
    clocks::init();
    softirq::init();
    cpu_time_stats::init();
}

// K1 MUSE Book --- (system time is global, not private to a user namespace)
pub(crate) fn require_sys_time() -> Result<()> {
    use crate::{
        process::{UserNamespace, credentials::capabilities::CapSet, posix_thread::AsPosixThread},
        security::lsm::hooks,
    };
    let thread = current_thread!();
    hooks::on_capable(hooks::CapableContext::new(
        UserNamespace::get_init_singleton().as_ref(),
        thread.as_posix_thread().unwrap(),
        CapSet::SYS_TIME,
    ))
}
// K1 MUSE Book ---

// K1 MUSE Book --- (legacy timezone metadata used by UTC hwclock initialization)
struct TimezoneState {
    value: timezone_t,
    first: bool,
}

impl TimezoneState {
    const fn new() -> Self {
        Self {
            value: timezone_t {
                tz_minuteswest: 0,
                tz_dsttime: 0,
            },
            first: true,
        }
    }

    fn set(&mut self, value: timezone_t, has_time: bool) -> Result<()> {
        if !(-900..=900).contains(&value.tz_minuteswest) {
            return_errno_with_message!(
                Errno::EINVAL,
                "timezone is outside the Linux fifteen-hour range"
            );
        }
        // This kernel samples the RTC as UTC. Do not pretend to implement
        // Linux's first-call local-RTC warp or persistent_clock_is_local.
        // hwclock --hctosys --utc first supplies zero to disable that warp.
        if self.first && !has_time && value.tz_minuteswest != 0 {
            return_errno_with_message!(
                Errno::EOPNOTSUPP,
                "first-call local RTC clock warp is unsupported"
            );
        }
        self.value = value;
        self.first = false;
        Ok(())
    }
}

static TIMEZONE: SpinLock<TimezoneState> = SpinLock::new(TimezoneState::new());

/// Reads the timezone metadata shared by gettimeofday and the vDSO.
pub(crate) fn timezone() -> timezone_t {
    TIMEZONE.disable_irq().lock().value
}

pub(crate) fn set_timezone(value: timezone_t, has_time: bool) -> Result<()> {
    TIMEZONE.disable_irq().lock().set(value, has_time)?;
    // No timezone lock is held while entering the vDSO lock. The vDSO reads
    // current metadata under its own lock, so delayed setters cannot publish
    // an older timezone after a newer setter's refresh.
    #[cfg(not(target_arch = "loongarch64"))]
    crate::vdso::refresh_timezone();
    Ok(())
}
// K1 MUSE Book ---

// K1 MUSE Book --- (settable realtime without implicit RTC I/O)
/// Sets CLOCK_REALTIME and refreshes its cached representations.
/// RTC persistence is handled by RTC_SET_TIME and the distro's hwclock service.
pub(crate) fn set_realtime(new_unix: Duration) -> Result<()> {
    system_time::set_wall_base_from_unix(new_unix)?;
    clocks::refresh_coarse_realtime();
    #[cfg(not(target_arch = "loongarch64"))]
    crate::vdso::refresh_realtime();
    Ok(())
}
// K1 MUSE Book ---

pub(super) fn init_on_each_cpu() {
    cpu_time_stats::init_on_each_cpu();
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct timespec_t {
    pub sec: time_t,
    pub nsec: i64,
}

impl From<Duration> for timespec_t {
    fn from(duration: Duration) -> timespec_t {
        let sec = duration.as_secs() as time_t;
        let nsec = duration.subsec_nanos() as i64;
        debug_assert!(sec >= 0); // `nsec >= 0` always holds.
        timespec_t { sec, nsec }
    }
}

impl TryFrom<timespec_t> for Duration {
    type Error = Error;

    fn try_from(value: timespec_t) -> Result<Self> {
        if value.sec < 0 || value.nsec < 0 {
            return_errno_with_message!(Errno::EINVAL, "timesepc_t cannot be negative");
        }
        if value.nsec >= NSEC_PER_SEC {
            // The value of nanoseconds must be strictly less than 10^9,
            // otherwise the value for seconds should be set.
            return_errno_with_message!(Errno::EINVAL, "nsec is not normalized");
        }

        Ok(Duration::new(value.sec as u64, value.nsec as u32))
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct timeval_t {
    pub sec: time_t,
    pub usec: suseconds_t,
}

impl timeval_t {
    /// Normalizes time by adding carries from microseconds to seconds.
    ///
    /// Some Linux system calls do this before checking the validity (e.g., the [select]
    /// implementation).
    ///
    /// [select]: https://elixir.bootlin.com/linux/v6.10.5/source/fs/select.c#L716
    pub(crate) fn normalize(&self) -> Self {
        Self {
            sec: self.sec.wrapping_add(self.usec / USEC_PER_SEC),
            usec: self.usec % USEC_PER_SEC,
        }
    }
}

impl From<Duration> for timeval_t {
    fn from(duration: Duration) -> timeval_t {
        let sec = duration.as_secs() as time_t;
        let usec = duration.subsec_micros() as suseconds_t;
        debug_assert!(sec >= 0); // `usec >= 0` always holds.
        timeval_t { sec, usec }
    }
}

impl TryFrom<timeval_t> for Duration {
    type Error = Error;

    fn try_from(timeval: timeval_t) -> Result<Self> {
        if timeval.sec < 0 || timeval.usec < 0 {
            return_errno_with_message!(Errno::EINVAL, "timeval_t cannot be negative");
        }
        if timeval.usec >= USEC_PER_SEC {
            // The value of microseconds must be strictly less than 10^6,
            // otherwise the value for seconds should be set.
            return_errno_with_message!(Errno::EINVAL, "usec is not normalized");
        }

        Ok(Duration::new(
            timeval.sec as u64,
            (timeval.usec * NSEC_PER_USEC) as u32,
        ))
    }
}

// The various flags for setting POSIX.1b interval timers:
pub(crate) const TIMER_ABSTIME: i32 = 0x01;

/// Unix time measures time by the number of seconds that have elapsed since
/// the Unix epoch, without adjustments made due to leap seconds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct UnixTime {
    sec: u32,
}

impl From<Duration> for UnixTime {
    fn from(duration: Duration) -> Self {
        Self {
            sec: duration.as_secs() as u32,
        }
    }
}

impl From<UnixTime> for Duration {
    fn from(time: UnixTime) -> Self {
        Duration::from_secs(time.sec as _)
    }
}

/// This struct is corresponding to the `itimerval` struct in Linux.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct itimerval_t {
    pub it_interval: timeval_t,
    pub it_value: timeval_t,
}

/// This struct is corresponding to the `itimerspec` struct in Linux.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct itimerspec_t {
    pub it_interval: timespec_t,
    pub it_value: timespec_t,
}

/// This struct is corresponding to the `timezone` struct in Linux.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(crate) struct timezone_t {
    pub tz_minuteswest: i32,
    pub tz_dsttime: i32,
}

// K1 MUSE Book --- (UTC hwclock timezone sequence and unsupported local-RTC warp)
#[cfg(ktest)]
mod timezone_tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn utc_initialization_accepts_later_timezone_metadata_without_warping() {
        let mut state = TimezoneState::new();
        let local = timezone_t {
            tz_minuteswest: -480,
            tz_dsttime: 0,
        };
        assert_eq!(
            state.set(local, false).unwrap_err().error(),
            Errno::EOPNOTSUPP
        );
        assert!(state.first);
        assert_eq!(state.value.tz_minuteswest, 0);
        state.set(timezone_t::default(), false).unwrap();
        state.set(local, false).unwrap();
        assert!(!state.first);
        assert_eq!(state.value.tz_minuteswest, -480);
        let mut simultaneous = TimezoneState::new();
        simultaneous.set(local, true).unwrap();
        assert_eq!(simultaneous.value.tz_minuteswest, -480);
    }

    #[ktest]
    fn timezone_range_errors_preserve_prior_metadata_and_first_call_state() {
        let mut state = TimezoneState::new();
        for minutes in [-901, 901, i32::MIN, i32::MAX] {
            assert_eq!(
                state
                    .set(
                        timezone_t {
                            tz_minuteswest: minutes,
                            tz_dsttime: 1
                        },
                        true
                    )
                    .unwrap_err()
                    .error(),
                Errno::EINVAL
            );
            assert!(state.first);
            assert_eq!(state.value.tz_minuteswest, 0);
        }
        state.set(timezone_t::default(), false).unwrap();
        for minutes in [-900, 900] {
            state
                .set(
                    timezone_t {
                        tz_minuteswest: minutes,
                        tz_dsttime: 42,
                    },
                    false,
                )
                .unwrap();
            assert_eq!(state.value.tz_minuteswest, minutes);
            assert_eq!(state.value.tz_dsttime, 42);
        }
    }
}
// K1 MUSE Book ---
