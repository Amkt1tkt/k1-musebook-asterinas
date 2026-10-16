// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SPM8821 power key → KEY_POWER (oneshot PLIC 64 + 200 ms poll).

//! SPM8821 power key — TWSI8 @ 0x41, Linux `spacemit-pwrkey` + `spm8821.h`.
//!
//! Status/enable live in the PMIC event block:
//! - status base `0x91`, enable base `0x98`, seven registers
//! - PWRON is offset 6 → status `0x97`, enable `0x9e`
//! - bit0 `PWRON_RINTR` = release, bit1 `PWRON_FINTR` = press
//! - ack: write the latched status bits back to `0x97`
//!
//! The PMIC's combined line is PLIC 64. A 200 ms status poll always runs as
//! well. Both paths feed one [`Taskless`] bottom half. Oneshot: mask the
//! PLIC line in the bottom half (after ostd writes completion), drain/ack,
//! then re-enable. Bottom halves must use [`crate::pmic_try_lock`]: blocking
//! could wait on a PMIC transfer held by the interrupted task.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use aster_input::{
    event_type_codes::{EventTypes, KeyCode, KeyStatus, SynEvent},
    input_dev::{InputCapability, InputDevice, InputEvent, InputId, RegisteredInputDevice},
};
use aster_softirq::Taskless;
use ostd::{
    arch::{
        boot::DEVICE_TREE,
        irq::{IRQ_CHIP, InterruptSourceInFdt, MappedIrqLine},
    },
    irq::IrqLine,
};
use spin::{Mutex, Once};

use crate::{pmic_read, pmic_write};

const PMIC_PLIC_IRQ: u32 = 64;
const STATUS_BASE: u8 = 0x91;
const ENABLE_BASE: u8 = 0x98;
const NSTATUS: u8 = 7;
const PWRON_OFF: u8 = 6;
const PWRON_ENABLE: u8 = ENABLE_BASE + PWRON_OFF;

/// `SPM8821_E_PWRON_RINTR_MSK` — release.
const PWRON_RISE: u8 = 1 << 0;
/// `SPM8821_E_PWRON_FINTR_MSK` — press.
const PWRON_FALL: u8 = 1 << 1;

/// `PWR_STS0`. Live power-path status, not latched. Printed on change
/// at debug log level.
const PWR_STS0: u8 = 0x7f;

const POLL_EVERY_TICKS: u32 = 200;
/// Line-triggered drains that found nothing latched before the line is
/// left masked permanently (poll only).
const EMPTY_IRQ_LIMIT: u32 = 16;

/// `(pressed, released)` edges latched in status `0x97`.
pub(crate) fn pwron_edges(status: u8) -> (bool, bool) {
    (status & PWRON_FALL != 0, status & PWRON_RISE != 0)
}

static DEV: Mutex<Option<RegisteredInputDevice>> = Mutex::new(None);
static PRESSED: AtomicBool = AtomicBool::new(false);
static TICK: AtomicU32 = AtomicU32::new(0);
static IRQ_LINE: Once<MappedIrqLine> = Once::new();
static BH: Once<Arc<Taskless>> = Once::new();
/// Last `PWR_STS0` seen by the poll; `0x100` = never read.
static LAST_STS0: AtomicU32 = AtomicU32::new(0x100);
/// Set by the top half; the next drain takes it, masks the line and treats
/// itself as line-triggered.
static IRQ_ARRIVED: AtomicBool = AtomicBool::new(false);
/// PLIC 64 is masked at the chip, waiting for a drain that acks something.
static IRQ_MASKED: AtomicBool = AtomicBool::new(false);
/// Line-triggered drains that found every status register zero.
static EMPTY_IRQS: AtomicU32 = AtomicU32::new(0);
/// The line is masked for good: too many empties. Poll only.
static IRQ_GIVEN_UP: AtomicBool = AtomicBool::new(false);

