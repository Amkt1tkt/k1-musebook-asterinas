// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 DVFS clock / voltage helpers.

//! CPU cluster clock path (clst0 + clst1).
//!
//! The ostd CCU already owns APMU/APBS/MPMU `IoMem` windows
//! (`APMU_SIZE = 0x3cc` covers `APMU_CPU_C0_CLK_CTRL` @ 0x38C). A second
//! acquire would fail, so this module uses the existing public
//! [`ostd::arch::ccu`] accessors instead of mapping the blocks again.
//!
//! Layout from BSP `drivers/clk/spacemit/ccu-spacemit-k1x.c`:
//! - `APMU_CPU_C0_CLK_CTRL` = 0x38C, `APMU_CPU_C1_CLK_CTRL` = 0x390
//! - `cpu_c0_core_clk` / `cpu_c1_pclk`: `SPACEMIT_CCU_MUX_FC`, mux `[2:0]`,
//!   FC strobe `BIT(12)` (`CLK_DIV_TYPE_1REG_FC_V2`)
//! - `cpu_c0_hi_clk` / `cpu_c1_hi_clk`: mux bit 13 (`pll3_d2` / `pll3_d1`)
//! - ACE div `[8:6]`, TCM div `[11:9]` (C0 only), programmed value = divider-1
//!
//! Parents of the core mux (`cpu_c0_parent_names`):
//! 0 `pll1_d4_614p4`  1 `pll1_d3_819p2`  2 `pll1_d6_409p6`
//! 3 `pll1_d5_491p52` 4 `pll1_d2_1228p8` 5 `pll3_d3`
//! 6 `pll2_d3`        7 `cpu_c0_hi_clk`
//!
//! PLL3 (`APB_SPARE10/11/12` = APBS +0x124/128/12c, lock `MPMU_POSR` bit 29)
//! is only reprogrammed for 1.0 / 1.6 / 1.8 GHz. Sequence matches
//! `ccu_pll_set_rate`: disable → write cfg → enable → poll lock.
//! Never change PLL3 while a cluster is still muxed onto it.

use crate::opp::Opp;

/// `APMU_CPU_C0_CLK_CTRL` — `ccu-spacemit-k1x.c`.
#[cfg(target_arch = "riscv64")]
const APMU_CPU_C0_CLK_CTRL: usize = 0x38C;
/// `APMU_CPU_C1_CLK_CTRL`.
#[cfg(target_arch = "riscv64")]
const APMU_CPU_C1_CLK_CTRL: usize = 0x390;

const MUX_SHIFT: u32 = 0;
const MUX_MASK: u32 = 0x7;
const ACE_MASK: u32 = 0x7;
#[cfg(target_arch = "riscv64")]
const ACE_SHIFT: u32 = 6;
#[cfg(target_arch = "riscv64")]
const TCM_SHIFT: u32 = 9;
#[cfg(target_arch = "riscv64")]
const TCM_MASK: u32 = 0x7;
/// `SPACEMIT_CCU_MUX_FC(..., BIT(12), 0, 3, ...)`.
#[cfg(target_arch = "riscv64")]
const FC_BIT: u32 = 1 << 12;
/// `cpu_c0_hi` mux bit 13: 0 = `pll3_d2`, 1 = `pll3_d1`.
const HI_SHIFT: u32 = 13;
const HI_MASK: u32 = 0x1;

const MUX_PLL1_D4: u32 = 0;
const MUX_PLL1_D3: u32 = 1;
const MUX_PLL1_D2: u32 = 4;
const MUX_PLL3_D3: u32 = 5;
const MUX_HI: u32 = 7;

const HI_PLL3_D2: u32 = 0;
const HI_PLL3_D1: u32 = 1;

