// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 DWC3 / ComboPHY glue then xHCI initialization.

//! SpacemiT K1 DWC3 (USB3) — PHY + host-mode glue, then xHCI.
//!
//! Sequence matches Linux `dwc3-spacemit.c` + ComboPHY `phy_init` + U-Boot
//! `dwc3_core_soft_reset` (Linux 6.6 skips the last step in host mode):
//! 1. Hub GPIO123, CCU `UsbAxi` then `Usb30`
//! 2. Pulse `RESET_USB3_0` (assert → wait → deassert) **before** PHY init
//! 3. ComboPHY `phy_sel` USB3, USB2 PHY (`mv_usb2_phy_init`), ComboPHY analog
//!    reset pulse + `spacemit_combphy_init_usb`
//! 5. GCTL `CORESOFTRESET` + GUSB2/GUSB3 `PHYSOFTRST` (U-Boot `xhci-dwc3.c`)
//! 6. GCTL `PRTCAP_HOST`, GUSB2PHYCFG / GUSB3PIPECTL / GUCTL1 Muse Book quirks
//! 7. kthread: xHCI start + enum + MSC → `usbblk0`
//!
//! Addresses: DT first (`snps,dwc3` / `spacemit,usb2-phy` @ 0xc0a30000 /
//! `spacemit,k1x-combphy`), hardcoded fallback from `k1-x.dtsi`.

use alloc::{sync::Arc, vec::Vec};

use aster_softirq::Taskless;
use ostd::{
    arch::{boot::DEVICE_TREE, ccu},
    io::IoMem,
    mm::{VmIoOnce, dma::dma_io_fence},
    sync::Mutex,
};
use spin::Once;

use crate::{
    block_device::UsbBlockDevice,
    delay::{delay_ms, delay_us},
    enumerate, hid, msc,
    xhci::Host,
};

const USB2PHY_BASE: usize = 0xc0a3_0000;
const USB2PHY_SIZE: usize = 0x200;
const COMBPHY_BASE: usize = 0xc0b1_0000;
const COMBPHY_SIZE: usize = 0x800;
const DWC3_BASE: usize = 0xc0a0_0000;
const DWC3_SIZE: usize = 0x1_0000; // k1-x.dtsi `dwc3@c0a00000` reg size

/// ComboPHY mode select — APMU+0x110 = 0xd4282910 (`phy_sel` in DT).
/// `SPACEMIT_COMBPHY_MODE_SEL` BIT(3) in `phy-spacemit-k1x-combphy.c`.
const PHY_SEL_APMU_OFF: usize = 0x110;
const COMBPHY_MODE_USB3: u32 = 1 << 3;
const APMU_USB_CLK_RES_CTRL: usize = 0x5c;

// usb2phy — `phy-k1x-ci-usb2.h` / `mv_usb2_phy_init`
const USB2_PHY_REG01: usize = 0x4;
const USB2_PHY_REG01_PLL_IS_READY: u32 = 1 << 0;
const USB2_PHY_REG01_BRINGUP: u32 = 0x60ef;
const USB2_PHY_REG04: usize = 0x10;
const USB2_PHY_REG04_AUTO_CLEAR_DIS: u32 = 1 << 2;
const USB2_PHY_REG06: usize = 0x18;
const USB2_CFG_HS_SRC_SEL: u32 = 1 << 0;
const USB2_PHY_REG0D: usize = 0x34;
const USB2_PHY_REG0D_BRINGUP: u32 = 0x1c;
const USB2_ANALOG_REG14_13: usize = 0xa4;
const USB2_ANALOG_HSDAC_IREG_EN: u32 = 1 << 4;
const USB2_ANALOG_HSDAC_ISEL_MASK: u32 = 0xf;
const USB2_ANALOG_HSDAC_ISEL_15_INC: u32 = 0xc;

