// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: civil calendar conversion for RTC and wall-clock operations.

//! Calendar conversion for RTC device access and kernel wall-clock operations.
//!
//! Days-from-civil / civil-from-days is Howard Hinnant's public-domain
//! algorithm (`http://howardhinnant.github.io/date_algorithms.html`).
//! No `chrono` / `time` crate: those are not available to every target
//! this crate builds for.

use crate::SystemTime;

/// Unix epoch seconds + nanoseconds → a [`SystemTime`] in UTC.
///
/// `None` if the year does not fit in the `u16` field (years 0..=65535).
pub fn system_time_from_unix(secs: u64, nanos: u64) -> Option<SystemTime> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    // Check before civil_from_days narrows its era to i32. Out-of-range
    // timestamps must not wrap back into a representable calendar year.
    let end = days_from_civil(i32::from(u16::MAX) + 1, 1, 1)? * 86_400;
    if secs >= end {
        return None;
    }
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    if !(0..=u16::MAX as i32).contains(&year) {
        return None;
    }
    Some(SystemTime {
        year: year as u16,
        month,
        day,
        hour: (sod / 3600) as u8,
        minute: ((sod % 3600) / 60) as u8,
        second: (sod % 60) as u8,
        nanos,
    })
}

/// UTC [`SystemTime`] → seconds since 1970-01-01, discarding nanoseconds.
pub fn unix_secs_from_system_time(time: SystemTime) -> Option<u64> {
    if !is_valid_date(time.year, time.month, time.day) {
        return None;
    }
    if time.hour >= 24 || time.minute >= 60 || time.second >= 60 {
        return None;
    }
    let days = days_from_civil(i32::from(time.year), time.month, time.day)?;
    let sod = u64::from(time.hour) * 3600 + u64::from(time.minute) * 60 + u64::from(time.second);
    days.checked_mul(86_400)?.checked_add(sod)
}

/// Day of week, 0 = Sunday … 6 = Saturday (Linux `tm_wday`).
///
/// Sakamoto's method. `month` is 1..=12.
pub fn weekday_sun0(year: u16, month: u8, day: u8) -> u8 {
    const T: [u8; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if !(1..=12).contains(&month) {
        return 0;
    }
    let y = if month < 3 {
        i32::from(year) - 1
    } else {
        i32::from(year)
    };
    (y + y.div_euclid(4) - y.div_euclid(100)
        + y.div_euclid(400)
        + i32::from(T[month as usize - 1])
        + i32::from(day))
    .rem_euclid(7) as u8
}

/// Days since 1970-01-01 for a Gregorian date. `None` if the month is bad.
fn days_from_civil(mut year: i32, month: u8, day: u8) -> Option<u64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    if month <= 2 {
        year -= 1;
    }
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u32;
    let mp = if month > 2 {
        u32::from(month) - 3
    } else {
        u32::from(month) + 9
    };
    let doy = (153 * mp + 2) / 5 + u32::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let z = i64::from(era) * 146_097 + i64::from(doe) - 719_468;
    if z < 0 {
        return None;
    }
    Some(z as u64)
}

/// Inverse of [`days_from_civil`]: `z` is days since 1970-01-01.
fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Returns whether a calendar is valid within the `/dev/rtc0` year range.
pub fn is_plausible(time: SystemTime) -> bool {
    (1970..=2200).contains(&time.year)
        && is_valid_date(time.year, time.month, time.day)
        && time.hour < 24
        && time.minute < 60
        && time.second < 60
}

