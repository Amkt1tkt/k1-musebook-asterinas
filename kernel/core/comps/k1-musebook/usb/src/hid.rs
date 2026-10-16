// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: HID boot keyboard/mouse report parse + evdev path.

//! HID boot-protocol keyboard/mouse: report parse (shared with EHCI) +
//! xHCI interrupt IN → aster-input.
//!
//! Keyboard report: HID 1.11 Appendix B.1 (modifier + reserved + 6 usages).
//! Mouse report: Appendix B.2 (buttons + X + Y; byte 3 wheel is common).
//! Class requests: HID 1.11 §7.2 SET_PROTOCOL / SET_IDLE.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
#[cfg(target_arch = "riscv64")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_arch = "riscv64")]
use aster_input::input_dev::RegisteredInputDevice;
use aster_input::{
    event_type_codes::{EventTypes, KeyCode, KeyStatus, RelCode, SynEvent},
    input_dev::{InputCapability, InputDevice, InputEvent, InputId},
};
#[cfg(target_arch = "riscv64")]
use ostd::sync::Mutex;

#[cfg(target_arch = "riscv64")]
use crate::desc::{
    Endpoint, HID_PROTOCOL_BOOT, HID_REQ_SET_IDLE, HID_REQ_SET_PROTOCOL, IfaceKind,
    USB_RECIP_INTERFACE, USB_TYPE_CLASS, setup,
};

/// ErrorRollOver / POST Fail / ErrorUndefined — HID Usage Tables keyboard page.
const HID_USAGE_PHANTOM: [u8; 3] = [0x01, 0x02, 0x03];

const MOD_BITS: [(u8, KeyCode); 8] = [
    (1 << 0, KeyCode::LeftCtrl),
    (1 << 1, KeyCode::LeftShift),
    (1 << 2, KeyCode::LeftAlt),
    (1 << 3, KeyCode::LeftMeta),
    (1 << 4, KeyCode::RightCtrl),
    (1 << 5, KeyCode::RightShift),
    (1 << 6, KeyCode::RightAlt),
    (1 << 7, KeyCode::RightMeta),
];

/// USB HID boot keyboard usage ID → Linux/Asterinas [`KeyCode`].
pub(crate) fn hid_usage_to_keycode(usage: u8) -> Option<KeyCode> {
    Some(match usage {
        0x04 => KeyCode::A,
        0x05 => KeyCode::B,
        0x06 => KeyCode::C,
        0x07 => KeyCode::D,
        0x08 => KeyCode::E,
        0x09 => KeyCode::F,
        0x0a => KeyCode::G,
        0x0b => KeyCode::H,
        0x0c => KeyCode::I,
        0x0d => KeyCode::J,
        0x0e => KeyCode::K,
        0x0f => KeyCode::L,
        0x10 => KeyCode::M,
        0x11 => KeyCode::N,
        0x12 => KeyCode::O,
        0x13 => KeyCode::P,
        0x14 => KeyCode::Q,
        0x15 => KeyCode::R,
        0x16 => KeyCode::S,
        0x17 => KeyCode::T,
        0x18 => KeyCode::U,
        0x19 => KeyCode::V,
        0x1a => KeyCode::W,
        0x1b => KeyCode::X,
        0x1c => KeyCode::Y,
        0x1d => KeyCode::Z,
        0x1e => KeyCode::Num1,
        0x1f => KeyCode::Num2,
        0x20 => KeyCode::Num3,
        0x21 => KeyCode::Num4,
        0x22 => KeyCode::Num5,
        0x23 => KeyCode::Num6,
        0x24 => KeyCode::Num7,
        0x25 => KeyCode::Num8,
        0x26 => KeyCode::Num9,
        0x27 => KeyCode::Num0,
        0x28 => KeyCode::Enter,
        0x29 => KeyCode::Esc,
        0x2a => KeyCode::Backspace,
        0x2b => KeyCode::Tab,
        0x2c => KeyCode::Space,
        0x2d => KeyCode::Minus,
        0x2e => KeyCode::Equal,
        0x2f => KeyCode::LeftBrace,
        0x30 => KeyCode::RightBrace,
        0x31 => KeyCode::Backslash,
        0x33 => KeyCode::Semicolon,
        0x34 => KeyCode::Apostrophe,
        0x35 => KeyCode::Grave,
        0x36 => KeyCode::Comma,
        0x37 => KeyCode::Dot,
        0x38 => KeyCode::Slash,
        0x39 => KeyCode::CapsLock,
        0x3a => KeyCode::F1,
        0x3b => KeyCode::F2,
        0x3c => KeyCode::F3,
        0x3d => KeyCode::F4,
        0x3e => KeyCode::F5,
        0x3f => KeyCode::F6,
        0x40 => KeyCode::F7,
        0x41 => KeyCode::F8,
        0x42 => KeyCode::F9,
        0x43 => KeyCode::F10,
        0x44 => KeyCode::F11,
        0x45 => KeyCode::F12,
        0x49 => KeyCode::Insert,
        0x4a => KeyCode::Home,
        0x4b => KeyCode::PageUp,
        0x4c => KeyCode::Delete,
        0x4d => KeyCode::End,
        0x4e => KeyCode::PageDown,
        0x4f => KeyCode::Right,
        0x50 => KeyCode::Left,
        0x51 => KeyCode::Down,
        0x52 => KeyCode::Up,
        _ => return None,
    })
}

