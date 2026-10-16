// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: xHCI hub / device enumeration.

//! Root-port + hub enumeration on a polling xHCI host.

use alloc::vec::Vec;

use ostd::sync::Mutex;

use crate::{
    delay::delay_ms,
    desc::{
        self, ConfigHead, DeviceDesc, IfaceKind, USB_CLASS_HUB, USB_DIR_IN, USB_DT_CONFIG,
        USB_DT_DEVICE, USB_DT_HUB, USB_DT_SS_HUB, USB_RECIP_DEVICE, USB_RECIP_OTHER,
        USB_REQ_CLEAR_FEATURE, USB_REQ_GET_DESCRIPTOR, USB_REQ_GET_STATUS,
        USB_REQ_SET_CONFIGURATION, USB_REQ_SET_FEATURE, USB_TYPE_CLASS, setup,
    },
    hid,
    xhci::{
        Host, PortInfo,
        context::{SPEED_FS, SPEED_HS, SPEED_LS, SPEED_SS},
    },
};

const HUB_PORT_CONNECTION: u16 = 1 << 0;
const HUB_PORT_ENABLE: u16 = 1 << 1;
const HUB_PORT_RESET: u16 = 1 << 4;
const HUB_PORT_LS: u16 = 1 << 9;
const HUB_PORT_HS: u16 = 1 << 10;
const HUB_C_CONNECTION: u16 = 1 << 0;
const HUB_C_RESET: u16 = 1 << 4;
const FEAT_PORT_RESET: u16 = 4;
const FEAT_PORT_POWER: u16 = 8;
const FEAT_C_CONNECTION: u16 = 16;
const FEAT_C_RESET: u16 = 20;
/// USB3 SET_HUB_DEPTH (hub class, bRequest 12).
const HUB_SET_DEPTH: u8 = 12;

pub(crate) struct FoundMsc {
    pub slot: u8,
    pub vid: u16,
    pub pid: u16,
}

/// Walk every root port; recurse into hubs (onboard USB3 hub on Muse Book).
/// Per root port: the PORTSC (change bits masked) the last failed attempt
/// saw, and how many attempts failed in a row against that same value.
///
/// `scan` is driven by the block registry's 250 ms hotplug poll. Port reset
/// can spin for about a second under the host lock, so repeated failures
/// must not consume every poll. After [`MAX_FAILURES`], leave the port alone
/// until PORTSC changes (connection or link state).
struct PortBackoff {
    portsc: u32,
    failures: u8,
}

const MAX_FAILURES: u8 = 3;
static BACKOFF: Mutex<[PortBackoff; 16]> = Mutex::new(
    [const {
        PortBackoff {
            portsc: 0,
            failures: 0,
        }
    }; 16],
);

fn steady(portsc: u32) -> u32 {
    portsc & !crate::xhci::regs::PORT_CHANGE
}

/// A hub the last scan configured: which slot it sits in and which of its
/// ports had a device connected. Polled by [`topology_changed`].
struct HubRec {
    slot: u8,
    nports: u8,
    connected: u32,
}

/// Topology and slots owned by the last scan. Release those slots before a
/// new scan so repeated enumeration cannot exhaust the controller.
struct Topology {
    scanned: bool,
    root: [u32; 16],
    hubs: Vec<HubRec>,
    slots: Vec<u8>,
}

static TOPO: Mutex<Topology> = Mutex::new(Topology {
    scanned: false,
    root: [0; 16],
    hubs: Vec::new(),
    slots: Vec::new(),
});

fn note_slot(slot: u8) {
    TOPO.lock().slots.push(slot);
}