// ComboPHY USB3 — `spacemit_combphy_init_usb`
const COMB_USB_REG1: usize = 0x68;
const COMB_USB_REG1_VAL: u32 = 0x0;
const COMB_USB_REG2: usize = 0x12 << 2;
const COMB_USB_REG2_VAL: u32 = 0x603a_2276;
const COMB_USB_REG3: usize = 0x02 << 2;
const COMB_USB_REG3_VAL: u32 = 0x97c;
const COMB_USB_REG4: usize = 0x06 << 2;
const COMB_USB_REG4_VAL: u32 = 0x0;
const COMB_USB_PLL_REG: usize = 0x8;
const COMB_USB_PLL_MASK: u32 = 0x1;
const COMB_USB_LFPS: usize = 0x58;
const COMB_USB_LFPS_THRES_DEFAULT: u32 = 0x3;

// DWC3 globals — `drivers/usb/dwc3/core.h`
const DWC3_GCTL: usize = 0xc110;
const DWC3_GSTS: usize = 0xc118;
const DWC3_GUCTL1: usize = 0xc11c;
const DWC3_GSNPSID: usize = 0xc120;
const DWC3_GHWPARAMS0: usize = 0xc140;
const DWC3_GHWPARAMS1: usize = 0xc144;
const DWC3_GHWPARAMS3: usize = 0xc14c;
const DWC3_GUCTL2: usize = 0xc19c;
const DWC3_GUSB2PHYCFG0: usize = 0xc200;
const DWC3_GUSB3PIPECTL0: usize = 0xc2c0;
const DWC3_GFLADJ: usize = 0xc630;
const DWC3_GDBGLTSSM: usize = 0xc164;
const DWC3_DCTL: usize = 0xc704;

const DWC3_GCTL_PRTCAP_MASK: u32 = 3 << 12;
const DWC3_GCTL_PRTCAP_HOST: u32 = 1 << 12;
const DWC3_GCTL_CORESOFTRESET: u32 = 1 << 11;
const DWC3_GCTL_SCALEDOWN_MASK: u32 = 3 << 4;
const DWC3_GCTL_DISSCRAMBLE: u32 = 1 << 3;
const DWC3_GUSB2PHYCFG_PHYSOFTRST: u32 = 1 << 31;
const DWC3_GUSB2PHYCFG_SUSPHY: u32 = 1 << 6;
const DWC3_GUSB2PHYCFG_ENBLSLPM: u32 = 1 << 8;
const DWC3_GUSB2PHYCFG_PHYIF: u32 = 1 << 3;
const DWC3_GUSB2PHYCFG_USBTRDTIM_MASK: u32 = 0xf << 10;
const USBTRDTIM_UTMI_8_BIT: u32 = 9 << 10;
const DWC3_GUSB3PIPECTL_PHYSOFTRST: u32 = 1 << 31;
const DWC3_GUSB3PIPECTL_UX_EXIT_PX: u32 = 1 << 27;
const DWC3_GUSB3PIPECTL_SUSPHY: u32 = 1 << 17;
const DWC3_GUSB3PIPECTL_DEPOCHANGE: u32 = 1 << 18;
const DWC3_GUCTL1_TX_IPGAP_LINECHECK_DIS: u32 = 1 << 28;
const DWC3_GUCTL1_PARKMODE_DISABLE_SS: u32 = 1 << 17;
const DWC3_GUCTL2_RST_ACTBITLATER: u32 = 1 << 14;

const PLL_TIMEOUT_MS: u64 = 50;
const POST_CCU_US: u64 = 150;
const PHYSOFTRST_MS: u64 = 100;
const HUB_GPIO: u32 = 123;

struct Phys {
    _usb2phy: IoMem,
    _combphy: Option<IoMem>,
}

