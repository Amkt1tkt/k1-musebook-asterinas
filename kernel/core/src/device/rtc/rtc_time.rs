// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux `struct rtc_time` for `/dev/rtc0` / hwclock.

//! Linux `struct rtc_time` and the civil-time conversion `hwclock` depends on.
//!
//! Field meaning is the System V / Linux `struct tm` convention, **not**
//! [`aster_time::SystemTime`]:
//!
//! * `tm_year` is years since 1900 (`2026` → `126`)
//! * `tm_mon` is 0..=11 (`March` → `2`)
//! * `tm_mday` is 1..=31
//! * `tm_wday` is 0 = Sunday … 6 = Saturday
//! * `tm_yday` is 0..=365 (days since 1 January)
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.18/source/include/uapi/linux/rtc.h>.

use aster_time::{SystemTime, unix_secs_from_system_time};

use crate::prelude::*;

/// Userspace `struct rtc_time`. Nine `int`s, no padding on LP64.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.18/source/include/uapi/linux/rtc.h#L27>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Pod)]
pub(super) struct RtcTime {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

/// Linux `rtc_valid_tm`: refuse a time the hardware RTC class would reject.
///
/// `tm_wday` / `tm_yday` / `tm_isdst` are ignored on set (same as Linux).
/// Negative `tm_mon` / `tm_hour` / `tm_min` / `tm_sec` are treated as unsigned
/// so they fail the upper-bound check — `rtc_valid_tm` in
/// `drivers/rtc/lib.c`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.18/source/drivers/rtc/lib.c#L57>.
pub(super) fn validate_rtc_time(tm: &RtcTime) -> Result<()> {
    if tm.tm_year < 70 {
        return_errno_with_message!(Errno::EINVAL, "rtc_time tm_year is before 1970");
    }
    if (tm.tm_mon as u32) >= 12 {
        return_errno_with_message!(Errno::EINVAL, "rtc_time tm_mon is out of range");
    }
    let year = match tm.tm_year.checked_add(1900) {
        Some(y) => y,
        None => return_errno_with_message!(Errno::EINVAL, "rtc_time tm_year overflow"),
    };
    if !(1970..=2200).contains(&year) {
        return_errno_with_message!(Errno::EINVAL, "rtc_time year is outside 1970..=2200");
    }
    let dim = days_in_month(year, tm.tm_mon);
    if tm.tm_mday < 1 || tm.tm_mday > dim {
        return_errno_with_message!(Errno::EINVAL, "rtc_time tm_mday is out of range");
    }
    if (tm.tm_hour as u32) >= 24 {
        return_errno_with_message!(Errno::EINVAL, "rtc_time tm_hour is out of range");
    }
    if (tm.tm_min as u32) >= 60 {
        return_errno_with_message!(Errno::EINVAL, "rtc_time tm_min is out of range");
    }
    if (tm.tm_sec as u32) >= 60 {
        return_errno_with_message!(Errno::EINVAL, "rtc_time tm_sec is out of range");
    }
    Ok(())
}

pub(super) fn system_time_from_rtc_time(tm: RtcTime) -> Result<SystemTime> {
    validate_rtc_time(&tm)?;
    // `validate_rtc_time` already checked the 1900 offset and month base.
    Ok(SystemTime {
        year: (tm.tm_year + 1900) as u16,
        month: (tm.tm_mon + 1) as u8,
        day: tm.tm_mday as u8,
        hour: tm.tm_hour as u8,
        minute: tm.tm_min as u8,
        second: tm.tm_sec as u8,
        nanos: 0,
    })
}

pub(super) fn rtc_time_from_system_time(time: SystemTime) -> Result<RtcTime> {
    let secs = unix_secs_from_system_time(time).ok_or_else(|| {
        Error::with_message(Errno::EINVAL, "system time is not a valid Unix instant")
    })?;
    let year = i32::from(time.year);
    let mon0 = i32::from(time.month) - 1;
    if !(1..=12).contains(&time.month) || mon0 < 0 {
        return_errno_with_message!(Errno::EINVAL, "system time month is out of range");
    }
    Ok(RtcTime {
        tm_sec: i32::from(time.second),
        tm_min: i32::from(time.minute),
        tm_hour: i32::from(time.hour),
        tm_mday: i32::from(time.day),
        tm_mon: mon0,
        tm_year: year - 1900,
        tm_wday: weekday_sun0_from_unix_secs(secs),
        tm_yday: yearday(year, mon0, i32::from(time.day)),
        tm_isdst: 0,
    })
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, mon0: i32) -> i32 {
    const D: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if mon0 == 1 && is_leap(year) {
        29
    } else {
        D[mon0 as usize]
    }
}

/// 0-based day of year. `mon0` is 0..=11.
fn yearday(year: i32, mon0: i32, mday: i32) -> i32 {
    const CUM: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leap = if mon0 >= 2 && is_leap(year) { 1 } else { 0 };
    CUM[mon0 as usize] + mday - 1 + leap
}

/// 1970-01-01 is a Thursday (`tm_wday = 4`).
fn weekday_sun0_from_unix_secs(secs: u64) -> i32 {
    ((secs / 86_400 + 4) % 7) as i32
}

#[cfg(ktest)]
mod tests {
    use aster_time::system_time_from_unix;
    use ostd::prelude::*;

