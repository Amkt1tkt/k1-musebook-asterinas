// SPDX-License-Identifier: MPL-2.0

use alloc::sync::Arc;

// K1 MUSE Book --- (writable PMIC RTC)
use crate::RtcWriteError;
use crate::SystemTime;
// K1 MUSE Book ---

/// Generic interface for RTC drivers.
pub(crate) trait Driver {
    /// Creates a RTC driver.
    /// Returns [`Some<Self>`] on success, [`None`] otherwise (e.g. platform unsupported).
    fn try_new() -> Option<Self>
    where
        Self: Sized;

    /// Reads RTC.
    fn read_rtc(&self) -> SystemTime;

    // K1 MUSE Book --- (default: no write path; SPM8821 overrides)
    /// Writes RTC. Drivers without a write path leave the default
    /// [`RtcWriteError::Unsupported`]. `/dev/rtc0` `RTC_SET_TIME`
    /// surfaces that as `EINVAL`; it does not invent a write for them.
    fn write_rtc(&self, _time: SystemTime) -> Result<(), RtcWriteError> {
        Err(RtcWriteError::Unsupported)
    }
    // K1 MUSE Book ---
}

macro_rules! declare_rtc_drivers {
    ( $( #[cfg $cfg:tt ] $module:ident :: $name:ident),* $(,)? ) => {
        pub(super) fn init_rtc_driver() -> Arc<dyn Driver + Send + Sync> {
            // Iterate all possible drivers and pick one that can be initialized.
            $(
                #[cfg $cfg]
                if let Some(driver) = $module::$name::try_new() {
                    return Arc::new(driver);
                }
            )*

            ostd::warn!("No RTC device found, falling back to a dummy RTC");

            Arc::new(RtcDummy)
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod cmos;
#[cfg(target_arch = "riscv64")]
mod goldfish;
#[cfg(target_arch = "loongarch64")]
mod loongson;
#[cfg(target_arch = "aarch64")]
mod pl031;
// K1 MUSE Book --- (SPM8821 PMIC RTC; packing is also built under ktest)
#[cfg(target_arch = "riscv64")]
mod spm8821;
#[cfg(any(target_arch = "riscv64", ktest))]
mod spm8821_regs;
// K1 MUSE Book ---

// K1 MUSE Book --- (PMIC RTC first: Muse Book clock that holds across power-off)
// declare_rtc_drivers! {
//     #[cfg(target_arch = "x86_64")] cmos::RtcCmos,
//     #[cfg(target_arch = "riscv64")] goldfish::RtcGoldfish,
//     #[cfg(target_arch = "loongarch64")] loongson::RtcLoongson,
//     #[cfg(target_arch = "aarch64")] pl031::RtcPl031,
// }
declare_rtc_drivers! {
    #[cfg(target_arch = "x86_64")] cmos::RtcCmos,
    #[cfg(target_arch = "riscv64")] spm8821::RtcSpm8821,
    #[cfg(target_arch = "riscv64")] goldfish::RtcGoldfish,
    #[cfg(target_arch = "loongarch64")] loongson::RtcLoongson,
    #[cfg(target_arch = "aarch64")] pl031::RtcPl031,
}
// K1 MUSE Book ---

struct RtcDummy;

impl Driver for RtcDummy {
    fn try_new() -> Option<Self> {
        Some(Self)
    }

    fn read_rtc(&self) -> SystemTime {
        SystemTime {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            nanos: 0,
        }
    }
}