static PHYS: Once<Phys> = Once::new();
static DWC3_IO: Mutex<Option<IoMem>> = Mutex::new(None);
static XHCI: Once<Arc<Mutex<Host>>> = Once::new();
static HID_BH: Once<Arc<Taskless>> = Once::new();
static BLK: Mutex<Vec<Arc<UsbBlockDevice>>> = Mutex::new(Vec::new());

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

    if k1_musebook_gpio::set_value(HUB_GPIO, 1) {
        log::info!("[k1-usb] usb3hub GPIO{} = 1", HUB_GPIO);
    } else {
        log::warn!(
            "[k1-usb] usb3hub GPIO{} set skipped (gpio not ready)",
            HUB_GPIO
        );
    }

    ccu::enable(ccu::Periph::UsbAxi);
    ccu::enable(ccu::Periph::Usb30);
    delay_us(POST_CCU_US);
    // `dwc3_spacemit_init`: clocks already on, then RESET_USB3_0 assert/deassert
    // *before* phy_setup. `enable` already deasserted bits 9-11; the pulse
    // below is the real controller reset (and must show a mid-value change).
    if !pulse_usb3_reset() {
        log::warn!("[k1-usb] USB3 reset failed");
        return;
    }

    match ccu::apmu_rmw(PHY_SEL_APMU_OFF, 0, COMBPHY_MODE_USB3) {
        Some(v) => log::info!(
            "[k1-usb] combphy phy_sel USB3 (APMU+{:#x}={:#010x})",
            PHY_SEL_APMU_OFF,
            v
        ),
        None => {
            log::warn!("[k1-usb] combphy phy_sel RMW failed");
            return;
        }
    }

    let (phy_base, phy_size) = dt_reg(
        &["spacemit,usb2-phy"],
        USB2PHY_BASE,
        USB2PHY_SIZE,
        Some(USB2PHY_BASE),
    );
    let Some(usb2phy) = acquire_window(phy_base, phy_size, USB2_ANALOG_REG14_13 + 4) else {
        log::warn!("[k1-usb] failed to map usb2phy at {:#x}", phy_base);
        return;
    };
    log::info!("[k1-usb] usb2phy mapped at {:#x}", phy_base);
    if !usb2phy_init(&usb2phy) {
        log::warn!(
            "[k1-usb] usb2phy PLL timeout REG01={:#010x}",
            usb2phy.read_once::<u32>(USB2_PHY_REG01).unwrap_or(0)
        );
        return;
    }

    // `spacemit_combphy_init`: set_mode (done) → deassert phy_rst → program.
    // After PCIe PortA Rterm, bit 8 of APMU 0x3cc is already 0, so a bare
    // deassert is a no-op and the analog mux never resamples USB3. Pulse
    // only bit 8 (PortB/C APMU words are untouched).
    pulse_combophy_reset();
    delay_us(POST_CCU_US);

    let (comb_base, comb_size) =
        dt_reg(&["spacemit,k1x-combphy"], COMBPHY_BASE, COMBPHY_SIZE, None);
    let combphy = match acquire_window(comb_base, comb_size, COMB_USB_REG1 + 4) {
        Some(io) => {
            combphy_init_usb(&io);
            Some(io)
        }
        None => {
            // The ComboPHY is also PCIe PortA's PUPHY, which the PCIe driver
            // (initialised first) keeps mapped for Rterm calibration. Borrow
            // that mapping: skipping the USB3 PHY init leaves the PIPE clock
            // dead and the xHCI `HCRST` handshake never completes.
            match aster_pci::k1_porta_phy() {
                Some(io) if comb_base == COMBPHY_BASE => {
                    log::info!(
                        "[k1-usb] combphy puphy {:#x} held by PCIe; borrowing its mapping",
                        comb_base
                    );
                    let io = io.clone();
                    combphy_init_usb(&io);
                    Some(io)
                }
                _ => {
                    log::warn!(
                        "[k1-usb] combphy puphy {:#x} busy and not shareable; SS will fail",
                        comb_base
                    );
                    None
                }
            }
        }
    };

    let (dwc_base, dwc_size) = dt_reg(&["snps,dwc3"], DWC3_BASE, DWC3_SIZE, Some(DWC3_BASE));
    let Some(io) = acquire_window(dwc_base, dwc_size, DWC3_DCTL + 4) else {
        log::warn!("[k1-usb] failed to map dwc3 at {:#x}", dwc_base);
        return;
    };
    log::info!(
        "[k1-usb] dwc3 mapped at {:#x} size={:#x}",
        dwc_base,
        dwc_size
    );

    let snpsid: u32 = io.read_once(DWC3_GSNPSID).unwrap_or(0);
    let hwparams0: u32 = io.read_once(DWC3_GHWPARAMS0).unwrap_or(0);
    let hwparams1: u32 = io.read_once(DWC3_GHWPARAMS1).unwrap_or(0);
    let hwparams3: u32 = io.read_once(DWC3_GHWPARAMS3).unwrap_or(0);
    log::debug!(
        "[k1-usb] dwc3 GSNPSID={:#010x} GHWPARAMS0={:#010x} GHWPARAMS1={:#010x} GHWPARAMS3={:#010x}",
        snpsid,
        hwparams0,
        hwparams1,
        hwparams3
    );
    let family = snpsid & 0xffff_0000;
    if snpsid == 0 || snpsid == 0xffff_ffff || (family != 0x5533_0000 && family != 0x3331_0000) {
        log::warn!("[k1-usb] dwc3 GSNPSID looks dead");
        return;
    }

    dump_dwc3(&io, "pre-softrst");
    dwc3_core_soft_reset(&io);
    dump_dwc3(&io, "post-softrst");
    if let Some(ref phy) = combphy {
        log::info!(
            "[k1-usb] combphy after softrst PLL={:#010x} REG2={:#010x}",
            phy.read_once::<u32>(COMB_USB_PLL_REG).unwrap_or(0),
            phy.read_once::<u32>(COMB_USB_REG2).unwrap_or(0)
        );
    }
    log::info!(
        "[k1-usb] usb2phy after softrst REG01={:#010x}",
        usb2phy.read_once::<u32>(USB2_PHY_REG01).unwrap_or(0)
    );

    dwc3_host_config(&io);
    dma_io_fence();

    PHYS.call_once(|| Phys {
        _usb2phy: usb2phy,
        _combphy: combphy,
    });
    *DWC3_IO.lock() = Some(io);
    log::info!("[k1-usb] dwc3 host glue OK (xHCI in kthread)");
}