/// Whether anything on the bus moved since the last [`scan`]: a root port's
/// steady PORTSC, or a hub port's connection bit / C_CONNECTION. Costs one
/// GET_PORT_STATUS per hub port, so callers should rate-limit (once a
/// second is plenty; a human plugs in a stick slower than that). Returns
/// `true` before the first scan.
pub(crate) fn topology_changed(host: &Mutex<Host>) -> bool {
    let (roots, hubs) = {
        let t = TOPO.lock();
        if !t.scanned {
            return true;
        }
        let hubs: Vec<(u8, u8, u32)> = t
            .hubs
            .iter()
            .map(|h| (h.slot, h.nports, h.connected))
            .collect();
        (t.root, hubs)
    };
    let nports = host.lock().max_ports().min(16);
    for port in 1..=nports {
        let sc = steady(host.lock().portsc(port));
        if sc != roots[(port - 1) as usize] {
            log::info!(
                "[k1-usb] xhci root port{} PORTSC {:#010x} -> {:#010x}; rescanning",
                port,
                roots[(port - 1) as usize],
                sc
            );
            return true;
        }
    }
    for (slot, nports, connected) in hubs {
        for p in 1..=nports {
            let Ok((st, ch)) = hub_port_status(host, slot, p) else {
                log::info!(
                    "[k1-usb] xhci hub slot{} port{} status unreadable; rescanning",
                    slot,
                    p
                );
                return true;
            };
            let now = (st & HUB_PORT_CONNECTION != 0) as u32;
            let was = (connected >> (p - 1)) & 1;
            if now != was || ch & HUB_C_CONNECTION != 0 {
                log::warn!(
                    "[k1-usb] xhci hub slot{} port{} connection {} -> {} (change={:#06x}); rescanning",
                    slot,
                    p,
                    was,
                    now,
                    ch
                );
                return true;
            }
        }
    }
    false
}

/// Releases every slot the previous scan allocated, so a rescan starts from
/// an empty controller instead of leaking the 4 slots this xHC has.
fn release_previous(host: &Mutex<Host>) {
    let slots = {
        let mut t = TOPO.lock();
        t.hubs.clear();
        core::mem::take(&mut t.slots)
    };
    for slot in slots.into_iter().rev() {
        if let Err(e) = host.lock().disable_slot(slot) {
            log::info!("[k1-usb] xhci Disable Slot {}: {:?}", slot, e);
        }
    }
}

pub(crate) fn scan(host: &Mutex<Host>) -> Vec<FoundMsc> {
    release_previous(host);
    let mut found = Vec::new();
    let nports = {
        let h = host.lock();
        h.max_ports()
    };
    for port in 1..=nports.min(16) {
        let sc = host.lock().portsc(port);
        let idx = (port - 1) as usize;
        let (skip, first_failure) = {
            let mut bo = BACKOFF.lock();
            let b = &mut bo[idx];
            if b.failures > 0 && steady(sc) != b.portsc {
                log::info!(
                    "[k1-usb] xhci root port{} PORTSC {:#010x} -> {:#010x}; retrying enumeration",
                    port,
                    b.portsc,
                    steady(sc)
                );
                b.failures = 0;
            }
            (b.failures >= MAX_FAILURES, b.failures == 0)
        };
        if skip || sc & crate::xhci::regs::PORT_CCS == 0 {
            continue;
        }
        log::info!(
            "[k1-usb] xhci root port{} PORTSC={:#010x} ccs={}",
            port,
            sc,
            sc & 1
        );
        match enum_root_port(host, port) {
            Ok(mut v) => {
                BACKOFF.lock()[idx].failures = 0;
                found.append(&mut v);
            }
            Err(e) => {
                log::warn!("[k1-usb] xhci root port{} enum failed: {:?}", port, e);
                if first_failure {
                    host.lock().dump("enum");
                }
                let mut bo = BACKOFF.lock();
                let b = &mut bo[idx];
                b.portsc = steady(host.lock().portsc(port));
                b.failures = b.failures.saturating_add(1);
                if b.failures >= MAX_FAILURES {
                    log::warn!(
                        "[k1-usb] xhci root port{} failed {} times; leaving it alone until PORTSC changes",
                        port,
                        b.failures
                    );
                }
            }
        }
    }
    {
        let mut t = TOPO.lock();
        for port in 1..=nports.min(16) {
            t.root[(port - 1) as usize] = steady(host.lock().portsc(port));
        }
        t.scanned = true;
    }
    found
}

fn enum_root_port(host: &Mutex<Host>, port: u8) -> Result<Vec<FoundMsc>, crate::xhci::Error> {
    let pinfo = host.lock().reset_port(port)?;
    let slot = host.lock().enable_slot()?;
    note_slot(slot);
    host.lock().address_device(slot, pinfo, 0)?;
    delay_ms(2);
    enum_addressed(host, slot, pinfo, 0, 0)
}

