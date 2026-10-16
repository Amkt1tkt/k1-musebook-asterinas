// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SPM8821 PMIC RTC register packing (no I/O).

//! Register layout and calendar packing for the SPM8821 PMIC RTC. No I/O,
//! so the ktests run on every target. See [`super::spm8821`] for the bus
//! sequences and where the numbers come from.

use crate::SystemTime;

/// Control register.
#[cfg_attr(not(target_arch = "riscv64"), expect(dead_code))]
pub(super) const REG_CTL: u8 = 0x1d;
pub(super) const CTL_CRYSTAL_EN: u8 = 1 << 0;
pub(super) const CTL_OUT_32K_EN: u8 = 1 << 1;
pub(super) const CTL_RTC_EN: u8 = 1 << 2;
pub(super) const CTL_RTC_CLK_SEL: u8 = 1 << 3;
/// What `spacemit_rtc_probe` ORs into the control register. Alarm and tick
/// bits are deliberately not here.
pub(super) const PROBE_ENABLE_BITS: u8 =
    CTL_CRYSTAL_EN | CTL_OUT_32K_EN | CTL_RTC_EN | CTL_RTC_CLK_SEL;

/// The six calendar registers in [`Fields`] order: `(address, mask)`.
pub(super) const REGS: [(u8, u8); 6] = [
    (0x0d, 0x3f), // seconds
    (0x0e, 0x3f), // minutes
    (0x0f, 0x1f), // hours
    (0x10, 0x1f), // day − 1
    (0x11, 0x0f), // month, 0-based
    (0x12, 0x3f), // year − 2000
];

/// Raw field values exactly as the chip stores them: 0-based day and
/// month, year from 2000.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Fields {
    pub second: u8,
    pub minute: u8,
    pub hour: u8,
    /// Day of month − 1.
    pub day: u8,
    /// Month − 1.
    pub month: u8,
    /// Year − 2000.
    pub year: u8,
}

pub(super) fn unpack_fields(raw: [u8; 6]) -> Fields {
    Fields {
        second: raw[0],
        minute: raw[1],
        hour: raw[2],
        day: raw[3],
        month: raw[4],
        year: raw[5],
    }
}

impl Fields {
    pub(super) fn to_raw(self) -> [u8; 6] {
        [
            self.second & REGS[0].1,
            self.minute & REGS[1].1,
            self.hour & REGS[2].1,
            self.day & REGS[3].1,
            self.month & REGS[4].1,
            self.year & REGS[5].1,
        ]
    }

    /// `None` if the registers hold something no calendar accepts (fresh
    /// silicon, or a month field of 12..=15).
    pub(super) fn to_system_time(self) -> Option<SystemTime> {
        let time = SystemTime {
            year: 2000 + u16::from(self.year),
            month: self.month.checked_add(1)?,
            day: self.day.checked_add(1)?,
            hour: self.hour,
            minute: self.minute,
            second: self.second,
            nanos: 0,
        };
        crate::civil::is_plausible(time).then_some(time)
    }

    /// `None` outside 2000..=2063 or with an out-of-range field. The chip
    /// has six bits of year; Linux publishes the same
    /// `RTC_TIMESTAMP_BEGIN_2000..END_2063` range for this device.
    pub(super) fn from_system_time(time: SystemTime) -> Option<Self> {
        if !crate::civil::is_plausible(time) || !(2000..=2063).contains(&time.year) {
            return None;
        }
        Some(Self {
            second: time.second,
            minute: time.minute,
            hour: time.hour,
            day: time.day - 1,
            month: time.month - 1,
            year: (time.year - 2000) as u8,
        })
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    fn t(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> SystemTime {
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

    #[ktest]
    fn round_trips_through_zero_based_day_and_month() {
        let time = t(2026, 9, 3, 10, 30, 0);
        let f = Fields::from_system_time(time).unwrap();
        assert_eq!(f.to_raw(), [0, 30, 10, 2, 8, 26]);
        assert_eq!(unpack_fields(f.to_raw()).to_system_time(), Some(time));
    }

    #[ktest]
    fn year_range_is_the_chips_six_bits() {
        assert!(Fields::from_system_time(t(1999, 12, 31, 23, 59, 59)).is_none());
        assert!(Fields::from_system_time(t(2064, 1, 1, 0, 0, 0)).is_none());
        assert_eq!(
            Fields::from_system_time(t(2063, 12, 31, 0, 0, 0))
                .unwrap()
                .to_raw(),
            [0, 0, 0, 30, 11, 63]
        );
    }

    #[ktest]
    fn garbage_month_does_not_become_a_time() {
        // Month field 12..=15 is representable in four bits but not a month.
        assert!(
            unpack_fields([0, 0, 0, 0, 13, 0])
                .to_system_time()
                .is_none()
        );
        // Day field 31 → day 32.
        assert!(
            unpack_fields([0, 0, 0, 31, 0, 0])
                .to_system_time()
                .is_none()
        );
        // A fresh chip at 2000-01-01 00:00:00 is fine.
        assert_eq!(
            unpack_fields([0; 6]).to_system_time(),
            Some(t(2000, 1, 1, 0, 0, 0))
        );
    }

    #[ktest]
    fn probe_bits_leave_alarm_and_tick_alone() {
        assert_eq!(PROBE_ENABLE_BITS, 0x0f);
        assert_eq!(0xe0 | PROBE_ENABLE_BITS, 0xef);
    }

    #[ktest]
    fn rejects_nonexistent_calendar_dates() {
        for time in [
            t(2023, 2, 29, 0, 0, 0),
            t(2024, 2, 30, 0, 0, 0),
            t(2024, 4, 31, 0, 0, 0),
        ] {
            let raw = [
                0,
                0,
                0,
                time.day - 1,
                time.month - 1,
                (time.year - 2000) as u8,
            ];
            assert_eq!(unpack_fields(raw).to_system_time(), None);
            assert!(Fields::from_system_time(time).is_none());
        }

        let leap_day = t(2024, 2, 29, 0, 0, 0);
        let fields = Fields::from_system_time(leap_day).unwrap();
        assert_eq!(
            unpack_fields(fields.to_raw()).to_system_time(),
            Some(leap_day)
        );
    }
}
