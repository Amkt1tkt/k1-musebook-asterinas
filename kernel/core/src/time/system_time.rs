// SPDX-License-Identifier: MPL-2.0

use core::time::Duration;

use aster_time::{read_monotonic_time, read_start_time};
use spin::Once;
use time::{Date, Month, PrimitiveDateTime, Time};

use crate::prelude::*;

/// This struct corresponds to `SystemTime` in Rust std.
// K1 MUSE Book --- (WALL_BASE needs Clone+Copy so clock_settime can restamp now())
// #[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
// pub(crate) struct SystemTime(PrimitiveDateTime);
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SystemTime(PrimitiveDateTime);
// K1 MUSE Book ---

pub(crate) static START_TIME: Once<SystemTime> = Once::new();
// K1 MUSE Book --- (coarse monotonic time no longer depends on a realtime origin)
// pub(super) static START_TIME_AS_DURATION: Once<Duration> = Once::new();
// K1 MUSE Book ---

// K1 MUSE Book --- (settable wall clock: now = WALL_BASE + monotonic)
/// Wall-clock origin for [`SystemTime::now`].
///
/// [`START_TIME`] stays at the boot RTC sample so `/proc/stat` `btime`
/// does not move when userspace calls `clock_settime`. This is the
/// value that does: `now = WALL_BASE + monotonic`.
static WALL_BASE: Once<SpinLock<SystemTime>> = Once::new();

// K1 MUSE Book ---

pub(super) fn init() {
    let start_time = convert_system_time(read_start_time()).unwrap();
    // K1 MUSE Book --- (remove the unused immutable realtime offset)
    // START_TIME_AS_DURATION
    //     .call_once(|| start_time.duration_since(&SystemTime::UNIX_EPOCH).unwrap());
    // K1 MUSE Book ---
    START_TIME.call_once(|| start_time);
    // K1 MUSE Book --- (seed WALL_BASE from the boot RTC sample)
    WALL_BASE.call_once(|| SpinLock::new(start_time));
    // K1 MUSE Book ---
}

impl SystemTime {
    /// The unix epoch, which represents 1970-01-01 00:00:00
    pub(crate) const UNIX_EPOCH: SystemTime = SystemTime::unix_epoch();

    const fn unix_epoch() -> Self {
        // 1970-01-01 00:00:00
        let Ok(date) = Date::from_ordinal_date(1970, 1) else {
            unreachable!()
        };
        let Ok(time) = Time::from_hms_nano(0, 0, 0, 0) else {
            unreachable!()
        };

        SystemTime(PrimitiveDateTime::new(date, time))
    }

    /// Returns the current system time
    // K1 MUSE Book --- (now = WALL_BASE + monotonic so clock_settime moves the wall)
    // pub(crate) fn now() -> Self {
    //     // The get real time result should always be valid
    //     START_TIME
    //         .get()
    //         .unwrap()
    //         .checked_add(read_monotonic_time())
    //         .unwrap()
    // }
    pub(crate) fn now() -> Self {
        wall_base().checked_add(read_monotonic_time()).unwrap()
    }

    // K1 MUSE Book ---

    /// Add a duration to self. If the result does not exceed inner bounds return Some(t), else return None.
    pub(crate) fn checked_add(&self, duration: Duration) -> Option<Self> {
        let duration = convert_to_time_duration(duration);
        self.0.checked_add(duration).map(SystemTime)
    }

    /// Subtract a duration from self. If the result does not exceed inner bounds return Some(t), else return None.
    // K1 MUSE Book --- (used by set_wall_base_from_unix)
    // #[expect(dead_code)]
    // K1 MUSE Book ---
    pub(crate) fn checked_sub(&self, duration: Duration) -> Option<Self> {
        let duration = convert_to_time_duration(duration);
        self.0.checked_sub(duration).map(SystemTime)
    }

    /// Returns the duration since an earlier time. Return error if `earlier` is later than self.
    pub(crate) fn duration_since(&self, earlier: &SystemTime) -> Result<Duration> {
        if self.0 < earlier.0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "duration_since can only accept an earlier time"
            );
        }
        let duration = self.0 - earlier.0;
        Ok(convert_to_core_duration(duration))
    }

    /// Return the difference between current time and the time when self was created.
    /// Return Error if current time is earlier than creating time.
    /// The error can happen if self was created by checked_add.
    #[expect(dead_code)]
    pub(crate) fn elapsed(&self) -> Result<Duration> {
        let now = SystemTime::now();
        now.duration_since(self)
    }
}

// K1 MUSE Book --- (clock_settime / settimeofday)
fn wall_base() -> SystemTime {
    WALL_BASE
        .get()
        .map(|lock| *lock.disable_irq().lock())
        .or_else(|| START_TIME.get().copied())
        .unwrap()
}

