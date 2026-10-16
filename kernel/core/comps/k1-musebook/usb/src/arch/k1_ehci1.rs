// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 EHCI1 (USB2 Port1) host + HID keyboard.

//! SpacemiT K1 EHCI1 (USB2 Port1) host initialization and HID polling.
//!
//! Cross-checked against:
//! - Linux `ehci-k1x-ci.c` / `ehci-hcd.c` / `ehci_def.h` / `ehci.h`
//! - `k1-x.dtsi` `ehci1@c0980100` (CAPLENGTH=0x40 → ops at +0x40)
//!
//! Initialization sequence (polling, no IRQ):
//! 1. VBUS + USBMODE host + RUN + CONFIGFLAG
//! 2. Root-port reset → GET_DESCRIPTOR (Chipidea PORTSC PSPD: 0=FS 1=LS 2=HS)
//! 3. If hub: SET_ADDRESS → SET_CONFIG → port power/reset → downstream enum
//! 4. If HID boot keyboard found: SET_PROTOCOL + periodic interrupt IN poll
//!
//! DMA: K1 USB is cache-incoherent. Use CPU-uncached `DmaStream` mappings for live schedules.

use alloc::sync::Arc;

use aster_softirq::Taskless;
use ostd::{
    arch::{boot::DEVICE_TREE, ccu, read_tsc, tsc_freq},
    io::IoMem,
    mm::{
        HasDaddr, HasPaddr, HasSize, VmIo, VmIoOnce,
        dma::{DmaStream, dma_io_fence},
    },
};
use spin::Once;

use crate::desc::USB_REQ_SET_ADDRESS;

const EHCI1_BASE: usize = 0xc098_0100;
/// Cap + ops through PORTSC[0] (+0x84) and Chipidea USBMODE (+0xa8).
const EHCI1_SIZE: usize = 0x200;

const USBPHY1_BASE: usize = 0xc09c_0000;
const USBPHY1_SIZE: usize = 0x200;

// usbphy1 — Linux `phy-k1x-ci-usb2.c` / `phy-k1x-ci-usb2.h`
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

/// EHCI1 wakeup / VBUS drive — inside CCU APMU window (offset 0x3c4).
const WAKEUP_APMU_OFF: usize = 0x3c4;
const USB_VBUS_DRV: u32 = 1 << 21;

// Cap regs (relative to EHCI1_BASE)
const REG_HCSPARAMS: usize = 0x04;
const REG_HCCPARAMS: usize = 0x08;

// Ops = CAPLENGTH (0x40) + ...
const OPS: usize = 0x40;
const REG_USBCMD: usize = OPS;
const REG_USBSTS: usize = OPS + 0x04;
const REG_USBINTR: usize = OPS + 0x08;
const REG_FRINDEX: usize = OPS + 0x0c;
const REG_CTRLDSSEGMENT: usize = OPS + 0x10;
const REG_PERIODICLISTBASE: usize = OPS + 0x14;
const REG_ASYNCLISTADDR: usize = OPS + 0x18;
/// Chipidea HOST TTCTRL (ops+0x1C).
const REG_TTCTRL: usize = OPS + 0x1C;
const REG_CONFIGFLAG: usize = OPS + 0x40;
const REG_PORTSC0: usize = OPS + 0x44;
const REG_USBMODE: usize = OPS + 0x68;
/// Chipidea HOSTPC[0] (port speed) — ops+0x84.
const REG_HOSTPC0: usize = OPS + 0x84;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_PSE: u32 = 1 << 4;
const CMD_ASE: u32 = 1 << 5;
const CMD_IAAD: u32 = 1 << 6;
const CMD_PARK: u32 = 1 << 11;

const STS_HALT: u32 = 1 << 12;
#[expect(
    dead_code,
    reason = "USBSTS reclaim status belongs to the controller status bit definitions"
)]
const STS_RECL: u32 = 1 << 13;
const STS_PSS: u32 = 1 << 14;
const STS_ASS: u32 = 1 << 15;
const STS_ERR: u32 = 1 << 1;
const STS_INT: u32 = 1 << 0;
const STS_IAA: u32 = 1 << 5;

const FLAG_CF: u32 = 1 << 0;

const PORT_CONNECT: u32 = 1 << 0;
const PORT_PE: u32 = 1 << 2;
const PORT_RESET: u32 = 1 << 8;
const PORT_POWER: u32 = 1 << 12;
const PORT_LS_MASK: u32 = 3 << 10;
const PORT_CSC: u32 = 1 << 1;
const PORT_PEC: u32 = 1 << 3;
const PORT_OCC: u32 = 1 << 5;
const PORT_RWC: u32 = PORT_CSC | PORT_PEC | PORT_OCC;
/// Chipidea: port speed also lives in PORTSC[27:26] (HOSTPC often reads 0 on K1).
const PORT_PSPD_MASK: u32 = 3 << 26;

const HOSTPC_PSPD_MASK: u32 = 3 << 25;
// Chipidea TDI PORTSC[27:26] / HOSTPC[26:25]: 0=FS, 1=LS, 2=HS (Linux ehci_port_speed).

const USBMODE_CM_HC: u32 = 3;

const EHCI_LIST_END: u32 = 1;
const Q_TYPE_QH: u32 = 1 << 1;
const QH_HEAD: u32 = 1 << 15;
const QH_TOGGLE_CTL: u32 = 1 << 14;
const QH_HIGH_SPEED: u32 = 2 << 12;
const QH_FULL_SPEED: u32 = 0 << 12;
const QH_LOW_SPEED: u32 = 1 << 12;
/// FS/LS control EP through a TT (EHCI 3.6 / Linux QH_CONTROL_EP).
const QH_CONTROL_EP: u32 = 1 << 27;
const EHCI_TUNE_RL_HS: u32 = 4;
const EHCI_TUNE_MULT_HS: u32 = 1;
const EHCI_TUNE_MULT_TT: u32 = 1;

const QTD_STS_ACTIVE: u32 = 1 << 7;
const QTD_STS_HALT: u32 = 1 << 6;
const QTD_IOC: u32 = 1 << 15;
const QTD_PID_OUT: u32 = 0 << 8;
const QTD_PID_IN: u32 = 1 << 8;
const QTD_PID_SETUP: u32 = 2 << 8;
const QTD_TOGGLE: u32 = 1 << 31;
const QTD_CERR3: u32 = 3 << 10;

// DMA page1 layout (page0 = 1024-entry periodic list).
const OFF_CTRL_QH: usize = 0x000; // 128B slot, self-linked H=1
const OFF_QTD0: usize = 0x100;
const OFF_QTD1: usize = 0x140;
const OFF_QTD2: usize = 0x180;
const OFF_SETUP: usize = 0x200;
const OFF_DATA: usize = 0x220; // up to 256B config/hub desc
const OFF_INT_QH: usize = 0x400;
const OFF_INT_QTD: usize = 0x480;
const OFF_INT_BUF: usize = 0x500;
const PAGE_SIZE: usize = 4096;

/// Endpoint / TT context for control transfers.
#[derive(Clone, Copy)]
struct DevEp {
    addr: u8,
    eps: u32,
    maxpkt: u32,
    tt_hub: u8,
    tt_port: u8,
}

impl DevEp {
    fn use_tt(self) -> bool {
        self.eps != QH_HIGH_SPEED
    }
}

// USB chapter-9 / hub class
const USB_REQ_CLEAR_FEATURE: u8 = 1;
const USB_REQ_SET_FEATURE: u8 = 3;
const USB_REQ_GET_DESCRIPTOR: u8 = 6;
const USB_REQ_SET_CONFIGURATION: u8 = 9;
const USB_DT_DEVICE: u8 = 1;
const USB_DT_CONFIG: u8 = 2;
const USB_DT_INTERFACE: u8 = 4;
const USB_DT_ENDPOINT: u8 = 5;
const USB_DT_HUB: u16 = 0x29;
const USB_CLASS_HUB: u8 = 0x09;
const USB_CLASS_HID: u8 = 0x03;
const USB_HID_BOOT: u8 = 0x01;
const USB_HID_PROTO_KEYBOARD: u8 = 0x01;
const USB_HID_SET_IDLE: u8 = 0x0a;
const USB_HID_SET_PROTOCOL: u8 = 0x0b;
const HUB_PORT_CONNECTION: u16 = 0x0001;
const HUB_PORT_ENABLE: u16 = 0x0002;
const HUB_PORT_RESET: u16 = 0x0010;
const HUB_PORT_LOW_SPEED: u16 = 0x0200;
const HUB_PORT_HIGH_SPEED: u16 = 0x0400;
const HUB_C_CONNECTION: u16 = 0x0001;
const HUB_C_RESET: u16 = 0x0010;
const FEAT_PORT_RESET: u16 = 4;
const FEAT_PORT_POWER: u16 = 8;
const FEAT_C_CONNECTION: u16 = 16;
const FEAT_C_RESET: u16 = 20;

const PLL_TIMEOUT_MS: u64 = 50;
const PLL_POLL_US: u64 = 50;
const POST_CCU_US: u64 = 150;

struct Ehci1 {
    _phy: IoMem,
    io: IoMem,
    /// Keep DMA alive for the lifetime of the host / HID poll.
    dma: DmaStream,
}

static EHCI1: Once<Ehci1> = Once::new();
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static HID_BH: Once<Arc<Taskless>> = Once::new();
/// Timer ticks between HID interrupt polls (~10ms at 1000Hz).
static HID_POLL_DIV: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
// This independent scheduling hint avoids busy-waiting for PSS in a bottom half.
static HID_TRANSITION: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static HID_POLL: spin::Mutex<HidPollState> = spin::Mutex::new(HidPollState {
    phase: HidPollPhase::Running,
    initial_toggle: 0,
});