/// `APB_SPARE10_REG` PLL3 `swcr1`.
#[cfg(target_arch = "riscv64")]
const APBS_PLL3_SWCR1: usize = 0x124;
/// `APB_SPARE11_REG` PLL3 `swcr2` (div1..div8 enables).
#[cfg(target_arch = "riscv64")]
const APBS_PLL3_SWCR2: usize = 0x128;
/// `APB_SPARE12_REG` PLL3 `swcr3` (`div_frc`, `div_int`, `pll_en`).
#[cfg(target_arch = "riscv64")]
const APBS_PLL3_SWCR3: usize = 0x12c;
/// `MPMU_POSR`.
#[cfg(target_arch = "riscv64")]
const MPMU_POSR: usize = 0x10;
/// `POSR_PLL3_LOCK`.
#[cfg(target_arch = "riscv64")]
const POSR_PLL3_LOCK: u32 = 1 << 29;
#[cfg(target_arch = "riscv64")]
const PLL_EN: u32 = 1 << 31;

/// `pll3_rate_tbl` `PLL_RATE(3200000000, 0x67, 0xdd, 0x50, 0x00, 0x43, 0xeaaaab)`.
#[cfg(target_arch = "riscv64")]
const PLL3_3200M_SWCR1: u32 = 0x0050_dd67;
#[cfg(target_arch = "riscv64")]
const PLL3_3200M_SWCR3: u32 = (0x43 << 24) | 0x00ea_aaab;
/// `PLL_RATE(3000000000, 0x66, 0xdd, 0x50, 0x00, 0x3f, 0xe00000)`.
#[cfg(target_arch = "riscv64")]
const PLL3_3000M_SWCR1: u32 = 0x0050_dd66;
#[cfg(target_arch = "riscv64")]
const PLL3_3000M_SWCR3: u32 = (0x3f << 24) | 0x00e0_0000;
/// `PLL_RATE(1800000000, 0x61, 0xcd, 0x50, 0x00, 0x4b, 0x000000)`.
#[cfg(target_arch = "riscv64")]
const PLL3_1800M_SWCR1: u32 = 0x0050_cd61;
#[cfg(target_arch = "riscv64")]
const PLL3_1800M_SWCR3: u32 = 0x4b << 24;

/// `ccu_pll.c` `PLL_DELAYTIME` (590 × 5 µs) plus the initial 50 µs wait.
#[cfg(target_arch = "riscv64")]
const PLL_LOCK_POLLS: u32 = 600;

/// How we drive one cluster to a given OPP. Pure; the MMIO layer applies it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MuxPlan {
    pub core_mux: u32,
    pub hi_sel: u32,
    /// `None` = do not touch PLL3.
    pub pll3_hz: Option<u64>,
}

/// Clock request derived from an OPP. Isolated from MMIO so ktests can
/// pin the mux/PLL pairing against the BSP notifier.
pub(crate) fn plan_for(opp: &Opp) -> Option<MuxPlan> {
    match opp.khz {
        614_400 => Some(MuxPlan {
            core_mux: MUX_PLL1_D4,
            hi_sel: HI_PLL3_D2,
            pll3_hz: None,
        }),
        819_000 => Some(MuxPlan {
            core_mux: MUX_PLL1_D3,
            hi_sel: HI_PLL3_D2,
            pll3_hz: None,
        }),
        // The PLL3/3 parent needs an explicit 3.0 GHz PLL configuration.
        1_000_000 => Some(MuxPlan {
            core_mux: MUX_PLL3_D3,
            hi_sel: HI_PLL3_D2,
            pll3_hz: Some(3_000_000_000),
        }),
        1_228_800 => Some(MuxPlan {
            core_mux: MUX_PLL1_D2,
            hi_sel: HI_PLL3_D2,
            pll3_hz: None,
        }),
        // `spacemit_processor_notifier` PRECHANGE: PLL3 = 3.2 G, HI = 1.6 G
        // (`TURBO0_FREQUENCY` / `TURBO1_FREQUENCY`).
        1_600_000 => Some(MuxPlan {
            core_mux: MUX_HI,
            hi_sel: HI_PLL3_D2,
            pll3_hz: Some(3_200_000_000),
        }),
        // PRECHANGE `freqs->new > TURBO0`: PLL3 = 1.8 G, HI = pll3_d1.
        1_800_000 => Some(MuxPlan {
            core_mux: MUX_HI,
            hi_sel: HI_PLL3_D1,
            pll3_hz: Some(1_800_000_000),
        }),
        _ => None,
    }
}