fn is_valid_date(year: u16, month: u8, day: u8) -> bool {
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    (1..=days_in_month).contains(&day)
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    #[ktest]
    fn unix_seconds_reject_unrepresentable_years_before_conversion() {
        let end = days_from_civil(65536, 1, 1).unwrap() * 86_400;
        let last = system_time_from_unix(end - 1, 999_999_999).unwrap();
        assert_eq!((last.year, last.month, last.day), (65535, 12, 31));
        assert_eq!((last.hour, last.minute, last.second), (23, 59, 59));
        for secs in [end, end + 1, u64::MAX / 2, u64::MAX] {
            assert_eq!(system_time_from_unix(secs, 0), None);
        }
        assert_eq!(weekday_sun0(0, 1, 1), 6);
        assert_eq!(weekday_sun0(0, 2, 29), 2);
    }

    /// 1970-01-01 is day 0 and a Thursday (`tm_wday = 4`).
    #[ktest]
    fn unix_epoch_round_trips() {
        let t = system_time_from_unix(0, 0).unwrap();
        assert_eq!(t.year, 1970);
        assert_eq!(t.month, 1);
        assert_eq!(t.day, 1);
        assert_eq!(t.hour, 0);
        assert_eq!(t.minute, 0);
        assert_eq!(t.second, 0);
        assert_eq!(unix_secs_from_system_time(t), Some(0));
        assert_eq!(weekday_sun0(1970, 1, 1), 4);
    }

    /// 2024-03-15 12:34:56 UTC. 2024 is a leap year; 15 March is a Friday.
    #[ktest]
    fn leap_year_afternoon_round_trips() {
        const UNIX: u64 = 1_710_506_096;
        let t = system_time_from_unix(UNIX, 123).unwrap();
        assert_eq!(t.year, 2024);
        assert_eq!(t.month, 3);
        assert_eq!(t.day, 15);
        assert_eq!(t.hour, 12);
        assert_eq!(t.minute, 34);
        assert_eq!(t.second, 56);
        assert_eq!(t.nanos, 123);
        assert_eq!(unix_secs_from_system_time(t), Some(UNIX));
        assert_eq!(weekday_sun0(2024, 3, 15), 5);
    }

    /// A known non-leap February 28, and the next day is 1 March.
    #[ktest]
    fn non_leap_feb_28() {
        let t = SystemTime {
            year: 2023,
            month: 2,
            day: 28,
            hour: 0,
            minute: 0,
            second: 0,
            nanos: 0,
        };
        let secs = unix_secs_from_system_time(t).unwrap();
        let next = system_time_from_unix(secs + 86_400, 0).unwrap();
        assert_eq!(next.year, 2023);
        assert_eq!(next.month, 3);
        assert_eq!(next.day, 1);
    }

    #[ktest]
    fn plausible_rejects_garbage_month() {
        let mut t = system_time_from_unix(0, 0).unwrap();
        assert!(is_plausible(t));
        t.month = 13;
        assert!(!is_plausible(t));
        t.month = 1;
        t.hour = 24;
        assert!(!is_plausible(t));
    }

    #[ktest]
    fn rejects_nonexistent_calendar_dates() {
        for (year, month, day) in [
            (2023, 2, 29),
            (2024, 2, 30),
            (2024, 2, 31),
            (2024, 4, 31),
            (2024, 6, 31),
            (2024, 9, 31),
            (2024, 11, 31),
            (2024, 1, 0),
            (2024, 1, 32),
            (2024, 0, 1),
            (2024, 13, 1),
        ] {
            let mut time = system_time_from_unix(0, 0).unwrap();
            time.year = year;
            time.month = month;
            time.day = day;
            assert!(!is_plausible(time), "invalid calendar accepted: {time:?}");
            assert_eq!(unix_secs_from_system_time(time), None);
        }
    }

    #[ktest]
    fn leap_days_follow_gregorian_century_rules() {
        for (year, valid) in [(2000, true), (2024, true), (2100, false), (2200, false)] {
            let mut time = system_time_from_unix(0, 0).unwrap();
            time.year = year;
            time.month = 2;
            time.day = 29;
            assert_eq!(is_plausible(time), valid);
            let secs = unix_secs_from_system_time(time);
            assert_eq!(secs.is_some(), valid);
            if let Some(secs) = secs {
                assert_eq!(system_time_from_unix(secs, 0), Some(time));
            }
        }
    }

    #[ktest]
    fn calendar_validation_preserves_year_ranges() {
        for (year, plausible, unix_valid) in [
            (1969, false, false),
            (1970, true, true),
            (2200, true, true),
            (2201, false, true),
        ] {
            let mut time = system_time_from_unix(0, 0).unwrap();
            time.year = year;
            assert_eq!(is_plausible(time), plausible);
            assert_eq!(unix_secs_from_system_time(time).is_some(), unix_valid);
        }
    }
}
