// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SPM8821 PMIC RTC (TWSI8), the Muse Book clock that holds across power-off.

//! SpacemiT SPM8821 PMIC real-time clock (`pmic,rtc,spm8821`), the RTC that
//! actually keeps time on the K1 Muse Book.
//!
//! The SoC's own MMP RTC block (`rtc@d4010000`) is `status = "disabled"` in
//! the vendor `k1-x_MUSE-Book.dts`; the battery-backed clock is this one,
//! inside the PMIC on TWSI8 @ 0x41.
//!
//! Register map and sequences are from the vendor BSP `drivers/rtc/
//! rtc-spt-pmic.c` + `include/linux/mfd/spacemit/spm8821.h`
//! (`SPM8821_RTC_REG_DESC`, `union rtc_ctl_desc`):
//!
//! | reg    | field               | mask   |
//! |--------|---------------------|--------|
//! | `0x0d` | seconds             | `0x3f` |
//! | `0x0e` | minutes             | `0x3f` |
//! | `0x0f` | hours               | `0x1f` |
//! | `0x10` | day of month − 1    | `0x1f` |
//! | `0x11` | month, 0-based      | `0x0f` |
//! | `0x12` | year − 2000         | `0x3f` |
//! | `0x1d` | control (see below) | `0xff` |
//!
//! Control `0x1d` bits: 0 `crystal_en`, 1 `out_32k_en`, 2 `rtc_en`,
//! 3 `rtc_clk_sel`, 4 `tick_type`, 5 `alarm_en`, 6 `tick_en`.
//!
//! - probe: set `crystal_en | out_32k_en | rtc_en | rtc_clk_sel`, leave the
//!   alarm/tick bits as found.
//! - read: one 6-byte block read (`regmap_bulk_read`), repeated until two
//!   passes agree, so a seconds rollover between bytes cannot produce
//!   `xx:59:00`. Mainline `rtc-spacemit-p1.c` does exactly this and says
//!   why: the documented latch is "unstable due to a bug in the hardware
//!   design".
//! - write: clear `rtc_en`, one 6-byte block write (`regmap_bulk_write`),
//!   block-read back until it matches, set `rtc_en` again.
//!
//! Use block transactions for the counter fields. Separate byte-wise
//! transactions can read back written values without updating the running
//! counter. The transfer sequence follows the vendor driver; readback alone
//! does not establish that the clock is advancing.
//! `k1.diag=rtc` logs raw reads when the value changes.
//!
//! Each block is one I2C transaction under the shared `PMIC_XFER` lock in
//! `aster-i2c`, same as the power key drain and cpufreq's dcdc1 writes.

use ostd::{
    arch::boot::DEVICE_TREE,
    sync::{Mutex, MutexGuard, SpinLock},
    task::Task,
};

use super::spm8821_regs::{CTL_RTC_EN, Fields, PROBE_ENABLE_BITS, REG_CTL, REGS, unpack_fields};
use crate::{RtcWriteError, SystemTime, rtc::Driver};

/// How many times the read path retries for two agreeing passes. A pass is
/// one block transaction; the counter changes at most once per second, so
/// two passes almost always agree on the first try.
const READ_STABLE_TRIES: usize = 8;
/// Read-back attempts after a write. The BSP loops forever; we give up.
const WRITE_VERIFY_TRIES: usize = 8;

pub(super) struct RtcSpm8821 {
    /// Serializes the disable/write/verify/enable sequence.
    write_lock: Mutex<()>,
}

impl Driver for RtcSpm8821 {
    fn try_new() -> Option<Self> {
        let dt = DEVICE_TREE.get()?;
        let node = dt.find_compatible(&["pmic,rtc,spm8821"])?;
        if node
            .property("status")
            .and_then(|p| p.as_str())
            .is_some_and(|s| s != "okay")
        {
            return None;
        }

        let ctl = k1_musebook_i2c::pmic_read(REG_CTL)?;
        let want = ctl | PROBE_ENABLE_BITS;
        if want != ctl && !k1_musebook_i2c::pmic_write(REG_CTL, want) {
            ostd::warn!("[k1-rtc] SPM8821 ctl {:#04x} write failed", REG_CTL);
            return None;
        }
        let after = k1_musebook_i2c::pmic_read(REG_CTL).unwrap_or(want);

        let rtc = Self {
            write_lock: Mutex::new(()),
        };
        let raw = read_pass();
        match raw.map(unpack_fields) {
            Some(f) => ostd::info!(
                "[k1-rtc] SPM8821 PMIC RTC on TWSI8, ctl {:#04x} → {:#04x}, raw {:02x?} = {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                ctl,
                after,
                raw.unwrap_or_default(),
                2000 + u16::from(f.year),
                f.month + 1,
                f.day + 1,
                f.hour,
                f.minute,
                f.second
            ),
            None => ostd::info!(
                "[k1-rtc] SPM8821 PMIC RTC on TWSI8, ctl {:#04x} → {:#04x}, block read failed",
                ctl,
                after
            ),
        }
        if aster_cmdline::diag_enabled("rtc") {
            // Is the counter ticking at all? Costs ~1.1 s of boot, so only
            // under the knob.
            busy_wait_ms(1100);
            let again = read_pass();
            ostd::info!(
                "[k1-rtc] diag: 1.1 s later raw {:02x?} ({})",
                again.unwrap_or_default(),
                match (raw, again) {
                    (Some(a), Some(b)) if a == b => "NOT ticking",
                    (Some(_), Some(_)) => "ticking",
                    _ => "read failed",
                }
            );
        }
        Some(rtc)
    }