/// Divider field for ACE/TCM. Programmed value is `divider - 1`
/// (`ccu_mix_calc_best_rate`).
pub(crate) fn div_field(parent_khz: u32, child_khz: u32) -> u32 {
    if child_khz == 0 {
        return 1;
    }
    let div = parent_khz.div_ceil(child_khz).max(1);
    (div - 1).min(ACE_MASK)
}

/// Recalc kHz from a raw `APMU_CPU_C*_CLK_CTRL` word and the PLL3 rate
/// we last programmed (0 if unknown — HI / pll3_d3 then report 0).
pub(crate) fn recalc_khz(reg: u32, pll3_hz: u64) -> u32 {
    let mux = (reg >> MUX_SHIFT) & MUX_MASK;
    match mux {
        MUX_PLL1_D4 => 614_400,
        MUX_PLL1_D3 => 819_200, // PLL is 819.2; OPP spells 819000
        2 => 409_600,
        3 => 491_520,
        MUX_PLL1_D2 => 1_228_800,
        MUX_PLL3_D3 => (pll3_hz / 3 / 1000) as u32,
        6 => 0, // pll2_d3 — not used by the OPP table
        MUX_HI => {
            let hi = (reg >> HI_SHIFT) & HI_MASK;
            let div = if hi == HI_PLL3_D1 { 1 } else { 2 };
            (pll3_hz / div / 1000) as u32
        }
        _ => 0,
    }
}

/// Whether the current mux is fed by PLL3 (must park before a PLL3 write).
pub(crate) fn mux_uses_pll3(reg: u32) -> bool {
    let mux = (reg >> MUX_SHIFT) & MUX_MASK;
    mux == MUX_PLL3_D3 || mux == MUX_HI
}

/// Apply `opp` to both clusters. On any MMIO / lock failure the clusters
/// are left on the safe PLL1 parent (614.4 MHz) if we already parked.
#[cfg(target_arch = "riscv64")]
pub(crate) fn set_rate(_old: &Opp, new: &Opp) -> Result<(), &'static str> {
    let Some(plan) = plan_for(new) else {
        return Err("no mux plan");
    };

    let c0 = ostd::arch::ccu::apmu_read(APMU_CPU_C0_CLK_CTRL).ok_or("apmu c0 unreadable")?;
    let c1 = ostd::arch::ccu::apmu_read(APMU_CPU_C1_CLK_CTRL).ok_or("apmu c1 unreadable")?;

    // PRECHANGE: ACE/TCM → current parent/2 (`spacemit_processor_notifier`
    // "binary division is safe"). Divider field 1 = /2.
    set_divs(c0, APMU_CPU_C0_CLK_CTRL, 1, Some(1))?;
    set_divs(c1, APMU_CPU_C1_CLK_CTRL, 1, None)?;

    if let Some(pll3) = plan.pll3_hz {
        park_if_on_pll3()?;
        set_pll3(pll3)?;
        // Ungate pll3_d1/d2/d3 (`SPACEMIT_CCU_GATE_FACTOR` bits 0..2).
        ostd::arch::ccu::apbs_rmw(APBS_PLL3_SWCR2, 0, 0x7).ok_or("pll3 swcr2")?;
        set_hi(plan.hi_sel)?;
    }

    set_core_mux(plan.core_mux)?;

    // POSTCHANGE: ACE/TCM to the DT rates.
    let ace = new.ace_khz.unwrap_or(new.khz / 2);
    let ace_div = div_field(new.khz, ace);
    let tcm = new.tcm_khz.unwrap_or(new.khz / 2);
    let tcm_div = div_field(new.khz, tcm);
    let c0 = ostd::arch::ccu::apmu_read(APMU_CPU_C0_CLK_CTRL).ok_or("apmu c0 unreadable")?;
    let c1 = ostd::arch::ccu::apmu_read(APMU_CPU_C1_CLK_CTRL).ok_or("apmu c1 unreadable")?;
    set_divs(c0, APMU_CPU_C0_CLK_CTRL, ace_div, Some(tcm_div))?;
    // C1 has ACE only (`cpu_c1_ace_clk`); TCM is C0.
    set_divs(c1, APMU_CPU_C1_CLK_CTRL, ace_div, None)?;

    delay_us((crate::opp::CLOCK_LATENCY_NS / 1000) as u64);
    Ok(())
}