pub(crate) fn kbd_report_is_phantom(report: &[u8; 8]) -> bool {
    report[2..8].iter().any(|u| HID_USAGE_PHANTOM.contains(u))
}

/// Diff two boot keyboard reports. `None` = phantom/rollover (keep previous).
pub(crate) fn kbd_events(prev: &[u8; 8], cur: &[u8; 8]) -> Option<Vec<InputEvent>> {
    if kbd_report_is_phantom(cur) {
        return None;
    }
    if prev == cur {
        return Some(Vec::new());
    }

    let mut events: Vec<InputEvent> = Vec::new();
    let old_mod = prev[0];
    let new_mod = cur[0];
    for &(bit, key) in &MOD_BITS {
        let was = old_mod & bit != 0;
        let now = new_mod & bit != 0;
        if was != now {
            let st = if now {
                KeyStatus::Pressed
            } else {
                KeyStatus::Released
            };
            events.push(InputEvent::from_key_and_status(key, st));
        }
    }

    // Key slots [2..8]; empty (0) skipped. Phantom usages already rejected.
    for &usage in &prev[2..8] {
        if usage == 0 {
            continue;
        }
        if !cur[2..8].contains(&usage)
            && let Some(kc) = hid_usage_to_keycode(usage)
        {
            events.push(InputEvent::from_key_and_status(kc, KeyStatus::Released));
        }
    }
    for &usage in &cur[2..8] {
        if usage == 0 {
            continue;
        }
        if !prev[2..8].contains(&usage)
            && let Some(kc) = hid_usage_to_keycode(usage)
        {
            events.push(InputEvent::from_key_and_status(kc, KeyStatus::Pressed));
        }
    }
    Some(events)
}

/// Boot mouse: buttons + signed X/Y + optional wheel.
///
/// Returns `(new_buttons, events)`. `None` if the report is shorter than 3.
pub(crate) fn mouse_events(prev_btns: u8, report: &[u8]) -> Option<(u8, Vec<InputEvent>)> {
    if report.len() < 3 {
        return None;
    }
    let btns = report[0] & 0x07;
    let dx = report[1] as i8 as i32;
    let dy = report[2] as i8 as i32;
    let wheel = if report.len() >= 4 {
        report[3] as i8 as i32
    } else {
        0
    };

    let mut events = Vec::new();
    const BTN_MAP: [(u8, KeyCode); 3] = [
        (1 << 0, KeyCode::BtnLeft),
        (1 << 1, KeyCode::BtnRight),
        (1 << 2, KeyCode::BtnMiddle),
    ];
    for &(bit, key) in &BTN_MAP {
        let was = prev_btns & bit != 0;
        let now = btns & bit != 0;
        if was != now {
            let st = if now {
                KeyStatus::Pressed
            } else {
                KeyStatus::Released
            };
            events.push(InputEvent::from_key_and_status(key, st));
        }
    }
    if dx != 0 {
        events.push(InputEvent::from_relative_move(RelCode::X, dx));
    }
    if dy != 0 {
        events.push(InputEvent::from_relative_move(RelCode::Y, dy));
    }
    if wheel != 0 {
        events.push(InputEvent::from_relative_move(RelCode::Wheel, wheel));
    }
    Some((btns, events))
}

#[derive(Debug)]
struct UsbHidKbd {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    capability: InputCapability,
}