pub(super) fn scan_and_register() {
    let Some(io) = DWC3_IO.lock().take() else {
        return;
    };
    let host = match Host::start(io) {
        Ok(h) => Arc::new(Mutex::new(h)),
        Err(e) => {
            log::warn!("[k1-usb] xhci start failed: {:?}", e);
            return;
        }
    };
    XHCI.call_once(|| host.clone());
    attach_devices(&host);
    // The hard timer only schedules the serial bottom half. HID completion
    // dispatch and best-effort detach can acquire input locks and wait for a
    // command, so they must not run in hard IRQ context.
    HID_BH.call_once(|| Taskless::new(hid_bottom_half));
    ostd::timer::register_callback_on_cpu(hid_timer_poll);
    log::info!("[k1-usb] hid poll via bottom half (8ms) + poll_hotplug");
}

static HID_POLL_DIV: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// `poll_hotplug` calls, for the once-a-second topology check.
static HOTPLUG_TICK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn hid_timer_poll() {
    use core::sync::atomic::Ordering;
    if !HID_POLL_DIV
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(8)
    {
        return;
    }
    if let Some(bh) = HID_BH.get() {
        bh.schedule();
    }
}

fn hid_bottom_half() {
    let Some(host) = XHCI.get() else {
        return;
    };
    hid::poll(host);
}

pub(super) fn poll_hotplug() -> Vec<Arc<UsbBlockDevice>> {
    let Some(host) = XHCI.get() else {
        return Vec::new();
    };
    hid::poll(host);
    // Full rescan resets already-addressed HID slots. Keep the MSC-only
    // retry while no HID is live; disk hotplug after a keyboard attaches
    // is unsupported. Only rescan when a root port or a hub port actually
    // changed, and only look once a second: the registry calls this every
    // 250 ms. Unconditional rescans would repeatedly re-address the hubs
    // and exhaust the controller's four slots.
    use core::sync::atomic::Ordering;
    let tick = HOTPLUG_TICK.fetch_add(1, Ordering::Relaxed);
    if tick.is_multiple_of(4)
        && BLK.lock().is_empty()
        && !hid::has_live()
        && enumerate::topology_changed(host)
    {
        attach_devices(host);
    }
    // Hand out every device the registry has not seen yet — including the
    // ones `scan_and_register` attached at boot from the usb kthread, which
    // may have raced past the registry's `collect_all`. Without a consumer
    // thread, a bio enqueued while another caller holds the synchronous
    // path in `UsbBlockDevice::enqueue` would strand in the queue forever.
    let blk = BLK.lock();
    let mut handed = HANDED_TO_REGISTRY.lock();
    let fresh = blk[*handed..].to_vec();
    *handed = blk.len();
    fresh
}