/// Report a common live frequency only when both clusters agree.
#[cfg(target_arch = "riscv64")]
pub(crate) fn read_cur_khz() -> Option<u32> {
    let c0 = ostd::arch::ccu::apmu_read(APMU_CPU_C0_CLK_CTRL)?;
    let c1 = ostd::arch::ccu::apmu_read(APMU_CPU_C1_CLK_CTRL)?;
    let pll3 = pll3_rate_hz().unwrap_or(0);
    let khz = recalc_khz(c0, pll3);
    if khz != recalc_khz(c1, pll3) {
        return None;
    }
    if khz == 0 {
        None
    } else if khz == 819_200 {
        Some(819_000)
    } else {
        Some(khz)
    }
}

#[cfg(target_arch = "riscv64")]
fn park_if_on_pll3() -> Result<(), &'static str> {
    let c0 = ostd::arch::ccu::apmu_read(APMU_CPU_C0_CLK_CTRL).ok_or("apmu c0")?;
    let c1 = ostd::arch::ccu::apmu_read(APMU_CPU_C1_CLK_CTRL).ok_or("apmu c1")?;
    if mux_uses_pll3(c0) || mux_uses_pll3(c1) {
        // PLL1_D4 is always up (`CLK_IGNORE_UNUSED`). Safe parking lot.
        set_core_mux(MUX_PLL1_D4)?;
    }
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn set_core_mux(mux: u32) -> Result<(), &'static str> {
    set_one_mux(APMU_CPU_C0_CLK_CTRL, mux)?;
    set_one_mux(APMU_CPU_C1_CLK_CTRL, mux)?;
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn set_one_mux(off: usize, mux: u32) -> Result<(), &'static str> {
    let cur = ostd::arch::ccu::apmu_read(off).ok_or("apmu read")?;
    if ((cur >> MUX_SHIFT) & MUX_MASK) == (mux & MUX_MASK) {
        return Ok(());
    }
    ostd::arch::ccu::apmu_rmw(off, MUX_MASK << MUX_SHIFT, (mux & MUX_MASK) << MUX_SHIFT)
        .ok_or("apmu mux rmw")?;
    trigger_fc(off)
}

#[cfg(target_arch = "riscv64")]
fn set_hi(sel: u32) -> Result<(), &'static str> {
    ostd::arch::ccu::apmu_rmw(
        APMU_CPU_C0_CLK_CTRL,
        HI_MASK << HI_SHIFT,
        (sel & HI_MASK) << HI_SHIFT,
    )
    .ok_or("c0 hi")?;
    ostd::arch::ccu::apmu_rmw(
        APMU_CPU_C1_CLK_CTRL,
        HI_MASK << HI_SHIFT,
        (sel & HI_MASK) << HI_SHIFT,
    )
    .ok_or("c1 hi")?;
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn set_divs(cur: u32, off: usize, ace_div: u32, tcm_div: Option<u32>) -> Result<(), &'static str> {
    let ace = ace_div & ACE_MASK;
    let mut next = (cur & !(ACE_MASK << ACE_SHIFT)) | (ace << ACE_SHIFT);
    if let Some(tcm) = tcm_div {
        next = (next & !(TCM_MASK << TCM_SHIFT)) | ((tcm & TCM_MASK) << TCM_SHIFT);
    }
    if next == cur {
        return Ok(());
    }
    let clear = (ACE_MASK << ACE_SHIFT)
        | if tcm_div.is_some() {
            TCM_MASK << TCM_SHIFT
        } else {
            0
        };
    let set = (ace << ACE_SHIFT) | tcm_div.map_or(0, |tcm| (tcm & TCM_MASK) << TCM_SHIFT);
    ostd::arch::ccu::apmu_rmw(off, clear, set).ok_or("ace/tcm rmw")?;
    Ok(())
}

