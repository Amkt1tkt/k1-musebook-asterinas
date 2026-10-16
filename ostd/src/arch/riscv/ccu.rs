// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SpacemiT K1 clock/reset controller (CCU) used by board drivers

//! Minimal SpacemiT K1 (k1-x) clock & reset controller ("CCU").
//!
//! The K1 CCU is a set of MMIO register blocks (MPMU/APMU/APBC/APBS/...) where
//! each peripheral has a single control register holding both its clock-gate
//! bits and its reset bit. This module is intentionally minimal: rather than
//! model the whole clock tree, it exposes [`enable`] to bring a specific
//! peripheral out of reset and ungate its clock, growing [`Periph`] as drivers
//! need more devices.
//!
//! Register semantics (cross-checked against the vendor Linux BSP
//! `drivers/clk/spacemit/ccu-spacemit-k1x.c` and
//! `drivers/reset/reset-spacemit-k1x.c`):
//!   - gate:  `reg = (reg & !gate_mask) | gate_on`
//!   - reset: `reg = (reg & !reset_mask) | reset_deassert`
//!     (APBC resets are active-high: deassert = 0; APMU resets are active-low:
//!     deassert = the bit set.)
//!
//! Shared clock/reset registers are updated under one IRQ-safe lock. The lock
//! covers only register access; hardware delays and status polls run unlocked.

use spin::Once;

use crate::{
    arch::boot::DEVICE_TREE,
    io::IoMem,
    mm::VmIoOnce,
    sync::{LocalIrqDisabled, SpinLock},
};

// Sub-block base addresses (fixed on the K1; the vendor DT `spacemit,k1x-clock`
// `reg` list, which our minimal DTB omits). Sizes are from that same node.
const APBC_BASE: usize = 0xd401_5000;
const APBC_SIZE: usize = 0x1000;
/// APBS ("apbs" in the vendor DT, `0xd4090000`): PLL1/2/3 spare registers
/// (`APB_SPARE*`), plus the TWSI side-band enable at `+0x104`.
const APBS_BASE: usize = 0xd409_0000;
const APBS_SIZE: usize = 0x1000;
/// MPMU (`0xd4050000`): PLL lock-status `POSR` (+0x10) and `WDTPCR` (+0x200).
/// Window kept below `0xd4051000`, which this module also maps as `mpmu_gate`
/// (ACGR / ARSR / APRR). `WDTPCR` is the SoC WDT's clock+reset; reading
/// `WDT_WSR` after a reboot needs the APB clock on and must not pulse reset
/// (the status bit survives a WDT reset, not an MPMU block reset).
const MPMU_BASE: usize = 0xd405_0000;
const MPMU_SIZE: usize = 0x300;
/// The page holding `MPMU_ACGR` (`0xd4051024`), the shared clock-gate register.
///
/// A second window rather than a wider `MPMU_SIZE`, because the two are not
/// contiguous in purpose: this page is gates, the one above is PLL status.
/// Keep one shared owner so consumers can update their gates without
/// acquiring overlapping MMIO regions.
const MPMU_GATE_BASE: usize = 0xd405_1000;
const MPMU_GATE_SIZE: usize = 0x1000;
/// `MPMU_ACGR` within [`MPMU_GATE_BASE`]. Bit 6 is the TWSI fabric clock, bit 15
/// is `pll1_d4_614p4` -- the GPU's clock parent. BSP
/// `drivers/clk/spacemit/ccu-spacemit-k1x.c`.
const MPMU_ACGR: usize = 0x024;
/// SoC WDT / PMU timers (`watchdog@d4080000`). This shared mapping provides
/// read access to the sticky reset status without a second MMIO owner.
const WDT_BASE: usize = 0xd408_0000;
const WDT_SIZE: usize = 0x100;
const WDT_WSR: usize = 0xc0;

const APMU_BASE: usize = 0xd428_2800;
/// APMU region claimed by this CCU. Truncated before the PCIe port CMD/LOGIC
/// registers at `0xd428_2bcc` so the DWC Root Complex driver can acquire that
/// window separately (`aster-pci` `APMU_BLOCK_BASE`). The exclusive `IoMem`
/// allocations must not overlap.
const APMU_SIZE: usize = 0x3cc; // 0xd428_2800 .. 0xd428_2bcc

/// Which CCU sub-block a peripheral's control register lives in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Apbc,
    Apmu,
    Mpmu,
}

/// A peripheral's clock-gate + reset description within one CCU register.
struct Desc {
    block: Block,
    /// Register offset within the sub-block.
    offset: usize,
    /// Bits that select the clock gate.
    gate_mask: u32,
    /// Value (within `gate_mask`) that enables the clock.
    gate_on: u32,
    /// Bits that select the reset.
    reset_mask: u32,
    /// Value (within `reset_mask`) that releases the reset.
    reset_deassert: u32,
    /// The frequency-change strobe, or zero if this peripheral has none.
    ///
    /// Setting the mux and divider fields of a `*_CLK_RES_CTRL` register does
    /// not by itself change the clock: the vendor's `ccu_mix_trigger_fc` sets
    /// this bit and waits for hardware to clear it, and only then is the new
    /// setting live. A peripheral whose clock is programmed but never committed
    /// can end up with no clock at all, and an unclocked block on this SoC does
    /// not fault when read -- it stops answering, and the read never returns.
    fc_mask: u32,
}

