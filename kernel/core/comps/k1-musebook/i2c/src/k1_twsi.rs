// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: PXA-style TWSI polling controller (TWSI4/5/6/8).

//! SpacemiT K1 TWSI (PXA-style I2C) — polling bus + PMIC / touchpad initialization.
//!
//! - TWSI8 @ `0xd401d800`: spm8821 PMIC @ 0x41
//! - TWSI4 @ `0xd4012800`: Muse Book hid-over-i2c touchpad @ 0x2d
//! - TWSI5 @ `0xd4013800`: Muse Book SBS battery @ 0x0b
//! - TWSI6 @ `0xd4018800`: Muse Book LT8911EXB eDP bridge @ 0x29

use ostd::{
    arch::{boot::DEVICE_TREE, read_tsc, tsc_freq},
    io::IoMem,
    mm::{VmIoOnce, dma::dma_io_fence},
};
use spin::Once;

const TWSI8_BASE: usize = 0xd401_d800;
const TWSI4_BASE: usize = 0xd401_2800;
/// Linux `k1-x.dtsi` `i2c5: i2c@d4013800` and U-Boot `spacemit_i2c.c`
/// `i2c_base[5]`. U-Boot `K1X_TWSI5_BASE` (`0xD4020800`) disagrees and
/// is unused by that driver — do not follow the header.
const TWSI5_BASE: usize = 0xd401_3800;
const TWSI6_BASE: usize = 0xd401_8800;
const TWSI_SIZE: usize = 0x40;

const REG_CR: usize = 0x00;
const REG_SR: usize = 0x04;
const REG_SAR: usize = 0x08;
const REG_DBR: usize = 0x0c;
const REG_LCR: usize = 0x10;
const REG_WCR: usize = 0x14;

const ICR_START: u32 = 1 << 0;
const ICR_STOP: u32 = 1 << 1;
const ICR_ACKNAK: u32 = 1 << 2;
const ICR_TB: u32 = 1 << 3;
const ICR_UR: u32 = 1 << 10;
const ICR_SCLE: u32 = 1 << 13;
const ICR_IUE: u32 = 1 << 14;
const ICR_ALDIE: u32 = 1 << 18;
const ICR_ITEIE: u32 = 1 << 19;
const ICR_IRFIE: u32 = 1 << 20;
const ICR_GCD: u32 = 1 << 21;
const ICR_BEIE: u32 = 1 << 22;
const ICR_MODE_MASK: u32 = 0x300;

const ISR_ACKNAK: u32 = 1 << 14;
const ISR_IBB: u32 = 1 << 16;
const ISR_ITE: u32 = 1 << 19;
const ISR_IRF: u32 = 1 << 20;

const ICR_INIT_SM: u32 = ICR_BEIE | ICR_IRFIE | ICR_ITEIE | ICR_GCD | ICR_SCLE;
const ISR_INIT: u32 = 0x1fd_e000;
const I2C_LCR: u32 = 0x082c_469f;
const I2C_WCR: u32 = 0x142a;

/// TWSI side-band enable, inside the APBS block (`0xd409_0000` + 0x104).
const APBS_TWSI_SIDE_EN_OFF: usize = 0x104;
const PAD_EN_A: usize = 0xd401_e228;
const PAD_EN_B: usize = 0xd401_e22c;

const PMIC_ADDR: u8 = 0x41;
const SPM8821_ID_REG: u8 = 0x00;
const SPM8821_BUCK1_VSEL: u8 = 0x48;
const SPM8821_VERSION_ID: u8 = 0xa1;

/// Linux `K1X_PADCONF(..., MUX_MODE4, EDGE_NONE|PULL_UP|PAD_3V_DS2)`.
const MFPR_I2C4: u32 = 0x0000_d044;
/// `pinctrl_i2c5_1`: `MUX_MODE5 | EDGE_NONE | PULL_UP | PAD_1V8_DS2`.
const MFPR_I2C5: u32 = 0x0000_d045;
/// Linux `K1X_PADCONF(..., MUX_MODE5, EDGE_NONE|PULL_UP|PAD_1V8_DS0)` — i2c6_2.
const MFPR_I2C6: u32 = 0x0000_c045;

const POLL_US: u64 = 10;
const POLL_MAX: u32 = 1000;

/// Polling TWSI controller (one MMIO instance).
pub(super) struct TwsiBus {
    io: IoMem,
    name: &'static str,
}

static TWSI8: Once<TwsiBus> = Once::new();
static TWSI4: Once<TwsiBus> = Once::new();
static TWSI5: Once<TwsiBus> = Once::new();
static TWSI6: Once<TwsiBus> = Once::new();

