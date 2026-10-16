// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: TWSI / SPM8821 PMIC / power key / lid / LT8911 / I2C-HID.

//! I2C bus drivers for the SpacemiT K1 MUSE Book.
//!
//! - TWSI8: spm8821 PMIC probe + power key
//! - TWSI4: hid-over-i2c touchpad (ABS-MT, GPIO IRQ, REL fallback)
//! - TWSI5: SBS battery (client is `k1-musebook-power`)
//! - GPIO74: lid switch (`spacemit,k1x-lid`)

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use component::{ComponentInitError, init_component};

macro_rules! __log_prefix {
    () => {
        "i2c: "
    };
}

mod k1_i2c_hid;
mod k1_lid;
mod k1_lt8911;
pub mod k1_pmic_power;
mod k1_pmic_pwrkey;
mod k1_twsi;

use spin::Mutex;

/// Serializes every TWSI8 / SPM8821 byte xfer. The power-key poll, the
/// reset-reason dump, and `k1-musebook-cpufreq` dcdc1 all share this bus; the
/// controller resets around each transaction, so two overlapping xfers
/// would tear the start/stop. Battery is TWSI5 and does not take this lock.
///
/// It is a plain spin lock and IRQs stay on while it is held, so **nothing
/// that runs on the back of an interrupt may block on it**. Bottom halves
/// use [`pmic_try_lock`].
static PMIC_XFER: Mutex<()> = Mutex::new(());

/// Exclusive access to the PMIC for a bottom half: several transfers under
/// one acquisition, none of them blocking on the lock.
pub struct PmicGuard {
    bus: &'static k1_twsi::TwsiBus,
    _guard: spin::MutexGuard<'static, ()>,
}

impl PmicGuard {
    /// Read one SPM8821 register. `None` on a NAK / timeout.
    pub fn read(&self, reg: u8) -> Option<u8> {
        self.bus.read_reg8(0x41, reg).ok()
    }

    /// Write one SPM8821 register. `false` on a NAK / timeout.
    pub fn write(&self, reg: u8, val: u8) -> bool {
        self.bus.write_reg8(0x41, reg, val).is_ok()
    }
}

/// `None` if TWSI8 never probed **or someone else holds the bus right now**.
/// The caller is in interrupt-ish context and must come back later rather
/// than wait; PMIC event status is latched, so nothing is lost by skipping.
pub fn pmic_try_lock() -> Option<PmicGuard> {
    let bus = k1_twsi::twsi8()?;
    let guard = PMIC_XFER.try_lock()?;
    Some(PmicGuard { bus, _guard: guard })
}

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    k1_twsi::init();
    Ok(())
}

/// Runs the Muse Book LT8911EXB (MIPI-DSI -> eDP) bridge initialization + link
/// training over the already-initialized TWSI6 bus, then asserts the
/// bridge's backlight-enable GPIO. Intended to be called by the framebuffer
/// crate's eDP path once DSI/DPU are programmed. `pclk_10khz` is the actual
/// DPU pixel clock in 10 kHz units so the PCR PLL can track it.
pub fn lt8911_enable(pclk_10khz: u32) -> bool {
    match k1_twsi::twsi6() {
        Some(bus) => k1_lt8911::enable_edp(bus, pclk_10khz),
        None => false,
    }
}

/// Read one SPM8821 register on TWSI8. `None` if this is not a K1 or
/// the PMIC bus never came up.
pub fn pmic_read(reg: u8) -> Option<u8> {
    let _guard = PMIC_XFER.lock();
    k1_twsi::twsi8()?.read_reg8(0x41, reg).ok()
}

/// Write one SPM8821 register on TWSI8. `false` if this is not a K1 or
/// the PMIC bus never came up / the transfer failed.
pub fn pmic_write(reg: u8, val: u8) -> bool {
    let _guard = PMIC_XFER.lock();
    k1_twsi::twsi8().is_some_and(|bus| bus.write_reg8(0x41, reg, val).is_ok())
}

/// Read `buf.len()` consecutive SPM8821 registers starting at `reg` in
/// **one** I2C transaction (write `reg`, repeated START, read N) — what
/// Linux `regmap_bulk_read` does on this chip. The RTC calendar must be
/// read this way: the six registers are latched as a snapshot for the
/// duration of one transfer, not across six separate ones.
pub fn pmic_read_block(reg: u8, buf: &mut [u8]) -> bool {
    let _guard = PMIC_XFER.lock();
    k1_twsi::twsi8().is_some_and(|bus| bus.write_then_read(0x41, &[reg], buf).is_ok())
}

/// Write `vals` to consecutive SPM8821 registers starting at `reg` in one
/// transaction (`regmap_bulk_write`).
pub fn pmic_write_block(reg: u8, vals: &[u8]) -> bool {
    if vals.len() > 15 {
        return false;
    }
    let mut frame = [0u8; 16];
    frame[0] = reg;
    frame[1..=vals.len()].copy_from_slice(vals);
    let _guard = PMIC_XFER.lock();
    k1_twsi::twsi8().is_some_and(|bus| bus.write_all(0x41, &frame[..=vals.len()]).is_ok())
}

/// TWSI5 controller is mapped and out of reset. False on non-K1 or if
/// pinctrl / clock / MMIO failed — not the same as a battery NAK.
pub fn twsi5_up() -> bool {
    k1_twsi::twsi5().is_some()
}

/// SMBus read-word (LE) on TWSI5. `"no bus"` if [`twsi5_up`] is false.
pub fn twsi5_read_word(addr7: u8, cmd: u8) -> Result<u16, &'static str> {
    let Some(bus) = k1_twsi::twsi5() else {
        return Err("no bus");
    };
    bus.read_reg16(addr7, cmd)
}
