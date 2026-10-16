// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: DesignWare PCIe Root Complex for the PortC NVMe slot.

//! SpacemiT K1 (X60) PCIe Root Complex host bridge.
//!
//! The K1 SoC integrates Synopsys DesignWare (DWC) PCIe controllers rather than
//! a generic ECAM host bridge. Before the generic [`aster_pci`](crate) bus layer
//! can enumerate devices, the DWC Root Complex must be brought up by software:
//! clock/reset (via the APMU), the PUPHY (Rterm calibration + PLL lock), link
//! training (LTSSM), and the internal Address Translation Unit (iATU) that maps
//! CPU windows to PCIe configuration and memory space.
//!
//! Once the link is up, configuration accesses to the single downstream device
//! (bus 1) are served through the iATU CFG window, and this module exposes
//! [`cfg_read32`]/[`cfg_write32`] so the generic PCI layer can enumerate the
//! device as usual.
//!
//! The register sequences are cross-checked against the vendor U-Boot driver
//! (`drivers/pci/pcie_dw_k1x.c`, `pcie_dw_common.c`) and the reset controller
//! (`drivers/reset/reset-spacemit-k1x.c`).
//!
//! [`init`] brings up only PortC (M.2 NVMe on the MUSE-Book). PortB (`pcie1_rc`)
//! is left down: the board DTS marks it `status = "disabled"`, and WiFi is
//! out of scope for this port.
//!
//! `CLK_PCIE{0,1,2}` in the vendor CCU are the APMU `PCIE_CLK_RES_CTRL_*`
//! gate bits `[2:0]` (`0x7`). Those registers live at `0xd428_2bcc + port*8`,
//! which ostd's CCU deliberately does **not** claim (`APMU_SIZE = 0x3cc`) so
//! this driver can own them. There is no `ccu::Periph::Pcie1`; PortB reuses
//! the same APMU CMD_CLK_GATE path as PortC.

use core::ops::RangeInclusive;

use ostd::{
    Error,
    io::IoMem,
    mm::{VmIoOnce, dma::dma_io_fence},
};
use spin::Once;

use super::k1_logic::{
    ATU_ENABLE, ATU_TYPE_CFG0, ATU_TYPE_MEM, DOWNSTREAM_BUS, atu_prog, bdf_of, cfg_win_off_from_bdf,
};
use crate::PciDeviceLocation;

// ===================================================================
// Port layout (from the K1 device tree `pcie{0,1,2}_rc` nodes).
// ===================================================================

/// Register windows and lane count for the board's NVMe controller.
#[derive(Clone, Copy, Debug)]
struct PortDesc {
    /// APMU pair index (0/1/2) and `k1x,pcie-port`.
    index: usize,
    /// `num-lanes` (MUSE-Book overlays PortB to 1).
    lanes: usize,
    dbi: usize,
    atu: usize,
    cfg: usize,
    cfg_size: usize,
    phy: usize,
    mem_base: usize,
    mem_size: usize,
}

/// The Root Complex index wired to the NVMe slot on the MUSE-Book (PortC).
const RC: usize = 2;

/// PortC link width (Gen2 x2).
const RC_LANES: usize = 2;

/// PortC configuration aperture size (1 MiB), matching board firmware.
const CFG_WINDOW_SIZE: usize = 0x10_0000;

/// PortC windows used by the board firmware. The configuration aperture is
/// 1 MiB; the endpoint BAR window starts at 0xa0200000.
const PORTC: PortDesc = PortDesc {
    index: RC,
    lanes: RC_LANES,
    dbi: 0xca80_0000,
    atu: 0xcab0_0000,
    cfg: 0xa000_0000,
    cfg_size: CFG_WINDOW_SIZE,
    phy: 0xc0d1_0000,
    mem_base: 0xa020_0000,
    mem_size: 0x1600_0000,
};

/// Base of the APMU register block holding all ports' PCIe clock/reset/control
/// register pairs. Each port occupies 8 bytes: `+0x00` CMD, `+0x04` LOGIC.
const APMU_BLOCK_BASE: usize = 0xD428_2BCC;
const APMU_BLOCK_SIZE: usize = 0x20;
/// PortA (RC index 0) PUPHY, shared by all ports for Rterm calibration.
const PORTA_PHY_BASE: usize = 0xC0B1_0000;

/// Controller MMIO window size (DBI / iATU / PHY / PHY-AHB).
const CTRL_WIN: usize = 0x1000;

// ===================================================================
// APMU CMD / LOGIC register bits (PCIECTRL_K1X_CONF_DEVICE_CMD).
// ===================================================================