/// Returns the complete nonnegative realtime origin for a vDSO update.
#[cfg(not(target_arch = "loongarch64"))]
pub(crate) fn wall_origin() -> Duration {
    wall_base().duration_since(&SystemTime::UNIX_EPOCH).unwrap()
}

/// Linux TIME_SETTOD_SEC_MAX leaves thirty years of uptime below signed ktime overflow.
const MAX_SET_TIME_SECS: u64 = (i64::MAX as u64 / 1_000_000_000) - 30 * 365 * 24 * 3600;

/// Validates the input range before the syscall checks time-setting authority.
pub(crate) fn validate_realtime_input(new_unix: Duration) -> Result<()> {
    if new_unix.as_secs() >= MAX_SET_TIME_SECS {
        return_errno_with_message!(Errno::EINVAL, "realtime is outside the supported range");
    }
    Ok(())
}

/// Validates a new realtime value and derives its nonnegative wall-clock origin.
/// Linux rejects CLOCK_REALTIME values earlier than the current CLOCK_MONOTONIC.
fn wall_origin_for(new_unix: Duration, monotonic: Duration) -> Result<SystemTime> {
    validate_realtime_input(new_unix)?;
    if new_unix < monotonic {
        return_errno_with_message!(Errno::EINVAL, "realtime is outside the supported range");
    }
    let new_now = SystemTime::UNIX_EPOCH
        .checked_add(new_unix)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "realtime overflow"))?;
    new_now
        .checked_sub(monotonic)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "realtime underflow"))
}

pub(super) fn set_wall_base_from_unix(new_unix: Duration) -> Result<()> {
    let new_base = wall_origin_for(new_unix, read_monotonic_time())?;
    let lock = WALL_BASE
        .get()
        .expect("wall clock is initialized before userspace");
    *lock.disable_irq().lock() = new_base;
    Ok(())
}
// K1 MUSE Book ---

/// convert ostd::time::Time to System time
fn convert_system_time(system_time: aster_time::SystemTime) -> Result<SystemTime> {
    let month = match Month::try_from(system_time.month) {
        Ok(month) => month,
        Err(_) => return_errno_with_message!(Errno::EINVAL, "unknown month in system time"),
    };
    let date = match Date::from_calendar_date(system_time.year as _, month, system_time.day) {
        Ok(date) => date,
        Err(_) => return_errno_with_message!(Errno::EINVAL, "Invalid system date"),
    };
    let time_ = match Time::from_hms_nano(
        system_time.hour,
        system_time.minute,
        system_time.second,
        system_time.nanos.try_into().unwrap(),
    ) {
        Ok(time_) => time_,
        Err(_) => return_errno_with_message!(Errno::EINVAL, "Invalid system time"),
    };
    Ok(SystemTime(PrimitiveDateTime::new(date, time_)))
}

/// FIXME: need to further check precision loss
/// convert core::time::Duration to time::Duration
const fn convert_to_time_duration(duration: Duration) -> time::Duration {
    let seconds = duration.as_secs() as i64;
    let nanoseconds = duration.subsec_nanos() as i32;
    time::Duration::new(seconds, nanoseconds)
}

/// FIXME: need to further check precision loss
/// convert time::Duration to core::time::Duration
const fn convert_to_core_duration(duration: time::Duration) -> Duration {
    let seconds = duration.whole_seconds() as u64;
    let nanoseconds = duration.subsec_nanoseconds() as u32;
    Duration::new(seconds, nanoseconds)
}

// K1 MUSE Book --- (wall-clock setting boundaries and subsecond precision)
#[cfg(ktest)]
mod settime_tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn the_wall_origin_preserves_subsecond_borrow_and_carry() {
        let requested = Duration::new(1_700_000_000, 100_000_000);
        let monotonic = Duration::new(123, 900_000_000);
        let origin = wall_origin_for(requested, monotonic).unwrap();
        assert_eq!(
            origin.duration_since(&SystemTime::UNIX_EPOCH).unwrap(),
            Duration::new(1_699_999_876, 200_000_000)
        );
        assert_eq!(
            origin
                .checked_add(monotonic)
                .unwrap()
                .duration_since(&SystemTime::UNIX_EPOCH)
                .unwrap(),
            requested
        );
    }

    #[ktest]
    fn setting_realtime_before_monotonic_or_outside_the_calendar_is_rejected() {
        assert_eq!(
            wall_origin_for(Duration::new(2, 0), Duration::new(2, 1))
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert_eq!(
            wall_origin_for(Duration::MAX, Duration::ZERO)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert!(wall_origin_for(Duration::from_secs(MAX_SET_TIME_SECS), Duration::ZERO).is_err());
        assert!(
            wall_origin_for(
                Duration::new(MAX_SET_TIME_SECS - 1, 999_999_999),
                Duration::ZERO
            )
            .is_ok()
        );
        assert!(wall_origin_for(Duration::new(2, 1), Duration::new(2, 1)).is_ok());
    }
}
// K1 MUSE Book ---