pub(super) fn probe() {
    if pmic_read(0x00).is_none() {
        return;
    }

    ack_all_status();
    let before = match pmic_read(PWRON_ENABLE) {
        Some(cur) => {
            if !pmic_write(PWRON_ENABLE, cur | PWRON_RISE | PWRON_FALL) {
                log::warn!("[k1-pmic] failed to unmask PWRON at {:#04x}", PWRON_ENABLE);
            }
            cur
        }
        None => {
            log::warn!("[k1-pmic] cannot read enable {:#04x}", PWRON_ENABLE);
            return;
        }
    };

    let device = Arc::new(PmicPwrkey::new());
    *DEV.lock() = Some(aster_input::register_device(device));
    BH.call_once(|| Taskless::new(handle_pmic_events));

    let after = pmic_read(PWRON_ENABLE).unwrap_or(0xff);
    log::info!(
        "[k1-pmic] PWRON enable {:#04x}: {:#04x} → {:#04x}",
        PWRON_ENABLE,
        before,
        after
    );

    if install_plic().is_err() {
        log::warn!(
            "[k1-pmic] PLIC {} unavailable; 200ms status poll",
            PMIC_PLIC_IRQ
        );
    } else {
        log::info!("[k1-pmic] PWRON on PLIC {} + 200ms poll", PMIC_PLIC_IRQ);
    }
    ostd::timer::register_callback_on_cpu(pwrkey_timer_poll);
}

fn install_plic() -> Result<(), ()> {
    let Some(interrupt) = lookup_pmic_interrupt() else {
        return Err(());
    };
    let Ok(line) = IrqLine::alloc() else {
        return Err(());
    };
    let Some(chip) = IRQ_CHIP.get() else {
        return Err(());
    };
    let mut mapped = chip.map_fdt_pin_to(interrupt, line).map_err(|err| {
        log::warn!(
            "[k1-pmic] could not map PLIC {} on phandle {:#x}: {:?}",
            interrupt.arguments[0],
            interrupt.interrupt_parent,
            err
        );
    })?;
    mapped.on_active(move |_trap| {
        // Nothing but bookkeeping here. The mask happens in the bottom half,
        // after ostd has written the PLIC completion — see
        // `MappedIrqLine::set_enabled` for why it cannot happen earlier.
        IRQ_ARRIVED.store(true, Ordering::Release);
        if let Some(bh) = BH.get() {
            bh.schedule();
        }
    });
    IRQ_LINE.call_once(|| mapped);
    Ok(())
}

fn lookup_pmic_interrupt() -> Option<InterruptSourceInFdt> {
    let dt = DEVICE_TREE.get()?;
    if let Some(node) = dt.find_compatible(&["spacemit,spm8821"])
        && let (Some(interrupt), Some(parent)) = (
            node.interrupts().and_then(|mut pins| pins.next()),
            node.property("interrupt-parent")
                .and_then(|parent| parent.as_usize()),
        )
    {
        return Some(InterruptSourceInFdt {
            arguments: [interrupt as u32],
            interrupt_parent: parent as u32,
        });
    }
    let plic = dt.find_compatible(&["sifive,plic-1.0.0", "riscv,plic0"])?;
    let parent = plic.property("phandle")?.as_usize()? as u32;
    Some(InterruptSourceInFdt {
        arguments: [PMIC_PLIC_IRQ],
        interrupt_parent: parent,
    })
}

/// Probe-time ack, thread context: blocking reads are fine here.
fn ack_all_status() {
    for i in 0..NSTATUS {
        let reg = STATUS_BASE + i;
        if let Some(v) = pmic_read(reg)
            && v != 0
        {
            let _ = pmic_write(reg, v);
        }
    }
}

/// Bottom half for both PLIC 64 and the poll. Softirq context: it may do
/// I2C and submit evdev events, but it must **never block on the PMIC
/// lock**.
fn handle_pmic_events() {
    if IRQ_ARRIVED.swap(false, Ordering::AcqRel) && !IRQ_GIVEN_UP.load(Ordering::Relaxed) {
        // Oneshot: the PMIC INT pin is a level, and only an I2C ack can
        // lower it. The completion has been written by now, so masking is
        // safe; `rearm_irq` unmasks once something was acked.
        if let Some(line) = IRQ_LINE.get() {
            line.set_enabled(false);
        }
        IRQ_MASKED.store(true, Ordering::Release);
    }
    let from_irq = IRQ_MASKED.load(Ordering::Acquire);

    let Some(pmic) = crate::pmic_try_lock() else {
        return;
    };

    let mut st = [0u8; NSTATUS as usize];
    let mut read_ok = true;
    for (i, slot) in st.iter_mut().enumerate() {
        match pmic.read(STATUS_BASE + i as u8) {
            Some(v) => *slot = v,
            None => read_ok = false,
        }
    }
    let status = st[PWRON_OFF as usize];
    let any = st.iter().any(|&v| v != 0);
    let sts0 = pmic.read(PWR_STS0);

    if status != 0 {
        log::debug!("[k1-pmic] 0x97={:#04x}", status);
    } else if log::log_enabled!(log::Level::Debug) {
        let cur = sts0.map_or(0x100, u32::from);
        let prev = LAST_STS0.swap(cur, Ordering::Relaxed);
        if prev != cur {
            log::debug!("[k1-pmic] PWR_STS0 {:#04x} → {:#04x}", prev, cur);
        }
    }

    // W1C ack of everything latched, whichever register it is in.
    let mut ack_ok = true;
    let mut pwron_ack_ok = false;
    for (i, &v) in st.iter().enumerate() {
        if v != 0 {
            let acked = pmic.write(STATUS_BASE + i as u8, v);
            ack_ok &= acked;
            if i == PWRON_OFF as usize {
                pwron_ack_ok = acked;
            }
        }
    }

    if from_irq {
        rearm_irq(any && read_ok && ack_ok);
    }
    drop(pmic);
    if pwron_ack_ok {
        apply_pwron_status(status);
    }
}