impl UsbHidKbd {
    fn new(vid: u16, pid: u16) -> Self {
        let mut capability = InputCapability::new();
        capability.set_supported_event_type(EventTypes::KEY);
        capability.set_supported_event_type(EventTypes::SYN);
        capability.add_standard_keyboard_keys();
        Self {
            name: "usb-kbd".to_string(),
            phys: "usb-xhci/input0".to_string(),
            uniq: "".to_string(),
            id: InputId::new(InputId::BUS_USB, vid, pid, 0x0111),
            capability,
        }
    }
}

impl InputDevice for UsbHidKbd {
    fn name(&self) -> &str {
        &self.name
    }
    fn phys(&self) -> &str {
        &self.phys
    }
    fn uniq(&self) -> &str {
        &self.uniq
    }
    fn id(&self) -> InputId {
        self.id
    }
    fn capability(&self) -> &InputCapability {
        &self.capability
    }
}

#[derive(Debug)]
struct UsbHidMouse {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    capability: InputCapability,
}

impl UsbHidMouse {
    fn new(vid: u16, pid: u16) -> Self {
        let mut capability = InputCapability::new();
        capability.set_supported_event_type(EventTypes::SYN);
        capability.set_supported_event_type(EventTypes::KEY);
        capability.set_supported_event_type(EventTypes::REL);
        capability.set_supported_key(KeyCode::BtnLeft);
        capability.set_supported_key(KeyCode::BtnRight);
        capability.set_supported_key(KeyCode::BtnMiddle);
        capability.set_supported_relative_axis(RelCode::X);
        capability.set_supported_relative_axis(RelCode::Y);
        capability.set_supported_relative_axis(RelCode::Wheel);
        Self {
            name: "usb-mouse".to_string(),
            phys: "usb-xhci/input1".to_string(),
            uniq: "".to_string(),
            id: InputId::new(InputId::BUS_USB, vid, pid, 0x0111),
            capability,
        }
    }
}

impl InputDevice for UsbHidMouse {
    fn name(&self) -> &str {
        &self.name
    }
    fn phys(&self) -> &str {
        &self.phys
    }
    fn uniq(&self) -> &str {
        &self.uniq
    }
    fn id(&self) -> InputId {
        self.id
    }
    fn capability(&self) -> &InputCapability {
        &self.capability
    }
}

#[cfg(target_arch = "riscv64")]
mod xhci_dev {
    use super::*;
    use crate::xhci::Host;