/// How many of [`BLK`]'s devices have been handed to the block registry
/// (which spawns their `handle_requests` thread).
static HANDED_TO_REGISTRY: Mutex<usize> = Mutex::new(0);

fn attach_devices(host: &Arc<Mutex<Host>>) -> Vec<Arc<UsbBlockDevice>> {
    let found = enumerate::scan(host);
    let mut fresh = Vec::new();
    let mut minor = BLK.lock().len() as u32;
    for f in found {
        match msc::bringup(host, f.slot, f.vid, f.pid) {
            Ok(disk) => {
                let name = alloc::format!("usbblk{}", minor);
                match UsbBlockDevice::register(host.clone(), disk, &name, minor) {
                    Ok(dev) => {
                        BLK.lock().push(dev.clone());
                        fresh.push(dev);
                        minor += 1;
                    }
                    Err(e) => log::warn!("[k1-usb] usbblk register failed: {:?}", e),
                }
            }
            Err(e) => log::info!("[k1-usb] msc bringup slot{}: {:?}", f.slot, e),
        }
    }
    if fresh.is_empty() {
        log::debug!("[k1-usb] xhci enum done; no MSC disk (insert U-disk and retry / reboot)");
    }
    fresh
}

fn pulse_usb3_reset() -> bool {
    // RESET_USB3_0: APMU 0x5c bits 9/10/11, active-low (assert = clear).
    let before = ccu::apmu_read(APMU_USB_CLK_RES_CTRL).unwrap_or(0);
    let asserted = ccu::assert_reset(ccu::Periph::Usb30);
    let mid = ccu::apmu_read(APMU_USB_CLK_RES_CTRL).unwrap_or(0);
    delay_us(POST_CCU_US);
    let deasserted = ccu::deassert_reset(ccu::Periph::Usb30);
    delay_us(POST_CCU_US);
    let after = ccu::apmu_read(APMU_USB_CLK_RES_CTRL).unwrap_or(0);
    log::info!(
        "[k1-usb] RESET_USB3_0 pulse APMU+{:#x} {:#010x} -> {:#010x} -> {:#010x} (assert_ok={} deassert_ok={})",
        APMU_USB_CLK_RES_CTRL,
        before,
        mid,
        after,
        asserted as u8,
        deasserted as u8
    );
    asserted && deasserted
}

fn pulse_combophy_reset() {
    match aster_pci::k1_combophy_reset_pulse() {
        Ok((before, asserted, after)) => log::info!(
            "[k1-usb] combphy reset pulse: APMU[0x3cc] {:#010x} -> {:#010x} -> {:#010x} (bit8 {} -> {} -> {})",
            before,
            asserted,
            after,
            (before >> 8) & 1,
            (asserted >> 8) & 1,
            (after >> 8) & 1
        ),
        Err(err) => {
            log::warn!(
                "[k1-usb] combphy reset pulse failed ({:?}); falling back to deassert",
                err
            );
            match aster_pci::k1_combophy_reset_deassert() {
                Ok((before, after)) => log::info!(
                    "[k1-usb] combphy reset deassert: APMU[0x3cc] {:#010x} -> {:#010x} (bit8 was {})",
                    before,
                    after,
                    (before >> 8) & 1
                ),
                Err(e) => log::warn!("[k1-usb] combphy reset deassert failed: {:?}", e),
            }
        }
    }
}

