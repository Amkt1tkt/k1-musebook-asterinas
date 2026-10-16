// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux RTC ioctl numbers (`include/uapi/linux/rtc.h`).

//! Linux RTC ioctl numbers (`include/uapi/linux/rtc.h`).
//!
//! Encoded values `hwclock` actually sends:
//!
//! * `RTC_RD_TIME`  = `_IOR('p', 0x09, struct rtc_time)` = `0x80247009`
//! * `RTC_SET_TIME` = `_IOW('p', 0x0a, struct rtc_time)` = `0x4024700a`
//! * `RTC_UIE_ON`   = `_IO('p', 0x03)` = `0x7003`
//! * `RTC_UIE_OFF`  = `_IO('p', 0x04)` = `0x7004`
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.18/source/include/uapi/linux/rtc.h#L47>.

use super::rtc_time::RtcTime;
use crate::{
    ioc,
    util::ioctl::{InData, NoData, OutData},
};

// ```c
// #define RTC_RD_TIME   _IOR('p', 0x09, struct rtc_time)
// #define RTC_SET_TIME  _IOW('p', 0x0a, struct rtc_time)
// #define RTC_UIE_ON    _IO('p', 0x03)
// #define RTC_UIE_OFF   _IO('p', 0x04)
// ```

pub(super) type RdTime = ioc!(RTC_RD_TIME, b'p', 0x09, OutData<RtcTime>);
pub(super) type SetTime = ioc!(RTC_SET_TIME, b'p', 0x0a, InData<RtcTime>);
pub(super) type UieOn = ioc!(RTC_UIE_ON, b'p', 0x03, NoData);
pub(super) type UieOff = ioc!(RTC_UIE_OFF, b'p', 0x04, NoData);