    const FAIL_LIMIT: u8 = 8;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Kbd,
        Mouse,
    }

    struct HidDev {
        kind: Kind,
        slot: u8,
        ep_addr: u8,
        registered: RegisteredInputDevice,
        last_kbd: [u8; 8],
        last_mouse_btns: u8,
        fails: u8,
    }

    static DEVS: Mutex<Vec<HidDev>> = Mutex::new(Vec::new());
    static LOGGED_KBD: AtomicBool = AtomicBool::new(false);
    static LOGGED_MOUSE: AtomicBool = AtomicBool::new(false);

    pub(super) fn has_live() -> bool {
        !DEVS.lock().is_empty()
    }

    pub(super) fn bringup(
        host: &Mutex<Host>,
        slot: u8,
        vid: u16,
        pid: u16,
        iface: u8,
        ep: Endpoint,
        kind: IfaceKind,
    ) {
        let kind = match kind {
            IfaceKind::HidBootKbd => Kind::Kbd,
            IfaceKind::HidBootMouse => Kind::Mouse,
            _ => return,
        };
        let label = match kind {
            Kind::Kbd => "kbd",
            Kind::Mouse => "mouse",
        };
        if let Err(e) = host
            .lock()
            .configure_int_in(slot, ep.addr, ep.max_packet, ep.interval)
        {
            log::info!(
                "[k1-usb] hid {} attach {:04x}:{:04x} slot{} configure: {:?}",
                label,
                vid,
                pid,
                slot,
                e
            );
            return;
        }
        // HID 1.11 §7.2.6 SET_PROTOCOL(boot) + §7.2.4 SET_IDLE(0 = indefinite).
        let proto = setup(
            USB_TYPE_CLASS | USB_RECIP_INTERFACE,
            HID_REQ_SET_PROTOCOL,
            HID_PROTOCOL_BOOT,
            iface as u16,
            0,
        );
        if let Err(e) = host.lock().ctrl(slot, proto, None, false) {
            log::warn!("[k1-usb] hid {} SET_PROTOCOL failed: {:?}", label, e);
            host.lock().release_int_in(slot, ep.addr);
            return;
        }
        let idle = setup(
            USB_TYPE_CLASS | USB_RECIP_INTERFACE,
            HID_REQ_SET_IDLE,
            0,
            iface as u16,
            0,
        );
        if let Err(e) = host.lock().ctrl(slot, idle, None, false) {
            log::info!("[k1-usb] hid {} SET_IDLE: {:?} (continue)", label, e);
        }

        let registered = match kind {
            Kind::Kbd => aster_input::register_device(Arc::new(UsbHidKbd::new(vid, pid))),
            Kind::Mouse => aster_input::register_device(Arc::new(UsbHidMouse::new(vid, pid))),
        };
        if let Err(e) = host.lock().int_in_arm(slot, ep.addr) {
            log::info!("[k1-usb] hid {} arm: {:?}", label, e);
        }
        DEVS.lock().push(HidDev {
            kind,
            slot,
            ep_addr: ep.addr,
            registered,
            last_kbd: [0; 8],
            last_mouse_btns: 0,
            fails: 0,
        });
        log::info!(
            "[k1-usb] hid {} attach {:04x}:{:04x} slot{} iface={} ep={:#x} interval={}",
            label,
            vid,
            pid,
            slot,
            iface,
            ep.addr,
            ep.interval
        );
    }

    pub(super) fn poll(host: &Mutex<Host>) {
        let Some(mut h) = host.try_lock() else {
            return;
        };
        poll_host(&mut h);
    }

    pub(super) fn poll_host(host: &mut Host) {
        host.pump();
        let mut drop_idx: Vec<usize> = Vec::new();
        {
            // A bottom half may preempt a thread querying or attaching HID.
            // Retry on the next tick instead of spinning on that thread's lock.
            let Some(mut devs) = DEVS.try_lock() else {
                return;
            };
            for (i, dev) in devs.iter_mut().enumerate() {
                if let Err(e) = host.int_in_arm(dev.slot, dev.ep_addr) {
                    dev.fails = dev.fails.saturating_add(1);
                    if dev.fails == 1 {
                        log::info!(
                            "[k1-usb] hid {} arm slot{}: {:?}",
                            kind_label(dev.kind),
                            dev.slot,
                            e
                        );
                    }
                    if dev.fails >= FAIL_LIMIT {
                        drop_idx.push(i);
                    }
                    continue;
                }
                let mut raw = [0u8; 8];
                match host.int_in_poll(dev.slot, dev.ep_addr, &mut raw) {
                    None => {}
                    Some(Ok(n)) => {
                        dev.fails = 0;
                        dispatch(dev, &raw[..n]);
                        let _ = host.int_in_arm(dev.slot, dev.ep_addr);
                    }
                    Some(Err(e)) => {
                        dev.fails = dev.fails.saturating_add(1);
                        if dev.fails == 1 || dev.fails >= FAIL_LIMIT {
                            log::warn!(
                                "[k1-usb] hid {} xfer slot{} fail#{}: {:?}",
                                kind_label(dev.kind),
                                dev.slot,
                                dev.fails,
                                e
                            );
                        }
                        if dev.fails >= FAIL_LIMIT {
                            drop_idx.push(i);
                        } else {
                            let _ = host.int_in_arm(dev.slot, dev.ep_addr);
                        }
                    }
                }
            }
        }
        for i in drop_idx.into_iter().rev() {
            detach(host, i);
        }
    }

    fn dispatch(dev: &mut HidDev, raw: &[u8]) {
        match dev.kind {
            Kind::Kbd => {
                let mut cur = [0u8; 8];
                let n = raw.len().min(8);
                cur[..n].copy_from_slice(&raw[..n]);
                let Some(mut events) = kbd_events(&dev.last_kbd, &cur) else {
                    return;
                };
                if !events.is_empty() {
                    if !LOGGED_KBD.swap(true, Ordering::Relaxed) {
                        log::info!("[k1-usb] hid kbd event path live (aster-input)");
                    }
                    events.push(InputEvent::from_sync_event(SynEvent::Report));
                    dev.registered.submit_events(&events);
                }
                dev.last_kbd = cur;
            }
            Kind::Mouse => {
                let Some((btns, mut events)) = mouse_events(dev.last_mouse_btns, raw) else {
                    return;
                };
                if !events.is_empty() {
                    if !LOGGED_MOUSE.swap(true, Ordering::Relaxed) {
                        log::info!("[k1-usb] hid mouse event path live (aster-input)");
                    }
                    events.push(InputEvent::from_sync_event(SynEvent::Report));
                    dev.registered.submit_events(&events);
                }
                dev.last_mouse_btns = btns;
            }
        }
    }

    fn detach(host: &mut Host, idx: usize) {
        let Some(mut devs) = DEVS.try_lock() else {
            return;
        };
        if idx >= devs.len() {
            return;
        }
        let dev = devs.remove(idx);
        let slot = dev.slot;
        let ep = dev.ep_addr;
        let kind = dev.kind;
        drop(dev.registered);
        host.release_int_in(slot, ep);
        let others = devs.iter().any(|d| d.slot == slot);
        drop(devs);
        if !others {
            // Stop Endpoint is unimplemented; Disable Slot is best-effort.
            if let Err(e) = host.disable_slot(slot) {
                log::info!(
                    "[k1-usb] hid {} detach slot{} Disable Slot failed: {:?}; ring retained",
                    kind_label(kind),
                    slot,
                    e
                );
            }
        }
        log::warn!(
            "[k1-usb] hid {} detach slot{} ep={:#x} ({} consecutive xfer fails)",
            kind_label(kind),
            slot,
            ep,
            FAIL_LIMIT
        );
    }

    fn kind_label(k: Kind) -> &'static str {
        match k {
            Kind::Kbd => "kbd",
            Kind::Mouse => "mouse",
        }
    }
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn has_live() -> bool {
    xhci_dev::has_live()
}