fn enum_addressed(
    host: &Mutex<Host>,
    slot: u8,
    pinfo: PortInfo,
    depth: u8,
    parent_slot: u8,
) -> Result<Vec<FoundMsc>, crate::xhci::Error> {
    let mut d8 = [0u8; 8];
    ctrl_in(
        host,
        slot,
        setup(
            USB_DIR_IN,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_DEVICE as u16) << 8,
            0,
            8,
        ),
        &mut d8,
    )?;
    if d8[0] != 18 || d8[1] != USB_DT_DEVICE {
        return Err(crate::xhci::Error::Protocol);
    }
    host.lock().eval_ep0_mps(slot, d8[7])?;
    let mut d18 = [0u8; 18];
    ctrl_in(
        host,
        slot,
        setup(
            USB_DIR_IN,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_DEVICE as u16) << 8,
            0,
            18,
        ),
        &mut d18,
    )?;
    let Some(dev) = DeviceDesc::parse(&d18) else {
        log::warn!("[k1-usb] xhci slot{} bad device descriptor", slot);
        return Err(crate::xhci::Error::Protocol);
    };
    log::info!(
        "[k1-usb] xhci slot{} {:04x}:{:04x} class={:02x} usb={:#06x} mps0={}",
        slot,
        dev.vid,
        dev.pid,
        dev.class,
        dev.usb,
        dev.max_packet0
    );

    let mut cfg9 = [0u8; 9];
    ctrl_in(
        host,
        slot,
        setup(
            USB_DIR_IN,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_CONFIG as u16) << 8,
            0,
            9,
        ),
        &mut cfg9,
    )?;
    let Some(head) = ConfigHead::parse(&cfg9) else {
        return Err(crate::xhci::Error::Protocol);
    };
    let total = (head.total as usize).min(256);
    let mut cfg = alloc::vec![0u8; total];
    ctrl_in(
        host,
        slot,
        setup(
            USB_DIR_IN,
            USB_REQ_GET_DESCRIPTOR,
            (USB_DT_CONFIG as u16) << 8,
            0,
            total as u16,
        ),
        &mut cfg,
    )?;

    let mut out = Vec::new();
    if dev.class == USB_CLASS_HUB
        || desc::parse_config(&cfg)
            .iter()
            .any(|i| i.iface.class == USB_CLASS_HUB)
    {
        if depth >= 3 {
            log::warn!("[k1-usb] xhci hub depth cap; skip");
            return Ok(out);
        }
        out.append(&mut enum_hub(
            host,
            slot,
            pinfo,
            &cfg,
            head.value,
            depth,
            parent_slot,
        )?);
        return Ok(out);
    }

    if let Some((iface, inn, outep, cfgval)) = desc::find_msc_bot(&cfg) {
        let _ = iface;
        ctrl_nodata(
            host,
            slot,
            setup(0, USB_REQ_SET_CONFIGURATION, cfgval as u16, 0, 0),
        )?;
        let mps = inn.max_packet.min(outep.max_packet) & 0x7ff;
        host.lock()
            .configure_bulk(slot, outep.addr, inn.addr, mps)?;
        log::info!(
            "[k1-usb] xhci MSC BOT slot{} cfg={} in={:#x} out={:#x}",
            slot,
            cfgval,
            inn.addr,
            outep.addr
        );
        out.push(FoundMsc {
            slot,
            vid: dev.vid,
            pid: dev.pid,
        });
        return Ok(out);
    }

    let kbd = desc::find_hid_boot(&cfg, IfaceKind::HidBootKbd);
    let mouse = desc::find_hid_boot(&cfg, IfaceKind::HidBootMouse);
    if kbd.is_some() || mouse.is_some() {
        let cfgval = kbd
            .as_ref()
            .map(|(_, _, c)| *c)
            .or_else(|| mouse.as_ref().map(|(_, _, c)| *c))
            .unwrap_or(1);
        if let Err(e) = ctrl_nodata(
            host,
            slot,
            setup(0, USB_REQ_SET_CONFIGURATION, cfgval as u16, 0, 0),
        ) {
            log::info!("[k1-usb] hid SET_CONFIGURATION slot{}: {:?}", slot, e);
            return Ok(out);
        }
        if let Some((iface, ep, _)) = kbd {
            hid::bringup(
                host,
                slot,
                dev.vid,
                dev.pid,
                iface.number,
                ep,
                IfaceKind::HidBootKbd,
            );
        }
        if let Some((iface, ep, _)) = mouse {
            hid::bringup(
                host,
                slot,
                dev.vid,
                dev.pid,
                iface.number,
                ep,
                IfaceKind::HidBootMouse,
            );
        }
    }
    Ok(out)
}