fn usb2phy_init(phy: &IoMem) -> bool {
    // `mv_usb2_phy_init` in `drivers/usb/phy/phy-k1x-ci-usb2.c`.
    delay_us(POST_CCU_US);
    if !wait_usb2_pll(phy) {
        return false;
    }
    log::info!("[k1-usb] usb2phy PLL ready");
    phy.write_once(USB2_PHY_REG01, &USB2_PHY_REG01_BRINGUP).ok();
    phy.write_once(USB2_PHY_REG0D, &USB2_PHY_REG0D_BRINGUP).ok();
    let mut ana: u32 = phy.read_once(USB2_ANALOG_REG14_13).unwrap_or(0);
    ana = (ana & !USB2_ANALOG_HSDAC_ISEL_MASK)
        | USB2_ANALOG_HSDAC_ISEL_15_INC
        | USB2_ANALOG_HSDAC_IREG_EN;
    phy.write_once(USB2_ANALOG_REG14_13, &ana).ok();
    let mut reg06: u32 = phy.read_once(USB2_PHY_REG06).unwrap_or(0);
    reg06 &= !USB2_CFG_HS_SRC_SEL;
    phy.write_once(USB2_PHY_REG06, &reg06).ok();
    let mut reg04: u32 = phy.read_once(USB2_PHY_REG04).unwrap_or(0);
    reg04 |= USB2_PHY_REG04_AUTO_CLEAR_DIS;
    phy.write_once(USB2_PHY_REG04, &reg04).ok();
    log::info!(
        "[k1-usb] usb2phy bringup ANA={:#010x} REG06={:#010x} REG04={:#010x}",
        phy.read_once::<u32>(USB2_ANALOG_REG14_13).unwrap_or(0),
        phy.read_once::<u32>(USB2_PHY_REG06).unwrap_or(0),
        phy.read_once::<u32>(USB2_PHY_REG04).unwrap_or(0)
    );
    true
}

fn combphy_init_usb(phy: &IoMem) {
    // `spacemit_combphy_init_usb` — defaults for amp/deemp (no OVRD).
    phy.write_once(COMB_USB_REG1, &COMB_USB_REG1_VAL).ok();
    phy.write_once(COMB_USB_REG2, &COMB_USB_REG2_VAL).ok();
    phy.write_once(COMB_USB_REG3, &COMB_USB_REG3_VAL).ok();
    phy.write_once(COMB_USB_REG4, &COMB_USB_REG4_VAL).ok();
    let mut ok = false;
    for _ in 0..1000 {
        let pll: u32 = phy.read_once(COMB_USB_PLL_REG).unwrap_or(0);
        if pll & COMB_USB_PLL_MASK == COMB_USB_PLL_MASK {
            ok = true;
            break;
        }
        delay_us(10);
    }
    let mut lfps: u32 = phy.read_once(COMB_USB_LFPS).unwrap_or(0);
    lfps = (lfps & !(0x7 << 8)) | (COMB_USB_LFPS_THRES_DEFAULT << 8);
    phy.write_once(COMB_USB_LFPS, &lfps).ok();
    log::info!(
        "[k1-usb] combphy USB3 init pll_ok={} REG2={:#010x} PLL={:#010x} LFPS={:#010x}",
        ok as u8,
        phy.read_once::<u32>(COMB_USB_REG2).unwrap_or(0),
        phy.read_once::<u32>(COMB_USB_PLL_REG).unwrap_or(0),
        phy.read_once::<u32>(COMB_USB_LFPS).unwrap_or(0)
    );
    if !ok {
        log::warn!("[k1-usb] combphy USB PLL timeout; SuperSpeed may be unavailable");
    }
}