struct HidPollState {
    phase: HidPollPhase,
    // Saved for the registered endpoint before another hub port can reuse INT_QH.
    initial_toggle: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HidPollPhase {
    Running,
    Stopping { started_at: u64 },
    Starting { started_at: u64 },
}

#[derive(Debug, Eq, PartialEq)]
enum HidPollAction {
    Wait,
    Stop,
    Rearm,
    Started,
}

impl HidPollPhase {
    // EHCI 4.6: change PSE only after its previous value is reflected in PSS.
    // A completed qTD alone does not release the still-linked QH overlay.
    fn next_action(
        self,
        status: u32,
        token: u32,
        now_ticks: u64,
        timeout_ticks: u64,
    ) -> Result<HidPollAction, &'static str> {
        if status & STS_HALT != 0 {
            return Err("controller halted during keyboard polling");
        }
        match self {
            Self::Running => {
                if status & STS_PSS == 0 {
                    return Err("keyboard periodic schedule stopped unexpectedly");
                }
                if token & QTD_STS_ACTIVE != 0 {
                    Ok(HidPollAction::Wait)
                } else {
                    Ok(HidPollAction::Stop)
                }
            }
            Self::Stopping { started_at } => {
                if status & STS_PSS == 0 {
                    Ok(HidPollAction::Rearm)
                } else if now_ticks.wrapping_sub(started_at) >= timeout_ticks {
                    Err("keyboard periodic schedule stop timeout")
                } else {
                    Ok(HidPollAction::Wait)
                }
            }
            Self::Starting { started_at } => {
                if status & STS_PSS != 0 {
                    Ok(HidPollAction::Started)
                } else if now_ticks.wrapping_sub(started_at) >= timeout_ticks {
                    Err("keyboard periodic schedule start timeout")
                } else {
                    Ok(HidPollAction::Wait)
                }
            }
        }
    }
}

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

    ccu::enable(ccu::Periph::UsbP1);
    delay_us(POST_CCU_US);

    let Ok(phy) = IoMem::acquire(USBPHY1_BASE..USBPHY1_BASE + USBPHY1_SIZE) else {
        log::warn!("[k1-usb] failed to map usbphy1 at {:#x}", USBPHY1_BASE);
        return;
    };
    log::info!("[k1-usb] usbphy1 mapped at {:#x}", USBPHY1_BASE);

    if !wait_pll_ready(&phy) {
        log::warn!("[k1-usb] usbphy1 PLL ready timeout");
        return;
    }
    log::info!("[k1-usb] usbphy1 PLL ready");
    phy_bringup_host(&phy);

    let Ok(io) = IoMem::acquire(EHCI1_BASE..EHCI1_BASE + EHCI1_SIZE) else {
        log::warn!("[k1-usb] failed to map ehci1 at {:#x}", EHCI1_BASE);
        return;
    };
    log::info!("[k1-usb] ehci1 mapped at {:#x}", EHCI1_BASE);

    let cap: u32 = io.read_once(0).unwrap();
    let caplength = (cap & 0xff) as u8;
    let hciversion = ((cap >> 16) & 0xffff) as u16;
    log::debug!(
        "[k1-usb] ehci1 CAPLENGTH={:#04x} HCIVERSION={:#06x}",
        caplength,
        hciversion
    );
    if caplength != 0x40 || cap == 0 || cap == 0xffff_ffff {
        log::warn!("[k1-usb] ehci1 CAP regs unexpected ({:#010x})", cap);
        return;
    }

    // Drive VBUS for host mode (Linux `mv_ehci_setvbus`).
    match ccu::apmu_rmw(WAKEUP_APMU_OFF, 0, USB_VBUS_DRV) {
        Some(v) => log::info!(
            "[k1-usb] ehci1 VBUS_DRV (APMU+{:#x}={:#010x})",
            WAKEUP_APMU_OFF,
            v
        ),
        None => log::warn!("[k1-usb] ehci1 VBUS_DRV RMW failed"),
    }

    // CPU and controller update schedule overlays in the same cache lines.
    let Ok(dma) = DmaStream::alloc_uncached(2) else {
        log::warn!("[k1-usb] ehci1 DMA alloc failed");
        return;
    };
    log::info!("[k1-usb] ehci1 uncached DMA size={:#x}", dma.size());
    if !dma.is_cpu_uncached()
        || dma
            .daddr()
            .checked_add(dma.size())
            .is_none_or(|end| end > (1usize << 32))
    {
        log::info!("[k1-usb] ehci1 requires uncached DMA below 4GiB");
        return;
    }

    // Retain MMIO, PHY and DMA before the first device-visible pointer.
    // A failed Run handshake cannot prove that the controller stopped DMA.
    let host = EHCI1.call_once(|| Ehci1 { _phy: phy, io, dma });
    let io = &host.io;
    let dma = &host.dma;
    if let Err(msg) = host_start(io, dma) {
        log::warn!("[k1-usb] ehci1 host start failed: {}", msg);
        return;
    }

    let hcs: u32 = io.read_once(REG_HCSPARAMS).unwrap();
    let n_ports = hcs & 0xf;
    let ppc = (hcs >> 4) & 1;
    log::info!(
        "[k1-usb] ehci1 HCSPARAMS={:#010x} N_PORTS={} PPC={}",
        hcs,
        n_ports,
        ppc
    );

    if ppc != 0 {
        let mut p = r32(io, REG_PORTSC0);
        p = (p & !PORT_RWC) | PORT_POWER;
        w32(io, REG_PORTSC0, p);
        delay_ms(20);
    }

    let portsc = r32(io, REG_PORTSC0);
    let ccs = portsc & PORT_CONNECT != 0;
    let pe = portsc & PORT_PE != 0;
    let pp = portsc & PORT_POWER != 0;
    let ls = (portsc & PORT_LS_MASK) >> 10;
    log::info!(
        "[k1-usb] ehci1 PORTSC={:#010x} CCS={} PE={} PP={} LS={}",
        portsc,
        ccs as u8,
        pe as u8,
        pp as u8,
        ls
    );

    if !ccs {
        log::info!("[k1-usb] ehci1 port: no device connected");
        log::info!("[k1-usb] ehci1 probe OK (host up, empty port)");
        return;
    }

    match port_reset_and_get_desc(io, dma) {
        Ok((mut root, vid, pid, buf)) => {
            log::info!(
                "[k1-usb] ehci1 desc18: {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} ...",
                buf[0],
                buf[1],
                buf[2],
                buf[3],
                buf[4],
                buf[5],
                buf[6],
                buf[7]
            );
            log::info!(
                "[k1-usb] ehci1 idVendor={:#06x} idProduct={:#06x} class={:#04x}",
                vid,
                pid,
                buf[4]
            );
            if buf[4] == USB_CLASS_HUB
                && let Err(msg) = hub_bringup(io, dma, &mut root)
            {
                log::warn!("[k1-usb] ehci1 hub bringup failed: {}", msg);
            }
        }
        Err(msg) => log::warn!("[k1-usb] ehci1 enum failed: {}", msg),
    }

    // Dispatch input reports in a serial bottom half, outside hard timer IRQ.
    // Release the keyboard guard before arming the controller.
    let has_keyboard = super::k1_hid_kbd::HID_KBD.lock().is_some();
    if has_keyboard && let Some(host) = EHCI1.get() {
        if let Err(e) = hid_arm_periodic(&host.io, &host.dma) {
            log::error!("[k1-usb] ehci1 HID arm failed: {}", e);
        } else {
            HID_BH.call_once(|| Taskless::new(hid_bottom_half));
            ostd::timer::register_callback_on_cpu(hid_timer_poll);
            log::info!("[k1-usb] ehci1 HID keyboard polling via bottom half");
        }
    }

    log::info!("[k1-usb] ehci1 probe OK");
}

fn host_start(io: &IoMem, dma: &DmaStream) -> Result<(), &'static str> {
    // Dump pre-state for initialization diagnostics.
    let cmd0 = r32(io, REG_USBCMD);
    let sts0 = r32(io, REG_USBSTS);
    let mode0 = r32(io, REG_USBMODE);
    log::info!(
        "[k1-usb] ehci1 pre-start USBCMD={:#010x} USBSTS={:#010x} USBMODE={:#010x}",
        cmd0,
        sts0,
        mode0
    );

    // Chipidea/Marvell CI: STS_HALT is meaningless until USBMODE is host.
    // Linux `ehci_halt` skips the handshake when `!tdi_in_host_mode`.
    // Manual 16.1: USB#1 is host-only but still uses the CI programming model.
    let mut mode = mode0;
    if mode == 0xffff_ffff {
        return Err("USBMODE unreadable");
    }
    mode = (mode & !0x3) | USBMODE_CM_HC;
    w32(io, REG_USBMODE, mode);
    delay_us(100);
    log::info!("[k1-usb] ehci1 USBMODE -> {:#010x}", r32(io, REG_USBMODE));

    w32(io, REG_USBINTR, 0);

    // Halt (now that we are in host mode).
    let sts = r32(io, REG_USBSTS);
    if sts == 0xffff_ffff {
        return Err("USBSTS unreadable");
    }
    if sts & STS_HALT == 0 {
        let mut cmd = r32(io, REG_USBCMD);
        cmd &= !(CMD_RUN | (1 << 6)/* IAAD */);
        w32(io, REG_USBCMD, cmd);
        if !handshake(io, REG_USBSTS, STS_HALT, STS_HALT, 20_000) {
            log::warn!(
                "[k1-usb] ehci1 halt fail USBCMD={:#010x} USBSTS={:#010x}",
                r32(io, REG_USBCMD),
                r32(io, REG_USBSTS)
            );
            return Err("halt timeout");
        }
    }

    // Reset HC (must be halted).
    let mut cmd = r32(io, REG_USBCMD) | CMD_RESET;
    w32(io, REG_USBCMD, cmd);
    if !handshake(io, REG_USBCMD, CMD_RESET, 0, 250_000) {
        return Err("reset timeout");
    }

    // Re-apply host mode after controller reset (Linux `tdi_reset`).
    mode = r32(io, REG_USBMODE);
    mode = (mode & !0x3) | USBMODE_CM_HC;
    w32(io, REG_USBMODE, mode);

    let hcc: u32 = io.read_once(REG_HCCPARAMS).unwrap();
    log::info!(
        "[k1-usb] ehci1 HCCPARAMS={:#010x} 64bit={} paddr={:#x} daddr={:#x}",
        hcc,
        hcc & 1,
        dma.paddr(),
        dma.daddr()
    );
    // Always clear segment; Chipidea still honors CTRLDSSEGMENT when 64-bit capable.
    w32(io, REG_CTRLDSSEGMENT, 0);

    // Periodic frame list (1024 × EHCI_LIST_END) on DMA page 0.
    for i in 0..1024 {
        let _ = dma.write_val(i * 4, &EHCI_LIST_END);
    }

    // Idle async QH (H=1, Halt, self-link) until the first transfer.
    let qh_dma = (dma.daddr() + PAGE_SIZE + OFF_CTRL_QH) as u32;
    init_idle_qh(dma, PAGE_SIZE + OFF_CTRL_QH, qh_dma);
    dma_sync_to_dev(dma);

    w32(io, REG_PERIODICLISTBASE, dma.daddr() as u32);
    w32(io, REG_ASYNCLISTADDR, qh_dma);
    w32(io, REG_USBINTR, 0);
    w32(io, REG_FRINDEX, 0);

    // Run + CF. Clear PARK.
    cmd = r32(io, REG_USBCMD);
    cmd &= !(CMD_RESET | (1 << 7) /* LRESET */ | CMD_ASE | CMD_PSE | CMD_PARK | (3 << 8));
    cmd |= CMD_RUN;
    w32(io, REG_USBCMD, cmd);
    w32(io, REG_CONFIGFLAG, FLAG_CF);
    delay_ms(5);

    if !handshake(io, REG_USBSTS, STS_HALT, 0, 100_000) {
        log::warn!(
            "[k1-usb] ehci1 run fail USBCMD={:#010x} USBSTS={:#010x}",
            r32(io, REG_USBCMD),
            r32(io, REG_USBSTS)
        );
        return Err("run timeout");
    }

    log::info!(
        "[k1-usb] ehci1 host running CF=1 USBSTS={:#010x}",
        r32(io, REG_USBSTS)
    );
    Ok(())
}

