// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: LT8911EXB MIPI-DSI → eDP bridge (probe + enable_edp).

//! Lontium LT8911EXB (MIPI-DSI → eDP) — Muse Book bridge driver.
//!
//! Board wiring (U-Boot / Linux `k1-x_MUSE-Book.dts`):
//! - I2C6 @ `0xd4018800`, device `0x29`
//! - `standby-gpios` = GPIO92, `enable-gpios` = GPIO83
//! - `reset-gpios` = GPIO114, `bl-gpios` = GPIO75
//!
//! Probe powers and resets the bridge, then reads its chip ID.
//! `enable_edp` configures the video path and trains the link after DPU/DSI setup.

use super::k1_twsi::{self, TwsiBus};

const LT8911_ADDR: u8 = 0x29;

const GPIO_STANDBY: u32 = 92;
const GPIO_ENABLE: u32 = 83;
const GPIO_RESET: u32 = 114;
const GPIO_BL: u32 = 75;

/// Expected chip ID bytes at bank `0x81` regs `0x00..0x02` (U-Boot comment `0xe0517`).
const CHIP_ID_L: u8 = 0x17;
const CHIP_ID_M: u8 = 0x05;
const CHIP_ID_H: u8 = 0xe0;

pub(super) fn probe(bus: &TwsiBus) {
    log::info!("[k1-lt8911] power/reset sequence (GPIO92/83/114) ...");
    if !power_on() {
        log::warn!("[k1-lt8911] GPIO drive failed (is aster-gpio ready?)");
        return;
    }

    log::info!(
        "[k1-lt8911] chip ID via {} @0x{:02x} ...",
        bus.name(),
        LT8911_ADDR
    );
    match read_chip_id(bus) {
        Ok((l, m, h)) => {
            let ok = l == CHIP_ID_L && m == CHIP_ID_M && h == CHIP_ID_H;
            log::info!(
                "[k1-lt8911] chipid 0x{:02x}{:02x}{:02x} {}",
                h,
                m,
                l,
                if ok {
                    "OK (LT8911EXB)"
                } else {
                    "(unexpected — continue anyway)"
                }
            );
        }
        Err(e) => log::warn!("[k1-lt8911] chip ID failed: {}", e),
    }
}

fn power_on() -> bool {
    // U-Boot `edp_panel_probe`: standby pulse, enable high, then reset pulse.
    if !k1_musebook_gpio::set_value(GPIO_STANDBY, 1) {
        return false;
    }
    k1_twsi::delay_ms(50);
    if !k1_musebook_gpio::set_value(GPIO_STANDBY, 0) {
        return false;
    }
    k1_twsi::delay_ms(50);
    if !k1_musebook_gpio::set_value(GPIO_STANDBY, 1) {
        return false;
    }
    k1_twsi::delay_ms(100);

    if !k1_musebook_gpio::set_value(GPIO_ENABLE, 1) {
        return false;
    }

    // Reset_LT8911EXB: 1 → 0 → 1
    if !k1_musebook_gpio::set_value(GPIO_RESET, 1) {
        return false;
    }
    k1_twsi::delay_ms(20);
    if !k1_musebook_gpio::set_value(GPIO_RESET, 0) {
        return false;
    }
    k1_twsi::delay_ms(20);
    if !k1_musebook_gpio::set_value(GPIO_RESET, 1) {
        return false;
    }
    k1_twsi::delay_ms(50);

    // Panel BL GPIO is driven later by `enable_edp` once link training
    // succeeds (matches U-Boot: the panel's `probe()` only powers the chip;
    // `enable_backlight()` is a separate, later call).
    true
}

fn read_chip_id(bus: &TwsiBus) -> Result<(u8, u8, u8), &'static str> {
    // Select register bank 0x81, unlock 0x08=0x7f, then read ID.
    bus.write_reg8(LT8911_ADDR, 0xff, 0x81)?;
    bus.write_reg8(LT8911_ADDR, 0x08, 0x7f)?;
    let l = bus.read_reg8(LT8911_ADDR, 0x00)?;
    let m = bus.read_reg8(LT8911_ADDR, 0x01)?;
    let h = bus.read_reg8(LT8911_ADDR, 0x02)?;
    Ok((l, m, h))
}