fn enum_hub(
    host: &Mutex<Host>,
    slot: u8,
    pinfo: PortInfo,
    cfg: &[u8],
    cfgval: u8,
    depth: u8,
    _parent_slot: u8,
) -> Result<Vec<FoundMsc>, crate::xhci::Error> {
    ctrl_nodata(
        host,
        slot,
        setup(0, USB_REQ_SET_CONFIGURATION, cfgval as u16, 0, 0),
    )?;
    let ss = pinfo.speed >= SPEED_SS || cfg_has_ss_hub(cfg);
    if ss {
        // SET_HUB_DEPTH — USB 3.2 §10.14.2.8
        ctrl_nodata(
            host,
            slot,
            setup(
                USB_TYPE_CLASS | USB_RECIP_DEVICE,
                HUB_SET_DEPTH,
                depth as u16,
                0,
                0,
            ),
        )?;
    }
    let dt = if ss { USB_DT_SS_HUB } else { USB_DT_HUB };
    let mut hubd = [0u8; 16];
    let n = ctrl_in(
        host,
        slot,
        setup(
            USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_DEVICE,
            USB_REQ_GET_DESCRIPTOR,
            (dt as u16) << 8,
            0,
            12,
        ),
        &mut hubd,
    )?;
    if n < 6 || hubd[1] != dt || !(1..=15).contains(&hubd[2]) {
        // This topology uses one 4-bit route nibble and a u32 connection mask.
        return Err(crate::xhci::Error::Protocol);
    }
    let nports = hubd[2];
    let pwr_ms = u64::from(hubd[5]) * 2;
    log::info!(
        "[k1-usb] xhci hub slot{} ss={} ports={} pwr_on={}ms",
        slot,
        ss as u8,
        nports,
        pwr_ms
    );
    for p in 1..=nports {
        if let Err(e) = ctrl_nodata(
            host,
            slot,
            setup(
                USB_TYPE_CLASS | USB_RECIP_OTHER,
                USB_REQ_SET_FEATURE,
                FEAT_PORT_POWER,
                p as u16,
                0,
            ),
        ) {
            log::info!(
                "[k1-usb] xhci hub slot{} port{} SET_FEATURE(PORT_POWER): {:?}",
                slot,
                p,
                e
            );
        }
    }
    delay_ms(pwr_ms.max(100));
    // Allow one extra power-good interval when a SuperSpeed hub still
    // reports all-zero status immediately after bPwrOn2PwrGood.
    if ss && hub_port_status(host, slot, 1).is_ok_and(|(st, _)| st == 0) {
        delay_ms(pwr_ms.max(100));
    }

    let mut found = Vec::new();
    let mut connected = 0u32;
    for p in 1..=nports {
        let (st, ch) = match hub_port_status(host, slot, p) {
            Ok(v) => v,
            Err(e) => {
                log::info!("[k1-usb] xhci hub slot{} port{} status: {:?}", slot, p, e);
                continue;
            }
        };
        log::warn!(
            "[k1-usb] xhci hub slot{} port{} status={:#06x} change={:#06x}",
            slot,
            p,
            st,
            ch
        );
        if st & HUB_PORT_CONNECTION == 0 {
            continue;
        }
        connected |= 1 << (p - 1);
        if ch & HUB_C_CONNECTION != 0 {
            let _ = ctrl_nodata(
                host,
                slot,
                setup(
                    USB_TYPE_CLASS | USB_RECIP_OTHER,
                    USB_REQ_CLEAR_FEATURE,
                    FEAT_C_CONNECTION,
                    p as u16,
                    0,
                ),
            );
        }
        let _ = ctrl_nodata(
            host,
            slot,
            setup(
                USB_TYPE_CLASS | USB_RECIP_OTHER,
                USB_REQ_SET_FEATURE,
                FEAT_PORT_RESET,
                p as u16,
                0,
            ),
        );
        let mut enabled = false;
        let mut child_speed = SPEED_HS;
        for _ in 0..50 {
            delay_ms(10);
            let (st, ch) = hub_port_status(host, slot, p)?;
            if ch & HUB_C_RESET != 0 || (st & HUB_PORT_ENABLE != 0 && st & HUB_PORT_RESET == 0) {
                let _ = ctrl_nodata(
                    host,
                    slot,
                    setup(
                        USB_TYPE_CLASS | USB_RECIP_OTHER,
                        USB_REQ_CLEAR_FEATURE,
                        FEAT_C_RESET,
                        p as u16,
                        0,
                    ),
                );
                enabled = st & HUB_PORT_ENABLE != 0;
                child_speed = hub_child_speed(st, ss);
                break;
            }
        }
        if !enabled {
            log::warn!("[k1-usb] xhci hub slot{} port{} reset failed", slot, p);
            continue;
        }
        let route = if depth == 0 {
            p as u32
        } else {
            let parent_route = host.lock().slot_info(slot).map(|s| s.route).unwrap_or(0);
            route_nibble(parent_route, p)
        };
        let rh_port = host
            .lock()
            .slot_info(slot)
            .map(|s| s.port)
            .unwrap_or(pinfo.port);
        let (tt_hub, tt_port) = if child_speed == SPEED_LS || child_speed == SPEED_FS {
            (slot, p)
        } else {
            (0, 0)
        };
        log::info!(
            "[k1-usb] xhci hub child port{} speed={} route={:#x} tt={}/{}",
            p,
            child_speed,
            route,
            tt_hub,
            tt_port
        );
        let child = match host.lock().enable_slot() {
            Ok(s) => s,
            Err(e) => {
                log::info!("[k1-usb] xhci Enable Slot for hub port{}: {:?}", p, e);
                continue;
            }
        };
        note_slot(child);
        if let Err(e) =
            host.lock()
                .address_child(child, rh_port, child_speed, route, tt_hub, tt_port)
        {
            log::info!("[k1-usb] xhci Address Device hub child: {:?}", e);
            let _ = host.lock().disable_slot(child);
            TOPO.lock().slots.retain(|&s| s != child);
            continue;
        }
        delay_ms(2);
        let child_info = PortInfo {
            port: rh_port,
            speed: child_speed,
            portsc: 0,
        };
        match enum_addressed(host, child, child_info, depth + 1, slot) {
            Ok(mut v) => found.append(&mut v),
            Err(e) => log::info!("[k1-usb] xhci hub port{} child enum: {:?}", p, e),
        }
    }
    TOPO.lock().hubs.push(HubRec {
        slot,
        nports,
        connected,
    });
    Ok(found)
}