fn dwc3_core_soft_reset(io: &IoMem) {
    // U-Boot `dwc3_core_soft_reset` / `dwc3_phy_reset` in xhci-dwc3.c.
    // Linux 6.6 skips this in host mode; without it the PIPE/UTMI interface
    // never syncs after the ComboPHY was used for PCIe Rterm.
    let mut pipe: u32 = io.read_once(DWC3_GUSB3PIPECTL0).unwrap_or(0);
    let mut p2: u32 = io.read_once(DWC3_GUSB2PHYCFG0).unwrap_or(0);
    pipe &= !DWC3_GUSB3PIPECTL_SUSPHY;
    p2 &= !DWC3_GUSB2PHYCFG_SUSPHY;
    io.write_once(DWC3_GUSB3PIPECTL0, &pipe).ok();
    io.write_once(DWC3_GUSB2PHYCFG0, &p2).ok();

    let mut gctl: u32 = io.read_once(DWC3_GCTL).unwrap_or(0);
    gctl |= DWC3_GCTL_CORESOFTRESET;
    io.write_once(DWC3_GCTL, &gctl).ok();

    pipe |= DWC3_GUSB3PIPECTL_PHYSOFTRST;
    p2 |= DWC3_GUSB2PHYCFG_PHYSOFTRST;
    io.write_once(DWC3_GUSB3PIPECTL0, &pipe).ok();
    io.write_once(DWC3_GUSB2PHYCFG0, &p2).ok();
    log::info!(
        "[k1-usb] dwc3 softrst assert GCTL={:#010x} GUSB2PHYCFG={:#010x} GUSB3PIPECTL={:#010x}",
        io.read_once::<u32>(DWC3_GCTL).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB2PHYCFG0).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB3PIPECTL0).unwrap_or(0)
    );
    delay_ms(PHYSOFTRST_MS);

    pipe &= !DWC3_GUSB3PIPECTL_PHYSOFTRST;
    p2 &= !DWC3_GUSB2PHYCFG_PHYSOFTRST;
    io.write_once(DWC3_GUSB3PIPECTL0, &pipe).ok();
    io.write_once(DWC3_GUSB2PHYCFG0, &p2).ok();
    delay_ms(PHYSOFTRST_MS);

    gctl = io.read_once(DWC3_GCTL).unwrap_or(gctl);
    gctl &= !DWC3_GCTL_CORESOFTRESET;
    io.write_once(DWC3_GCTL, &gctl).ok();
    log::info!(
        "[k1-usb] dwc3 softrst release GCTL={:#010x} GUSB2PHYCFG={:#010x} GUSB3PIPECTL={:#010x}",
        io.read_once::<u32>(DWC3_GCTL).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB2PHYCFG0).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB3PIPECTL0).unwrap_or(0)
    );
}

fn dwc3_host_config(io: &IoMem) {
    // Muse Book `dwc3@c0a00000`: dr_mode=host, utmi, dis_enblslpm,
    // dis_u2/u3_susphy, dis-del-phy-power-chg, dis-tx-ipgap-linecheck,
    // parkmode-disable-ss. Linux `dwc3_phy_setup` + `dwc3_set_prtcap`.
    let mut pipe: u32 = io.read_once(DWC3_GUSB3PIPECTL0).unwrap_or(0);
    pipe &= !DWC3_GUSB3PIPECTL_UX_EXIT_PX;
    pipe &= !DWC3_GUSB3PIPECTL_SUSPHY;
    pipe &= !DWC3_GUSB3PIPECTL_DEPOCHANGE; // snps,dis-del-phy-power-chg-quirk
    io.write_once(DWC3_GUSB3PIPECTL0, &pipe).ok();

    let mut p2: u32 = io.read_once(DWC3_GUSB2PHYCFG0).unwrap_or(0);
    p2 &= !DWC3_GUSB2PHYCFG_PHYIF; // UTMI 8-bit
    p2 &= !DWC3_GUSB2PHYCFG_USBTRDTIM_MASK;
    p2 |= USBTRDTIM_UTMI_8_BIT;
    p2 &= !DWC3_GUSB2PHYCFG_SUSPHY; // snps,dis_u2_susphy_quirk
    p2 &= !DWC3_GUSB2PHYCFG_ENBLSLPM; // snps,dis_enblslpm_quirk
    io.write_once(DWC3_GUSB2PHYCFG0, &p2).ok();

    let mut gctl: u32 = io.read_once(DWC3_GCTL).unwrap_or(0);
    gctl &= !DWC3_GCTL_PRTCAP_MASK;
    gctl &= !DWC3_GCTL_SCALEDOWN_MASK;
    gctl &= !DWC3_GCTL_DISSCRAMBLE;
    gctl |= DWC3_GCTL_PRTCAP_HOST;
    io.write_once(DWC3_GCTL, &gctl).ok();

    let mut u1: u32 = io.read_once(DWC3_GUCTL1).unwrap_or(0);
    u1 |= DWC3_GUCTL1_TX_IPGAP_LINECHECK_DIS;
    u1 |= DWC3_GUCTL1_PARKMODE_DISABLE_SS;
    io.write_once(DWC3_GUCTL1, &u1).ok();

    // DWC_usb3 ≥ 3.10a (`core.c`): GUCTL2.RST_ACTBITLATER. We are 3.30a.
    let mut u2: u32 = io.read_once(DWC3_GUCTL2).unwrap_or(0);
    u2 |= DWC3_GUCTL2_RST_ACTBITLATER;
    io.write_once(DWC3_GUCTL2, &u2).ok();

    delay_ms(1);
    log::debug!(
        "[k1-usb] dwc3 GCTL={:#010x} GUSB2PHYCFG={:#010x} GUSB3PIPECTL={:#010x} GUCTL1={:#010x} GUCTL2={:#010x} GFLADJ={:#010x}",
        io.read_once::<u32>(DWC3_GCTL).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB2PHYCFG0).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB3PIPECTL0).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUCTL1).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUCTL2).unwrap_or(0),
        io.read_once::<u32>(DWC3_GFLADJ).unwrap_or(0)
    );
    dump_dwc3(io, "host-config");
}