fn dma_sync_to_dev(dma: &DmaStream) {
    // Cache maintenance is a no-op on the required NC mapping; ordering is explicit.
    let _ = dma.sync_to_device(0..dma.size());
    dma_io_fence();
}

fn dma_sync_qtds(dma: &DmaStream, page1: usize) {
    // Invalidate so CPU observes HC writebacks to qTD/QH overlay.
    let _ = dma.sync_from_device(page1 + OFF_CTRL_QH..page1 + OFF_QTD2 + 0x40);
}

fn zero_words(dma: &DmaStream, off: usize, nwords: usize) {
    for i in 0..nwords {
        let _ = dma.write_val(off + i * 4, &0u32);
    }
}

fn init_idle_qh(dma: &DmaStream, off: usize, qh_dma: u32) {
    // Single idle QH: H=1, EPS=HS, Halt, self-link (U-Boot reclaim head).
    // 32 words covers Linux ehci_qh_hw including hw_buf_hi[5].
    zero_words(dma, off, 32);
    let next = (qh_dma & !0x1f) | Q_TYPE_QH;
    let _ = dma.write_val(off, &next);
    let info1 = QH_HEAD | QH_HIGH_SPEED;
    let _ = dma.write_val(off + 0x04, &info1);
    let _ = dma.write_val(off + 0x10, &EHCI_LIST_END);
    let _ = dma.write_val(off + 0x14, &EHCI_LIST_END);
    let _ = dma.write_val(off + 0x18, &QTD_STS_HALT);
}

fn port_reset_and_get_desc(
    io: &IoMem,
    dma: &DmaStream,
) -> Result<(DevEp, u16, u16, [u8; 18]), &'static str> {
    // Port reset (EHCI 2.3.9): assert ≥50ms, clear, wait for HC to drop RESET.
    let mut p = r32(io, REG_PORTSC0);
    p = (p & !PORT_RWC) | PORT_RESET;
    w32(io, REG_PORTSC0, p);
    delay_ms(50);
    p = r32(io, REG_PORTSC0);
    p &= !PORT_RESET;
    w32(io, REG_PORTSC0, p | PORT_RWC);
    if !handshake(io, REG_PORTSC0, PORT_RESET, 0, 2_000) {
        return Err("port reset stuck");
    }
    delay_ms(50);

    let portsc = r32(io, REG_PORTSC0);
    if portsc & PORT_PE == 0 {
        return Err("port not enabled after reset");
    }
    let hostpc = r32(io, REG_HOSTPC0);
    let pspd_port = (portsc & PORT_PSPD_MASK) >> 26;
    let pspd_hostpc = (hostpc & HOSTPC_PSPD_MASK) >> 25;
    let pspd = pspd_port;
    log::info!(
        "[k1-usb] ehci1 after reset PORTSC={:#010x} HOSTPC={:#010x}",
        portsc,
        hostpc
    );
    log::info!(
        "[k1-usb] ehci1 speed raw: PORTSC.PSPD={} HOSTPC.PSPD={} -> {}",
        pspd_port,
        pspd_hostpc,
        speed_label(pspd)
    );

    // Chipidea TDI: 0=FS, 1=LS, 2=HS
    let (eps, maxpkt, speed_name_s) = match pspd {
        2 => (QH_HIGH_SPEED, 64u32, "HS"),
        1 => (QH_LOW_SPEED, 8u32, "LS"),
        _ => (QH_FULL_SPEED, 8u32, "FS"),
    };
    let use_tt = eps != QH_HIGH_SPEED;
    log::info!(
        "[k1-usb] ehci1 decide speed={} maxpkt={} eps={:#x} root_tt={}",
        speed_name_s,
        maxpkt,
        eps,
        use_tt as u8
    );

    let page1 = PAGE_SIZE;
    log::info!(
        "[k1-usb] ehci1 DMA page1={:#x} ctrl_qh={:#x} (single-QH H=1)",
        dma.daddr() + page1,
        dma.daddr() + page1 + OFF_CTRL_QH
    );

    let tries: [DevEp; 3] = if use_tt {
        [
            DevEp {
                addr: 0,
                eps,
                maxpkt,
                tt_hub: 0,
                tt_port: 1,
            },
            DevEp {
                addr: 0,
                eps: QH_FULL_SPEED,
                maxpkt: 8,
                tt_hub: 0,
                tt_port: 1,
            },
            DevEp {
                addr: 0,
                eps: QH_LOW_SPEED,
                maxpkt: 8,
                tt_hub: 0,
                tt_port: 1,
            },
        ]
    } else {
        [
            DevEp {
                addr: 0,
                eps: QH_HIGH_SPEED,
                maxpkt: 64,
                tt_hub: 0,
                tt_port: 0,
            },
            DevEp {
                addr: 0,
                eps: QH_FULL_SPEED,
                maxpkt: 8,
                tt_hub: 0,
                tt_port: 1,
            },
            DevEp {
                addr: 0,
                eps: QH_LOW_SPEED,
                maxpkt: 8,
                tt_hub: 0,
                tt_port: 1,
            },
        ]
    };

    let mut ok = tries[0];
    let mut desc8_ok = false;
    for (i, try_dev) in tries.iter().enumerate() {
        let tag = if i == 0 { speed_name_s } else { "fallback" };
        log::debug!(
            "[k1-usb] ehci1 try GET_DESC8 {} hub={} port={} eps={:#x}",
            tag,
            try_dev.tt_hub,
            try_dev.tt_port,
            try_dev.eps
        );
        match ctrl_in(
            io,
            dma,
            try_dev,
            &[
                0x80,
                USB_REQ_GET_DESCRIPTOR,
                0x00,
                USB_DT_DEVICE,
                0x00,
                0x00,
                8,
                0x00,
            ],
            8,
            true,
        ) {
            Ok(()) => {
                ok = *try_dev;
                desc8_ok = true;
                break;
            }
            Err(e) => {
                log::debug!("[k1-usb] ehci1 try {} failed: {}", tag, e);
                delay_ms(10);
            }
        }
    }
    if !desc8_ok {
        return Err("GET_DESC8 failed all speed/TT configs");
    }

    let head = read_data(dma, 8);
    log::info!(
        "[k1-usb] ehci1 desc8: {:02x}{:02x}{:02x}{:02x} {:02x}{:02x}{:02x}{:02x} (hub={} port={} eps={:#x})",
        head[0],
        head[1],
        head[2],
        head[3],
        head[4],
        head[5],
        head[6],
        head[7],
        ok.tt_hub,
        ok.tt_port,
        ok.eps
    );
    if head[1] != USB_DT_DEVICE {
        return Err("not a device descriptor");
    }
    let maxpkt0 = head[7] as u32;
    if maxpkt0 == 8 || maxpkt0 == 16 || maxpkt0 == 32 || maxpkt0 == 64 {
        ok.maxpkt = maxpkt0;
    }

    ctrl_in(
        io,
        dma,
        &ok,
        &[
            0x80,
            USB_REQ_GET_DESCRIPTOR,
            0x00,
            USB_DT_DEVICE,
            0x00,
            0x00,
            18,
            0x00,
        ],
        18,
        false,
    )?;

    let full_bytes = read_data(dma, 18);
    let mut full = [0u8; 18];
    full.copy_from_slice(&full_bytes[..18]);
    if full[1] != USB_DT_DEVICE {
        return Err("not a device descriptor (18)");
    }
    let vid = u16::from_le_bytes([full[8], full[9]]);
    let pid = u16::from_le_bytes([full[10], full[11]]);
    Ok((ok, vid, pid, full))
}

fn read_data(dma: &DmaStream, len: usize) -> [u8; 256] {
    let page1 = PAGE_SIZE;
    let _ = dma.sync_from_device(page1 + OFF_DATA..page1 + OFF_DATA + len);
    let mut buf = [0u8; 256];
    let n = len.min(256);
    for (i, byte) in buf[..n].iter_mut().enumerate() {
        *byte = dma.read_val(page1 + OFF_DATA + i).unwrap_or(0);
    }
    buf
}

fn setup_packet(bm: u8, req: u8, wvalue: u16, windex: u16, wlen: u16) -> [u8; 8] {
    [
        bm,
        req,
        (wvalue & 0xff) as u8,
        (wvalue >> 8) as u8,
        (windex & 0xff) as u8,
        (windex >> 8) as u8,
        (wlen & 0xff) as u8,
        (wlen >> 8) as u8,
    ]
}