// ---------------------------------------------------------------------------
// eDP initialization: MIPI-DSI RX -> PCR -> eDP TX -> link training.
//
// Ported from U-Boot `lcd_lt8911ext_edp_1080p.c` / `spacemit_edp.c`
// (`edp_panel_enable_backlight`), fixed for the Muse Book's 1920x1080 eDP
// panel (`eDP_lane` = 2, `_eDP_2G7_`, 4 MIPI lanes, 8bpc). EDID read, MIPI
// "video_check", and DPCD/PCR status polling are debug-only in the vendor
// driver and are skipped here.

/// `MIPI_Timing[]` from the vendor lcd file — must match `k1_edp` DPU/DSI
/// porches: `{hfp48, hs32, hbp200, hact1920, htotal2200, vfp3, vs6, vbp31,
/// vact1080, vtotal1120}`. The pixel clock is *not* a constant here: it is
/// whatever `ccu::enable_lcd_dsi` actually programmed, passed in by the
/// caller (PCR must track the real pxclk or dessc locks poorly).
const MIPI_HFP: u16 = 48;
const MIPI_HS: u16 = 32;
const MIPI_HBP: u16 = 200;
const MIPI_HACT: u16 = 1920;
const MIPI_HTOTAL: u16 = 2200;
const MIPI_VFP: u16 = 3;
const MIPI_VS: u16 = 6;
const MIPI_VBP: u16 = 31;
const MIPI_VACT: u16 = 1080;
const MIPI_VTOTAL: u16 = 1120;

/// `#define eDP_lane 2` — this bridge always outputs 2 eDP lanes regardless
/// of the 4-lane MIPI input.
const EDP_LANE: u8 = 2;
/// `#define PCR_PLL_PREDIV 0x40`.
const PCR_PLL_PREDIV: u8 = 0x40;
/// `Level = _Level7_` (15 mA swing, the vendor default for this panel).
const SWING_TAP0_MSB: u8 = 0x82;
const SWING_TAP0_LSB: u8 = 0x00;

/// Runs the full LT8911EXB eDP initialization + link training, then asserts the
/// bridge-side backlight-enable GPIO. Returns `true` if link training
/// reported success.
///
/// Mirrors U-Boot `edp_panel_enable_backlight()`, skipping EDID read,
/// `LT8911EXB_video_check`, and the `DpcdRead`/`PCR_Status` debug polls.
pub(super) fn enable_edp(bus: &TwsiBus, pclk_10khz: u32) -> bool {
    match configure_edp(bus, pclk_10khz) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("[k1-lt8911] eDP initialization failed: {}", err);
            false
        }
    }
}

fn configure_edp(bus: &TwsiBus, pclk_10khz: u32) -> Result<(), &'static str> {
    if pclk_10khz == 0 || pclk_10khz > 20_000 {
        return Err("unsupported pixel clock");
    }
    reset_pulse()?;
    if read_chip_id(bus)? != (CHIP_ID_L, CHIP_ID_M, CHIP_ID_H) {
        return Err("unexpected bridge chip ID");
    }
    edp_video_cfg(bus)?;
    lt8911_init(bus, pclk_10khz)?;
    tx_swing_preset(bus)?;
    link_train(bus)?;
    k1_twsi::delay_ms(100);
    if !link_train_result_check(bus)? {
        return Err("link training timeout");
    }
    wait_pcr_stable(bus)?;
    if !k1_musebook_gpio::set_value(GPIO_BL, 1) {
        return Err("backlight GPIO unavailable");
    }
    log::info!("[k1-lt8911] eDP link and PCR stable; backlight enabled");
    Ok(())
}