fn dump_dwc3(io: &IoMem, why: &str) {
    log::debug!(
        "[k1-usb] dwc3 [{}] GCTL={:#010x} GSTS={:#010x} DCTL={:#010x} GUSB2PHYCFG={:#010x} GUSB3PIPECTL={:#010x} LTSSM={:#010x}",
        why,
        io.read_once::<u32>(DWC3_GCTL).unwrap_or(0),
        io.read_once::<u32>(DWC3_GSTS).unwrap_or(0),
        io.read_once::<u32>(DWC3_DCTL).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB2PHYCFG0).unwrap_or(0),
        io.read_once::<u32>(DWC3_GUSB3PIPECTL0).unwrap_or(0),
        io.read_once::<u32>(DWC3_GDBGLTSSM).unwrap_or(0)
    );
}

fn wait_usb2_pll(phy: &IoMem) -> bool {
    let deadline = PLL_TIMEOUT_MS.saturating_mul(1000);
    let mut waited = 0u64;
    while waited < deadline {
        let reg01: u32 = phy.read_once(USB2_PHY_REG01).unwrap_or(0);
        if reg01 & USB2_PHY_REG01_PLL_IS_READY != 0 {
            return true;
        }
        delay_us(50);
        waited = waited.saturating_add(50);
    }
    false
}

fn dt_reg(
    compat: &[&str],
    fallback: usize,
    fallback_size: usize,
    want_base: Option<usize>,
) -> (usize, usize) {
    let Some(dt) = DEVICE_TREE.get() else {
        return (fallback, fallback_size);
    };
    for n in dt.all_nodes() {
        let ok = n
            .compatible()
            .is_some_and(|c| c.all().any(|s| compat.contains(&s)));
        if !ok {
            continue;
        }
        let Some(reg) = n.reg().and_then(|mut i| i.next()) else {
            continue;
        };
        let base = reg.starting_address as usize;
        if let Some(want) = want_base
            && base != want
        {
            continue;
        }
        let size = reg.size.unwrap_or(fallback_size);
        log::info!(
            "[k1-usb] DT {} @ {:#x} size={:#x}",
            compat.first().copied().unwrap_or("?"),
            base,
            size
        );
        return (base, size);
    }
    log::info!(
        "[k1-usb] DT {} missing; fallback {:#x}",
        compat.first().copied().unwrap_or("?"),
        fallback
    );
    (fallback, fallback_size)
}

fn acquire_window(base: usize, size: usize, minimum: usize) -> Option<IoMem> {
    if !base.is_multiple_of(4) || size < minimum {
        return None;
    }
    IoMem::acquire(base..base.checked_add(size)?).ok()
}