/// Control IN (SETUP + DATA IN + STATUS OUT), or zero-length data via `ctrl_nodata`.
fn ctrl_in(
    io: &IoMem,
    dma: &DmaStream,
    dev: &DevEp,
    setup: &[u8; 8],
    data_len: u16,
    quiet: bool,
) -> Result<(), &'static str> {
    ctrl_xfer(io, dma, dev, setup, Some(data_len), quiet)
}

fn ctrl_nodata(
    io: &IoMem,
    dma: &DmaStream,
    dev: &DevEp,
    setup: &[u8; 8],
    quiet: bool,
) -> Result<(), &'static str> {
    ctrl_xfer(io, dma, dev, setup, None, quiet)
}

fn ctrl_xfer(
    io: &IoMem,
    dma: &DmaStream,
    dev: &DevEp,
    setup: &[u8; 8],
    data_in: Option<u16>,
    quiet: bool,
) -> Result<(), &'static str> {
    if FAILED.load(core::sync::atomic::Ordering::Acquire) {
        return Err("EHCI rejected after unconfirmed schedule stop");
    }
    stop_schedule(io, CMD_ASE | CMD_PSE, STS_ASS | STS_PSS)?;
    let page1 = PAGE_SIZE;
    let ctrl_qh = (dma.daddr() + page1 + OFF_CTRL_QH) as u32;
    let qtd0 = (dma.daddr() + page1 + OFF_QTD0) as u32;
    let qtd1 = (dma.daddr() + page1 + OFF_QTD1) as u32;
    let qtd2 = (dma.daddr() + page1 + OFF_QTD2) as u32;
    let setup_dma = (dma.daddr() + page1 + OFF_SETUP) as u32;
    let data_dma = (dma.daddr() + page1 + OFF_DATA) as u32;

    let _ = dma.write_bytes(page1 + OFF_SETUP, setup);
    if let Some(n) = data_in {
        for i in 0..(n as usize) {
            let _ = dma.write_val(page1 + OFF_DATA + i, &0u8);
        }
    }

    fill_qtd(
        dma,
        page1 + OFF_QTD0,
        qtd1,
        setup_dma,
        8,
        QTD_PID_SETUP | QTD_CERR3 | QTD_STS_ACTIVE,
    );
    match data_in {
        Some(n) => {
            fill_qtd(
                dma,
                page1 + OFF_QTD1,
                qtd2,
                data_dma,
                n as u32,
                QTD_PID_IN | QTD_TOGGLE | QTD_CERR3 | QTD_STS_ACTIVE,
            );
            fill_qtd(
                dma,
                page1 + OFF_QTD2,
                EHCI_LIST_END,
                0,
                0,
                QTD_PID_OUT | QTD_TOGGLE | QTD_CERR3 | QTD_IOC | QTD_STS_ACTIVE,
            );
        }
        None => {
            // SETUP + STATUS IN (no data stage)
            fill_qtd(
                dma,
                page1 + OFF_QTD1,
                EHCI_LIST_END,
                0,
                0,
                QTD_PID_IN | QTD_TOGGLE | QTD_CERR3 | QTD_IOC | QTD_STS_ACTIVE,
            );
            // qTD2 unused (inactive)
            fill_qtd(dma, page1 + OFF_QTD2, EHCI_LIST_END, 0, 0, 0);
        }
    }

    zero_words(dma, page1 + OFF_CTRL_QH, 32);
    let (info1, info2) = qh_ctrl_words(dev);
    if !quiet {
        log::info!(
            "[k1-usb] ehci1 QH ctrl info1={:#010x} info2={:#010x} (addr={} tt={} hub={} port={} maxpkt={})",
            info1,
            info2,
            dev.addr,
            dev.use_tt() as u8,
            dev.tt_hub,
            dev.tt_port,
            dev.maxpkt
        );
    }
    let self_link = (ctrl_qh & !0x1f) | Q_TYPE_QH;
    let _ = dma.write_val(page1 + OFF_CTRL_QH, &self_link);
    let _ = dma.write_val(page1 + OFF_CTRL_QH + 0x04, &info1);
    let _ = dma.write_val(page1 + OFF_CTRL_QH + 0x08, &info2);
    let _ = dma.write_val(page1 + OFF_CTRL_QH + 0x0c, &0u32);
    let _ = dma.write_val(page1 + OFF_CTRL_QH + 0x10, &(qtd0 & !0x1f));
    let _ = dma.write_val(page1 + OFF_CTRL_QH + 0x14, &EHCI_LIST_END);
    let _ = dma.write_val(page1 + OFF_CTRL_QH + 0x18, &0u32);

    dma_sync_to_dev(dma);
    run_async_and_wait(io, dma, page1, ctrl_qh, quiet)
}

fn qh_ctrl_words(dev: &DevEp) -> (u32, u32) {
    let addr = (dev.addr as u32) & 0x7f;
    if dev.use_tt() {
        let i1 = QH_CONTROL_EP | QH_HEAD | (dev.maxpkt << 16) | QH_TOGGLE_CTL | dev.eps | addr;
        let i2 = (EHCI_TUNE_MULT_TT << 30)
            | ((dev.tt_hub as u32 & 0x7f) << 16)
            | ((dev.tt_port as u32 & 0x7f) << 23);
        (i1, i2)
    } else {
        let i1 =
            (EHCI_TUNE_RL_HS << 28) | QH_HEAD | (dev.maxpkt << 16) | QH_TOGGLE_CTL | dev.eps | addr;
        let i2 = EHCI_TUNE_MULT_HS << 30;
        (i1, i2)
    }
}

fn hub_bringup(io: &IoMem, dma: &DmaStream, root: &mut DevEp) -> Result<(), &'static str> {
    const HUB_ADDR: u8 = 1;
    log::info!("[k1-usb] ehci1 hub: SET_ADDRESS({})", HUB_ADDR);
    ctrl_nodata(
        io,
        dma,
        root,
        &setup_packet(0x00, USB_REQ_SET_ADDRESS, HUB_ADDR as u16, 0, 0),
        false,
    )?;
    delay_ms(2);
    root.addr = HUB_ADDR;

    // Config descriptor header → full
    ctrl_in(
        io,
        dma,
        root,
        &setup_packet(
            0x80,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_CONFIG as u16) << 8,
            0,
            9,
        ),
        9,
        true,
    )?;
    let cfg9 = read_data(dma, 9);
    if cfg9[1] != USB_DT_CONFIG {
        return Err("not a config descriptor");
    }
    let total = u16::from_le_bytes([cfg9[2], cfg9[3]]);
    let cfg_val = cfg9[5];
    let total_n = total.min(256);
    log::info!(
        "[k1-usb] ehci1 hub: config wTotalLength={} bConfigurationValue={}",
        total,
        cfg_val
    );
    ctrl_in(
        io,
        dma,
        root,
        &setup_packet(
            0x80,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_CONFIG as u16) << 8,
            0,
            total_n,
        ),
        total_n,
        true,
    )?;

    log::info!("[k1-usb] ehci1 hub: SET_CONFIGURATION({})", cfg_val);
    ctrl_nodata(
        io,
        dma,
        root,
        &setup_packet(0x00, USB_REQ_SET_CONFIGURATION, cfg_val as u16, 0, 0),
        false,
    )?;

    // Hub descriptor (class)
    ctrl_in(
        io,
        dma,
        root,
        &setup_packet(0xa0, USB_REQ_GET_DESCRIPTOR, USB_DT_HUB << 8, 0, 9),
        9,
        true,
    )?;
    let hubd = read_data(dma, 9);
    let n_ports = hubd[2];
    let pwr_good_ms = (hubd[5] as u64) * 2;
    log::info!(
        "[k1-usb] ehci1 hub: bNbrPorts={} bPwrOn2PwrGood={}ms",
        n_ports,
        pwr_good_ms
    );

    for port in 1..=n_ports {
        ctrl_nodata(
            io,
            dma,
            root,
            &setup_packet(0x23, USB_REQ_SET_FEATURE, FEAT_PORT_POWER, port as u16, 0),
            true,
        )?;
    }
    delay_ms(pwr_good_ms.max(100));

    let mut next_addr: u8 = 2;
    for port in 1..=n_ports {
        let (status, change) = hub_port_status(io, dma, root, port)?;
        log::warn!(
            "[k1-usb] ehci1 hub port{}: status={:#06x} change={:#06x}",
            port,
            status,
            change
        );
        if status & HUB_PORT_CONNECTION == 0 {
            continue;
        }
        if change & HUB_C_CONNECTION != 0 {
            let _ = ctrl_nodata(
                io,
                dma,
                root,
                &setup_packet(
                    0x23,
                    USB_REQ_CLEAR_FEATURE,
                    FEAT_C_CONNECTION,
                    port as u16,
                    0,
                ),
                true,
            );
        }

        log::info!("[k1-usb] ehci1 hub port{}: RESET", port);
        ctrl_nodata(
            io,
            dma,
            root,
            &setup_packet(0x23, USB_REQ_SET_FEATURE, FEAT_PORT_RESET, port as u16, 0),
            true,
        )?;
        // Wait for C_RESET
        let mut ok_reset = false;
        for _ in 0..50 {
            delay_ms(10);
            let (st, ch) = hub_port_status(io, dma, root, port)?;
            if ch & HUB_C_RESET != 0 || (st & HUB_PORT_ENABLE != 0 && st & HUB_PORT_RESET == 0) {
                let _ = ctrl_nodata(
                    io,
                    dma,
                    root,
                    &setup_packet(0x23, USB_REQ_CLEAR_FEATURE, FEAT_C_RESET, port as u16, 0),
                    true,
                );
                ok_reset = st & HUB_PORT_ENABLE != 0;
                log::info!(
                    "[k1-usb] ehci1 hub port{}: after reset status={:#06x} enabled={}",
                    port,
                    st,
                    ok_reset as u8
                );
                break;
            }
        }
        if !ok_reset {
            log::warn!("[k1-usb] ehci1 hub port{}: reset failed", port);
            continue;
        }

        let (st, _) = hub_port_status(io, dma, root, port)?;
        let (eps, maxpkt) = if st & HUB_PORT_HIGH_SPEED != 0 {
            (QH_HIGH_SPEED, 64u32)
        } else if st & HUB_PORT_LOW_SPEED != 0 {
            (QH_LOW_SPEED, 8u32)
        } else {
            (QH_FULL_SPEED, 8u32)
        };
        // Downstream FS/LS goes through this hub's TT; HS is direct.
        let mut child = DevEp {
            addr: 0,
            eps,
            maxpkt,
            tt_hub: if eps == QH_HIGH_SPEED { 0 } else { root.addr },
            tt_port: if eps == QH_HIGH_SPEED { 0 } else { port },
        };
        log::info!(
            "[k1-usb] ehci1 hub port{}: child speed eps={:#x} tt_hub={} tt_port={}",
            port,
            child.eps,
            child.tt_hub,
            child.tt_port
        );

        delay_ms(10);
        if let Err(e) = ctrl_in(
            io,
            dma,
            &child,
            &setup_packet(
                0x80,
                USB_REQ_GET_DESCRIPTOR,
                (USB_DT_DEVICE as u16) << 8,
                0,
                8,
            ),
            8,
            true,
        ) {
            log::warn!("[k1-usb] ehci1 hub port{}: GET_DESC8 failed: {}", port, e);
            continue;
        }
        let d8 = read_data(dma, 8);
        if d8[1] != USB_DT_DEVICE {
            log::warn!(
                "[k1-usb] ehci1 hub port{}: bad desc type {:#x}",
                port,
                d8[1]
            );
            continue;
        }
        let mp0 = d8[7] as u32;
        if mp0 == 8 || mp0 == 16 || mp0 == 32 || mp0 == 64 {
            child.maxpkt = mp0;
        }
        if let Err(e) = ctrl_in(
            io,
            dma,
            &child,
            &setup_packet(
                0x80,
                USB_REQ_GET_DESCRIPTOR,
                (USB_DT_DEVICE as u16) << 8,
                0,
                18,
            ),
            18,
            true,
        ) {
            log::warn!("[k1-usb] ehci1 hub port{}: GET_DESC18 failed: {}", port, e);
            continue;
        }
        let d18 = read_data(dma, 18);
        let vid = u16::from_le_bytes([d18[8], d18[9]]);
        let pid = u16::from_le_bytes([d18[10], d18[11]]);
        log::info!(
            "[k1-usb] ehci1 hub port{}: device {:04x}:{:04x} class={:02x} subclass={:02x} proto={:02x}",
            port,
            vid,
            pid,
            d18[4],
            d18[5],
            d18[6]
        );

        let addr = next_addr;
        next_addr = next_addr.saturating_add(1);
        if let Err(e) = ctrl_nodata(
            io,
            dma,
            &child,
            &setup_packet(0x00, USB_REQ_SET_ADDRESS, addr as u16, 0, 0),
            true,
        ) {
            log::warn!("[k1-usb] ehci1 hub port{}: SET_ADDRESS failed: {}", port, e);
            continue;
        }
        delay_ms(2);
        child.addr = addr;

        match try_hid_boot_keyboard(io, dma, &child, vid, pid) {
            Ok(()) => log::info!("[k1-usb] ehci1 hub port{}: HID boot keyboard OK", port),
            Err("no HID boot keyboard interface") => {}
            Err(e) => log::info!("[k1-usb] ehci1 hub port{}: HID keyboard probe: {}", port, e),
        }
    }
    Ok(())
}