pub(super) fn init() {
    let Some(dt) = DEVICE_TREE.get() else {
        return;
    };
    if dt
        .find_compatible(&["spacemit,k1-x", "spacemit,k1"])
        .is_none()
    {
        return;
    }

    if enable_twsi_fabric().is_err() {
        log::warn!("[k1-i2c] TWSI fabric/pad map skipped (already claimed or unavailable)");
    }

    init_twsi8_pmic();
    init_twsi4_touchpad();
    init_twsi5();
    init_twsi6_lt8911();
    super::k1_lid::probe();
}

fn init_twsi8_pmic() {
    k1_musebook_pinctrl::enable(k1_musebook_pinctrl::Periph::Twsi8);
    delay_us(10);

    let Ok(io) = IoMem::acquire(TWSI8_BASE..TWSI8_BASE + TWSI_SIZE) else {
        log::warn!("[k1-i2c] failed to map TWSI8 MMIO at {:#x}", TWSI8_BASE);
        return;
    };
    log::info!("[k1-i2c] TWSI8 mapped at {:#x}", TWSI8_BASE);

    let bus = TwsiBus { io, name: "TWSI8" };
    bus.reset();

    let id = bus.read_reg8(PMIC_ADDR, SPM8821_ID_REG);
    let vsel = bus.read_reg8(PMIC_ADDR, SPM8821_BUCK1_VSEL);
    let ver = bus.read_reg8(PMIC_ADDR, SPM8821_VERSION_ID);
    log::info!(
        "[k1-i2c] spm8821 @0x{:02x}: ID={:?} VSEL={:?} VER={:?}",
        PMIC_ADDR,
        id,
        vsel,
        ver
    );

    TWSI8.call_once(|| bus);
    super::k1_pmic_pwrkey::probe();
}

fn init_twsi4_touchpad() {
    // Muse Book: GPIO51/52 = I2C4 SCL/SDA (MUX4 + pull-up + 3V DS2).
    if k1_musebook_pinctrl::write_pad(51, MFPR_I2C4).is_err()
        || k1_musebook_pinctrl::write_pad(52, MFPR_I2C4).is_err()
    {
        log::warn!("[k1-i2c] TWSI4 pinctrl GPIO51/52 failed");
        return;
    }

    k1_musebook_pinctrl::enable(k1_musebook_pinctrl::Periph::Twsi4);
    delay_us(10);

    let Ok(io) = IoMem::acquire(TWSI4_BASE..TWSI4_BASE + TWSI_SIZE) else {
        log::warn!("[k1-i2c] failed to map TWSI4 MMIO at {:#x}", TWSI4_BASE);
        return;
    };
    log::info!("[k1-i2c] TWSI4 mapped at {:#x}", TWSI4_BASE);

    let bus = TwsiBus { io, name: "TWSI4" };
    bus.reset();
    TWSI4.call_once(|| bus);

    if let Some(b) = TWSI4.get() {
        super::k1_i2c_hid::probe_touchpad(b);
    }
}

fn init_twsi5() {
    // Muse Book: GPIO54/55 = I2C5 SCL/SDA (`pinctrl_i2c5_1`). Client lives
    // in `k1-musebook-power`; this crate only owns the controller.
    if k1_musebook_pinctrl::write_pad(54, MFPR_I2C5).is_err()
        || k1_musebook_pinctrl::write_pad(55, MFPR_I2C5).is_err()
    {
        log::warn!("[k1-i2c] TWSI5 pinctrl GPIO54/55 failed");
        return;
    }

    k1_musebook_pinctrl::enable(k1_musebook_pinctrl::Periph::Twsi5);
    delay_us(10);

    let Ok(io) = IoMem::acquire(TWSI5_BASE..TWSI5_BASE + TWSI_SIZE) else {
        log::warn!("[k1-i2c] failed to map TWSI5 MMIO at {:#x}", TWSI5_BASE);
        return;
    };
    log::info!("[k1-i2c] TWSI5 mapped at {:#x}", TWSI5_BASE);

    let bus = TwsiBus { io, name: "TWSI5" };
    bus.reset();
    TWSI5.call_once(|| bus);
}