    use super::*;

    fn civil(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> SystemTime {
        SystemTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            nanos: 0,
        }
    }

    /// Unix epoch: `tm_year = 70`, `tm_mon = 0`, Thursday, yday 0.
    #[ktest]
    fn epoch_uses_1900_offset_and_zero_based_month() {
        let t = system_time_from_unix(0, 0).unwrap();
        let tm = rtc_time_from_system_time(t).unwrap();
        assert_eq!(tm.tm_year, 70);
        assert_eq!(tm.tm_mon, 0);
        assert_eq!(tm.tm_mday, 1);
        assert_eq!(tm.tm_hour, 0);
        assert_eq!(tm.tm_min, 0);
        assert_eq!(tm.tm_sec, 0);
        assert_eq!(tm.tm_wday, 4);
        assert_eq!(tm.tm_yday, 0);
        assert_eq!(tm.tm_isdst, 0);

        let back = system_time_from_rtc_time(tm).unwrap();
        assert_eq!(unix_secs_from_system_time(back), Some(0));
    }

    /// 2024-03-15 12:34:56 Friday. 2024 is a leap year; March is `tm_mon = 2`.
    #[ktest]
    fn leap_year_afternoon_round_trips() {
        const UNIX: u64 = 1_710_506_096;
        let t = system_time_from_unix(UNIX, 0).unwrap();
        let tm = rtc_time_from_system_time(t).unwrap();
        assert_eq!(tm.tm_year, 124);
        assert_eq!(tm.tm_mon, 2);
        assert_eq!(tm.tm_mday, 15);
        assert_eq!(tm.tm_hour, 12);
        assert_eq!(tm.tm_min, 34);
        assert_eq!(tm.tm_sec, 56);
        assert_eq!(tm.tm_wday, 5);
        // 31 + 29 + 15 - 1 = 74.
        assert_eq!(tm.tm_yday, 74);

        let back = system_time_from_rtc_time(tm).unwrap();
        assert_eq!(unix_secs_from_system_time(back), Some(UNIX));
    }

    /// 2024-02-29 is valid; the next civil day is 1 March.
    #[ktest]
    fn leap_day_is_accepted() {
        let t = civil(2024, 2, 29, 0, 0, 0);
        let secs = unix_secs_from_system_time(t).unwrap();
        let tm = rtc_time_from_system_time(t).unwrap();
        assert_eq!(tm.tm_mon, 1);
        assert_eq!(tm.tm_mday, 29);
        assert_eq!(tm.tm_yday, 31 + 29 - 1);
        assert!(validate_rtc_time(&tm).is_ok());

        let next = system_time_from_unix(secs + 86_400, 0).unwrap();
        assert_eq!(next.month, 3);
        assert_eq!(next.day, 1);
    }

    /// 31 December of a leap year is `tm_mon = 11`, `tm_yday = 365`.
    #[ktest]
    fn new_years_eve_leap() {
        let tm = rtc_time_from_system_time(civil(2024, 12, 31, 23, 59, 59)).unwrap();
        assert_eq!(tm.tm_mon, 11);
        assert_eq!(tm.tm_mday, 31);
        assert_eq!(tm.tm_yday, 365);
        assert_eq!(tm.tm_hour, 23);
        assert_eq!(tm.tm_min, 59);
        assert_eq!(tm.tm_sec, 59);
    }

    #[ktest]
    fn rejects_year_before_1970() {
        let mut tm = rtc_time_from_system_time(civil(1970, 1, 1, 0, 0, 0)).unwrap();
        tm.tm_year = 69;
        assert!(validate_rtc_time(&tm).is_err());
        assert!(system_time_from_rtc_time(tm).is_err());
    }

    #[ktest]
    fn rejects_month_outside_0_11() {
        let mut tm = rtc_time_from_system_time(civil(2024, 1, 1, 0, 0, 0)).unwrap();
        tm.tm_mon = 12;
        assert!(validate_rtc_time(&tm).is_err());
        tm.tm_mon = -1;
        assert!(validate_rtc_time(&tm).is_err());
    }

    #[ktest]
    fn rejects_non_leap_feb_29() {
        let mut tm = rtc_time_from_system_time(civil(2023, 2, 28, 0, 0, 0)).unwrap();
        tm.tm_mday = 29;
        assert!(validate_rtc_time(&tm).is_err());
    }

    #[ktest]
    fn rejects_bad_clock_fields() {
        let base = rtc_time_from_system_time(civil(2024, 6, 1, 0, 0, 0)).unwrap();
        let mut tm = base;
        tm.tm_hour = 24;
        assert!(validate_rtc_time(&tm).is_err());
        tm = base;
        tm.tm_min = 60;
        assert!(validate_rtc_time(&tm).is_err());
        tm = base;
        tm.tm_sec = 60;
        assert!(validate_rtc_time(&tm).is_err());
        tm = base;
        tm.tm_mday = 0;
        assert!(validate_rtc_time(&tm).is_err());
        tm = base;
        tm.tm_hour = -1;
        assert!(validate_rtc_time(&tm).is_err());
    }
}