fn hub_port_status(
    io: &IoMem,
    dma: &DmaStream,
    hub: &DevEp,
    port: u8,
) -> Result<(u16, u16), &'static str> {
    ctrl_in(
        io,
        dma,
        hub,
        &setup_packet(0xa3, 0x00, 0, port as u16, 4),
        4,
        true,
    )?;
    let b = read_data(dma, 4);
    let status = u16::from_le_bytes([b[0], b[1]]);
    let change = u16::from_le_bytes([b[2], b[3]]);
    Ok((status, change))
}

/// Parse config for HID boot keyboard; SET_CONFIG + boot protocol + interrupt poll.
fn try_hid_boot_keyboard(
    io: &IoMem,
    dma: &DmaStream,
    dev: &DevEp,
    vid: u16,
    pid: u16,
) -> Result<(), &'static str> {
    ctrl_in(
        io,
        dma,
        dev,
        &setup_packet(
            0x80,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_CONFIG as u16) << 8,
            0,
            9,
        ),
        9,
        true,
    )?;
    let cfg9 = read_data(dma, 9);
    if cfg9[1] != USB_DT_CONFIG {
        return Err("no config");
    }
    let total = u16::from_le_bytes([cfg9[2], cfg9[3]]).min(256);
    let cfg_val = cfg9[5];
    ctrl_in(
        io,
        dma,
        dev,
        &setup_packet(
            0x80,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_CONFIG as u16) << 8,
            0,
            total,
        ),
        total,
        true,
    )?;
    let cfg = read_data(dma, total as usize);

    let mut iface: Option<(u8, u8, u8, u16)> = None; // iface, ep_addr, ep_maxpkt, interval
    let mut i = 0usize;
    let mut cur_iface = 0u8;
    let mut cur_is_kbd = false;
    while i + 1 < total as usize {
        let len = cfg[i] as usize;
        if len < 2 || i + len > total as usize {
            break;
        }
        let dtype = cfg[i + 1];
        if dtype == USB_DT_INTERFACE && len >= 9 {
            cur_iface = cfg[i + 2];
            let class = cfg[i + 5];
            let sub = cfg[i + 6];
            let proto = cfg[i + 7];
            cur_is_kbd =
                class == USB_CLASS_HID && sub == USB_HID_BOOT && proto == USB_HID_PROTO_KEYBOARD;
        } else if dtype == USB_DT_ENDPOINT && len >= 7 && cur_is_kbd {
            let ep = cfg[i + 2];
            let attr = cfg[i + 3];
            let mps = u16::from_le_bytes([cfg[i + 4], cfg[i + 5]]);
            let interval = cfg[i + 6];
            // Interrupt IN
            if ep & 0x80 != 0 && (attr & 0x03) == 0x03 {
                iface = Some((cur_iface, ep, mps.min(64) as u8, interval as u16));
                break;
            }
        }
        i += len;
    }

    let Some((ifnum, ep_addr, ep_mps, interval)) = iface else {
        return Err("no HID boot keyboard interface");
    };
    log::info!(
        "[k1-usb] ehci1 HID kbd: iface={} ep={:#04x} mps={} interval={}",
        ifnum,
        ep_addr,
        ep_mps,
        interval
    );

    ctrl_nodata(
        io,
        dma,
        dev,
        &setup_packet(0x00, USB_REQ_SET_CONFIGURATION, cfg_val as u16, 0, 0),
        false,
    )?;
    // Boot protocol (0 = boot, 1 = report).
    ctrl_nodata(
        io,
        dma,
        dev,
        &setup_packet(0x21, USB_HID_SET_PROTOCOL, 0, ifnum as u16, 0),
        true,
    )?;
    // SET_IDLE duration=0 means "infinite" → device NAKs until key change.
    // Use 4ms (duration=1) so idle reports complete during initialization.
    ctrl_nodata(
        io,
        dma,
        dev,
        &setup_packet(0x21, USB_HID_SET_IDLE, 0x0100, ifnum as u16, 0),
        true,
    )?;

    // One smoke-test interrupt transfer, then hand off to timer poll.
    match interrupt_in_once(io, dma, dev, ep_addr, ep_mps as u32, interval) {
        Ok(report) => {
            log::info!(
                "[k1-usb] ehci1 HID report[0]: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
                report[0],
                report[1],
                report[2],
                report[3],
                report[4],
                report[5],
                report[6],
                report[7]
            );
            // interrupt_in_once has confirmed PSS=0 and advanced this endpoint's
            // toggle. Subsequent hub control transfers do not reset this endpoint.
            let overlay: u32 = dma.read_val(PAGE_SIZE + OFF_INT_QH + 0x18).unwrap();
            HID_POLL.lock().initial_toggle = overlay & QTD_TOGGLE;
            let registered = super::k1_hid_kbd::register_keyboard(vid, pid);
            *super::k1_hid_kbd::HID_KBD.lock() = Some(super::k1_hid_kbd::HidKbdState {
                registered,
                last_report: report,
                ep_addr,
                maxpkt: ep_mps as u32,
                addr: dev.addr,
                eps: dev.eps,
                tt_hub: dev.tt_hub,
                tt_port: dev.tt_port,
            });
        }
        Err(e) => {
            log::warn!("[k1-usb] ehci1 HID interrupt failed: {}", e);
            return Err(e);
        }
    }
    Ok(())
}

/// Arm periodic interrupt IN and leave PSE on for background polling.
fn hid_arm_periodic(io: &IoMem, dma: &DmaStream) -> Result<(), &'static str> {
    let guard = super::k1_hid_kbd::HID_KBD.lock();
    let Some(kbd) = guard.as_ref() else {
        return Err("no HID keyboard");
    };
    let dev = DevEp {
        addr: kbd.addr,
        eps: kbd.eps,
        maxpkt: 8,
        tt_hub: kbd.tt_hub,
        tt_port: kbd.tt_port,
    };
    let ep_addr = kbd.ep_addr;
    let maxpkt = kbd.maxpkt;
    drop(guard);

    let initial_toggle = HID_POLL.lock().initial_toggle;
    interrupt_prepare(io, dma, &dev, ep_addr, maxpkt, initial_toggle)?;
    // Enable PSE and leave it running.
    let mut cmd = r32(io, REG_USBCMD) & !(CMD_ASE | CMD_IAAD | CMD_PARK | (3 << 8));
    w32(io, REG_USBCMD, cmd);
    stop_schedule(io, CMD_ASE, STS_ASS)?;
    w32(io, REG_PERIODICLISTBASE, dma.daddr() as u32);
    cmd = r32(io, REG_USBCMD) | CMD_PSE;
    w32(io, REG_USBCMD, cmd);
    if !handshake(io, REG_USBSTS, STS_PSS, STS_PSS, 20_000) {
        return Err("PSE enable timeout");
    }
    Ok(())
}

