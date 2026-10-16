// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: EHCI HID boot-keyboard → aster-input.

//! USB HID boot-protocol keyboard → aster-input.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::sync::atomic::{AtomicBool, Ordering};

use aster_input::{
    event_type_codes::SynEvent,
    input_dev::{InputCapability, InputDevice, InputEvent, InputId, RegisteredInputDevice},
};
use spin::Mutex;

static LOGGED_FIRST_KEY: AtomicBool = AtomicBool::new(false);

/// Diff two boot reports and submit press/release + SYN to input.
///
/// Parse/table live in [`crate::hid`] so xHCI HID boot uses the same path.
pub(super) fn dispatch_report_diff(
    registered: &RegisteredInputDevice,
    prev: &mut [u8; 8],
    cur: &[u8; 8],
) {
    let Some(mut events) = crate::hid::kbd_events(prev, cur) else {
        return;
    };
    if events.is_empty() {
        *prev = *cur;
        return;
    }

    if !LOGGED_FIRST_KEY.swap(true, Ordering::Relaxed) {
        log::info!("[k1-usb] HID key event path live (aster-input)");
    }

    events.push(InputEvent::from_sync_event(SynEvent::Report));
    registered.submit_events(&events);
    *prev = *cur;
}

#[derive(Debug)]
struct UsbHidKeyboard {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    capability: InputCapability,
}

impl UsbHidKeyboard {
    fn new(vid: u16, pid: u16) -> Self {
        let mut capability = InputCapability::new();
        capability.set_supported_event_type(aster_input::event_type_codes::EventTypes::KEY);
        capability.set_supported_event_type(aster_input::event_type_codes::EventTypes::SYN);
        capability.add_standard_keyboard_keys();
        Self {
            name: "usb-hid-keyboard".to_string(),
            phys: "usb-ehci1-hub/input0".to_string(),
            uniq: "".to_string(),
            id: InputId::new(InputId::BUS_USB, vid, pid, 0x0110),
            capability,
        }
    }
}

impl InputDevice for UsbHidKeyboard {
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

pub(super) struct HidKbdState {
    pub registered: RegisteredInputDevice,
    pub last_report: [u8; 8],
    pub ep_addr: u8,
    pub maxpkt: u32,
    /// DevEp fields (Copy) stored flat for IRQ-context poll.
    pub addr: u8,
    pub eps: u32,
    pub tt_hub: u8,
    pub tt_port: u8,
}

pub(super) static HID_KBD: Mutex<Option<HidKbdState>> = Mutex::new(None);

pub(super) fn register_keyboard(vid: u16, pid: u16) -> RegisteredInputDevice {
    let device = Arc::new(UsbHidKeyboard::new(vid, pid));
    aster_input::register_device(device)
}