/// Poll `Reg0xD087` bit4 (`PCR Clock stable`) like vendor `PCR_Status()`.
/// Retries with a PCR reset once if lock does not hold within ~1s.
fn wait_pcr_stable(bus: &TwsiBus) -> Result<(), &'static str> {
    wb(bus, 0xff, 0xd0)?;
    let mut stable_ms = 0u32;
    let mut last = 0u8;
    for i in 0..40 {
        // up to ~2s
        let reg = rb(bus, 0x87)?;
        last = reg;
        let stable = reg & 0x10 != 0;
        if stable {
            stable_ms += 50;
            // Require four consecutive lock samples before enabling scanout.
            if stable_ms >= 200 {
                log::info!("[k1-lt8911] PCR stable (held {}ms)", stable_ms);
                // One extra beat after lock (vendor uses mdelay(80) around here).
                k1_twsi::delay_ms(80);
                return Ok(());
            }
        } else {
            stable_ms = 0;
            if i == 20 {
                // Still unlocked after ~1s: pulse a PCR reset (bank 0x81 reg
                // 0x03) and let the tracking loop start over.
                log::info!("[k1-lt8911] PCR retry — pulsing PCR reset");
                wb(bus, 0xff, 0x81)?;
                wb(bus, 0x03, 0x7b)?;
                wb(bus, 0x03, 0xff)?;
                wb(bus, 0xff, 0xd0)?;
            }
        }
        k1_twsi::delay_ms(50);
    }
    log::warn!(
        "[k1-lt8911] PCR did not hold stable (last 0xD087={:#04x})",
        last
    );
    Err("PCR stability timeout")
}

/// `Reset_LT8911EXB`: 1 -> 0 -> 1 pulse on the reset GPIO only (standby/enable
/// are already latched by [`probe`]'s power-on sequence).
fn reset_pulse() -> Result<(), &'static str> {
    if !k1_musebook_gpio::set_value(GPIO_RESET, 1) {
        return Err("reset GPIO unavailable");
    }
    k1_twsi::delay_ms(20);
    if !k1_musebook_gpio::set_value(GPIO_RESET, 0) {
        return Err("reset GPIO unavailable");
    }
    k1_twsi::delay_ms(20);
    if !k1_musebook_gpio::set_value(GPIO_RESET, 1) {
        return Err("reset GPIO unavailable");
    }
    k1_twsi::delay_ms(50);
    Ok(())
}

fn wb(bus: &TwsiBus, reg: u8, val: u8) -> Result<(), &'static str> {
    bus.write_reg8(LT8911_ADDR, reg, val)
}

fn rb(bus: &TwsiBus, reg: u8) -> Result<u8, &'static str> {
    bus.read_reg8(LT8911_ADDR, reg)
}

/// `LT8911EXB_MIPI_Video_Timing` (bank `0xd0`).
fn mipi_video_timing(bus: &TwsiBus) -> Result<(), &'static str> {
    wb(bus, 0xff, 0xd0)?;
    wb(bus, 0x0d, (MIPI_VTOTAL / 256) as u8)?;
    wb(bus, 0x0e, (MIPI_VTOTAL % 256) as u8)?;
    wb(bus, 0x0f, (MIPI_VACT / 256) as u8)?;
    wb(bus, 0x10, (MIPI_VACT % 256) as u8)?;
    wb(bus, 0x11, (MIPI_HTOTAL / 256) as u8)?;
    wb(bus, 0x12, (MIPI_HTOTAL % 256) as u8)?;
    wb(bus, 0x13, (MIPI_HACT / 256) as u8)?;
    wb(bus, 0x14, (MIPI_HACT % 256) as u8)?;
    wb(bus, 0x15, (MIPI_VS % 256) as u8)?;
    wb(bus, 0x16, (MIPI_HS % 256) as u8)?;
    wb(bus, 0x17, (MIPI_VFP / 256) as u8)?;
    wb(bus, 0x18, (MIPI_VFP % 256) as u8)?;
    wb(bus, 0x19, (MIPI_HFP / 256) as u8)?;
    wb(bus, 0x1a, (MIPI_HFP % 256) as u8)?;
    Ok(())
}