const CMD_CLK_GATE: u32 = 0x7; // bits[2:0] AXI clock enables
const CMD_RST_SUB: u32 = (1 << 3) | (1 << 4) | (1 << 5); // bits[5:3] AXI resetn (1 = de-assert)
const CMD_RST_PWRDN_DEASSERT: u32 = 1 << 8; // global power-down release (high = reset)
const CMD_LTSSM_EN: u32 = 1 << 6;
const CMD_PCIE_RC_PERST: u32 = 1 << 12; // 1 = PERST# asserted (low)
const CMD_APP_HOLD_PHY_RST: u32 = 1 << 30;
const CMD_DEVICE_TYPE_RC: u32 = 1 << 31;
const CMD_SYS_AUX_PWR_DET: u32 = 1 << 9;

const LOGIC_IGNORE_PERSTN: u32 = 1 << 2;

// ===================================================================
// PUPHY register numbers (byte offset = reg_no << 2; per-lane += 0x400).
// ===================================================================

const PHY_PU_ADDR_CLK_CFG: u32 = 0x02;
const PHY_FORCE_RCV_DONE: u32 = 0x06;
const PHY_RC_CAL_REG1: u32 = 0x08;
const PHY_PLL_REG1: u32 = 0x12; // refclk select / SSC
const PHY_PLL_REG5: u32 = 0x13; // 100 MHz refclk output enable
const PHY_PLL_REG9: u32 = 0x14; // Rterm enable + calibration write-back
const PHY_RX_REG4: u32 = 0x15;
const PHY_REFCLK_MODE: u32 = 0x17; // refclk model + driver/receiver
const PHY_TX_REG1: u32 = 0x19; // Rterm high nibble write-back
const PHY_TX_REG3: u32 = 0x1A;
const PHY_RTERM_DONE: u32 = 0x21; // readonly_reg2: bit10 = tune done, [7:0] = result

/// PLL lock is polled at a fixed byte offset (`readl(base + 0x8)` bit0 in the BSP).
const PHY_PLL_LOCK_OFFSET: usize = 0x8;

#[inline(always)]
const fn phy_off(reg_no: u32) -> usize {
    (reg_no as usize) << 2
}

#[inline(always)]
const fn phy_off_lane(reg_no: u32, lane: usize) -> usize {
    ((reg_no as usize) << 2) + lane * 0x400
}

// ===================================================================
// DBI (own-config + DesignWare port-logic) offsets.
// ===================================================================

const DBI_COMMAND: usize = 0x04;
const DBI_CLASS_REVISION: usize = 0x08;
const DBI_BAR0: usize = 0x10;
const DBI_BAR1: usize = 0x14;
const DBI_PRIMARY_BUS: usize = 0x18;
const DBI_INTERRUPT_LINE: usize = 0x3C;
const DBI_PCIE_CAP_LINK_CAPABILITY: usize = 0x7C;
const DBI_PCIE_CAP_LINK_STATUS: usize = 0x80;
const DBI_PCIE_CAP_LINK_CTL_2: usize = 0xA0;
const DBI_PORT_DEBUG0: usize = 0x728;
const DBI_LINK_WIDTH_SPEED_CTL: usize = 0x80C;
const DBI_MISC_CONTROL_1: usize = 0x8BC;

const DBI_RO_WR_EN: u32 = 1 << 0;
const CMD_IO: u32 = 1 << 0;
const CMD_MEM: u32 = 1 << 1;
const CMD_BUSMASTER: u32 = 1 << 2;
const CMD_SERR: u32 = 1 << 8;

const TARGET_LINK_SPEED_MASK: u32 = 0xF;
const LINK_SPEED_GEN2: u32 = 0x2;
const PORT_LOGIC_SPEED_CHANGE: u32 = 1 << 17;

const LTSSM_STATE_MASK: u32 = 0x1F;
const LTSSM_STATE_L0: u32 = 0x11;

// ===================================================================
// iATU (unrolled: each region is `region << 9` from the ATU base).
// ===================================================================

const ATU_REGION_CTRL1: usize = 0x00;
const ATU_REGION_CTRL2: usize = 0x04;
const ATU_LOWER_BASE: usize = 0x08;
const ATU_UPPER_BASE: usize = 0x0C;
const ATU_LIMIT: usize = 0x10;
const ATU_LOWER_TARGET: usize = 0x14;
const ATU_UPPER_TARGET: usize = 0x18;

const ATU_REGION_CFG: u32 = 0;
const ATU_REGION_MEM: u32 = 1;

#[inline(always)]
const fn atu_off(region: u32) -> usize {
    (region as usize) << 9
}

// ===================================================================
// Timing helpers (busy-wait on the RISC-V `time` CSR).
// ===================================================================

fn ns_to_ticks(ns: u64) -> u64 {
    // ticks = freq(Hz) * ns / 1e9, computed to avoid overflow for small `ns`.
    ostd::arch::tsc_freq().saturating_mul(ns) / 1_000_000_000
}

