// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: last-reset-cause dump (SoC WDT + SPM8821 sticky flags).

//! Last-reset-cause dump. Read-only: writing EVENT / PWR_STS / WSR would
//! clear the bits we are trying to see on the reboot after a hang.

use ostd::arch::ccu;

const WDT_WMER: usize = 0xb8;
const WDT_WMR: usize = 0xbc;
const WDT_WSR: usize = 0xc0;

/// `MPMU_APRR` / `MPMU_ARSR` as offsets from the CCU gate page
/// (`0xd4051000`). Linux `k1x_wdt.c` quotes them from MPMU base.
const MPMU_APRR: usize = 0x020;
const MPMU_ARSR: usize = 0x028;
const MPMU_APRR_WDTR: u32 = 1 << 4;
const MPMU_ARSR_SWR_MASK: u32 = 0xf << 8;

/// SPM8821 / P1 on TWSI8 @ 0x41.
const PMIC_WDT_CTRL: u8 = 0x44;
const PMIC_PWR_CTRL0: u8 = 0x7c;
const PMIC_PWR_STS1: u8 = 0x80;
const PMIC_EVENT1: u8 = 0x92;
const PMIC_EVENT2: u8 = 0x93;
const PMIC_BUCK_EV0: u8 = 0x94;
const PMIC_BUCK_EV1: u8 = 0x95;
const PMIC_BUCK_EV2: u8 = 0x96;
const PMIC_PWRKY_EV: u8 = 0x97;
const PMIC_IRQ_PWRKY_EN: u8 = 0x9e;
const PMIC_PROT_EN: u8 = 0x9f;

pub(super) fn dump() {
    dump_soc_wdt();
    dump_pmic();
}

fn dump_soc_wdt() {
    let Some(wsr) = ccu::wdt_read(WDT_WSR) else {
        log::warn!("[k1-rst] WDT unread (CCU did not map it)");
        return;
    };
    let wmer = ccu::wdt_read(WDT_WMER).unwrap_or(0);
    let wmr = ccu::wdt_read(WDT_WMR).unwrap_or(0);
    let wts = wsr & 1;
    let we = wmer & 1;
    let match_s = crate::convert::wdt_match_ticks_to_seconds(wmr & 0xffff);

    let aprr = ccu::mpmu_gate_read(MPMU_APRR).unwrap_or(0);
    let arsr = ccu::mpmu_gate_read(MPMU_ARSR).unwrap_or(0);
    let swr = (arsr & MPMU_ARSR_SWR_MASK) >> 8;
    let wdtr = (aprr & MPMU_APRR_WDTR) != 0;

    if wts != 0 {
        log::info!(
            "[k1-rst] last SoC reset was the 16-bit WDT (WTS=1, ~{}s match, ARSR swr=0x{:x} WDTR={})",
            match_s,
            swr,
            wdtr as u8
        );
    } else {
        log::info!(
            "[k1-rst] last SoC reset was not the 16-bit WDT (WTS=0, ARSR swr=0x{:x} WDTR={})",
            swr,
            wdtr as u8
        );
    }
    if we != 0 {
        log::info!("[k1-rst] WMER.WE=1 — SoC WDT is enabled");
    }
}

fn dump_pmic() {
    let Some(wdt_ctrl) = k1_musebook_i2c::pmic_read(PMIC_WDT_CTRL) else {
        log::warn!("[k1-rst] pmic unread (TWSI8 not up)");
        return;
    };
    let pwr_ctrl0 = k1_musebook_i2c::pmic_read(PMIC_PWR_CTRL0).unwrap_or(0);
    let pwr_sts1 = k1_musebook_i2c::pmic_read(PMIC_PWR_STS1).unwrap_or(0);
    let ev1 = k1_musebook_i2c::pmic_read(PMIC_EVENT1).unwrap_or(0);
    let ev2 = k1_musebook_i2c::pmic_read(PMIC_EVENT2).unwrap_or(0);
    let buck_ov = k1_musebook_i2c::pmic_read(PMIC_BUCK_EV0).unwrap_or(0);
    let buck_uv = k1_musebook_i2c::pmic_read(PMIC_BUCK_EV1).unwrap_or(0);
    let buck_sc = k1_musebook_i2c::pmic_read(PMIC_BUCK_EV2).unwrap_or(0);
    let pwrky = k1_musebook_i2c::pmic_read(PMIC_PWRKY_EV).unwrap_or(0);
    let irq_pwrky = k1_musebook_i2c::pmic_read(PMIC_IRQ_PWRKY_EN).unwrap_or(0);
    let prot = k1_musebook_i2c::pmic_read(PMIC_PROT_EN).unwrap_or(0);

    let wdt_en = (wdt_ctrl >> 3) & 1;
    let wdt_scale = (wdt_ctrl >> 1) & 3;
    let wdt_s = match wdt_scale {
        0 => 1,
        1 => 4,
        2 => 8,
        _ => 16,
    };
    let wdt_rst_en = (pwr_ctrl0 >> 7) & 1;

    log::info!(
        "[k1-rst] pmic WDT_CTRL={:#04x} EN={} SCALE={}s WDT_RST_EN={} PWR_STS1={:#04x}",
        wdt_ctrl,
        wdt_en,
        wdt_s,
        wdt_rst_en,
        pwr_sts1
    );

    pmic_bit(ev1, 3, "E_WDT_TO (PMIC watchdog timeout, max 16s)");
    pmic_bit(ev2, 6, "E_TEMP_CRIT (PMIC 135/150C shutdown alarm)");
    pmic_bit(ev2, 5, "E_TEMP_SEVERE");
    pmic_bit(ev2, 4, "E_TEMP_WARN");
    pmic_bit(ev2, 2, "E_LDO_SC");
    pmic_bit(ev2, 1, "E_LDO_UV");
    pmic_bit(ev2, 0, "E_LDO_OV");
    pmic_bit(buck_uv, 0xff, "E_BUCKn_UV");
    pmic_bit(buck_sc, 0xff, "E_BUCKn_SC");
    pmic_bit(buck_ov, 0xff, "E_BUCKn_OV");
    pmic_bit(pwrky, 5, "E_VSYS_OV");
    pmic_bit(pwr_sts1, 4, "FLAG_PWRCTRL_SHUT");
    pmic_bit(pwr_sts1, 3, "FLAG_PWRKY_SHUT");
    pmic_bit(pwr_sts1, 2, "FLAG_VSYS_SHUT (VSYS low)");
    pmic_bit(pwr_sts1, 1, "FLAG_ERR_SHUT (overtemp/OV/UV/short)");
    pmic_bit(pwr_sts1, 0, "FLAG_SW_SHUT");
    pmic_bit(irq_pwrky, 6, "TEMP_CRIT_PROT enabled (MTP may set this)");
    pmic_bit(irq_pwrky, 7, "VSYS_OVP_EN enabled");
    pmic_bit(prot, 6, "TEMP_SEVERE_PROT enabled");
    pmic_bit(prot, 5, "BUCK_SCP_EN enabled");
    pmic_bit(prot, 4, "BUCK_UVP_EN enabled");
}

fn pmic_bit(val: u8, bit_or_any: u8, name: &str) {
    let hit = if bit_or_any == 0xff {
        val != 0
    } else {
        val & (1 << bit_or_any) != 0
    };
    if hit {
        log::warn!("[k1-rst] pmic flag {} (val={:#04x})", name, val);
    }
}