/// `LT8911EXB_eDP_Video_cfg` (bank `0xa8`, non-`_Msa_Active_Only_` path).
fn edp_video_cfg(bus: &TwsiBus) -> Result<(), &'static str> {
    wb(bus, 0xff, 0xa8)?;
    wb(bus, 0x2d, 0x88)?; // MSA from register

    wb(bus, 0x05, (MIPI_HTOTAL / 256) as u8)?;
    wb(bus, 0x06, (MIPI_HTOTAL % 256) as u8)?;
    wb(bus, 0x07, ((MIPI_HS + MIPI_HBP) / 256) as u8)?;
    wb(bus, 0x08, ((MIPI_HS + MIPI_HBP) % 256) as u8)?;
    wb(bus, 0x09, (MIPI_HS / 256) as u8)?;
    wb(bus, 0x0a, (MIPI_HS % 256) as u8)?;
    wb(bus, 0x0b, (MIPI_HACT / 256) as u8)?;
    wb(bus, 0x0c, (MIPI_HACT % 256) as u8)?;
    wb(bus, 0x0d, (MIPI_VTOTAL / 256) as u8)?;
    wb(bus, 0x0e, (MIPI_VTOTAL % 256) as u8)?;
    wb(bus, 0x11, ((MIPI_VS + MIPI_VBP) / 256) as u8)?;
    wb(bus, 0x12, ((MIPI_VS + MIPI_VBP) % 256) as u8)?;
    wb(bus, 0x14, (MIPI_VS % 256) as u8)?;
    wb(bus, 0x15, (MIPI_VACT / 256) as u8)?;
    wb(bus, 0x16, (MIPI_VACT % 256) as u8)?;
    Ok(())
}