/// `ccu_mix_trigger_fc`: set `common->fc`, poll until hardware clears it.
#[cfg(target_arch = "riscv64")]
fn trigger_fc(off: usize) -> Result<(), &'static str> {
    ostd::arch::ccu::apmu_rmw(off, 0, FC_BIT).ok_or("fc set")?;
    // Tight poll first (BSP timeout = 50), then a longer one (5000).
    if poll_fc_clear(off, 50) || poll_fc_clear(off, 5000) {
        return Ok(());
    }
    log::warn!(
        "[k1-cpufreq] FC timeout off={:#x} reg={:#x}",
        off,
        ostd::arch::ccu::apmu_read(off).unwrap_or(0)
    );
    Err("pll/fc timeout")
}

#[cfg(target_arch = "riscv64")]
fn poll_fc_clear(off: usize, tries: u32) -> bool {
    for _ in 0..tries {
        match ostd::arch::ccu::apmu_read(off) {
            Some(v) if v & FC_BIT == 0 => return true,
            None => return false,
            Some(_) => delay_us(1),
        }
    }
    false
}

#[cfg(target_arch = "riscv64")]
fn set_pll3(hz: u64) -> Result<(), &'static str> {
    let (swcr1, swcr3) = match hz {
        3_200_000_000 => (PLL3_3200M_SWCR1, PLL3_3200M_SWCR3),
        3_000_000_000 => (PLL3_3000M_SWCR1, PLL3_3000M_SWCR3),
        1_800_000_000 => (PLL3_1800M_SWCR1, PLL3_1800M_SWCR3),
        _ => return Err("unknown pll3 rate"),
    };

    let cur3 = ostd::arch::ccu::apbs_read(APBS_PLL3_SWCR3).ok_or("pll3 swcr3")?;
    let cur1 = ostd::arch::ccu::apbs_read(APBS_PLL3_SWCR1).ok_or("pll3 swcr1")?;
    if cur1 == swcr1
        && (cur3 & !PLL_EN) == swcr3
        && cur3 & PLL_EN != 0
        && ostd::arch::ccu::mpmu_read(MPMU_POSR).is_some_and(|v| v & POSR_PLL3_LOCK != 0)
    {
        return Ok(());
    }

    // `ccu_pll_set_rate`: disable, program, enable.
    ostd::arch::ccu::apbs_rmw(APBS_PLL3_SWCR3, PLL_EN, 0).ok_or("pll3 disable")?;
    ostd::arch::ccu::apbs_rmw(APBS_PLL3_SWCR1, !0, swcr1).ok_or("pll3 swcr1")?;
    ostd::arch::ccu::apbs_rmw(APBS_PLL3_SWCR3, !0, swcr3).ok_or("pll3 swcr3")?;
    ostd::arch::ccu::apbs_rmw(APBS_PLL3_SWCR3, 0, PLL_EN).ok_or("pll3 enable")?;
    delay_us(50);

    for _ in 0..PLL_LOCK_POLLS {
        if ostd::arch::ccu::mpmu_read(MPMU_POSR).unwrap_or(0) & POSR_PLL3_LOCK != 0 {
            return Ok(());
        }
        delay_us(5);
    }
    log::warn!(
        "[k1-cpufreq] PLL3 lock timeout hz={} POSR={:#x}",
        hz,
        ostd::arch::ccu::mpmu_read(MPMU_POSR).unwrap_or(0)
    );
    Err("pll lock timeout")
}