fn init_twsi6_lt8911() {
    // Muse Book: GPIO56/57 = I2C6 SCL/SDA (`pinctrl_i2c6_2`).
    if k1_musebook_pinctrl::write_pad(56, MFPR_I2C6).is_err()
        || k1_musebook_pinctrl::write_pad(57, MFPR_I2C6).is_err()
    {
        log::warn!("[k1-i2c] TWSI6 pinctrl GPIO56/57 failed");
        return;
    }

    k1_musebook_pinctrl::enable(k1_musebook_pinctrl::Periph::Twsi6);
    delay_us(10);

    let Ok(io) = IoMem::acquire(TWSI6_BASE..TWSI6_BASE + TWSI_SIZE) else {
        log::warn!("[k1-i2c] failed to map TWSI6 MMIO at {:#x}", TWSI6_BASE);
        return;
    };
    log::info!("[k1-i2c] TWSI6 mapped at {:#x}", TWSI6_BASE);

    let bus = TwsiBus { io, name: "TWSI6" };
    bus.reset();
    TWSI6.call_once(|| bus);

    if let Some(b) = TWSI6.get() {
        super::k1_lt8911::probe(b);
    }
}

/// TWSI4 bus handle for the touchpad driver (timer poll).
pub(super) fn twsi4() -> Option<&'static TwsiBus> {
    TWSI4.get()
}

pub(super) fn twsi5() -> Option<&'static TwsiBus> {
    TWSI5.get()
}

pub(super) fn twsi6() -> Option<&'static TwsiBus> {
    TWSI6.get()
}

/// TWSI8 / SPM8821 after [`init`].
pub(super) fn twsi8() -> Option<&'static TwsiBus> {
    TWSI8.get()
}

fn enable_twsi_fabric() -> Result<(), ()> {
    // `MPMU_ACGR` is shared — bit 6 here, bit 15 is the GPU's clock parent.
    k1_musebook_pinctrl::acgr_rmw(0, 1 << 6).ok_or(())?;
    k1_musebook_pinctrl::apbs_rmw(APBS_TWSI_SIDE_EN_OFF, 0, 1 << 4).ok_or(())?;
    k1_musebook_pinctrl::or_mfpr_abs(PAD_EN_A, 1 << 1).map_err(|_| ())?;
    k1_musebook_pinctrl::or_mfpr_abs(PAD_EN_B, 1 << 1).map_err(|_| ())?;
    log::info!("[k1-i2c] TWSI fabric + pads enabled");
    Ok(())
}

impl TwsiBus {
    fn read32(&self, offset: usize) -> u32 {
        self.io.read_once(offset).unwrap()
    }

    fn write32(&self, offset: usize, val: u32) {
        dma_io_fence();
        self.io.write_once(offset, &val).unwrap();
        dma_io_fence();
    }

    fn modify_cr(&self, f: impl FnOnce(u32) -> u32) {
        let cur = self.read32(REG_CR);
        self.write32(REG_CR, f(cur));
    }

    pub(super) fn reset(&self) {
        let mode = self.read32(REG_CR) & ICR_MODE_MASK;
        self.modify_cr(|v| v & !ICR_IUE);
        self.write32(REG_CR, ICR_UR);
        delay_us(100);
        self.write32(REG_CR, 0);
        self.write32(REG_SAR, 0);
        self.write32(REG_LCR, I2C_LCR);
        self.write32(REG_WCR, I2C_WCR);
        self.write32(REG_CR, ICR_INIT_SM | mode);
        self.write32(REG_SR, ISR_INIT);
        self.modify_cr(|v| v | ICR_IUE);
        delay_us(100);
    }

    fn wait_isr(&self, set_mask: u32, clr_mask: u32) -> bool {
        let mut n = POLL_MAX;
        loop {
            let v = self.read32(REG_SR);
            if (v & set_mask) == set_mask && (v & clr_mask) == 0 {
                return true;
            }
            if n == 0 {
                return false;
            }
            n -= 1;
            delay_us(POLL_US);
        }
    }