/// `LT8911EXB_init`: MIPI RX analog/digital, dessc PCR PLL, eDP Tx PHY +
/// `_eDP_2G7_` TxPLL/SSC, then poll TxPLL lock. 8bpc (`_8bit_`), no test
/// pattern, `_Nvid` = 0.
fn lt8911_init(bus: &TwsiBus, pclk_10khz: u32) -> Result<(), &'static str> {
    // init
    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x08, 0x7f)?; // i2c over aux issue
    wb(bus, 0x49, 0xff)?; // enable 0x87xx

    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x5a, 0x0e)?; // GPIO test output

    // power consumption
    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x05, 0x06)?;
    wb(bus, 0x43, 0x00)?;
    wb(bus, 0x44, 0x1f)?;
    wb(bus, 0x45, 0xf7)?;
    wb(bus, 0x46, 0xf6)?;
    wb(bus, 0x49, 0x7f)?;

    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x12, 0x33)?; // eDP_lane == 2

    // mipi Rx analog
    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x32, 0x51)?;
    wb(bus, 0x35, 0x22)?; // EQ current
    wb(bus, 0x3a, 0x77)?; // EQ 12.5db
    wb(bus, 0x3b, 0x77)?; // EQ 12.5db
    wb(bus, 0x4c, 0x0c)?;
    wb(bus, 0x4d, 0x00)?;

    // dessc_pcr pll analog
    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x6a, 0x40)?;
    wb(bus, 0x6b, PCR_PLL_PREDIV)?;

    let pclk = pclk_10khz;
    let (reg_0x6e, pcr_pll_postdiv): (u8, u32) = if pclk < 8800 {
        (0x82, 8) // pre-div=2, pixel_clk = 44~88MHz
    } else if pclk < 17600 {
        (0x81, 4) // pre-div=1, pixel_clk = 88~176MHz
    } else {
        (0x80, 2) // pre-div=0, pixel_clk = 176~200MHz
    };
    wb(bus, 0x6e, reg_0x6e)?;
    // Round the adaptive PCR multiplier to the nearest integer, minimizing
    // its initial frequency error before the tracking loop acquires lock.
    let pcr_m = ((pclk * pcr_pll_postdiv + 1250) / 2500) as u8;
    log::info!(
        "[k1-lt8911] PCR pclk_10khz={} postdiv={} reg6e={:#x} pcr_m={}",
        pclk,
        pcr_pll_postdiv,
        reg_0x6e,
        pcr_m
    );

    // dessc pll digital
    wb(bus, 0xff, 0x85)?;
    wb(bus, 0xa9, 0x31)?;
    wb(bus, 0xaa, 0x17)?;
    wb(bus, 0xab, 0xba)?;
    wb(bus, 0xac, 0xe1)?;
    wb(bus, 0xad, 0x47)?;
    wb(bus, 0xae, 0x01)?;
    wb(bus, 0xae, 0x11)?;

    // Digital Top
    wb(bus, 0xff, 0x85)?;
    wb(bus, 0xc0, 0x01)?; // select mipi Rx
    wb(bus, 0xb0, 0x00)?; // 8bit: disable dither

    // mipi Rx Digital
    wb(bus, 0xff, 0xd0)?;
    wb(bus, 0x00, 0x00)?; // PN swap dis + 4-lane (0 % 4 == 0)
    wb(bus, 0x02, 0x08)?; // settle
    wb(bus, 0x03, 0x00)?; // data sequence: no swap
    wb(bus, 0x08, 0x00)?;
    wb(bus, 0x0c, 0x80)?; // fifo position
    wb(bus, 0x1c, 0x80)?; // fifo position
    wb(bus, 0x24, 0x70)?; // pcr mode (de/hs/vs)
    wb(bus, 0x31, 0x0a)?;

    // stage1 hs mode
    wb(bus, 0x25, 0x90)?;
    wb(bus, 0x2a, 0x3a)?;
    wb(bus, 0x21, 0x4f)?; // hs_step
    wb(bus, 0x22, 0xff)?;

    // stage2 de mode
    wb(bus, 0x0a, 0x02)?;
    wb(bus, 0x38, 0x02)?;
    wb(bus, 0x39, 0x04)?;
    wb(bus, 0x3a, 0x08)?;
    wb(bus, 0x3b, 0x10)?;
    wb(bus, 0x3f, 0x04)?;
    wb(bus, 0x40, 0x08)?;
    wb(bus, 0x41, 0x10)?;
    wb(bus, 0x42, 0x60)?;

    // stage2 hs mode
    wb(bus, 0x1e, 0x0a)?;
    wb(bus, 0x23, 0xf0)?;
    wb(bus, 0x2b, 0x80)?;

    wb(bus, 0x26, pcr_m)?; // no test pattern (bit7 clear)

    mipi_video_timing(bus)?;

    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x03, 0x7b)?; // PCR reset
    wb(bus, 0x03, 0xff)?;

    // _eDP_2G7_
    wb(bus, 0xff, 0x87)?;
    wb(bus, 0x19, 0x31)?;
    wb(bus, 0x1a, 0x36)?; // sync m
    wb(bus, 0x1b, 0x00)?; // sync_k [7:0]
    wb(bus, 0x1c, 0x00)?; // sync_k [13:8]

    // txpll analog
    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x09, 0x00)?; // div hardware mode, for ssc
    wb(bus, 0x02, 0x42)?;
    wb(bus, 0x03, 0x00)?; // txpll en = 0
    wb(bus, 0x03, 0x01)?; // txpll en = 1

    wb(bus, 0xff, 0x87)?;
    wb(bus, 0x0c, 0x10)?; // cal en = 0

    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x09, 0xfc)?;
    wb(bus, 0x09, 0xfd)?;

    wb(bus, 0xff, 0x87)?;
    wb(bus, 0x0c, 0x11)?; // cal en = 1

    // ssc
    wb(bus, 0xff, 0x87)?;
    wb(bus, 0x13, 0x83)?;
    wb(bus, 0x14, 0x41)?;
    wb(bus, 0x16, 0x0a)?;
    wb(bus, 0x18, 0x0a)?;
    wb(bus, 0x19, 0x33)?;

    // Check Tx PLL, retry the cal-en pulse on lock failure.
    wb(bus, 0xff, 0x87)?;
    let mut locked = false;
    for _ in 0..5 {
        k1_twsi::delay_ms(5);
        if rb(bus, 0x37)? & 0x02 != 0 {
            locked = true;
            break;
        }
        wb(bus, 0xff, 0x81)?;
        wb(bus, 0x09, 0xfc)?;
        wb(bus, 0x09, 0xfd)?;
        wb(bus, 0xff, 0x87)?;
        wb(bus, 0x0c, 0x10)?;
        wb(bus, 0x0c, 0x11)?;
    }
    log::info!(
        "[k1-lt8911] tx pll {}",
        if locked { "locked" } else { "unlocked" }
    );

    if !locked {
        return Err("TX PLL lock timeout");
    }

    // AUX reset
    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x07, 0xfe)?;
    wb(bus, 0x07, 0xff)?;
    wb(bus, 0x0a, 0xfc)?;
    wb(bus, 0x0a, 0xfe)?;

    // tx phy
    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x11, 0x00)?;
    wb(bus, 0x13, 0x10)?;
    wb(bus, 0x14, 0x0c)?;
    wb(bus, 0x14, 0x08)?;
    wb(bus, 0x13, 0x20)?;

    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x0e, 0x35)?;

    // eDP Tx Digital
    wb(bus, 0xff, 0xa8)?;
    wb(bus, 0x27, 0x10)?; // 0x10 = mipi video (not test pattern)
    // _8bit_
    wb(bus, 0x17, 0x10)?;
    wb(bus, 0x18, 0x20)?;

    // nvid (_Nvid == 0 -> 0x0080)
    wb(bus, 0xff, 0xa0)?;
    wb(bus, 0x00, 0x00)?;
    wb(bus, 0x01, 0x80)?;
    Ok(())
}