/// Peripherals whose clock/reset this module knows how to bring up.
///
/// Values are taken from the vendor BSP; extend as more drivers are ported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Periph {
    /// Real-time clock (`APBC_RTC_CLK_RST`).
    Rtc,
    /// I2C/TWSI bus 0 (`APBC_TWSI0_CLK_RST`).
    Twsi0,
    /// I2C/TWSI bus 4 — Muse Book touchpad (`APBC_TWSI4_CLK_RST` @ 0x40).
    Twsi4,
    /// I2C/TWSI bus 5 — Muse Book SBS battery (`APBC_TWSI5_CLK_RST` @ 0x4c).
    Twsi5,
    /// I2C/TWSI bus 6 — Muse Book LT8911 eDP bridge (`APBC_TWSI6_CLK_RST` @ 0x60).
    Twsi6,
    /// I2C/TWSI bus 8 — power I2C / PMIC (`APBC_TWSI8_CLK_RST` @ 0x20).
    Twsi8,
    /// GPIO controller (`APBC_GPIO_CLK_RST`).
    Gpio,
    /// SDHCI0 — external SD slot (`APMU_SDH0_CLK_RES_CTRL` @ 0x54).
    Sdh0,
    /// SDHCI2 — eMMC (`APMU_SDH2_CLK_RES_CTRL` @ 0xe0). The shared AXI
    /// clock/reset live in the SDH0 register (0x54 bits 3/0), so callers
    /// must enable [`Periph::Sdh0`] first — `aster-mmc` already does.
    Sdh2,
    /// USB AXI fabric — `APMU_USB_CLK_RES_CTRL` @ 0x5c (`CLK_USB_AXI` + `RESET_USB_AXI`).
    ///
    /// BSP `ccu-spacemit-k1x.c` `usb_axi_clk` (gate `BIT(1)`) and
    /// `reset-spacemit-k1x.c` `RESET_USB_AXI` (`BIT(0)`, APMU active-low).
    /// Shared register with [`UsbP1`] / [`Usb30`]; enable this first so the
    /// USB3/EHCI masters see the AXI clock.
    UsbAxi,
    /// USB2 Port1 (EHCI1 / usbphy1) — `APMU_USB_CLK_RES_CTRL` @ 0x5c.
    UsbP1,
    /// USB3 / DWC3 — `APMU_USB_CLK_RES_CTRL` @ 0x5c (`CLK_USB30` + `RESET_USB3_0`).
    Usb30,
    /// HDMI mclk + DPU HDMI pipe — `APMU_HDMI_CLK_RES_CTRL` @ 0x1b8.
    Hdmi,
    /// PWM14 — Muse Book eDP panel backlight (`APBC_PWM14_CLK_RST` @ 0xd0).
    Pwm14,
    /// IMG BXE-2-32 GPU — `APMU_GPU_CLK_RES_CTRL` @ 0xcc.
    Gpu,
    /// On-die temperature sensor — `APBC_TSEN_CLK_RST` @ 0x6c.
    Tsen,
    /// SoC watchdog (PMU timers) — `MPMU_WDTPCR` @ 0x200.
    ///
    /// Clock only: [`enable`] deasserts reset and ungates. It never pulses
    /// `WDTPCR[RST]`, because that would wipe `WDT_WSR` (the sticky "last
    /// reset was a WDT" bit the diagnostic reads on the next boot).
    Wdt,
}

