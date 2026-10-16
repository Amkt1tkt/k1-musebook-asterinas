// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: lid / hall switch on GPIO74 → EV_SW / SW_LID.

//! Muse Book lid / hall switch — GPIO74, Linux `spacemit_lid.c`.
//!
//! DT (`k1-x_MUSE-Book.dts` `spacemit_lid`):
//! - `compatible = "spacemit,k1x-lid"`
//! - `lid-gpios = <&gpio 74 0>`
//!
//! Linux reports `input_report_switch(SW_LID, !gpiod_get_value)`. On this
//! board GPIO74 reads 1 when the lid is open, so `SW_LID = 1` means closed.
//! Both edges. A 200 ms poll covers a missed edge.

use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use aster_input::{
    event_type_codes::{EventTypes, SwCode, SynEvent},
    input_dev::{InputCapability, InputDevice, InputEvent, InputId, RegisteredInputDevice},
};
use aster_softirq::Taskless;
use ostd::arch::boot::DEVICE_TREE;
use spin::{Mutex, Once};

const LID_GPIO: u32 = 74;
const POLL_EVERY_TICKS: u32 = 200;

/// `SW_LID` is active (lid shut) when the hall GPIO is low.
pub(crate) fn sw_lid_from_level(level: u32) -> bool {
    level == 0
}

static DEV: Mutex<Option<RegisteredInputDevice>> = Mutex::new(None);
static CLOSED: AtomicU8 = AtomicU8::new(0);
static LAST_LEVEL: AtomicU32 = AtomicU32::new(u32::MAX);
static TICK: AtomicU32 = AtomicU32::new(0);
static BH: Once<Arc<Taskless>> = Once::new();
static IRQ_GPIO: AtomicU32 = AtomicU32::new(LID_GPIO);

pub(super) fn probe() {
    let gpio = lid_gpio_from_dt();
    IRQ_GPIO.store(gpio, Ordering::Relaxed);

    let Some(level) = k1_musebook_gpio::get_value(gpio) else {
        return;
    };
    store_level(level);

    let device = Arc::new(LidSwitch::new());
    *DEV.lock() = Some(aster_input::register_device(device));
    BH.call_once(|| Taskless::new(handle_lid));
    let closed = sw_lid_from_level(level);
    if let Some(dev) = DEV.lock().as_ref() {
        dev.submit_events(&[
            InputEvent::from_switch(SwCode::Lid, closed),
            InputEvent::from_sync_event(SynEvent::Report),
        ]);
    }

    match k1_musebook_gpio::register_irq(gpio, k1_musebook_gpio::Edge::Both, lid_gpio_irq) {
        Ok(()) => log::info!(
            "[k1-pmic] lid GPIO{} both-edge (SW_LID={} closed)",
            gpio,
            u32::from(sw_lid_from_level(level))
        ),
        Err(err) => log::warn!(
            "[k1-pmic] lid GPIO{} IRQ unavailable ({:?}); 200ms poll",
            gpio,
            err
        ),
    }
    ostd::timer::register_callback_on_cpu(lid_timer_poll);
}

fn lid_gpio_from_dt() -> u32 {
    let Some(dt) = DEVICE_TREE.get() else {
        return LID_GPIO;
    };
    let Some(node) = dt.find_compatible(&["spacemit,k1x-lid"]) else {
        return LID_GPIO;
    };
    // `lid-gpios = <&gpio 74 0>` — three cells; the pin is the second.
    if let Some(prop) = node.property("lid-gpios")
        && prop.value.len() >= 8
    {
        let pin = u32::from_be_bytes([prop.value[4], prop.value[5], prop.value[6], prop.value[7]]);
        if pin < 128 {
            return pin;
        }
    }
    LID_GPIO
}

fn lid_gpio_irq(_gpio: u32) {
    if let Some(bh) = BH.get() {
        bh.schedule();
    }
}

fn store_level(level: u32) -> bool {
    LAST_LEVEL.store(level, Ordering::Relaxed);
    let closed = sw_lid_from_level(level);
    CLOSED.store(u8::from(closed), Ordering::Relaxed);
    closed
}

fn handle_lid() {
    let gpio = IRQ_GPIO.load(Ordering::Relaxed);
    let Some(level) = k1_musebook_gpio::get_value(gpio) else {
        return;
    };
    if level == LAST_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let closed = store_level(level);
    let events = [
        InputEvent::from_switch(SwCode::Lid, closed),
        InputEvent::from_sync_event(SynEvent::Report),
    ];
    if let Some(dev) = DEV.lock().as_ref() {
        dev.submit_events(&events);
    }
}

fn lid_timer_poll() {
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    if !tick.is_multiple_of(POLL_EVERY_TICKS) {
        return;
    }
    // Share the GPIO bottom half: a timer IRQ must not interrupt handle_lid()
    // and acquire its plain DEV lock or submit the same transition twice.
    if let Some(bh) = BH.get() {
        bh.schedule();
    }
}

#[derive(Debug)]
struct LidSwitch {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    capability: InputCapability,
}

impl LidSwitch {
    fn new() -> Self {
        let mut capability = InputCapability::new();
        capability.set_supported_event_type(EventTypes::SYN);
        capability.set_supported_switch(SwCode::Lid);
        Self {
            name: "spacemit-lid".to_string(),
            phys: "gpio-74/input0".to_string(),
            uniq: String::new(),
            id: InputId::new(InputId::BUS_HOST, 0x0000, 0x0000, 0x0001),
            capability,
        }
    }
}

impl InputDevice for LidSwitch {
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

    fn switch_state_bits(&self) -> [u8; 4] {
        [CLOSED.load(Ordering::Relaxed), 0, 0, 0]
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::sw_lid_from_level;

    #[ktest]
    fn gpio_low_is_lid_shut() {
        assert!(sw_lid_from_level(0));
        assert!(!sw_lid_from_level(1));
    }
}