    fn read_rtc(&self) -> SystemTime {
        let epoch = SystemTime {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            nanos: 0,
        };
        let fields = {
            // Initial wall-clock setup runs before a Task exists. It cannot
            // block, and no runtime RTC operation has been published yet.
            let _guard = self.lock_access();
            _guard.and_then(|_guard| self.read_fields())
        };
        let Some(fields) = fields else {
            ostd::warn!("[k1-rtc] SPM8821 block read failed; reporting 1970");
            return epoch;
        };
        if aster_cmdline::diag_enabled("rtc") {
            // `hwclock` polls RTC_RD_TIME in a tight loop waiting for the
            // seconds to tick, so log only when the value changes.
            static LAST: SpinLock<[u8; 6]> = SpinLock::new([0xff; 6]);
            let raw = fields.to_raw();
            let changed = {
                let mut last = LAST.lock();
                let changed = *last != raw;
                *last = raw;
                changed
            };
            if changed {
                ostd::info!("[k1-rtc] diag: read raw {:02x?}", raw);
            }
        }
        match fields.to_system_time() {
            Some(t) => t,
            None => {
                ostd::warn!(
                    "[k1-rtc] SPM8821 calendar implausible ({:?}); reporting 1970",
                    fields
                );
                epoch
            }
        }
    }

    fn write_rtc(&self, time: SystemTime) -> Result<(), RtcWriteError> {
        let fields = Fields::from_system_time(time).ok_or(RtcWriteError::InvalidTime)?;
        let _guard = self.lock_access().ok_or(RtcWriteError::Hardware)?;

        let ctl = k1_musebook_i2c::pmic_read(REG_CTL).ok_or(RtcWriteError::Hardware)?;
        let disabled = k1_musebook_i2c::pmic_write(REG_CTL, ctl & !CTL_RTC_EN);
        let result = if disabled {
            self.write_and_verify(fields)
        } else {
            // A failed transfer may still have changed the control register.
            Err(RtcWriteError::Hardware)
        };

        // Re-enable even if the fields did not land: a stopped RTC is the
        // one outcome strictly worse than a wrong one.
        let enabled = k1_musebook_i2c::pmic_write(REG_CTL, ctl | CTL_RTC_EN);
        drop(_guard);
        ostd::info!(
            "[k1-rtc] update {:04}-{:02}-{:02} {:02}:{:02}:{:02}: verify={:?} enabled={}",
            time.year,
            time.month,
            time.day,
            time.hour,
            time.minute,
            time.second,
            result,
            enabled
        );

        match (result, enabled) {
            (Ok(()), true) => Ok(()),
            (Ok(()), false) => {
                ostd::warn!("[k1-rtc] SPM8821 fields landed but rtc_en re-set failed");
                Err(RtcWriteError::Hardware)
            }
            (Err(err), _) => {
                ostd::warn!(
                    "[k1-rtc] SPM8821 write did not read back {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    time.year,
                    time.month,
                    time.day,
                    time.hour,
                    time.minute,
                    time.second
                );
                Err(err)
            }
        }
    }
}

impl RtcSpm8821 {
    /// Runtime RTC ioctls run in a sleepable task; bootstrap uses try_lock.
    fn lock_access(&self) -> Option<MutexGuard<'_, ()>> {
        if Task::current().is_none() {
            self.write_lock.try_lock()
        } else {
            Some(self.write_lock.lock())
        }
    }

    /// Six raw registers, masked, once both passes agree.
    fn read_fields(&self) -> Option<Fields> {
        let mut prev = read_pass()?;
        for _ in 0..READ_STABLE_TRIES {
            let cur = read_pass()?;
            if cur == prev {
                return Some(unpack_fields(cur));
            }
            prev = cur;
        }
        // No stable snapshot: the last sample can still straddle a rollover.
        None
    }

    fn write_and_verify(&self, fields: Fields) -> Result<(), RtcWriteError> {
        let values = fields.to_raw();
        for _ in 0..WRITE_VERIFY_TRIES {
            // Whole calendar in one transaction, like regmap_bulk_write.
            // The upper bits of these registers are not documented as
            // anything; mainline writes the bare field values too.
            if !k1_musebook_i2c::pmic_write_block(REGS[0].0, &values) {
                return Err(RtcWriteError::Hardware);
            }
            let back = read_pass().ok_or(RtcWriteError::Hardware)?;
            if back == values {
                return Ok(());
            }
        }
        Err(RtcWriteError::Hardware)
    }
}

/// One block read of the six calendar registers, each masked to its field.
fn read_pass() -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    if !k1_musebook_i2c::pmic_read_block(REGS[0].0, &mut out) {
        return None;
    }
    for (slot, (_, mask)) in out.iter_mut().zip(REGS.iter()) {
        *slot &= mask;
    }
    Some(out)
}

fn busy_wait_ms(ms: u64) {
    use ostd::timer::Jiffies;
    let start = Jiffies::elapsed().as_u64();
    let mut spins = 0u64;
    while Jiffies::elapsed().as_u64().saturating_sub(start) < ms {
        spins += 1;
        if spins > 200_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
}