impl Periph {
    fn desc(self) -> Desc {
        match self {
            // APBC RTC @ 0x28: gate bits {0,1,7}=0x83; reset bit2 (active-high).
            Periph::Rtc => Desc {
                block: Block::Apbc,
                offset: 0x28,
                gate_mask: 0x83,
                gate_on: 0x83,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC TWSI0 @ 0x2c: gate bits {0,1}=0x3; reset bit2 (active-high).
            Periph::Twsi0 => Desc {
                block: Block::Apbc,
                offset: 0x2c,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC TWSI4 @ 0x40: same gate/reset pattern as other TWSI.
            Periph::Twsi4 => Desc {
                block: Block::Apbc,
                offset: 0x40,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC TWSI5 @ 0x4c: Linux `ccu-spacemit-k1x.c` `APBC_TWSI5_CLK_RST`
            // and `SPACEMIT_CCU_MUX_GATE(twsi5_clk, …, 4, 3, 0x3, 0x3, 0x0)` —
            // identical gate/reset to TWSI4/6, not to TWSI8 (whose gate mask is
            // 0x7 and disable value is 0x4). See [`enable`] for why this does
            // not get the U-Boot 0x4→0x7→0x3 pulse.
            Periph::Twsi5 => Desc {
                block: Block::Apbc,
                offset: 0x4c,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC TWSI6 @ 0x60: same gate/reset pattern as other TWSI.
            Periph::Twsi6 => Desc {
                block: Block::Apbc,
                offset: 0x60,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC TWSI8 @ 0x20: same gate/reset bits; U-Boot also pulses through
            // 0x4→0x7→0x3 (see [`enable`]), so the Desc is used for logging only.
            Periph::Twsi8 => Desc {
                block: Block::Apbc,
                offset: 0x20,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC GPIO @ 0x08: gate bits {0,1}=0x3; reset bit2 (active-high).
            Periph::Gpio => Desc {
                block: Block::Apbc,
                offset: 0x8,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APMU SDH0 @ 0x54: AXI+IO gates bits{3,4}; resets bits{0,1} active-low
            // (deassert = 1). Shared AXI reset bit0 with sdhci1/2.
            Periph::Sdh0 => Desc {
                block: Block::Apmu,
                offset: 0x54,
                gate_mask: 0x18,
                gate_on: 0x18,
                reset_mask: 0x3,
                reset_deassert: 0x3,
                fc_mask: 0,
            },
            // APMU SDH2 @ 0xe0: same layout as SDH0 minus the shared AXI bits
            // (those stay in 0x54): IO gate bit4, RESET_SDH2 bit1 active-low
            // (Linux `sdh2_clk` gate BIT(4); `RESET_SDH2` = {0xe0, BIT(1)}).
            Periph::Sdh2 => Desc {
                block: Block::Apmu,
                offset: 0xe0,
                gate_mask: 1 << 4,
                gate_on: 1 << 4,
                reset_mask: 1 << 1,
                reset_deassert: 1 << 1,
                fc_mask: 0,
            },
            // APMU USB @ 0x5c: CLK_USB_AXI = bit1 (`usb_axi_clk`);
            // RESET_USB_AXI = bit0 (APMU active-low: deassert = bit set).
            Periph::UsbAxi => Desc {
                block: Block::Apmu,
                offset: 0x5c,
                gate_mask: 1 << 1,
                gate_on: 1 << 1,
                reset_mask: 1 << 0,
                reset_deassert: 1 << 0,
                fc_mask: 0,
            },
            // APMU USB @ 0x5c: CLK_USB_P1 = bit5; RESET_USBP1_AXI = bit4
            // (APMU active-low: deassert = bit set).
            Periph::UsbP1 => Desc {
                block: Block::Apmu,
                offset: 0x5c,
                gate_mask: 1 << 5,
                gate_on: 1 << 5,
                reset_mask: 1 << 4,
                reset_deassert: 1 << 4,
                fc_mask: 0,
            },
            // APMU USB @ 0x5c: CLK_USB30 = bit8; RESET_USB3_0 = bits{9,10,11}.
            Periph::Usb30 => Desc {
                block: Block::Apmu,
                offset: 0x5c,
                gate_mask: 1 << 8,
                gate_on: 1 << 8,
                reset_mask: (1 << 9) | (1 << 10) | (1 << 11),
                reset_deassert: (1 << 9) | (1 << 10) | (1 << 11),
                fc_mask: 0,
            },
            // APMU HDMI @ 0x1b8: gate bit0; RESET_HDMI bit9 (APMU active-low).
            // Mux/div programmed in [`enable`] special-case (491.52 MHz).
            Periph::Hdmi => Desc {
                block: Block::Apmu,
                offset: 0x1b8,
                gate_mask: 1 << 0,
                gate_on: 1 << 0,
                reset_mask: 1 << 9,
                reset_deassert: 1 << 9,
                fc_mask: 0,
            },
            // APMU GPU @ 0xcc: gate bit4, RESET_GPU bit1 (APMU active-low, so
            // deassert = set). BSP `ccu-spacemit-k1x.c` builds `gpu_clk` as
            // DIV_FC_MUX_GATE(reg 0xcc, div bits[14:12], fc bit15,
            // mux bits[20:18], gate BIT(4)); `reset-spacemit-k1x.c` has
            // `[RESET_GPU] = { 0xcc, BIT(1), BIT(1), 0, APMU }`.
            //
            // Mux and divider are deliberately left as found. Unlike HDMI, whose
            // pixel clock is dictated by the mode, nothing states a GPU
            // frequency: the device tree node carries no `assigned-clock-rates`
            // and there is no OPP table here to consult. Choosing one would be
            // inventing a number that silicon has to live with.
            Periph::Gpu => Desc {
                block: Block::Apmu,
                offset: 0xcc,
                gate_mask: 1 << 4,
                gate_on: 1 << 4,
                reset_mask: 1 << 1,
                reset_deassert: 1 << 1,
                fc_mask: 1 << 15,
            },
            // APBC PWM14 @ 0xd0: same gate/reset pattern as other APBC (TWSI-style).
            Periph::Pwm14 => Desc {
                block: Block::Apbc,
                offset: 0xd0,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // APBC TSEN @ 0x6c: gate bits {0,1}=0x3; reset bit2 (active-high).
            // BSP `ccu-spacemit-k1x.c` `tsen_clk` + `reset-spacemit-k1x.c`
            // `RESET_TSEN`; manual `APBC_TSEN_CLK_RST` (PCLK + FCLK).
            Periph::Tsen => Desc {
                block: Block::Apbc,
                offset: 0x6c,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
            // MPMU WDTPCR @ 0x200: ABCLK+FNCLK bits {0,1}=0x3; RST bit2
            // (1 = hold reset). Manual `WDT (CP TIMERS) CONTROL REGISTER`.
            Periph::Wdt => Desc {
                block: Block::Mpmu,
                offset: 0x200,
                gate_mask: 0x3,
                gate_on: 0x3,
                reset_mask: 0x4,
                reset_deassert: 0x0,
                fc_mask: 0,
            },
        }
    }
}

struct Ccu {
    apbc: IoMem,
    apmu: IoMem,
    apbs: IoMem,
    mpmu: IoMem,
    mpmu_gate: IoMem,
    /// The SoC WDT window, if it could be claimed.
    ///
    /// A missing reset-status mapping must not disable the clock, reset,
    /// and power-domain registers required by other devices.
    wdt: Option<IoMem>,
}

static CCU: Once<Ccu> = Once::new();

// Gate and reset operations can touch distinct fields of the same register.
// Serialize each read/write pair without holding the lock during hardware waits.
static REGISTER_LOCK: SpinLock<(), LocalIrqDisabled> = SpinLock::new(());

fn with_register_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = REGISTER_LOCK.lock();
    // Atomic lock acquire/release order normal memory, not the I/O domain.
    crate::arch::mm::dma_io_fence();
    let _io_guard = RegisterIoGuard;
    f()
}

struct RegisterIoGuard;

impl Drop for RegisterIoGuard {
    fn drop(&mut self) {
        // The guard is dropped before REGISTER_LOCK, including on early errors.
        crate::arch::mm::dma_io_fence();
    }
}

fn register_rmw(io: &IoMem, offset: usize, clear: u32, set: u32) -> Option<u32> {
    with_register_lock(|| {
        let cur: u32 = io.read_once(offset).ok()?;
        let next = (cur & !clear) | set;
        io.write_once(offset, &next).ok()?;
        Some(next)
    })
}

/// Initializes the CCU on the K1. No-op on other platforms.
pub(crate) fn init() {
    let Some(dt) = DEVICE_TREE.get() else {
        return;
    };
    if dt
        .find_compatible(&["spacemit,k1-x", "spacemit,k1"])
        .is_none()
    {
        return; // not a K1; the CCU register blocks do not exist here.
    }

    let apbc = IoMem::acquire(APBC_BASE..APBC_BASE + APBC_SIZE);
    let apmu = IoMem::acquire(APMU_BASE..APMU_BASE + APMU_SIZE);
    let apbs = IoMem::acquire(APBS_BASE..APBS_BASE + APBS_SIZE);
    let mpmu = IoMem::acquire(MPMU_BASE..MPMU_BASE + MPMU_SIZE);
    let mpmu_gate = IoMem::acquire(MPMU_GATE_BASE..MPMU_GATE_BASE + MPMU_GATE_SIZE);
    let wdt = IoMem::acquire(WDT_BASE..WDT_BASE + WDT_SIZE).ok();
    if wdt.is_none() {
        crate::warn!(
            "[k1-ccu] WDT window {:#x} unavailable; reset-reason will not be reported",
            WDT_BASE
        );
    }
    match (apbc, apmu, apbs, mpmu, mpmu_gate) {
        (Ok(apbc), Ok(apmu), Ok(apbs), Ok(mpmu), Ok(mpmu_gate)) => {
            CCU.call_once(|| Ccu {
                apbc,
                apmu,
                apbs,
                mpmu,
                mpmu_gate,
                wdt,
            });
            crate::info!(
                "[k1-ccu] initialized (APBC + APMU[:{:#x}] + APBS + MPMU + WDT)",
                APMU_SIZE
            );
            // Clock the WDT APB without pulsing RST (that would clear WTS),
            // then print the sticky bit before any component raises a rail.
            // Only worth the register writes if the window was claimed:
            // without it nothing can be read back.
            if CCU.get().is_some_and(|ccu| ccu.wdt.is_some()) {
                enable(Periph::Wdt);
                dump_wdt_reset_reason();
            }
        }
        _ => crate::warn!("[k1-ccu] failed to map CCU register blocks"),
    }
}

/// Read an APBS register by offset (PLL spare registers, TWSI side-band).
pub fn apbs_read(offset: usize) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > APBS_SIZE {
        return None;
    }
    ccu.apbs.read_once(offset).ok()
}

/// Read-modify-write an APBS register by offset.
pub fn apbs_rmw(offset: usize, clear: u32, set: u32) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > APBS_SIZE {
        return None;
    }
    register_rmw(&ccu.apbs, offset, clear, set)
}

/// Brings `periph` out of reset and ungates its clock (idempotent).
///
/// Does nothing if the CCU was not initialized (e.g., on non-K1 platforms).
// `0 << n` and `1 << 0` are how a register field says which bits it occupies.
// Writing the identity cases as bare literals would make the surrounding
// `1 << 5` / `0xf << 1` lines the only place the layout is visible.
#[expect(clippy::identity_op)]
pub fn enable(periph: Periph) {
    let Some(ccu) = CCU.get() else {
        return;
    };
    let desc = periph.desc();
    let io = match desc.block {
        Block::Apbc => &ccu.apbc,
        Block::Apmu => &ccu.apmu,
        Block::Mpmu => &ccu.mpmu,
    };

    with_register_lock(|| {
        let cur: u32 = io.read_once(desc.offset).unwrap();

        // TWSI8: U-Boot `i2c_init_board` pulses absolute values
        // `0x4 → 0x7 → 0x3` (funclk=33 MHz in bits[6:4]=0). A plain RMW can leave
        // the functional-clock select wrong if the register was cold.
        //
        // TWSI8 has a distinct BSP gate encoding (mask=0x7, disable=0x4).
        // Other TWSI controllers use the generic gate/reset update below.
        if matches!(periph, Periph::Twsi8) {
            io.write_once(desc.offset, &0x4u32).unwrap();
            io.write_once(desc.offset, &0x7u32).unwrap();
            io.write_once(desc.offset, &0x3u32).unwrap();
        } else if matches!(periph, Periph::Sdh0 | Periph::Sdh2) {
            // U-Boot comment: SDH0_CLK_SEL=0, SDH0_CLK_DIV=1 (bits[7:5]=1).
            // Preserve reserved/other bits; force mux=0, div=1, resets deasserted,
            // AXI+IO clocks on. SDH2 (eMMC) has the identical mux/div layout
            // (Linux `sdh2_clk`: mux bits[10:8], div bits[7:5], gate bit4).
            let next =
                (cur & !((0x7 << 8) | (0x7 << 5))) | desc.reset_deassert | desc.gate_on | (1 << 5);
            io.write_once(desc.offset, &next).unwrap();
        } else if matches!(periph, Periph::Hdmi) {
            // Linux/U-Boot: parent pll1_d5_491p52 (mux=1 @ bits[7:5]), div≈1
            // @ bits[4:1], gate bit0, reset deassert bit9. FC strobe bit29.
            let next = (cur & !((0x7 << 5) | (0xf << 1) | (1 << 0) | (1 << 9)))
                | (1 << 5) // mux = 1
                | (0 << 1) // div field
                | desc.gate_on
                | desc.reset_deassert;
            io.write_once(desc.offset, &next).unwrap();
            let with_fc = next | (1 << 29);
            io.write_once(desc.offset, &with_fc).unwrap();
            io.write_once(desc.offset, &next).unwrap();
        } else {
            // Release reset first, then ungate the clock (vendor bring-up order).
            let deasserted = (cur & !desc.reset_mask) | desc.reset_deassert;
            io.write_once(desc.offset, &deasserted).unwrap();
            let ungated = (deasserted & !desc.gate_mask) | desc.gate_on;
            io.write_once(desc.offset, &ungated).unwrap();
        }
    });
}

/// Read an APMU register by offset within the CCU APMU window.
///
/// Returns `None` if the CCU is not initialized or `offset` is out of range.
pub fn apmu_read(offset: usize) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > APMU_SIZE {
        return None;
    }
    ccu.apmu.read_once(offset).ok()
}

/// Read-modify-write an APMU register by offset within the CCU APMU window.
///
/// Used for SoC glue bits that live in APMU but are not a full [`Periph`]
/// (e.g. ComboPHY `phy_sel` @ `0xd4282910` = APMU+`0x110`). Returns `None` if
/// the CCU is not initialized or `offset` is out of range.
pub fn apmu_rmw(offset: usize, clear: u32, set: u32) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > APMU_SIZE {
        return None;
    }
    register_rmw(&ccu.apmu, offset, clear, set)
}

/// `APMU_LCD_CLK_RES_CTRL1` — DPU/DSI clock mux/div/gate + LCD/MIPI/ESC resets.
const APMU_LCD_CLK_RES_CTRL1: usize = 0x44;
/// `APMU_LCD_CLK_RES_CTRL2` — `dpu_mclk`/`dpu_pxclk` mux/div/gate (2REG_FC_V4) +
/// `RESET_LCD_MCLK`.
const APMU_LCD_CLK_RES_CTRL2: usize = 0x4c;

/// `APB_SPARE7` (APBS +0x118): PLL2 `swcr1` — analog config bytes
/// `reg5[7:0] reg6[15:8] reg7[23:16] reg8[31:24]`.
const APBS_SPARE7_PLL2_CFG: usize = 0x118;
/// `APB_SPARE8` (APBS +0x11c): PLL2 `swcr2` — output-divider enables
/// (`pll2_d1..d8` = bits 0..7) — vendor `SPACEMIT_CCU_GATE_FACTOR(pll2_dN)`.
const APBS_SPARE8_PLL2_SEL: usize = 0x11c;
/// `APB_SPARE9` (APBS +0x120): PLL2 `swcr3` — `div_frc[23:0]`, `div_int[30:24]`,
/// `pll_en` bit31.
const APBS_SPARE9_PLL2_CTRL: usize = 0x120;
/// `MPMU_POSR` PLL lock-status register; `POSR_PLL2_LOCK` = bit28.
const MPMU_POSR: usize = 0x10;

/// PLL2 @ 2.8 GHz: `PLL_RATE(2800000000, 0x66, 0xdd, 0x50, 0x00, 0x3a,
/// 0x155555)` from the vendor `pll2_rate_tbl` — the rate the Muse Book
/// U-Boot DT requests via `pll2-freq`, giving `pll2_d3` = 933.33 MHz for
/// `dpu_bit_clk`.
const PLL2_2800M_SWCR1: u32 = 0x0050_dd66; // reg8=0x00 reg7=0x50 reg6=0xdd reg5=0x66
const PLL2_2800M_SWCR3: u32 = (0x3a << 24) | 0x15_5555;

/// Brings PLL2 up at 2.8 GHz and ungates the `pll2_d3` (933.33 MHz) output
/// divider, following the vendor `ccu_pll_set_rate`/`ccu_pll_enable` sequence
/// (config while disabled, set `pll_en`, poll `MPMU_POSR` for lock).
/// Idempotent. Returns `false` if the PLL fails to lock (~3 ms timeout).
fn pll2_enable_2800m() -> bool {
    let Some(swcr3) = apbs_read(APBS_SPARE9_PLL2_CTRL) else {
        return false;
    };
    let already_on = swcr3 == (PLL2_2800M_SWCR3 | (1 << 31));

    if !already_on {
        // Program while disabled (rate change requires pll_en=0).
        apbs_rmw(APBS_SPARE9_PLL2_CTRL, 1 << 31, 0);
        apbs_rmw(APBS_SPARE7_PLL2_CFG, !0, PLL2_2800M_SWCR1);
        apbs_rmw(APBS_SPARE9_PLL2_CTRL, !0, PLL2_2800M_SWCR3);
        apbs_rmw(APBS_SPARE9_PLL2_CTRL, 0, 1 << 31);
        delay_us(50);
    }

    // A matching control word does not prove that the PLL has locked.
    let mut locked = false;
    for _ in 0..600 {
        let posr = CCU
            .get()
            .and_then(|c| c.mpmu.read_once(MPMU_POSR).ok())
            .unwrap_or(0u32);
        if posr & (1 << 28) != 0 {
            locked = true;
            break;
        }
        delay_us(5);
    }
    if !locked {
        crate::warn!("[k1-ccu] pll2 did not lock at 2.8GHz");
        return false;
    }

    // Ungate pll2_d3 (bit2).
    apbs_rmw(APBS_SPARE8_PLL2_SEL, 0, 1 << 2).is_some()
}

/// Brings up the DPU/MIPI-DSI clock tree for the Muse Book internal eDP panel
/// (LT8911EXB bridge) and releases the associated APMU resets.
///
/// Cross-checked against vendor Linux `drivers/clk/spacemit/ccu-spacemit-k1x.c`
/// (`dpu_mclk`/`dpu_esc_clk`/`dpu_bit_clk`/`dpu_pxclk`/`dpu_hclk`) and `drivers/reset/reset-spacemit-k1x.c` (`RESET_LCD`,
/// `RESET_DSI_ESC`, `RESET_MIPI`, `RESET_LCD_MCLK`). Field layout, from the
/// `SPACEMIT_CCU_*` macro parameters:
///
/// `CTRL1` (`@0x44`):
/// - `dpu_esc_clk`:  mux `[1:0]`, gate bit2         (parent 0 = `pll1_d48_51p2_ap` = 51.2 MHz exact)
/// - `dpu_bit_clk`:  div `[19:17]`, mux `[22:20]`, gate bit16, FC strobe bit31
/// - `dpu_hclk`:     gate bit5 (no parent select)
/// - `RESET_DSI_ESC`: bit3, `RESET_LCD`: bit4, `RESET_MIPI`: bit15 (deassert = set)
///
/// `CTRL2` (`@0x4c`):
/// - `dpu_mclk`:  div `[4:1]`, mux `[7:5]`, gate bit0; parent 1 =
///   `pll1_d5_491p52` = 491.52 MHz exact, div=1
/// - `dpu_pxclk`: div `[20:17]`, mux `[23:21]`, gate bit16
/// - `RESET_LCD_MCLK`: bit9 (deassert = set)
///
/// `dpu_mclk`/`dpu_pxclk` are `CLK_DIV_TYPE_2REG_FC_V4`: mux/div/gate fields
/// live in `CTRL2`, but the frequency-change strobe bits (`dpu_mclk` = bit29,
/// `dpu_pxclk` = bit30) are pulsed on `CTRL1` (same register that carries
/// `dpu_bit_clk`'s own FC strobe, bit31).
///
/// Rate choice — replicate the Muse Book *U-Boot* configuration 1:1 (the one
/// that demonstrably scans out clean video on this panel). U-Boot DT:
/// `pll2-freq=2800MHz`, `bit-clk=933MHz`, `pix-clk=142MHz`; its
/// `ccu_mix_calc_best_rate` lands on:
/// - `dpu_bit_clk` = `pll2_d3` (mux=2) / 1 = **933.33 MHz** (exact)
/// - `dpu_pxclk`   = `pll1_d6_409p6` (mux=0) / 3 = **136.53 MHz** (closest)
///
/// The LT8911 bridge's adaptive clock recovery requires this vendor clock
/// pair. Returns the programmed `(pxclk_hz, bitclk_hz)`. If PLL2 cannot lock,
/// the fallback uses PLL1 at 153.6 MHz / 1228.8 MHz.
// See the note on `enable`: the identity shifts spell out field positions.
#[expect(clippy::identity_op)]
pub fn enable_lcd_dsi() -> (u32, u32) {
    let Some(ccu) = CCU.get() else {
        crate::warn!("[k1-ccu] enable_lcd_dsi: CCU not initialized");
        return (0, 0);
    };

    let use_vendor_rates = pll2_enable_2800m();
    if !use_vendor_rates {
        crate::warn!("[k1-ccu] enable_lcd_dsi: pll2 unavailable — falling back to 1228.8/153.6");
    } else {
        // dpu_pxclk parent pll1_d6_409p6: ensure the PLL1 /6 post-divider
        // gate is on (`APB_SPARE2` +0x104 bit5; usually already set since
        // many system clocks share it).
        apbs_rmw(0x104, 0, 1 << 5);
    }

    let io = &ccu.apmu;

    // --- CTRL2 steady-state fields: dpu_mclk (mux=1, div=1(→/1)) + dpu_pxclk +
    // RESET_LCD_MCLK deassert. Written before the FC strobe (which lives in
    // CTRL1) per the vendor `ccu_mix_set_rate` order.
    // pxclk parents: 0=409.6, 1=491.52, 2=614.4, 3=307.2 (+pll2_d7/d8).
    const DPU_MCLK: u32 = (1 << 5) | (0 << 1) | (1 << 0); // mux=1, div_field=0, gate
    const RESET_LCD_MCLK: u32 = 1 << 9;
    let (dpu_pxclk, pxclk_hz) = if use_vendor_rates {
        // pll1_d6_409p6 / 3 = 136.533 MHz (U-Boot's pick for pix-clk=142MHz).
        (
            (0u32 << 21) | (2 << 17) | (1 << 16), // mux=0, div_field=2 (→/3), gate
            136_533_333u32,
        )
    } else {
        // pll1_d4_614p4 / 4 = 153.6 MHz.
        ((2u32 << 21) | (3 << 17) | (1 << 16), 153_600_000u32)
    };
    let ctrl2_next = DPU_MCLK | dpu_pxclk | RESET_LCD_MCLK;
    register_rmw(io, APMU_LCD_CLK_RES_CTRL2, !0, ctrl2_next).unwrap();

    // --- CTRL1 steady-state fields: dpu_esc_clk (mux=0) + dpu_bit_clk +
    // dpu_hclk gate + RESET_DSI_ESC/RESET_LCD/RESET_MIPI deassert. No FC yet.
    // bitclk parents: 0=819.2, 1=pll2_d2, 2=pll2_d3, 3=1228.8, ...
    const DPU_ESC_GATE: u32 = 1 << 2;
    const DPU_HCLK_GATE: u32 = 1 << 5;
    const RESET_DSI_ESC: u32 = 1 << 3;
    const RESET_LCD: u32 = 1 << 4;
    const RESET_MIPI: u32 = 1 << 15;
    let (dpu_bit, bitclk_hz) = if use_vendor_rates {
        // pll2_d3 = 2800/3 = 933.33 MHz, div 1 (exact match for bit-clk=933MHz).
        ((2u32 << 20) | (0 << 17) | (1 << 16), 933_333_333u32)
    } else {
        // pll1_d2_1228p8 / 1.
        ((3u32 << 20) | (0 << 17) | (1 << 16), 1_228_800_000u32)
    };
    let ctrl1_next =
        DPU_ESC_GATE | dpu_bit | DPU_HCLK_GATE | RESET_DSI_ESC | RESET_LCD | RESET_MIPI;
    register_rmw(io, APMU_LCD_CLK_RES_CTRL1, !0, ctrl1_next).unwrap();

    // --- Pulse the three FC (frequency-change) strobes together on CTRL1:
    // dpu_bit_clk=bit31, dpu_mclk=bit29, dpu_pxclk=bit30.
    const FC_BITS: u32 = (1 << 31) | (1 << 29) | (1 << 30);
    register_rmw(io, APMU_LCD_CLK_RES_CTRL1, 0, FC_BITS).unwrap();
    delay_us(10);
    register_rmw(io, APMU_LCD_CLK_RES_CTRL1, !0, ctrl1_next).unwrap();

    crate::info!("[k1-ccu] LCD clocks: px={}Hz bit={}Hz", pxclk_hz, bitclk_hz);
    (pxclk_hz, bitclk_hz)
}

fn delay_us(us: u64) {
    let ticks = (crate::arch::tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = crate::arch::read_tsc();
    while crate::arch::read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}

/// Releases a peripheral's reset, leaving its clock gate alone.
///
/// [`enable`] does the reset and the gate together, which suits a driver that
/// owns both. A driver whose platform layer keeps them apart needs the halves
/// separately: `aster-pvr`'s `PlatformOps` mirrors upstream's
/// `pvr_power_sequence_ops`, where power domain, clock and reset are three
/// callbacks invoked in that order.
///
/// Returns whether the write happened, which is false on a platform with no
/// CCU. Idempotent.
pub fn deassert_reset(periph: Periph) -> bool {
    let Some(ccu) = CCU.get() else {
        return false;
    };
    let desc = periph.desc();
    let io = match desc.block {
        Block::Apbc => &ccu.apbc,
        Block::Apmu => &ccu.apmu,
        Block::Mpmu => &ccu.mpmu,
    };

    register_rmw(io, desc.offset, desc.reset_mask, desc.reset_deassert).is_some()
}

/// Ungates a peripheral's clock, leaving its reset alone.
///
/// The other half of [`deassert_reset`]; see there for why the two exist apart
/// from [`enable`].
///
/// Returns whether the write happened, which is false on a platform with no
/// CCU. Idempotent.
pub fn ungate(periph: Periph) -> bool {
    let Some(ccu) = CCU.get() else {
        return false;
    };
    let desc = periph.desc();
    let io = match desc.block {
        Block::Apbc => &ccu.apbc,
        Block::Apmu => &ccu.apmu,
        Block::Mpmu => &ccu.mpmu,
    };

    register_rmw(io, desc.offset, desc.gate_mask, desc.gate_on).is_some()
}

/// Asserts a peripheral's reset, leaving its clock gate alone.
///
/// The inverse of [`deassert_reset`]. Unlike [`pulse_reset`] the peripheral is
/// left held in reset, which is what a driver wants when it is powering the
/// block down rather than restarting it.
///
/// Returns whether the write happened, which is false on a platform with no
/// CCU. Idempotent.
pub fn assert_reset(periph: Periph) -> bool {
    let Some(ccu) = CCU.get() else {
        return false;
    };
    let desc = periph.desc();
    let io = match desc.block {
        Block::Apbc => &ccu.apbc,
        Block::Apmu => &ccu.apmu,
        Block::Mpmu => &ccu.mpmu,
    };

    // Asserting is "every reset bit at the value deassert does not use", which
    // keeps this correct for both polarities: APBC resets are active-high and
    // APMU's are active-low.
    register_rmw(
        io,
        desc.offset,
        desc.reset_mask,
        desc.reset_mask & !desc.reset_deassert,
    )
    .is_some()
}

/// Gates a peripheral's clock, leaving its reset alone.
///
/// The inverse of [`ungate`].
///
/// Returns whether the write happened, which is false on a platform with no
/// CCU. Idempotent.
pub fn gate(periph: Periph) -> bool {
    let Some(ccu) = CCU.get() else {
        return false;
    };
    let desc = periph.desc();
    let io = match desc.block {
        Block::Apbc => &ccu.apbc,
        Block::Apmu => &ccu.apmu,
        Block::Mpmu => &ccu.mpmu,
    };

    register_rmw(io, desc.offset, desc.gate_mask, 0).is_some()
}

/// Read-modify-write `MPMU_ACGR`, the clock-gate register shared by several
/// unrelated blocks.
///
/// Returns the value written, or `None` if the CCU is not initialized. Bit 6 is
/// the TWSI fabric clock and bit 15 is `pll1_d4_614p4`; a caller touching one
/// must not clobber the other, which is the whole reason this is a
/// read-modify-write in one place rather than an `IoMem` each consumer acquires
/// for itself.
pub fn acgr_rmw(clear: u32, set: u32) -> Option<u32> {
    let ccu = CCU.get()?;
    register_rmw(&ccu.mpmu_gate, MPMU_ACGR, clear, set)
}

/// Reads `MPMU_ACGR`.
pub fn acgr_read() -> Option<u32> {
    CCU.get()?.mpmu_gate.read_once(MPMU_ACGR).ok()
}

/// Read an MPMU register in the low window (`0xd4050000`, currently 0x300).
///
/// `WDTPCR` is +0x200. Returns `None` if the CCU is not initialized or
/// `offset` is out of range.
pub fn mpmu_read(offset: usize) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > MPMU_SIZE {
        return None;
    }
    ccu.mpmu.read_once(offset).ok()
}

/// Read a SoC WDT register (`0xd4080000` + `offset`).
///
/// `WSR` is +0xC0. Returns `None` if the CCU is not initialized or
/// `offset` is out of range.
pub fn wdt_read(offset: usize) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > WDT_SIZE {
        return None;
    }
    ccu.wdt.as_ref()?.read_once(offset).ok()
}

fn dump_wdt_reset_reason() {
    let Some(wsr) = wdt_read(WDT_WSR) else {
        return;
    };
    let wts = wsr & 1;
    crate::info!(
        "[k1-rst] early WSR={:#x} WTS={} (1 = last SoC reset was the 16-bit WDT)",
        wsr,
        wts
    );
}

/// Read a register in the MPMU gate page (`0xd4051000`).
///
/// `MPMU_APRR` is +0x020 (`0xd4051020`), `MPMU_ARSR` is +0x028
/// (`0xd4051028`), `MPMU_ACGR` is +0x024. Linux `k1x_wdt.c` uses ARSR
/// bits[11:8] as a software reboot-command nibble, not a hardware reset
/// cause; APRR bit4 is `WDTR` (leave WDT out of the SoC reset). Returns
/// `None` if the CCU is not initialized or `offset` is out of range.
pub fn mpmu_gate_read(offset: usize) -> Option<u32> {
    let ccu = CCU.get()?;
    if offset.checked_add(4)? > MPMU_GATE_SIZE {
        return None;
    }
    ccu.mpmu_gate.read_once(offset).ok()
}

/// Enables both gates of the GPU's PLL1 /4 parent clock.
///
/// The BSP clock tree chains APBS `APB_SPARE2_REG` bit 3 (`pll1_d4`) with
/// MPMU `MPMU_ACGR` bit 15 (`pll1_d4_614p4`). Both must be enabled before
/// the GPU's mux/divider and peripheral gate can supply a clock.
pub fn enable_gpu_parent_clock() {
    let Some(ccu) = CCU.get() else {
        return;
    };
    register_rmw(&ccu.apbs, 0x104, 0, 1 << 3).unwrap();
    register_rmw(&ccu.mpmu_gate, MPMU_ACGR, 0, 1 << 15).unwrap();
}

/// The GPU core clock rate in Hz, read out of `APMU_GPU_CLK_RES_CTRL`.
///
/// Firmware derives power-management timers from this rate. A zero or
/// incorrect rate can idle the GPU before a submitted job runs, so decode
/// the programmed mux and divider rather than assuming the parent rate.
///
/// # Decoding
///
/// The vendor builds this clock as
/// `SPACEMIT_CCU_DIV_FC_MUX_GATE(gpu_clk, ..., APMU_GPU_CLK_RES_CTRL, 12, 3,
/// BIT(15), 18, 3, BIT(4), ...)` in BSP `ccu-spacemit-k1x.c`: divider at bits
/// `[14:12]`, mux at `[20:18]`. `CCU_DIV_INIT`'s table is null and its flags are
/// zero, so `divider_recalc_rate` gives `divide = field + 1` — *not* the field
/// itself.
///
/// Only the four PLL1-derived parents have a rate this module can state. PLL2
/// is programmed here but its rate depends on what asked for it, and PLL3 is
/// not touched at all, so those four selections return `None` rather than a
/// guessed rate. The K1 initialization selects parent 0.
///
/// Reference: `gpu_parent_names[]` in BSP `ccu-spacemit-k1x.c`.
pub fn gpu_core_clock_hz() -> Option<u32> {
    /// `gpu_parent_names[]`, as rates. `None` where this module cannot say.
    const PARENT_HZ: [Option<u32>; 8] = [
        Some(614_400_000), // pll1_d4_614p4
        Some(491_520_000), // pll1_d5_491p52
        Some(819_200_000), // pll1_d3_819p2
        Some(409_600_000), // pll1_d6_409p6
        None,              // pll3_d6
        None,              // pll2_d3
        None,              // pll2_d4
        None,              // pll2_d5
    ];

    let ccu = CCU.get()?;
    let reg: u32 = ccu.apmu.read_once(Periph::Gpu.desc().offset).ok()?;

    let parent = PARENT_HZ[((reg >> 18) & 0x7) as usize]?;
    let divide = ((reg >> 12) & 0x7) + 1;

    Some(parent / divide)
}

/// Commits a peripheral's mux and divider by strobing its frequency-change bit.
///
/// The vendor's `ccu_mix_trigger_fc` (BSP `drivers/clk/spacemit/ccu_mix.c`) sets
/// the bit and polls until hardware clears it, and the clock framework reaches it
/// from `clk_set_rate`. Ungating alone is not enough: the GPU's
/// `APMU_GPU_CLK_RES_CTRL` comes out of reset already selecting a parent and a
/// divider, but until this strobe those fields are not live, and an unclocked
/// Rogue answers no reads at all rather than faulting.
///
/// The divider field is zero-based: `CCU_DIV_INIT(12, 3, NULL, 0)` uses
/// `divide = field + 1`, not the field itself. [`gpu_core_clock_hz`] decodes
/// the resulting rate for firmware.
///
/// Returns whether the strobe was seen to complete. `false` means either that
/// there is no CCU, that this peripheral has no frequency-change bit, or that
/// hardware never cleared it.
pub fn commit_clock(periph: Periph) -> bool {
    let Some(ccu) = CCU.get() else {
        return false;
    };
    let desc = periph.desc();
    if desc.fc_mask == 0 {
        return false;
    }
    let io = match desc.block {
        Block::Apbc => &ccu.apbc,
        Block::Apmu => &ccu.apmu,
        Block::Mpmu => &ccu.mpmu,
    };

    register_rmw(io, desc.offset, 0, desc.fc_mask).unwrap();

    // The vendor tries fifty times, then five thousand more before giving up.
    // Collapsed into one bound with a delay, because the point of the second
    // round upstream is only to wait longer.
    for _ in 0..5_000 {
        let val: u32 = io.read_once(desc.offset).unwrap();
        if val & desc.fc_mask == 0 {
            return true;
        }
        delay_us(1);
    }

    crate::warn!(
        "[k1-ccu] commit_clock {:?}: reg[{:#x}] never cleared {:#010x}",
        periph,
        desc.offset,
        desc.fc_mask
    );
    false
}

/// APMU register holding one power-status bit per domain.
///
/// A set bit means the domain is **on**. U-Boot's `k1x_pd_power_on` returns
/// early when the bit reads one and otherwise polls for it to become one, which
/// settles the question the register's name leaves open.
const APMU_POWER_STATUS: usize = 0xf0;

/// Power domains this module knows how to switch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerDomain {
    /// `K1X_PMU_GPU_PWR_DOMAIN`, domain index 2.
    Gpu,
}

/// One domain's control bits, all within [`APMU_POWER_STATUS`]'s block.
///
/// Each is released by **setting** it, including `isolation` — U-Boot writes
/// that bit under the comment "disable isolation". Reading the names as
/// descriptions of a resting state gets every one of them backwards.
struct DomainDesc {
    /// APMU offset of the domain's control register (`reg_pwr_ctrl`).
    ctrl: usize,
    isolation: u32,
    sleep1: u32,
    sleep2: u32,
    /// This domain's bit in [`APMU_POWER_STATUS`].
    status: u32,
}

impl PowerDomain {
    fn desc(self) -> DomainDesc {
        match self {
            // `k1-x.dtsi`'s `power-domain@SPT_PD_GPU`: reg_pwr_ctrl 0xd0,
            // bit_isolation 1, bit_sleep1 2, bit_sleep2 3, bit_pwr_stat 0.
            // It declares no `use_hw`, so this is a software-sequenced domain --
            // the hardware path is LCD's alone.
            PowerDomain::Gpu => DomainDesc {
                ctrl: 0xd0,
                isolation: 1 << 1,
                sleep1: 1 << 2,
                sleep2: 1 << 3,
                status: 1 << 0,
            },
        }
    }
}

/// Switches a power domain on, returning whether it is on when this returns.
///
/// The sequence is U-Boot's `k1x_pd_power_on` (BSP
/// `drivers/power/domain/k1x-power-domain.c`), which is the code that runs before
/// this kernel and is unambiguous about the polarities:
///
///  1. a **set** status bit means the domain is already on — return;
///  2. **set** `sleep1`, wait 25 µs;
///  3. **set** `sleep1 | sleep2`, wait 25 µs and again 25 µs;
///  4. **set** `isolation`, wait 15 µs;
///  5. poll until the status bit reads **one**.
///
/// The control bits release sleep and isolation when set; status is high
/// once the domain is powered. Polling is bounded and runs without the register
/// lock held.
pub fn power_domain_on(domain: PowerDomain) -> bool {
    let Some(ccu) = CCU.get() else {
        return false;
    };
    let desc = domain.desc();
    let io = &ccu.apmu;

    let status: u32 = io.read_once(APMU_POWER_STATUS).unwrap();

    if status & desc.status != 0 {
        return true;
    }

    // Wake the domain's MCU: `sleep1` first, then both together.
    register_rmw(io, desc.ctrl, 0, desc.sleep1).unwrap();
    delay_us(25);

    register_rmw(io, desc.ctrl, 0, desc.sleep1 | desc.sleep2).unwrap();
    delay_us(25);
    delay_us(25);

    // "disable isolation", in U-Boot's words, by setting the bit.
    register_rmw(io, desc.ctrl, 0, desc.isolation).unwrap();
    delay_us(15);

    for _ in 0..10_000 {
        let status: u32 = io.read_once(APMU_POWER_STATUS).unwrap();
        if status & desc.status != 0 {
            return true;
        }
        delay_us(6);
    }

    crate::warn!(
        "[k1-ccu] power_domain_on {:?}: timed out, ctrl[{:#x}]={:#010x} status[{:#x}]={:#010x}",
        domain,
        desc.ctrl,
        io.read_once::<u32>(desc.ctrl).unwrap(),
        APMU_POWER_STATUS,
        io.read_once::<u32>(APMU_POWER_STATUS).unwrap()
    );
    false
}

/// Soft-assert then deassert an APMU peripheral's reset bits (clock left as-is).
///
/// Used by DWC3 glue after PHY bring-up (`dwc3-spacemit` pulses `RESET_USB3_0`).
pub fn pulse_reset(periph: Periph) {
    let Some(ccu) = CCU.get() else {
        return;
    };
    let desc = periph.desc();
    if desc.block != Block::Apmu || desc.reset_mask == 0 {
        return;
    }
    let io = &ccu.apmu;
    // APMU resets are active-low. Preserve unrelated gates across the delay:
    // another consumer may update the shared register while reset is asserted.
    register_rmw(io, desc.offset, desc.reset_mask, 0).unwrap();
    delay_us(150);
    register_rmw(io, desc.offset, desc.reset_mask, desc.reset_deassert).unwrap();
}