    fn write_byte(
        &self,
        data: u8,
        cond_start: bool,
        cond_stop: bool,
        wait_ack: bool,
    ) -> Result<(), &'static str> {
        if !self.wait_isr(0, ISR_IBB) {
            return Err("bus busy");
        }
        self.modify_cr(|v| v & !(ICR_START | ICR_STOP));
        self.write32(REG_DBR, data as u32);
        if cond_start {
            self.modify_cr(|v| v | ICR_START);
        }
        if cond_stop {
            self.modify_cr(|v| v | ICR_STOP);
        }
        self.modify_cr(|v| (v & !ICR_ACKNAK) & !ICR_ALDIE);
        self.modify_cr(|v| v | ICR_TB);
        if !self.wait_isr(ISR_ITE, 0) {
            return Err("tx timeout");
        }
        self.write32(REG_SR, self.read32(REG_SR) | ISR_ITE);
        if wait_ack && !self.wait_isr(0, ISR_ACKNAK) {
            return Err("no ACK");
        }
        Ok(())
    }

    fn read_byte(&self, cond_stop: bool, send_nak: bool) -> Result<u8, &'static str> {
        if !self.wait_isr(0, ISR_IBB) {
            return Err("bus busy");
        }
        self.modify_cr(|v| v & !(ICR_START | ICR_STOP));
        if cond_stop {
            self.modify_cr(|v| v | ICR_STOP);
        }
        if send_nak {
            self.modify_cr(|v| v | ICR_ACKNAK);
        } else {
            self.modify_cr(|v| v & !ICR_ACKNAK);
        }
        self.modify_cr(|v| v & !ICR_ALDIE);
        self.modify_cr(|v| v | ICR_TB);
        if !self.wait_isr(ISR_IRF, 0) {
            return Err("rx timeout");
        }
        let data = self.read32(REG_DBR) as u8;
        self.write32(REG_SR, self.read32(REG_SR) | ISR_IRF);
        Ok(data)
    }

    /// SMBus read-word: command byte, then LE payload (SBS / SMBus spec).
    pub(super) fn read_reg16(&self, addr7: u8, reg: u8) -> Result<u16, &'static str> {
        let mut rd = [0u8; 2];
        self.write_then_read(addr7, &[reg], &mut rd)?;
        Ok(u16::from_le_bytes(rd))
    }

    /// SMBus 8-bit register read (PMIC).
    pub(super) fn read_reg8(&self, addr7: u8, reg: u8) -> Result<u8, &'static str> {
        self.reset();
        let addr_w = (addr7 << 1) & 0xfe;
        let addr_r = addr_w | 0x01;
        self.write_byte(addr_w, true, false, true)?;
        self.write_byte(reg, false, false, true)?;
        self.write_byte(addr_r, true, false, true)?;
        let val = self.read_byte(true, true)?;
        self.reset();
        Ok(val)
    }

    pub(super) fn write_reg8(&self, addr7: u8, reg: u8, val: u8) -> Result<(), &'static str> {
        self.reset();
        let addr_w = (addr7 << 1) & 0xfe;
        self.write_byte(addr_w, true, false, true)?;
        self.write_byte(reg, false, false, true)?;
        self.write_byte(val, false, true, true)?;
        self.reset();
        Ok(())
    }

    /// Write `wr` then repeated-start read into `rd` (i2c-hid register read).
    pub(super) fn write_then_read(
        &self,
        addr7: u8,
        wr: &[u8],
        rd: &mut [u8],
    ) -> Result<(), &'static str> {
        if wr.is_empty() || rd.is_empty() {
            return Err("empty xfer");
        }
        self.reset();
        let addr_w = (addr7 << 1) & 0xfe;
        let addr_r = addr_w | 0x01;
        self.write_byte(addr_w, true, false, true)?;
        for &b in wr {
            self.write_byte(b, false, false, true)?;
        }
        self.write_byte(addr_r, true, false, true)?;
        let n = rd.len();
        for (i, byte) in rd.iter_mut().enumerate() {
            let last = i + 1 == n;
            *byte = self.read_byte(last, last)?;
        }
        self.reset();
        Ok(())
    }

    /// Master write (command / SET_POWER).
    pub(super) fn write_all(&self, addr7: u8, wr: &[u8]) -> Result<(), &'static str> {
        if wr.is_empty() {
            return Err("empty write");
        }
        self.reset();
        let addr_w = (addr7 << 1) & 0xfe;
        self.write_byte(addr_w, true, false, true)?;
        for (i, &b) in wr.iter().enumerate() {
            let last = i + 1 == wr.len();
            self.write_byte(b, false, last, true)?;
        }
        self.reset();
        Ok(())
    }

    /// Master receive only (i2c-hid input report path).
    pub(super) fn read_all(&self, addr7: u8, rd: &mut [u8]) -> Result<(), &'static str> {
        if rd.is_empty() {
            return Err("empty read");
        }
        self.reset();
        let addr_r = ((addr7 << 1) & 0xfe) | 0x01;
        self.write_byte(addr_r, true, false, true)?;
        let n = rd.len();
        for (i, byte) in rd.iter_mut().enumerate() {
            let last = i + 1 == n;
            *byte = self.read_byte(last, last)?;
        }
        self.reset();
        Ok(())
    }

    pub(super) fn name(&self) -> &'static str {
        self.name
    }
}

fn delay_us(us: u64) {
    dma_io_fence();
    let ticks = (tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}

pub(super) fn delay_ms(ms: u64) {
    delay_us(ms.saturating_mul(1000));
}