fn delay_ns(ns: u64) {
    // Order preceding reset/PHY writes before measuring the required delay.
    dma_io_fence();
    let start = ostd::arch::read_tsc();
    let ticks = ns_to_ticks(ns);
    while ostd::arch::read_tsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

fn delay_us(us: u64) {
    delay_ns(us.saturating_mul(1_000));
}

fn delay_ms(ms: u64) {
    delay_ns(ms.saturating_mul(1_000_000));
}

// ===================================================================
// The host bridge.
// ===================================================================

/// A brought-up K1 DWC PCIe Root Complex, owning the MMIO regions needed for
/// configuration access after link-up.
#[derive(Debug)]
pub(super) struct K1PcieHost {
    port: &'static PortDesc,
    /// APMU block (clock/reset/control) covering all ports.
    apmu: IoMem,
    /// This port's PUPHY.
    phy: IoMem,
    /// PortA's PUPHY (Rterm calibration source, shared by all ports).
    phy0: IoMem,
    /// DBI (own-config + port-logic).
    dbi: IoMem,
    /// iATU registers.
    atu: IoMem,
    /// CFG window mapping to the downstream bus.
    cfg: IoMem,
}

/// The active K1 host (PortC / NVMe), set once bring-up succeeds.
static K1_HOST: Once<K1PcieHost> = Once::new();
/// Shared APMU and PortA PHY mappings reused by the USB ComboPHY hooks.
static SHARED_APMU: Once<IoMem> = Once::new();
static SHARED_PHY0: Once<IoMem> = Once::new();

/// Returns whether a K1 DWC PCIe host has been brought up and is serving
/// configuration accesses.
pub(super) fn is_active() -> bool {
    K1_HOST.is_completed()
}

// --- Small MMIO helpers, mirroring the vendor driver's read/modify/write. ---

fn r32(io: &IoMem, off: usize) -> u32 {
    let value = io.read_once(off).unwrap();
    dma_io_fence();
    value
}

fn w32(io: &IoMem, off: usize, val: u32) {
    dma_io_fence();
    io.write_once(off, &val).unwrap();
}

fn modify32(io: &IoMem, off: usize, f: impl FnOnce(u32) -> u32) {
    let v = r32(io, off);
    w32(io, off, f(v));
}

impl K1PcieHost {
    // --- APMU CMD / LOGIC accessors for this host's port. ---

    fn apmu_cmd_off(&self) -> usize {
        self.port.index * 8
    }
    fn apmu_logic_off(&self) -> usize {
        self.port.index * 8 + 4
    }
    fn porta_cmd_off() -> usize {
        0
    }

    fn apmu_cmd_modify(&self, f: impl FnOnce(u32) -> u32) {
        modify32(&self.apmu, self.apmu_cmd_off(), f);
    }
    fn apmu_logic_modify(&self, f: impl FnOnce(u32) -> u32) {
        modify32(&self.apmu, self.apmu_logic_off(), f);
    }

    fn dbi_modify(&self, off: usize, f: impl FnOnce(u32) -> u32) {
        modify32(&self.dbi, off, f);
    }

    fn dbi_ro_wr_enable(&self, enable: bool) {
        self.dbi_modify(DBI_MISC_CONTROL_1, |v| {
            if enable {
                v | DBI_RO_WR_EN
            } else {
                v & !DBI_RO_WR_EN
            }
        });
    }

    // --- Stage A: clock + reset (pcie_dw_k1x.c host bring-up). ---

    fn clock_reset(&self) {
        ostd::debug!("[k1-pcie] stage A: clock + reset");
        // Enable AXI clocks + de-assert sub-resets + select RC + hold PHY, while
        // clearing LTSSM/global-reset/aux-pwr/PERST. The global-reset bit (8) is
        // high-level-valid, so it must be cleared to leave global reset.
        self.apmu_cmd_modify(|v| {
            (v & !(CMD_LTSSM_EN | CMD_RST_PWRDN_DEASSERT | CMD_SYS_AUX_PWR_DET | CMD_PCIE_RC_PERST))
                | CMD_CLK_GATE
                | CMD_RST_SUB
                | CMD_DEVICE_TYPE_RC
                | CMD_APP_HOLD_PHY_RST
        });
        // Ignore the external PERST# input (we drive PERST from the RC side).
        self.apmu_logic_modify(|v| v | LOGIC_IGNORE_PERSTN);
        // Assert PERST#, wait 100 ms, then release (host_init timing).
        self.apmu_cmd_modify(|v| v | CMD_PCIE_RC_PERST);
        delay_ms(100);
        self.apmu_cmd_modify(|v| v & !CMD_PCIE_RC_PERST);
    }

    // --- Stage B: PUPHY init (Rterm calibration + PLL lock). ---

    /// Calibrates termination resistance through PortA before programming PortC.
    fn porta_rterm(&self) -> Result<u32, K1PcieError> {
        // Temporarily drive PortA's APMU control, program PortA PUPHY refclk in
        // driver mode, then wait for the tune-done bit.
        w32(&self.apmu, Self::porta_cmd_off(), 0x4000_003F);
        modify32(&self.apmu, Self::porta_cmd_off(), |v| v & 0xBFFF_FFFF);

        ostd::debug!("[k1-pcie] B: programming PortA PUPHY refclk");
        modify32(&self.phy0, phy_off(PHY_REFCLK_MODE), |v| v | (1 << 10));
        modify32(&self.phy0, phy_off(PHY_REFCLK_MODE), |v| v & !(0x3 << 8));
        modify32(&self.phy0, phy_off(PHY_REFCLK_MODE), |v| v | (0x1 << 8));
        modify32(&self.phy0, 0x400 + phy_off(PHY_REFCLK_MODE), |v| {
            v | (0x1 << 8)
        });
        modify32(&self.phy0, phy_off(PHY_PLL_REG1), |v| {
            (v & 0xFFFF_0FFF) | 0x0000_2000
        });
        modify32(&self.phy0, phy_off(PHY_PLL_REG5), |v| v | (1 << 4));
        modify32(&self.phy0, phy_off(PHY_PLL_REG1), |v| v & 0xFFF0_FFFF);
        w32(&self.phy0, phy_off(PHY_PU_ADDR_CLK_CFG), 0x0000_0B78);
        w32(&self.phy0, phy_off(PHY_FORCE_RCV_DONE), 0x0000_0400);
        // Force this port's PUPHY rx LFPS while calibrating.
        modify32(&self.phy, phy_off(PHY_FORCE_RCV_DONE), |v| {
            v | (1 << 17) | (1 << 15)
        });

        ostd::debug!("[k1-pcie] B: waiting for PortA Rterm calibration");
        if !wait_bit(&self.phy0, phy_off(PHY_RTERM_DONE), 1 << 10, true, 100_000) {
            ostd::warn!("[k1-pcie] PortA Rterm calibration timed out");
            return Err(K1PcieError::RtermTimeout);
        }
        let rcal = r32(&self.phy0, phy_off(PHY_RTERM_DONE));
        modify32(&self.apmu, Self::porta_cmd_off(), |v| v & !0x4000_003F);
        ostd::debug!("[k1-pcie] pcie_rcal = {:#010x}", rcal);
        Ok(rcal)
    }

    fn phy_init(&self) -> Result<(), K1PcieError> {
        ostd::debug!("[k1-pcie] stage B: PHY init (Rterm + PLL)");

        let rcal = self.porta_rterm()?;

        // B.2 Write the calibration result into each lane of this port.
        for lane in 0..self.port.lanes {
            modify32(&self.phy, phy_off_lane(PHY_PLL_REG9, lane), |v| {
                v | ((rcal & 0xF) << 8)
            });
            modify32(&self.phy, phy_off_lane(PHY_RX_REG4, lane), |v| {
                v & !(1 << 5)
            });
            modify32(&self.phy, phy_off_lane(PHY_TX_REG1, lane), |v| {
                v | (((rcal >> 4) & 0xF) << 12)
            });
            modify32(&self.phy, phy_off_lane(PHY_TX_REG3, lane), |v| {
                v | (1 << 25)
            });
            // rc_cal restart: 0 -> 1.
            modify32(&self.phy, phy_off_lane(PHY_RC_CAL_REG1, lane), |v| {
                v & !(1 << 22)
            });
            modify32(&self.phy, phy_off_lane(PHY_RC_CAL_REG1, lane), |v| {
                v | (1 << 22)
            });
        }
        // rc_cal_reg2 driver-mode bits + release the forced rx LFPS.
        modify32(&self.phy, phy_off(PHY_RC_CAL_REG1), |v| v | (0x3 << 29));
        modify32(&self.phy, phy_off(PHY_FORCE_RCV_DONE), |v| v & 0xFFFD_7FFF);

        // B.3 Release the PHY reset and program this port's refclk (driver mode).
        self.apmu_cmd_modify(|v| v & !CMD_APP_HOLD_PHY_RST);
        for lane in 0..self.port.lanes {
            modify32(&self.phy, phy_off_lane(PHY_REFCLK_MODE, lane), |v| {
                (v | (1 << 10)) & !(0x3 << 8) | (1 << 8)
            });
        }
        modify32(&self.phy, phy_off(PHY_PLL_REG1), |v| {
            (v & 0xFFFF_0FFF) | 0x0000_2000
        });
        modify32(&self.phy, phy_off(PHY_PLL_REG5), |v| v | (1 << 4));
        modify32(&self.phy, phy_off(PHY_PLL_REG1), |v| v & 0xFFF0_FFFF);
        for lane in 0..self.port.lanes {
            w32(
                &self.phy,
                phy_off_lane(PHY_PU_ADDR_CLK_CFG, lane),
                0x0000_0B78,
            );
        }
        w32(&self.phy, phy_off(PHY_FORCE_RCV_DONE), 0x0000_0400);

        delay_us(1_000);

        // B.5 Wait for PLL lock (BSP polls `base + 0x8` bit0).
        ostd::debug!("[k1-pcie] B: waiting for PHY PLL lock");
        if !wait_bit(&self.phy, PHY_PLL_LOCK_OFFSET, 1, true, 500_000) {
            ostd::warn!("[k1-pcie] PHY PLL lock timed out");
            return Err(K1PcieError::PllLock);
        }
        ostd::debug!("[k1-pcie] PHY PLL locked");
        Ok(())
    }

    // --- Stage C: LTSSM link training. ---

    fn link_up(&self) -> Result<(), K1PcieError> {
        ostd::debug!("[k1-pcie] stage C: link up");
        // C.1 Configure the target link speed (Gen2).
        self.dbi_ro_wr_enable(true);
        self.dbi_modify(DBI_PCIE_CAP_LINK_CAPABILITY, |v| {
            (v & !TARGET_LINK_SPEED_MASK) | LINK_SPEED_GEN2
        });
        self.dbi_modify(DBI_PCIE_CAP_LINK_CTL_2, |v| {
            (v & !TARGET_LINK_SPEED_MASK) | LINK_SPEED_GEN2
        });
        self.dbi_ro_wr_enable(false);

        // C.2 Set up the RC as a PCI-to-PCI bridge (setup_host).
        w32(&self.dbi, DBI_BAR0, 0x4); // BAR0 type = 64-bit
        w32(&self.dbi, DBI_BAR1, 0x0);
        self.dbi_modify(DBI_INTERRUPT_LINE, |v| (v & !0xFF00) | 0x0100);
        // primary = 0, secondary = 1, subordinate = 0xFF.
        self.dbi_modify(DBI_PRIMARY_BUS, |v| (v & !0x00FF_FFFF) | 0x00FF_0100);
        self.dbi_modify(DBI_COMMAND, |v| {
            (v & !0xFFFF) | (CMD_IO | CMD_MEM | CMD_BUSMASTER | CMD_SERR)
        });
        self.dbi_ro_wr_enable(true);
        // Class code 0x0604 (PCI bridge) in the upper 16 bits of 0x08.
        self.dbi_modify(DBI_CLASS_REVISION, |v| (v & 0x0000_FFFF) | (0x0604 << 16));
        self.dbi_ro_wr_enable(false);

        // C.3 Request a speed change and start the LTSSM.
        self.dbi_modify(DBI_LINK_WIDTH_SPEED_CTL, |v| v | PORT_LOGIC_SPEED_CHANGE);
        self.apmu_cmd_modify(|v| (v | CMD_LTSSM_EN) & !CMD_APP_HOLD_PHY_RST);

        // C.4 Wait for the LTSSM to reach L0.
        ostd::debug!("[k1-pcie] C: waiting for LTSSM L0");
        if !wait_bit_eq(
            &self.dbi,
            DBI_PORT_DEBUG0,
            LTSSM_STATE_MASK,
            LTSSM_STATE_L0,
            100_000,
        ) {
            ostd::warn!(
                "[k1-pcie] LTSSM did not reach L0 (debug0 = {:#010x})",
                r32(&self.dbi, DBI_PORT_DEBUG0)
            );
            return Err(K1PcieError::LinkDown);
        }
        delay_us(100);
        ostd::debug!(
            "[k1-pcie] link up: LINK_STATUS = {:#010x}",
            r32(&self.dbi, DBI_PCIE_CAP_LINK_STATUS)
        );
        Ok(())
    }

    // --- Stage D: iATU (CFG + MEM regions). ---

    fn program_atu(&self) -> Result<(), K1PcieError> {
        ostd::debug!("[k1-pcie] stage D: program iATU");
        let cfg = self.port.cfg;
        let (mem, mem_size) = (self.port.mem_base, self.port.mem_size);
        // Region 0: CFG0, mapping the CFG window to the downstream bus.
        self.atu_program(
            ATU_REGION_CFG,
            ATU_TYPE_CFG0,
            cfg as u64,
            (DOWNSTREAM_BUS as u64) << 24,
            self.port.cfg_size,
        )?;
        // Region 1: MEM, identity-mapping the device memory window.
        self.atu_program(
            ATU_REGION_MEM,
            ATU_TYPE_MEM,
            mem as u64,
            mem as u64,
            mem_size,
        )?;
        // Inbound (device->host) DMA needs no explicit iATU region: the DWC RC
        // forwards it to host memory once the bridge's Bus Master Enable is set
        // (see `finalize_rc_bridge`), matching the mainline/vendor drivers.
        Ok(())
    }

    fn atu_program(
        &self,
        region: u32,
        type_: u32,
        cpu_addr: u64,
        pci_addr: u64,
        size: usize,
    ) -> Result<(), K1PcieError> {
        let base = atu_off(region);
        let prog = atu_prog(type_, cpu_addr, pci_addr, size);
        w32(&self.atu, base + ATU_LOWER_BASE, prog.lower_base);
        w32(&self.atu, base + ATU_UPPER_BASE, prog.upper_base);
        w32(&self.atu, base + ATU_LIMIT, prog.limit);
        w32(&self.atu, base + ATU_LOWER_TARGET, prog.lower_target);
        w32(&self.atu, base + ATU_UPPER_TARGET, prog.upper_target);
        w32(&self.atu, base + ATU_REGION_CTRL1, prog.ctrl1);
        w32(&self.atu, base + ATU_REGION_CTRL2, prog.ctrl2);

        // The enable bit takes effect only after some delay; retry a few times.
        for _ in 0..5 {
            if r32(&self.atu, base + ATU_REGION_CTRL2) & ATU_ENABLE != 0 {
                ostd::debug!(
                    "[k1-pcie] iATU region {} type={:#x} cpu={:#x} pci={:#x} size={:#x} enabled",
                    region,
                    type_,
                    cpu_addr,
                    pci_addr,
                    size
                );
                return Ok(());
            }
            delay_us(10_000);
        }
        ostd::warn!("[k1-pcie] iATU region {} enable bit did not stick", region);
        Err(K1PcieError::Atu)
    }

    // --- Stage F: assign the downstream device's BAR0. ---
    //
    // The generic PCI layer keeps a firmware-assigned BAR base as-is (and treats
    // a zero base as uninitialized), so the RC firmware must place BAR0 within
    // the iATU MEM window before enumeration.

    fn assign_bar0(&self) {
        let mem_base = self.port.mem_base;
        let bdf = bdf_of(DOWNSTREAM_BUS, 0, 0);

        // Place BAR0 (64-bit memory) at the window base and enable MEM + master.
        self.raw_cfg_write32(bdf, DBI_BAR0 as u32, (mem_base as u32) | 0x4);
        self.raw_cfg_write32(bdf, DBI_BAR1 as u32, (mem_base >> 32) as u32);
        self.raw_cfg_write32(bdf, DBI_COMMAND as u32, CMD_MEM | CMD_BUSMASTER);
        ostd::debug!("[k1-pcie] assigned device BAR0 = {:#x}", mem_base);
    }

    /// Re-applies the RC bridge configuration header AFTER link training.
    ///
    /// LTSSM link training resets the type-1 config header (command, bus
    /// numbers, BARs) back to defaults, wiping the values written before the
    /// link came up. Critically, the bridge's Bus Master Enable governs whether
    /// downstream (device) memory requests are forwarded upstream to host DRAM;
    /// with it clear, all device->host DMA is master-aborted. Re-apply the
    /// header once the link is stable so DMA works.
    fn finalize_rc_bridge(&self) {
        // secondary = 1, subordinate = 0xFF, primary = 0.
        self.dbi_modify(DBI_PRIMARY_BUS, |v| (v & !0x00FF_FFFF) | 0x00FF_0100);
        // Enable memory decode + Bus Master (upstream forwarding) + SERR.
        self.dbi_modify(DBI_COMMAND, |v| {
            (v & !0xFFFF) | (CMD_IO | CMD_MEM | CMD_BUSMASTER | CMD_SERR)
        });
        ostd::debug!(
            "[k1-pcie] rc-bridge finalized: cmd={:#06x} bus={:#010x}",
            r32(&self.dbi, DBI_COMMAND) & 0xFFFF,
            r32(&self.dbi, DBI_PRIMARY_BUS)
        );
    }

    // --- Configuration space access through the CFG window. ---

    /// Reads a 32-bit configuration register of the downstream device.
    ///
    /// Only the downstream bus (bus 1) is reachable; other buses read as all-ones
    /// (no device), matching PCI semantics.
    fn raw_cfg_read32(&self, bdf: u32, offset: u32) -> u32 {
        // The downstream link is point-to-point: only bus 1, device 0 exists.
        // Returning all-ones for anything else avoids issuing configuration
        // reads that a not-fully-configured Root Complex may fail to complete.
        let Some(win_off) = cfg_win_off_from_bdf(bdf, offset) else {
            return 0xFFFF_FFFF;
        };
        r32(&self.cfg, win_off)
    }

    fn raw_cfg_write32(&self, bdf: u32, offset: u32, val: u32) {
        let Some(win_off) = cfg_win_off_from_bdf(bdf, offset) else {
            return;
        };
        w32(&self.cfg, win_off, val);
    }
}

/// Errors that can occur while bringing up the K1 PCIe Root Complex.
#[derive(Debug, Clone, Copy)]
enum K1PcieError {
    RtermTimeout,
    PllLock,
    LinkDown,
    Atu,
}

/// Polls until `(read(off) & mask) == mask` (if `want_set`) or `== 0` (if not),
/// up to `timeout_us` microseconds. Returns whether the condition was met.
fn wait_bit(io: &IoMem, off: usize, mask: u32, want_set: bool, timeout_us: u64) -> bool {
    let deadline_start = ostd::arch::read_tsc();
    let timeout_ticks = ns_to_ticks(timeout_us.saturating_mul(1_000));
    loop {
        let v = r32(io, off);
        let matched = if want_set {
            v & mask == mask
        } else {
            v & mask == 0
        };
        if matched {
            return true;
        }
        if ostd::arch::read_tsc().wrapping_sub(deadline_start) >= timeout_ticks {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Polls until `(read(off) & mask) == want`, up to `timeout_us` microseconds.
fn wait_bit_eq(io: &IoMem, off: usize, mask: u32, want: u32, timeout_us: u64) -> bool {
    let start = ostd::arch::read_tsc();
    let timeout_ticks = ns_to_ticks(timeout_us.saturating_mul(1_000));
    loop {
        if r32(io, off) & mask == want {
            return true;
        }
        if ostd::arch::read_tsc().wrapping_sub(start) >= timeout_ticks {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// Detects and brings up the K1 DWC PCIe Root Complex.
///
/// Returns the downstream bus range on success, or `None` if this is not a K1
/// platform or bring-up failed (in which case the generic ECAM path is used).
pub(super) fn init() -> Option<RangeInclusive<u8>> {
    let device_tree = ostd::arch::boot::DEVICE_TREE.get()?;
    device_tree.find_compatible(&["spacemit,k1-x", "spacemit,k1"])?;
    ostd::info!(
        "[k1-pcie] detected SpacemiT K1; bringing up DWC PCIe RC{} (PortC/NVMe)",
        RC
    );

    let host = match acquire_host(&PORTC) {
        Ok(host) => host,
        Err(e) => {
            ostd::error!("[k1-pcie] failed to map controller MMIO: {:?}", e);
            return None;
        }
    };
    ostd::info!("[k1-pcie] controller MMIO mapped");

    // NB: do not read PUPHY/DBI registers before stage A enables the PCIe
    // clocks; such accesses can stall the AXI bus.
    host.clock_reset();
    ostd::info!("[k1-pcie] clock/reset + PERST done");
    if let Err(e) = host.phy_init() {
        ostd::error!("[k1-pcie] PHY init failed: {:?}", e);
        return None;
    }
    ostd::info!("[k1-pcie] PHY PLL locked");
    if let Err(e) = host.link_up() {
        ostd::error!("[k1-pcie] link training failed: {:?}", e);
        return None;
    }
    ostd::info!("[k1-pcie] LTSSM L0");
    if let Err(e) = host.program_atu() {
        ostd::error!("[k1-pcie] iATU programming failed: {:?}", e);
        return None;
    }
    host.assign_bar0();
    // Re-apply the RC bridge header (Bus Master Enable etc.) now that the link
    // is up: LTSSM training resets the config header, and without Bus Master
    // Enable the bridge will not forward device->host DMA (master abort).
    host.finalize_rc_bridge();

    // The wrapper MSI monitor is unsupported: enabling it interferes with
    // inbound DMA. NVMe polls completion queues on this platform.
    K1_HOST.call_once(|| host);
    ostd::info!(
        "[k1-pcie] Root Complex ready; downstream bus {}",
        DOWNSTREAM_BUS
    );
    Some(DOWNSTREAM_BUS..=DOWNSTREAM_BUS)
}

fn shared_apmu() -> Result<IoMem, Error> {
    if let Some(io) = SHARED_APMU.get() {
        return Ok(io.clone());
    }
    let io = IoMem::acquire(APMU_BLOCK_BASE..APMU_BLOCK_BASE + APMU_BLOCK_SIZE)?;
    Ok(SHARED_APMU.call_once(|| io).clone())
}

fn shared_phy0() -> Result<IoMem, Error> {
    if let Some(io) = SHARED_PHY0.get() {
        return Ok(io.clone());
    }
    let io = IoMem::acquire(PORTA_PHY_BASE..PORTA_PHY_BASE + CTRL_WIN)?;
    Ok(SHARED_PHY0.call_once(|| io).clone())
}

fn acquire_host(port: &'static PortDesc) -> Result<K1PcieHost, Error> {
    Ok(K1PcieHost {
        port,
        apmu: shared_apmu()?,
        phy: IoMem::acquire(port.phy..port.phy + CTRL_WIN)?,
        phy0: shared_phy0()?,
        dbi: IoMem::acquire(port.dbi..port.dbi + CTRL_WIN)?,
        atu: IoMem::acquire(port.atu..port.atu + CTRL_WIN)?,
        cfg: IoMem::acquire(port.cfg..port.cfg + port.cfg_size)?,
    })
}

/// Serves a 32-bit configuration read for the generic PCI layer.
pub(super) fn cfg_read32(location: &PciDeviceLocation, offset: u32) -> u32 {
    let Some(host) = K1_HOST.get() else {
        return 0xFFFF_FFFF;
    };
    host.raw_cfg_read32(
        bdf_of(location.bus, location.device, location.function),
        offset,
    )
}

/// Serves a 32-bit configuration write for the generic PCI layer.
pub(super) fn cfg_write32(location: &PciDeviceLocation, offset: u32, value: u32) {
    if let Some(host) = K1_HOST.get() {
        host.raw_cfg_write32(
            bdf_of(location.bus, location.device, location.function),
            offset,
            value,
        );
    }
}

/// PortA PUPHY window (`0xc0b10000`), if PCIe bring-up already mapped it.
///
/// PCIe uses this shared ComboPHY for termination calibration; USB3 later
/// configures it for SuperSpeed. Reusing the mapping preserves IoMem's
/// exclusive physical-range ownership.
pub fn k1_porta_phy() -> Option<&'static IoMem> {
    SHARED_PHY0.get()
}

/// Deasserts `RESET_COMBO_PHY` and returns `(before, after)` of the register.
///
/// The ComboPHY reset is bit 8 of `APMU_PCIE_CLK_RES_CTRL_0` (APMU `0x3cc`,
/// the first word of the PCIe APMU window this driver owns). Polarity, from
/// the vendor reset table (`reset-spacemit-k1x.c`):
///
/// ```text
/// [RESET_COMBO_PHY] = { 0x3cc, mask BIT(8), deassert_val 0, assert_val BIT(8) }
/// [RESET_PCIE0]     = { 0x3cc, mask BIT(3|4|5|8), deassert BIT(3|4|5), assert BIT(8) }
/// ```
///
/// Bit 8 asserts reset when set and releases it when cleared. Accessing the
/// ComboPHY while reset is asserted returns zeroes and prevents PLL lock.
/// PortA Rterm calibration's `w32(0x3cc, 0x4000_003F)` already clears this bit,
/// but USB PHY initialization must deassert reset independently of PCIe.
pub fn k1_combophy_reset_deassert() -> Result<(u32, u32), Error> {
    let apmu = shared_apmu()?;
    let before = r32(&apmu, 0);
    modify32(&apmu, 0, |v| v & !(1 << 8));
    Ok((before, r32(&apmu, 0)))
}

/// Assert then deassert `RESET_COMBO_PHY` (APMU `0x3cc` bit 8 only).
///
/// Vendor `spacemit_combphy_init` does `set_mode` then `reset_deassert`.
/// On a cold boot bit 8 is typically 1, so that deassert is the analog mux
/// sampling edge for `phy_sel`. After PortA Rterm this bit is already 0 and
/// deassert is a no-op — USB3 register pokes can lock the PLL without the
/// PIPE output actually switching off PCIe. Pulsing bit 8 recreates the
/// vendor edge. PortB/C live in the next APMU words (`0x3d4`/`0x3dc`) and
/// are not touched; PortA is only the Rterm source, not a live link.
pub fn k1_combophy_reset_pulse() -> Result<(u32, u32, u32), Error> {
    let apmu = shared_apmu()?;
    let before = r32(&apmu, 0);
    modify32(&apmu, 0, |v| v | (1 << 8));
    let asserted = r32(&apmu, 0);
    delay_us(150);
    modify32(&apmu, 0, |v| v & !(1 << 8));
    Ok((before, asserted, r32(&apmu, 0)))
}

#[cfg(ktest)]
mod portc_table {
    use ostd::prelude::ktest;

    /// Checks the firmware-compatible PortC aperture and lane configuration.
    #[ktest]
    fn portc_windows_match_the_proven_nvme_path() {
        let p = &super::PORTC;
        assert_eq!(p.index, 2);
        assert_eq!(p.lanes, 2);
        assert_eq!(p.dbi, 0xCA80_0000);
        assert_eq!(p.atu, 0xCAB0_0000);
        assert_eq!(p.cfg, 0xA000_0000);
        assert_eq!(p.cfg_size, 0x10_0000);
        assert_eq!(p.phy, 0xC0D1_0000);
        assert_eq!(p.mem_base, 0xA020_0000);
        assert_eq!(p.mem_size, 0x1600_0000);
    }
}