#[cfg(not(target_arch = "riscv64"))]
pub(crate) fn has_live() -> bool {
    false
}

/// Configure interrupt IN, SET_PROTOCOL(boot), SET_IDLE, register evdev.
#[cfg(target_arch = "riscv64")]
pub(crate) fn bringup(
    host: &Mutex<crate::xhci::Host>,
    slot: u8,
    vid: u16,
    pid: u16,
    iface: u8,
    ep: Endpoint,
    kind: IfaceKind,
) {
    xhci_dev::bringup(host, slot, vid, pid, iface, ep, kind);
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn poll(host: &Mutex<crate::xhci::Host>) {
    xhci_dev::poll(host);
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn usage_letter_and_unknown() {
        assert_eq!(hid_usage_to_keycode(0x04), Some(KeyCode::A));
        assert_eq!(hid_usage_to_keycode(0x28), Some(KeyCode::Enter));
        assert_eq!(hid_usage_to_keycode(0x00), None);
        assert_eq!(hid_usage_to_keycode(0x01), None);
    }

    #[ktest]
    fn phantom_rollover_ignored() {
        let prev = [0u8; 8];
        let mut cur = [0u8; 8];
        cur[2] = 0x01;
        assert!(kbd_events(&prev, &cur).is_none());
        cur[2] = 0x01;
        cur[3] = 0x01;
        cur[4] = 0x01;
        assert!(kbd_events(&prev, &cur).is_none());
        assert!(kbd_report_is_phantom(&cur));
    }

    #[ktest]
    fn kbd_press_release_and_modifier() {
        let prev = [0u8; 8];
        let mut down = [0u8; 8];
        down[2] = 0x04;
        let ev = kbd_events(&prev, &down).unwrap();
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::A,
            KeyStatus::Pressed
        )));

        down[0] = 1 << 1;
        let ev = kbd_events(&prev, &down).unwrap();
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::LeftShift,
            KeyStatus::Pressed
        )));
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::A,
            KeyStatus::Pressed
        )));

        let ev = kbd_events(&down, &prev).unwrap();
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::A,
            KeyStatus::Released
        )));
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::LeftShift,
            KeyStatus::Released
        )));
        assert!(kbd_events(&prev, &prev).unwrap().is_empty());
    }

    #[ktest]
    fn mouse_buttons_motion_wheel() {
        let (btns, ev) = mouse_events(0, &[0x05, 5, 0xfb, 2]).unwrap();
        assert_eq!(btns, 0x05);
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::BtnLeft,
            KeyStatus::Pressed
        )));
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::BtnMiddle,
            KeyStatus::Pressed
        )));
        assert!(ev.contains(&InputEvent::from_relative_move(RelCode::X, 5)));
        assert!(ev.contains(&InputEvent::from_relative_move(RelCode::Y, -5)));
        assert!(ev.contains(&InputEvent::from_relative_move(RelCode::Wheel, 2)));

        let (btns, ev) = mouse_events(0x01, &[0x00, 0, 0]).unwrap();
        assert_eq!(btns, 0);
        assert!(ev.contains(&InputEvent::from_key_and_status(
            KeyCode::BtnLeft,
            KeyStatus::Released
        )));
        assert!(mouse_events(0, &[0x00, 0]).is_none());
    }
}