/// Decide what to do with the masked PLIC line after a drain it caused.
/// Acked something → unmask now. Acked nothing → leave it masked so the
/// poll drives the next drain; after [`EMPTY_IRQ_LIMIT`] of those give
/// the line up entirely.
fn rearm_irq(acked_something: bool) {
    if IRQ_GIVEN_UP.load(Ordering::Relaxed) {
        return;
    }
    if acked_something {
        EMPTY_IRQS.store(0, Ordering::Relaxed);
        IRQ_MASKED.store(false, Ordering::Release);
        if let Some(line) = IRQ_LINE.get() {
            line.set_enabled(true);
        }
        return;
    }
    let empties = EMPTY_IRQS.fetch_add(1, Ordering::Relaxed) + 1;
    if empties >= EMPTY_IRQ_LIMIT {
        IRQ_GIVEN_UP.store(true, Ordering::Relaxed);
        log::warn!(
            "[k1-pmic] PLIC {} asserted {} times with nothing latched in 0x91..0x97; \
             leaving it masked, 200ms poll only",
            PMIC_PLIC_IRQ,
            empties
        );
    }
}

/// Shared by the PLIC bottom half and the 200 ms poll.
fn apply_pwron_status(status: u8) {
    let (fall, rise) = pwron_edges(status);
    if !fall && !rise {
        return;
    }

    let mut events = Vec::new();
    if fall && !PRESSED.load(Ordering::Relaxed) {
        events.push(InputEvent::from_key_and_status(
            KeyCode::Power,
            KeyStatus::Pressed,
        ));
        PRESSED.store(true, Ordering::Relaxed);
    }
    if rise && PRESSED.load(Ordering::Relaxed) {
        events.push(InputEvent::from_key_and_status(
            KeyCode::Power,
            KeyStatus::Released,
        ));
        PRESSED.store(false, Ordering::Relaxed);
    }
    if events.is_empty() {
        return;
    }
    events.push(InputEvent::from_sync_event(SynEvent::Report));
    if let Some(dev) = DEV.lock().as_ref() {
        dev.submit_events(&events);
    }
}

fn pwrkey_timer_poll() {
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    if !tick.is_multiple_of(POLL_EVERY_TICKS) {
        return;
    }
    if let Some(bh) = BH.get() {
        bh.schedule();
    }
}

#[derive(Debug)]
struct PmicPwrkey {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    capability: InputCapability,
}

impl PmicPwrkey {
    fn new() -> Self {
        let mut capability = InputCapability::new();
        capability.set_supported_event_type(EventTypes::SYN);
        capability.set_supported_key(KeyCode::Power);
        Self {
            name: "spm8821-pwrkey".to_string(),
            phys: "i2c-twsi8-0x41/input0".to_string(),
            uniq: String::new(),
            id: InputId::new(InputId::BUS_I2C, 0x0000, 0x8821, 0x0001),
            capability,
        }
    }
}

impl InputDevice for PmicPwrkey {
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

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::{PWRON_FALL, PWRON_RISE, pwron_edges};

    #[ktest]
    fn pwron_status_0x97_bit1_is_press_and_bit0_is_release() {
        assert_eq!(pwron_edges(0), (false, false));
        assert_eq!(pwron_edges(PWRON_FALL), (true, false));
        assert_eq!(pwron_edges(PWRON_RISE), (false, true));
        assert_eq!(pwron_edges(PWRON_FALL | PWRON_RISE), (true, true));
        assert_eq!(pwron_edges(0xfc), (false, false));
    }
}
