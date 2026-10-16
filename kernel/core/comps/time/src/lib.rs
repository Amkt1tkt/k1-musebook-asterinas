// SPDX-License-Identifier: MPL-2.0

//! The system time of Asterinas.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::sync::Arc;
use core::time::Duration;

pub use clocksource::{ClockSource, Instant};
use component::{ComponentInitError, init_component};
use rtc::Driver;
use spin::Once;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "time: "
    };
}

mod clocksource;
mod rtc;
mod tsc;
// K1 MUSE Book --- (civil calendar for RTC write / clock_settime / /dev/rtc0)
mod civil;
pub use civil::{is_plausible, system_time_from_unix, unix_secs_from_system_time, weekday_sun0};
// K1 MUSE Book ---

pub static VDSO_DATA_HIGH_RES_UPDATE_FN: Once<fn(Instant, u64)> = Once::new();

static RTC_DRIVER: Once<Arc<dyn Driver + Send + Sync>> = Once::new();

#[init_component]
fn time_init() -> Result<(), ComponentInitError> {
    let rtc = rtc::init_rtc_driver();
    RTC_DRIVER.call_once(|| rtc);
    tsc::init();
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SystemTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub nanos: u64,
}

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
impl From<chrono::NaiveDateTime> for SystemTime {
    fn from(time: chrono::NaiveDateTime) -> Self {
        use chrono::{Datelike, Timelike};

        Self {
            year: time.year() as u16,
            month: time.month() as u8,
            day: time.day() as u8,
            hour: time.hour() as u8,
            minute: time.minute() as u8,
            second: time.second() as u8,
            nanos: time.nanosecond() as u64,
        }
    }
}

static START_TIME: Once<SystemTime> = Once::new();

/// Returns the `START_TIME`, which is the system time when calibrating.
pub fn read_start_time() -> SystemTime {
    *START_TIME.get().unwrap()
}

/// Returns the monotonic time from the TSC clocksource.
pub fn read_monotonic_time() -> Duration {
    let instant = tsc::read_instant();
    Duration::new(instant.secs(), instant.nanos())
}

/// Returns the default (TSC) clocksource.
pub fn default_clocksource() -> Arc<ClockSource> {
    tsc::CLOCK.get().unwrap().clone()
}

// K1 MUSE Book --- (SPM8821 write path for the RTC device)
/// Why a [`write_rtc`] call failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcWriteError {
    /// This RTC has no write path (dummy, CMOS, Goldfish, Loongson).
    Unsupported,
    /// Month/day/hour out of range; the driver refused to program it.
    InvalidTime,
    /// MMIO / I2C failed or the value did not read back.
    Hardware,
}

/// Programs the platform RTC, returning [`RtcWriteError::Unsupported`] on
/// boards without a writable driver. Wall-clock setters do not call this:
/// changing kernel time and persisting the battery-backed clock are separate
/// operations.
///
/// `/dev/rtc0` `RTC_SET_TIME` also lands here. That ioctl must **not**
/// move the wall clock — see `kernel/core/src/device/rtc/`.
pub fn write_rtc(time: SystemTime) -> Result<(), RtcWriteError> {
    RTC_DRIVER
        .get()
        .ok_or(RtcWriteError::Unsupported)?
        .write_rtc(time)
}

/// Live calendar read from the platform RTC (not the boot sample).
///
/// [`None`] only if [`time_init`] has not run.
pub fn read_rtc() -> Option<SystemTime> {
    RTC_DRIVER.get().map(|d| d.read_rtc())
}
// K1 MUSE Book ---