#[cfg(target_arch = "riscv64")]
fn pll3_rate_hz() -> Option<u64> {
    let swcr1 = ostd::arch::ccu::apbs_read(APBS_PLL3_SWCR1)?;
    let swcr3 = ostd::arch::ccu::apbs_read(APBS_PLL3_SWCR3)?;
    if swcr3 & PLL_EN == 0 || ostd::arch::ccu::mpmu_read(MPMU_POSR)? & POSR_PLL3_LOCK == 0 {
        return None;
    }
    let swcr3 = swcr3 & !PLL_EN;
    if swcr1 == PLL3_3200M_SWCR1 && swcr3 == PLL3_3200M_SWCR3 {
        Some(3_200_000_000)
    } else if swcr1 == PLL3_3000M_SWCR1 && swcr3 == PLL3_3000M_SWCR3 {
        Some(3_000_000_000)
    } else if swcr1 == PLL3_1800M_SWCR1 && swcr3 == PLL3_1800M_SWCR3 {
        Some(1_800_000_000)
    } else {
        None
    }
}

#[cfg(target_arch = "riscv64")]
fn delay_us(us: u64) {
    let ticks = (ostd::arch::tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = ostd::arch::read_tsc();
    while ostd::arch::read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::opp::{STABLE_KHZ, lookup};

    #[ktest]
    fn mux_plan_matches_bsp_parents() {
        let p = plan_for(lookup(614_400).unwrap()).unwrap();
        assert_eq!(p.core_mux, 0);
        assert!(p.pll3_hz.is_none());

        let p = plan_for(lookup(819_000).unwrap()).unwrap();
        assert_eq!(p.core_mux, 1);

        let p = plan_for(lookup(1_228_800).unwrap()).unwrap();
        assert_eq!(p.core_mux, 4);
        assert_eq!(p.core_mux, MUX_PLL1_D2);
        assert_eq!(STABLE_KHZ, 1_228_800);

        let p = plan_for(lookup(1_000_000).unwrap()).unwrap();
        assert_eq!(p.core_mux, 5);
        assert_eq!(p.pll3_hz, Some(3_000_000_000));

        let p = plan_for(lookup(1_600_000).unwrap()).unwrap();
        assert_eq!(p.core_mux, 7);
        assert_eq!(p.hi_sel, 0);
        assert_eq!(p.pll3_hz, Some(3_200_000_000));

        let p = plan_for(lookup(1_800_000).unwrap()).unwrap();
        assert_eq!(p.core_mux, 7);
        assert_eq!(p.hi_sel, 1);
        assert_eq!(p.pll3_hz, Some(1_800_000_000));
    }

    #[ktest]
    fn ace_div_is_parent_over_child_minus_one() {
        assert_eq!(div_field(1_600_000, 800_000), 1);
        assert_eq!(div_field(1_228_800, 614_400), 1);
        assert_eq!(div_field(614_400, 307_200), 1);
        assert_eq!(div_field(1_000_000, 1_000_000), 0);
    }

    #[ktest]
    fn recalc_pll1_parents() {
        assert_eq!(recalc_khz(0, 0), 614_400);
        assert_eq!(recalc_khz(1, 0), 819_200);
        assert_eq!(recalc_khz(4, 0), 1_228_800);
        assert_eq!(recalc_khz(5, 3_000_000_000), 1_000_000);
        assert_eq!(recalc_khz(7, 3_200_000_000), 1_600_000);
        assert_eq!(recalc_khz(7 | (1 << 13), 1_800_000_000), 1_800_000);
        assert!(mux_uses_pll3(5));
        assert!(mux_uses_pll3(7));
        assert!(!mux_uses_pll3(4));
    }
}