/// `LT8911EX_TxSwingPreSet` for `Level = _Level7_` (15 mA), 2 eDP lanes.
fn tx_swing_preset(bus: &TwsiBus) -> Result<(), &'static str> {
    wb(bus, 0xff, 0x82)?;
    wb(bus, 0x22, SWING_TAP0_MSB)?; // lane 0 tap0
    wb(bus, 0x23, SWING_TAP0_LSB)?;
    wb(bus, 0x24, 0x80)?; // lane 0 tap1
    wb(bus, 0x25, 0x00)?;
    wb(bus, 0x26, SWING_TAP0_MSB)?; // lane 1 tap0 (eDP_lane == 2)
    wb(bus, 0x27, SWING_TAP0_LSB)?;
    wb(bus, 0x28, 0x80)?; // lane 1 tap1
    wb(bus, 0x29, 0x00)?;
    Ok(())
}

/// `LT8911EX_link_train`, `ScrambleMode = 0` (DP scramble mode).
fn link_train(bus: &TwsiBus) -> Result<(), &'static str> {
    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x06, 0xdf)?; // reset VID TX
    wb(bus, 0x06, 0xff)?;

    wb(bus, 0xff, 0x85)?;
    wb(bus, 0xa1, 0x02)?; // DP scramble mode

    // Aux setup
    wb(bus, 0xff, 0xac)?;
    wb(bus, 0x00, 0x60)?;
    wb(bus, 0xff, 0xa6)?;
    wb(bus, 0x2a, 0x00)?;

    wb(bus, 0xff, 0x81)?;
    wb(bus, 0x07, 0xfe)?;
    wb(bus, 0x07, 0xff)?;
    wb(bus, 0x0a, 0xfc)?;
    wb(bus, 0x0a, 0xfe)?;

    // link train
    wb(bus, 0xff, 0x85)?;
    wb(bus, 0x1a, EDP_LANE)?;

    // _link_train_enable_
    wb(bus, 0xff, 0xac)?;
    wb(bus, 0x00, 0x64)?;
    wb(bus, 0x01, 0x0a)?;
    wb(bus, 0x0c, 0x85)?;
    wb(bus, 0x0c, 0xc5)?;
    Ok(())
}

/// `LT8911EXB_LinkTrainResultCheck`. Returns `true` on success (`0x82 & 0x1f
/// == 0x1e`).
fn link_train_result_check(bus: &TwsiBus) -> Result<bool, &'static str> {
    wb(bus, 0xff, 0xac)?;
    for _ in 0..20 {
        let val = rb(bus, 0x82)?;
        if val & 0x20 != 0 {
            if val & 0x1f == 0x1e {
                return Ok(true);
            }
            // Soft retrain pulse (U-Boot fallback path).
            wb(bus, 0xff, 0xac)?;
            wb(bus, 0x00, 0x00)?;
            wb(bus, 0x01, 0x0a)?;
            wb(bus, 0x14, 0x80)?;
            wb(bus, 0x14, 0x81)?;
            k1_twsi::delay_ms(50);
            wb(bus, 0x14, 0x84)?;
            k1_twsi::delay_ms(50);
            wb(bus, 0x14, 0xc0)?;
            k1_twsi::delay_ms(50);
        } else {
            k1_twsi::delay_ms(50);
        }
    }
    Ok(false)
}