fn route_nibble(parent: u32, port: u8) -> u32 {
    if parent == 0 {
        port as u32 & 0xf
    } else {
        // Next free nibble (xHCI 8.19 / USB 3 route string).
        let mut shift = 0;
        let mut r = parent;
        while shift < 20 && r & 0xf != 0 {
            r >>= 4;
            shift += 4;
        }
        parent | (((port as u32) & 0xf) << shift)
    }
}

fn hub_child_speed(st: u16, ss_hub: bool) -> u8 {
    if ss_hub {
        // USB3 port status Speed [12:10]: 0 = SS when connected as SS.
        match (st >> 10) & 7 {
            0 => SPEED_SS,
            1 => SPEED_FS, // leftover encodings vary; prefer HS if bit 10 of USB2
            _ => SPEED_SS,
        }
    } else if st & HUB_PORT_HS != 0 {
        SPEED_HS
    } else if st & HUB_PORT_LS != 0 {
        SPEED_LS
    } else {
        SPEED_FS
    }
}

fn cfg_has_ss_hub(cfg: &[u8]) -> bool {
    desc::parse_config(cfg)
        .iter()
        .any(|i| i.iface.class == USB_CLASS_HUB && i.iface.protocol >= 3)
}

fn hub_port_status(
    host: &Mutex<Host>,
    slot: u8,
    port: u8,
) -> Result<(u16, u16), crate::xhci::Error> {
    let mut b = [0u8; 4];
    let count = ctrl_in(
        host,
        slot,
        setup(
            USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_OTHER,
            USB_REQ_GET_STATUS,
            0,
            port as u16,
            4,
        ),
        &mut b,
    )?;
    if count != b.len() {
        return Err(crate::xhci::Error::Protocol);
    }
    Ok((
        u16::from_le_bytes([b[0], b[1]]),
        u16::from_le_bytes([b[2], b[3]]),
    ))
}

fn ctrl_in(
    host: &Mutex<Host>,
    slot: u8,
    setup: [u8; 8],
    data: &mut [u8],
) -> Result<usize, crate::xhci::Error> {
    host.lock().ctrl(slot, setup, Some(data), true)
}

fn ctrl_nodata(host: &Mutex<Host>, slot: u8, setup: [u8; 8]) -> Result<usize, crate::xhci::Error> {
    host.lock().ctrl(slot, setup, None, false)
}