fn hid_timer_poll() {
    use core::sync::atomic::Ordering;
    if FAILED.load(Ordering::Acquire) {
        return;
    }
    let n = HID_POLL_DIV.fetch_add(1, Ordering::Relaxed);
    if !HID_TRANSITION.load(Ordering::Relaxed) && !n.is_multiple_of(10) {
        return;
    }
    if let Some(bh) = HID_BH.get() {
        bh.schedule();
    }
}

fn hid_bottom_half() {
    use core::sync::atomic::Ordering;
    let Some(host) = EHCI1.get() else {
        return;
    };
    if FAILED.load(Ordering::Acquire) {
        return;
    }
    if let Err(error) = hid_poll_once(host) {
        // Do not modify descriptors or reverse an unacknowledged PSE change.
        // EHCI1 retains the DMA allocation even if the controller still owns it.
        FAILED.store(true, Ordering::Release);
        HID_TRANSITION.store(false, Ordering::Relaxed);
        log::error!("[k1-usb] ehci1 keyboard polling stopped: {}", error);
    }
}

fn hid_poll_once(host: &Ehci1) -> Result<(), &'static str> {
    use core::sync::atomic::Ordering;
    // Only this serial taskless callback uses both locks. Initialization ends
    // before the timer callback is registered; neither acquisition can wait.
    let Some(mut guard) = super::k1_hid_kbd::HID_KBD.try_lock() else {
        return Ok(());
    };
    let Some(kbd) = guard.as_mut() else {
        return Ok(());
    };
    let Some(mut poll) = HID_POLL.try_lock() else {
        return Ok(());
    };

    let status = r32(&host.io, REG_USBSTS);
    let token: u32 = host.dma.read_val(PAGE_SIZE + OFF_INT_QTD + 0x08).unwrap();
    let timeout_ticks = (tsc_freq() / 50).max(1); // 20 ms, independent of timer delivery.
    match poll
        .phase
        .next_action(status, token, read_tsc(), timeout_ticks)?
    {
        HidPollAction::Wait => {}
        HidPollAction::Stop => {
            w32(&host.io, REG_USBCMD, r32(&host.io, REG_USBCMD) & !CMD_PSE);
            poll.phase = HidPollPhase::Stopping {
                started_at: read_tsc(),
            };
            HID_TRANSITION.store(true, Ordering::Relaxed);
        }
        HidPollAction::Rearm => {
            // PSS=0 establishes CPU ownership of both the qTD and QH overlay.
            // Read the QH's toggle after stopping: DTC=0 makes it authoritative.
            let overlay: u32 = host.dma.read_val(PAGE_SIZE + OFF_INT_QH + 0x18).unwrap();
            if token & QTD_STS_HALT != 0 {
                return Err("keyboard endpoint halted; polling stopped");
            }
            let report = if complete_hid_report(token) {
                let mut report = [0u8; 8];
                host.dma
                    .read_bytes(PAGE_SIZE + OFF_INT_BUF, &mut report)
                    .unwrap();
                Some(report)
            } else {
                // A short boot report must not turn the zero-filled tail into releases.
                None
            };
            interrupt_rearm_qtd(&host.dma, overlay);
            w32(&host.io, REG_USBCMD, r32(&host.io, REG_USBCMD) | CMD_PSE);
            poll.phase = HidPollPhase::Starting {
                started_at: read_tsc(),
            };
            if let Some(report) = report {
                super::k1_hid_kbd::dispatch_report_diff(
                    &kbd.registered,
                    &mut kbd.last_report,
                    &report,
                );
            }
        }
        HidPollAction::Started => {
            poll.phase = HidPollPhase::Running;
            HID_TRANSITION.store(false, Ordering::Relaxed);
        }
    }
    Ok(())
}

// Each submitted boot-keyboard qTD requests exactly eight report bytes.
fn complete_hid_report(token: u32) -> bool {
    const REMAINING_BYTES: u32 = 0x7fff << 16;
    token & (QTD_STS_ACTIVE | QTD_STS_HALT | REMAINING_BYTES) == 0
}

fn interrupt_prepare(
    io: &IoMem,
    dma: &DmaStream,
    dev: &DevEp,
    ep_addr: u8,
    maxpkt: u32,
    initial_toggle: u32,
) -> Result<(), &'static str> {
    if FAILED.load(core::sync::atomic::Ordering::Acquire) {
        return Err("EHCI rejected after unconfirmed schedule stop");
    }
    stop_schedule(io, CMD_ASE | CMD_PSE, STS_ASS | STS_PSS)?;
    let page1 = PAGE_SIZE;
    let int_qh = (dma.daddr() + page1 + OFF_INT_QH) as u32;
    let int_qtd = (dma.daddr() + page1 + OFF_INT_QTD) as u32;
    let buf_dma = (dma.daddr() + page1 + OFF_INT_BUF) as u32;
    let ep = (ep_addr & 0x0f) as u32;

    for i in 0..8 {
        let _ = dma.write_val(page1 + OFF_INT_BUF + i, &0u8);
    }

    fill_qtd(
        dma,
        page1 + OFF_INT_QTD,
        EHCI_LIST_END,
        buf_dma,
        8,
        QTD_PID_IN | QTD_CERR3 | QTD_IOC | QTD_STS_ACTIVE,
    );

    zero_words(dma, page1 + OFF_INT_QH, 32);
    let addr = (dev.addr as u32) & 0x7f;
    let info1 = (maxpkt.min(8) << 16) | dev.eps | (ep << 8) | addr;
    let mut info2 = (1u32 << 30) | 0x01;
    if dev.use_tt() {
        info2 |= ((dev.tt_hub as u32 & 0x7f) << 16) | ((dev.tt_port as u32 & 0x7f) << 23);
        info2 |= 0x1c << 8;
    }
    let qh_link = (int_qh & !0x1f) | Q_TYPE_QH;
    let _ = dma.write_val(page1 + OFF_INT_QH, &EHCI_LIST_END);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x04, &info1);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x08, &info2);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x0c, &0u32);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x10, &(int_qtd & !0x1f));
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x14, &EHCI_LIST_END);
    dma_io_fence();
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x18, &(initial_toggle & QTD_TOGGLE));

    for i in 0..1024 {
        let _ = dma.write_val(i * 4, &qh_link);
    }
    init_idle_qh(
        dma,
        page1 + OFF_CTRL_QH,
        (dma.daddr() + page1 + OFF_CTRL_QH) as u32,
    );
    dma_sync_to_dev(dma);
    let _ = io; // silence; PERIODICLISTBASE set by caller
    Ok(())
}

/// Rearms a quiesced periodic QH after its PSS=0 acknowledgement.
fn interrupt_rearm_qtd(dma: &DmaStream, overlay_token: u32) {
    let page1 = PAGE_SIZE;
    let int_qtd = (dma.daddr() + page1 + OFF_INT_QTD) as u32;
    let buf_dma = (dma.daddr() + page1 + OFF_INT_BUF) as u32;
    for i in 0..8 {
        let _ = dma.write_val(page1 + OFF_INT_BUF + i, &0u8);
    }
    fill_qtd(
        dma,
        page1 + OFF_INT_QTD,
        EHCI_LIST_END,
        buf_dma,
        8,
        QTD_PID_IN | QTD_CERR3 | QTD_IOC | QTD_STS_ACTIVE,
    );
    // DTC=0: retain the endpoint toggle across successive reports.
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x10, &(int_qtd & !0x1f));
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x14, &EHCI_LIST_END);
    dma_io_fence();
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x18, &(overlay_token & QTD_TOGGLE));
    dma_sync_to_dev(dma);
}

/// One interrupt-IN via periodic schedule (PSE), then PSE off.
fn interrupt_in_once(
    io: &IoMem,
    dma: &DmaStream,
    dev: &DevEp,
    ep_addr: u8,
    maxpkt: u32,
    _interval: u16,
) -> Result<[u8; 8], &'static str> {
    if FAILED.load(core::sync::atomic::Ordering::Acquire) {
        return Err("EHCI rejected after unconfirmed schedule stop");
    }
    stop_schedule(io, CMD_ASE | CMD_PSE, STS_ASS | STS_PSS)?;
    let page1 = PAGE_SIZE;
    let int_qh = (dma.daddr() + page1 + OFF_INT_QH) as u32;
    let int_qtd = (dma.daddr() + page1 + OFF_INT_QTD) as u32;
    let buf_dma = (dma.daddr() + page1 + OFF_INT_BUF) as u32;
    let ep = (ep_addr & 0x0f) as u32;

    for i in 0..8 {
        let _ = dma.write_val(page1 + OFF_INT_BUF + i, &0u8);
    }

    fill_qtd(
        dma,
        page1 + OFF_INT_QTD,
        EHCI_LIST_END,
        buf_dma,
        8,
        QTD_PID_IN | QTD_CERR3 | QTD_IOC | QTD_STS_ACTIVE,
    );

    zero_words(dma, page1 + OFF_INT_QH, 32);
    let addr = (dev.addr as u32) & 0x7f;
    // Periodic QH: no H-bit; DT from QH overlay (first DATA0).
    let info1 = (maxpkt.min(8) << 16) | dev.eps | (ep << 8) | addr;
    // S-mask bit0 = microframe 0; Mult=1. FS/LS TT fields when needed.
    let mut info2 = (1u32 << 30) | 0x01; // MULT=1, smask=0x01
    if dev.use_tt() {
        info2 |= ((dev.tt_hub as u32 & 0x7f) << 16) | ((dev.tt_port as u32 & 0x7f) << 23);
        // Complete-split mask: uframes 2-4 typical for interrupt
        info2 |= 0x1c << 8;
    }
    let qh_link = (int_qh & !0x1f) | Q_TYPE_QH;
    let _ = dma.write_val(page1 + OFF_INT_QH, &EHCI_LIST_END); // horizontal terminate
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x04, &info1);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x08, &info2);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x0c, &0u32);
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x10, &(int_qtd & !0x1f));
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x14, &EHCI_LIST_END);
    dma_io_fence();
    let _ = dma.write_val(page1 + OFF_INT_QH + 0x18, &0u32);

    // Point every frame at this QH (initialization: poll every frame).
    for i in 0..1024 {
        let _ = dma.write_val(i * 4, &qh_link);
    }
    // Keep async idle QH valid
    init_idle_qh(
        dma,
        page1 + OFF_CTRL_QH,
        (dma.daddr() + page1 + OFF_CTRL_QH) as u32,
    );
    dma_sync_to_dev(dma);

    // ASE off, PSE on
    let mut cmd = r32(io, REG_USBCMD) & !(CMD_ASE | CMD_IAAD | CMD_PARK | (3 << 8));
    w32(io, REG_USBCMD, cmd);
    stop_schedule(io, CMD_ASE, STS_ASS)?;
    w32(io, REG_PERIODICLISTBASE, dma.daddr() as u32);
    w32(io, REG_USBSTS, STS_INT | STS_ERR | STS_IAA | (1 << 2));
    cmd = r32(io, REG_USBCMD) | CMD_PSE;
    w32(io, REG_USBCMD, cmd);
    if !handshake(io, REG_USBSTS, STS_PSS, STS_PSS, 20_000) {
        log::warn!(
            "[k1-usb] ehci1 PSE enable fail STS={:#010x} CMD={:#010x}",
            r32(io, REG_USBSTS),
            r32(io, REG_USBCMD)
        );
        cmd = r32(io, REG_USBCMD) & !CMD_PSE;
        w32(io, REG_USBCMD, cmd);
        return Err("PSE enable timeout");
    }

    let mut waited = 0u64;
    let result = loop {
        let _ = dma.sync_from_device(page1 + OFF_INT_QH..page1 + OFF_INT_BUF + 8);
        let tok: u32 = dma
            .read_val(page1 + OFF_INT_QTD + 0x08)
            .unwrap_or(QTD_STS_ACTIVE);
        let ov: u32 = dma.read_val(page1 + OFF_INT_QH + 0x18).unwrap_or(0);
        if tok & QTD_STS_HALT != 0 {
            log::debug!(
                "[k1-usb] ehci1 INT HALT tok={:#010x} ov={:#010x} STS={:#010x}",
                tok,
                ov,
                r32(io, REG_USBSTS)
            );
            decode_qtd("int-qTD", tok);
            cmd = r32(io, REG_USBCMD) & !CMD_PSE;
            w32(io, REG_USBCMD, cmd);
            break Err("interrupt qTD halted");
        }
        if tok & QTD_STS_ACTIVE == 0 {
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            let result = if complete_hid_report(tok) {
                let mut report = [0u8; 8];
                dma.read_bytes(page1 + OFF_INT_BUF, &mut report).unwrap();
                Ok(report)
            } else {
                Err("short interrupt report")
            };
            cmd = r32(io, REG_USBCMD) & !CMD_PSE;
            w32(io, REG_USBCMD, cmd);
            break result;
        }
        if waited == 50_000 || waited == 200_000 {
            log::debug!(
                "[k1-usb] ehci1 INT wait {}us tok={:#010x} ov={:#010x} STS={:#010x} PSS={} FRINDEX={:#x}",
                waited,
                tok,
                ov,
                r32(io, REG_USBSTS),
                (r32(io, REG_USBSTS) & STS_PSS != 0) as u8,
                r32(io, REG_FRINDEX)
            );
        }
        if waited > 500_000 {
            log::debug!(
                "[k1-usb] ehci1 INT TIMEOUT tok={:#010x} ov={:#010x} STS={:#010x} CMD={:#010x}",
                tok,
                ov,
                r32(io, REG_USBSTS),
                r32(io, REG_USBCMD)
            );
            decode_qtd("int-qTD", tok);
            cmd = r32(io, REG_USBCMD) & !CMD_PSE;
            w32(io, REG_USBCMD, cmd);
            break Err("interrupt timeout");
        }
        delay_us(100);
        waited += 100;
    };

    stop_schedule(io, CMD_PSE, STS_PSS)?;
    // Restore the frame list only after the controller stopped scanning it.
    for i in 0..1024 {
        let _ = dma.write_val(i * 4, &EHCI_LIST_END);
    }
    dma_sync_to_dev(dma);
    result
}

fn speed_label(pspd: u32) -> &'static str {
    match pspd {
        0 => "FS",
        1 => "LS",
        2 => "HS",
        _ => "?",
    }
}

fn pid_label(tok: u32) -> &'static str {
    match (tok >> 8) & 3 {
        0 => "OUT",
        1 => "IN",
        2 => "SETUP",
        _ => "?",
    }
}

fn decode_qtd(tag: &str, tok: u32) {
    let bytes = (tok >> 16) & 0x7fff;
    let active = (tok & QTD_STS_ACTIVE) != 0;
    let halt = (tok & QTD_STS_HALT) != 0;
    let xerr = (tok & (1 << 3)) != 0;
    let babble = (tok & (1 << 4)) != 0;
    let dbe = (tok & (1 << 5)) != 0;
    let missed = (tok & (1 << 2)) != 0;
    let splitx = (tok & (1 << 1)) != 0;
    let cerr = (tok >> 10) & 3;
    log::debug!(
        "[k1-usb] ehci1 {} tok={:#010x} pid={} bytes={} act={} halt={} cerr={} xerr={} babble={} dbe={} miss={} splitx={}",
        tag,
        tok,
        pid_label(tok),
        bytes,
        active as u8,
        halt as u8,
        cerr,
        xerr as u8,
        babble as u8,
        dbe as u8,
        missed as u8,
        splitx as u8
    );
}

fn dump_schedule(io: &IoMem, dma: &DmaStream, page1: usize, tag: &str) {
    dma_sync_qtds(dma, page1);
    let ctrl_link: u32 = dma.read_val(page1 + OFF_CTRL_QH).unwrap_or(0);
    let ctrl_info1: u32 = dma.read_val(page1 + OFF_CTRL_QH + 0x04).unwrap_or(0);
    let ov_next: u32 = dma.read_val(page1 + OFF_CTRL_QH + 0x10).unwrap_or(0);
    let ov_tok: u32 = dma.read_val(page1 + OFF_CTRL_QH + 0x18).unwrap_or(0);
    log::debug!(
        "[k1-usb] ehci1 sched[{}] ASYNCLISTADDR={:#010x} STS={:#010x} CMD={:#010x}",
        tag,
        r32(io, REG_ASYNCLISTADDR),
        r32(io, REG_USBSTS),
        r32(io, REG_USBCMD)
    );
    log::debug!(
        "[k1-usb] ehci1 sched[{}] ctrl.hw_next={:#010x} info1={:#010x}",
        tag,
        ctrl_link,
        ctrl_info1
    );
    log::debug!(
        "[k1-usb] ehci1 sched[{}] overlay next={:#010x} token={:#010x}",
        tag,
        ov_next,
        ov_tok
    );
    let t0: u32 = dma.read_val(page1 + OFF_QTD0 + 0x08).unwrap_or(0);
    let t1: u32 = dma.read_val(page1 + OFF_QTD1 + 0x08).unwrap_or(0);
    let t2: u32 = dma.read_val(page1 + OFF_QTD2 + 0x08).unwrap_or(0);
    decode_qtd("qTD0", t0);
    decode_qtd("qTD1", t1);
    decode_qtd("qTD2", t2);
}

fn run_async_and_wait(
    io: &IoMem,
    dma: &DmaStream,
    page1: usize,
    ctrl_qh: u32,
    quiet: bool,
) -> Result<(), &'static str> {
    dma_sync_to_dev(dma);
    let t0_i: u32 = dma.read_val(page1 + OFF_QTD0 + 0x08).unwrap_or(0);
    let t1_i: u32 = dma.read_val(page1 + OFF_QTD1 + 0x08).unwrap_or(0);
    let t2_i: u32 = dma.read_val(page1 + OFF_QTD2 + 0x08).unwrap_or(0);
    let track0 = t0_i & QTD_STS_ACTIVE != 0;
    let track1 = t1_i & QTD_STS_ACTIVE != 0;
    let track2 = t2_i & QTD_STS_ACTIVE != 0;
    if !quiet {
        log::info!(
            "[k1-usb] ehci1 xfer initial t0={:#010x} t1={:#010x} t2={:#010x}",
            t0_i,
            t1_i,
            t2_i
        );
    }

    // ASE must be off before touching ASYNCLISTADDR (EHCI 2.3.7).
    let mut cmd = r32(io, REG_USBCMD) & !(CMD_ASE | CMD_IAAD | CMD_PARK | (3 << 8));
    w32(io, REG_USBCMD, cmd);
    stop_schedule(io, CMD_ASE, STS_ASS)?;

    w32(io, REG_ASYNCLISTADDR, ctrl_qh);
    w32(
        io,
        REG_USBSTS,
        STS_INT | STS_ERR | STS_IAA | (1 << 2), /* PCD */
    );
    if !quiet {
        log::info!(
            "[k1-usb] ehci1 arm ASYNCLISTADDR={:#010x} (readback {:#010x}) TTCTRL={:#010x}",
            ctrl_qh,
            r32(io, REG_ASYNCLISTADDR),
            r32(io, REG_TTCTRL)
        );
    }

    cmd = r32(io, REG_USBCMD) | CMD_ASE;
    w32(io, REG_USBCMD, cmd);
    if !handshake(io, REG_USBSTS, STS_ASS, STS_ASS, 20_000) {
        log::warn!(
            "[k1-usb] ehci1 ASE enable fail STS={:#010x} CMD={:#010x}",
            r32(io, REG_USBSTS),
            r32(io, REG_USBCMD)
        );
        return Err("ASE enable timeout");
    }
    if !quiet {
        log::info!(
            "[k1-usb] ehci1 ASE on STS={:#010x} FRINDEX={:#010x} PORTSC={:#010x}",
            r32(io, REG_USBSTS),
            r32(io, REG_FRINDEX),
            r32(io, REG_PORTSC0)
        );
    }

    let deadline = 1_000_000u64;
    let mut waited = 0u64;
    loop {
        dma_sync_qtds(dma, page1);
        let t0: u32 = dma
            .read_val(page1 + OFF_QTD0 + 0x08)
            .unwrap_or(QTD_STS_ACTIVE);
        let t1: u32 = dma
            .read_val(page1 + OFF_QTD1 + 0x08)
            .unwrap_or(QTD_STS_ACTIVE);
        let t2: u32 = dma
            .read_val(page1 + OFF_QTD2 + 0x08)
            .unwrap_or(QTD_STS_ACTIVE);

        let halt = (track0 && t0 & QTD_STS_HALT != 0)
            || (track1 && t1 & QTD_STS_HALT != 0)
            || (track2 && t2 & QTD_STS_HALT != 0);
        if halt {
            log::debug!(
                "[k1-usb] ehci1 qTD HALTED @{}us STS={:#010x}",
                waited,
                r32(io, REG_USBSTS)
            );
            decode_qtd("qTD0", t0);
            decode_qtd("qTD1", t1);
            decode_qtd("qTD2", t2);
            dump_schedule(io, dma, page1, "halt");
            cmd = r32(io, REG_USBCMD) & !CMD_ASE;
            w32(io, REG_USBCMD, cmd);
            return Err("qTD halted");
        }
        let done = (!track0 || t0 & QTD_STS_ACTIVE == 0)
            && (!track1 || t1 & QTD_STS_ACTIVE == 0)
            && (!track2 || t2 & QTD_STS_ACTIVE == 0);
        if done {
            if !quiet {
                log::info!(
                    "[k1-usb] ehci1 xfer DONE @{}us STS={:#010x}",
                    waited,
                    r32(io, REG_USBSTS)
                );
            }
            break;
        }
        if waited >= deadline {
            log::warn!(
                "[k1-usb] ehci1 xfer TIMEOUT @{}us STS={:#010x}",
                waited,
                r32(io, REG_USBSTS)
            );
            decode_qtd("qTD0", t0);
            decode_qtd("qTD1", t1);
            decode_qtd("qTD2", t2);
            dump_schedule(io, dma, page1, "timeout");
            cmd = r32(io, REG_USBCMD) & !CMD_ASE;
            w32(io, REG_USBCMD, cmd);
            return Err("control transfer timeout");
        }
        delay_us(100);
        waited += 100;
    }

    cmd = r32(io, REG_USBCMD) & !CMD_ASE;
    w32(io, REG_USBCMD, cmd);
    stop_schedule(io, CMD_ASE, STS_ASS)?;
    init_idle_qh(dma, page1 + OFF_CTRL_QH, ctrl_qh);
    dma_sync_to_dev(dma);
    Ok(())
}

fn fill_qtd(dma: &DmaStream, off: usize, next_dma: u32, buf_dma: u32, len: u32, token_flags: u32) {
    // 16 words: covers hw_buf_hi[5] when HCC 64-bit addressing is set.
    zero_words(dma, off, 16);
    let next = if next_dma == EHCI_LIST_END {
        EHCI_LIST_END
    } else {
        next_dma & !0x1f
    };
    let _ = dma.write_val(off, &next);
    let _ = dma.write_val(off + 0x04, &EHCI_LIST_END);
    if len > 0 && buf_dma != 0 {
        let _ = dma.write_val(off + 0x0c, &buf_dma);
    }
    // Publish Active only after its buffer and links are fully initialized.
    let token = token_flags | ((len & 0x7fff) << 16);
    dma_io_fence();
    let _ = dma.write_val(off + 0x08, &token);
}

fn r32(io: &IoMem, off: usize) -> u32 {
    dma_io_fence();
    let value = io.read_once(off).unwrap();
    dma_io_fence();
    value
}

fn w32(io: &IoMem, off: usize, val: u32) {
    dma_io_fence();
    io.write_once(off, &val).unwrap();
    dma_io_fence();
}

/// Schedule-status acknowledgement, rather than clearing an enable bit,
/// establishes that CPU-owned descriptors can be rewritten (EHCI 2.3.1/2.3.2).
fn stop_schedule(io: &IoMem, command: u32, status: u32) -> Result<(), &'static str> {
    w32(io, REG_USBCMD, r32(io, REG_USBCMD) & !command);
    if handshake(io, REG_USBSTS, status, 0, 20_000) {
        Ok(())
    } else {
        FAILED.store(true, core::sync::atomic::Ordering::Release);
        Err("EHCI schedule stop timeout; DMA retained")
    }
}

fn handshake(io: &IoMem, off: usize, mask: u32, done: u32, timeout_us: u64) -> bool {
    let mut waited = 0u64;
    while waited < timeout_us {
        if r32(io, off) & mask == done {
            return true;
        }
        delay_us(10);
        waited += 10;
    }
    false
}

/// Full host PHY initialization from Linux `mv_usb2_phy_init`.
fn phy_bringup_host(phy: &IoMem) {
    phy.write_once(USB2_PHY_REG01, &USB2_PHY_REG01_BRINGUP)
        .unwrap();
    phy.write_once(USB2_PHY_REG0D, &USB2_PHY_REG0D_BRINGUP)
        .unwrap();

    let mut ana: u32 = phy.read_once(USB2_ANALOG_REG14_13).unwrap();
    ana = (ana & !USB2_ANALOG_HSDAC_ISEL_MASK)
        | USB2_ANALOG_HSDAC_ISEL_15_INC
        | USB2_ANALOG_HSDAC_IREG_EN;
    phy.write_once(USB2_ANALOG_REG14_13, &ana).unwrap();

    // Select HS parallel data path.
    let mut reg06: u32 = phy.read_once(USB2_PHY_REG06).unwrap();
    reg06 &= !USB2_CFG_HS_SRC_SEL;
    phy.write_once(USB2_PHY_REG06, &reg06).unwrap();

    // Host mode: auto-clear disconnect.
    let mut reg04: u32 = phy.read_once(USB2_PHY_REG04).unwrap();
    reg04 |= USB2_PHY_REG04_AUTO_CLEAR_DIS;
    phy.write_once(USB2_PHY_REG04, &reg04).unwrap();

    log::info!(
        "[k1-usb] usbphy1 bringup ANA={:#010x} REG06={:#010x} REG04={:#010x}",
        phy.read_once::<u32>(USB2_ANALOG_REG14_13).unwrap(),
        phy.read_once::<u32>(USB2_PHY_REG06).unwrap(),
        phy.read_once::<u32>(USB2_PHY_REG04).unwrap()
    );
}

fn wait_pll_ready(phy: &IoMem) -> bool {
    let deadline_us = PLL_TIMEOUT_MS.saturating_mul(1000);
    let mut waited = 0u64;
    while waited < deadline_us {
        let reg01: u32 = phy.read_once(USB2_PHY_REG01).unwrap();
        if reg01 & USB2_PHY_REG01_PLL_IS_READY != 0 {
            return true;
        }
        delay_us(PLL_POLL_US);
        waited = waited.saturating_add(PLL_POLL_US);
    }
    false
}

fn delay_us(us: u64) {
    let ticks = (tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}

fn delay_ms(ms: u64) {
    delay_us(ms.saturating_mul(1000));
}

#[cfg(ktest)]
mod periodic_tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn only_complete_boot_reports_reach_input() {
        assert!(complete_hid_report(QTD_TOGGLE | QTD_PID_IN));
        for token in [QTD_STS_ACTIVE, QTD_STS_HALT, 1 << 16, 8 << 16] {
            assert!(!complete_hid_report(token));
        }
    }

    #[ktest]
    fn completed_interrupt_waits_for_periodic_ownership() {
        let phase = HidPollPhase::Running;
        assert_eq!(
            phase.next_action(STS_PSS, QTD_STS_ACTIVE, 100, 20),
            Ok(HidPollAction::Wait)
        );
        assert_eq!(
            phase.next_action(STS_PSS, 0, 100, 20),
            Ok(HidPollAction::Stop)
        );
        // Completion and even HALT do not grant permission to rewrite a linked QH.
        let phase = HidPollPhase::Stopping { started_at: 100 };
        for token in [0, QTD_STS_HALT] {
            assert_eq!(
                phase.next_action(STS_PSS, token, 119, 20),
                Ok(HidPollAction::Wait)
            );
            assert_eq!(
                phase.next_action(0, token, 119, 20),
                Ok(HidPollAction::Rearm)
            );
        }
        // PSE must not be cleared again before the new enable is acknowledged.
        let phase = HidPollPhase::Starting { started_at: 120 };
        assert_eq!(phase.next_action(0, 0, 121, 20), Ok(HidPollAction::Wait));
        assert_eq!(
            phase.next_action(STS_PSS, 0, 121, 20),
            Ok(HidPollAction::Started)
        );
    }

    #[ktest]
    fn periodic_transitions_fail_without_reusing_owned_descriptors() {
        for phase in [
            HidPollPhase::Stopping { started_at: 100 },
            HidPollPhase::Starting { started_at: 100 },
        ] {
            let status = if matches!(phase, HidPollPhase::Stopping { .. }) {
                STS_PSS
            } else {
                0
            };
            assert_eq!(
                phase.next_action(status, 0, 119, 20),
                Ok(HidPollAction::Wait)
            );
            assert!(phase.next_action(status, 0, 120, 20).is_err());
            assert!(phase.next_action(status, 0, 1000, 20).is_err());
        }
        assert!(HidPollPhase::Running.next_action(0, 0, 100, 20).is_err());
        assert!(
            HidPollPhase::Running
                .next_action(STS_HALT | STS_PSS, 0, 100, 20)
                .is_err()
        );
    }

    #[ktest]
    fn periodic_deadline_uses_elapsed_hardware_ticks() {
        let phase = HidPollPhase::Stopping {
            started_at: u64::MAX - 9,
        };
        assert_eq!(
            phase.next_action(STS_PSS, 0, 9, 20),
            Ok(HidPollAction::Wait)
        );
        assert!(phase.next_action(STS_PSS, 0, 10, 20).is_err());
        // A late poll can safely accept ownership already returned by hardware.
        assert_eq!(phase.next_action(0, 0, 100, 20), Ok(HidPollAction::Rearm));
    }
}
