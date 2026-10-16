// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: I2C-HID touchpad (MT protocol B + GPIO50 IRQ, k1.touchpad=rel fallback).

//! Muse Book touchpad — HID over I2C (Linux `hid-over-i2c`).
//!
//! DT (`k1-x_MUSE-Book.dts` `hid@2d`):
//! - i2c4 @ 0x2d, `hid-descr-addr = <0x0020>`
//! - interrupt GPIO50 falling (`interrupts = <50 2>`), INT is active-low
//!
//! Report layout for 0911:5288: report-id 1, five 5-byte contacts,
//! contact count at payload[26], clickpad button at payload[27] bit0.
//! Default path is Linux MT protocol B (`ABS_MT_*`) plus single-touch
//! `ABS_X`/`ABS_Y`. `k1.touchpad=rel` keeps the old relative-mouse path.
//!
//! Each contact uses the same [`CONTACT_LEN`] layout.

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use aster_input::{
    event_type_codes::{
        AbsCode, AbsInfo, EventTypes, InputProp, KeyCode, KeyStatus, RelCode, SynEvent,
    },
    input_dev::{InputCapability, InputDevice, InputEvent, InputId, RegisteredInputDevice},
};
use aster_softirq::Taskless;
use ostd::arch::boot::DEVICE_TREE;
use spin::{Mutex, Once};

use super::k1_twsi::{self, TwsiBus};

const TP_ADDR: u8 = 0x2d;
const HID_DESC_REG: u16 = 0x0020;
const GPIO_TP_IRQ: u32 = 50;

const I2C_HID_PWR_ON: u8 = 0x00;
const I2C_HID_OPCODE_RESET: u8 = 0x01;
const I2C_HID_OPCODE_SET_POWER: u8 = 0x08;

/// Timer ticks between polls while INT is asserted (`TIMER_FREQ` is 1000 Hz).
const POLL_EVERY_TICKS: u32 = 20;
/// Ticks after probe at which a never-asserted INT line reports itself and
/// the driver stops gating reads on it.
const GPIO_SILENT_WARN_TICKS: u32 = 5000;
/// Largest `|dx|` / `|dy|` accepted between two REL reports.
///
/// This is a contact-switch / torn-report guard, not a physical speed limit:
/// the logical range is not in the board-proven 8-byte layout.
const MAX_STEP: i32 = 200;
/// Ticks (1 kHz) after which a stored contact position is stale.
///
/// GPIO-gated polling can miss the final report with the tip bit clear.
/// Expire the stored position after five poll intervals so a new contact
/// cannot produce a delta from the previous contact. A stationary contact
/// that resumes after expiry loses one movement sample.
const CONTACT_STALE_TICKS: u32 = POLL_EVERY_TICKS * 5;
/// Bytes of report descriptor read at probe for the X/Y ranges.
///
/// Covers the supported touchpad's 710-byte descriptor.
const REPORT_DESC_DUMP_CAP: usize = 1024;
/// Reports drained in one bottom-half while INT stays low.
const IRQ_DRAIN_CAP: usize = 8;

/// Bytes per contact record: `[tip|confidence|id<<2][x_lo][x_hi][y_lo][y_hi]`.
const CONTACT_LEN: usize = 5;
/// Contacts the report carries, i.e. where the trailing fields start.
const CONTACT_SLOTS: usize = 5;
/// Offset in `payload` of the contact-count byte.
const CONTACT_COUNT_OFF: usize = 1 + CONTACT_SLOTS * CONTACT_LEN;
/// Offset in `payload` of the clickpad button byte, bit 0 = primary.
const BUTTON_OFF: usize = CONTACT_COUNT_OFF + 1;

/// Axis max when the HID report descriptor has no Generic Desktop X/Y
/// Logical Maximum. 4095 is the usual 12-bit HID digitizer ceiling.
const FALLBACK_ABS_MAX: i32 = 4095;

struct HidDesc {
    w_hid_desc_length: u16,
    bcd_version: u16,
    w_report_desc_length: u16,
    w_report_desc_register: u16,
    #[expect(dead_code)]
    w_input_register: u16,
    w_max_input_length: u16,
    #[expect(dead_code)]
    w_output_register: u16,
    #[expect(dead_code)]
    w_max_output_length: u16,
    w_command_register: u16,
    #[expect(dead_code)]
    w_data_register: u16,
    w_vendor_id: u16,
    w_product_id: u16,
    #[expect(dead_code)]
    w_version_id: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TouchpadMode {
    Abs,
    Rel,
}

impl FromStr for TouchpadMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rel" | "REL" => Ok(Self::Rel),
            "abs" | "ABS" => Ok(Self::Abs),
            _ => Err(()),
        }
    }
}

static TOUCHPAD_MODE: Once<TouchpadMode> = Once::new();
aster_cmdline::define_kv_param!("k1.touchpad", TOUCHPAD_MODE);

/// One HID contact record. `tip` is already folded with the report-level
/// contact count (count 0 means every slot is up).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Contact {
    id: u8,
    x: i32,
    y: i32,
}

struct TouchpadState {
    max_input: u16,
    registered: RegisteredInputDevice,
    mode: TouchpadMode,
    contact: ContactTracker,
    mt: MtTracker,
    last_left: bool,
    last_right: bool,
    unknown_id_logged: bool,
}

/// Tracks one absolute contact so consecutive reports can become evdev `REL_*`.
///
/// HID digitizer / precision-touchpad Y grows downward from a top-left origin,
/// which is already the evdev `REL_Y` sign (positive is down). PS/2 mice are
/// the opposite: the wire reports Y-up as positive, so `i8042` `mouse.rs`
/// negates before submitting.
struct ContactTracker {
    have_pos: bool,
    last_x: i32,
    last_y: i32,
    last_tip: bool,
    /// Tick of the last sample, for the [`CONTACT_STALE_TICKS`] rule.
    last_tick: u32,
}

/// Linux MT protocol B slot state.
/// What a slot uses to recognise "the same finger" in the next report.
///
/// Use the contact ID when reports provide distinct IDs; it survives
/// repacking when a finger lifts. Reports with all IDs zero fall back to
/// report position because no stable hardware identity is available.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContactKey {
    Hw(u8),
    Index(usize),
}

struct MtSlot {
    down: bool,
    /// What this slot is following.
    key: ContactKey,
    x: i32,
    y: i32,
}

struct MtTracker {
    slots: [MtSlot; CONTACT_SLOTS],
    /// Next positive ABS_MT_TRACKING_ID, wrapping after i32::MAX.
    next_tracking_id: i32,
    last_tick: u32,
    have_tick: bool,
    last_touch: bool,
    last_tool: u8,
}

/// `(left, right)` for one input report.
fn parse_buttons(payload: &[u8]) -> (bool, bool) {
    let Some(buttons) = payload.get(BUTTON_OFF).copied() else {
        return (false, false);
    };
    (buttons & 0x01 != 0, buttons & 0x02 != 0)
}

/// Parses every contact slot.
///
/// A contact count of zero is a lift even when contact 0's tip bit is
/// still set, which is the cheaper of the two lift signals to trust: it
/// is one byte and it cannot be stale within a report. It does not
/// replace the staleness rule, which covers the lift report that was
/// never read at all.
fn parse_contacts(payload: &[u8]) -> (u8, [Option<Contact>; CONTACT_SLOTS]) {
    let count = payload.get(CONTACT_COUNT_OFF).copied().unwrap_or(0);
    let mut slots = [None; CONTACT_SLOTS];
    if count == 0 {
        return (0, slots);
    }
    for (i, slot) in slots.iter_mut().enumerate() {
        let off = 1 + i * CONTACT_LEN;
        let Some(flags) = payload.get(off).copied() else {
            break;
        };
        let Some(xy) = payload.get(off + 1..off + 5) else {
            break;
        };
        // The report descriptor defines Confidence(1), Tip Switch(1),
        // padding(2), and Contact Id(4). Confidence may remain set during
        // lift; only Tip Switch identifies a finger that is still down.
        let tip = flags & 0x02 != 0;
        if (i as u8) < count && tip {
            *slot = Some(Contact {
                id: flags >> 4,
                x: u16::from_le_bytes([xy[0], xy[1]]) as i32,
                y: u16::from_le_bytes([xy[2], xy[3]]) as i32,
            });
        }
    }
    (count, slots)
}

/// HID short-item signed payload.
fn hid_item_s32(bytes: &[u8]) -> i32 {
    match bytes.len() {
        0 => 0,
        1 => bytes[0] as i8 as i32,
        2 => i16::from_le_bytes([bytes[0], bytes[1]]) as i32,
        _ => {
            let mut tmp = [0u8; 4];
            tmp[..bytes.len().min(4)].copy_from_slice(&bytes[..bytes.len().min(4)]);
            i32::from_le_bytes(tmp)
        }
    }
}

fn hid_item_u32(bytes: &[u8]) -> u32 {
    let mut tmp = [0u8; 4];
    tmp[..bytes.len().min(4)].copy_from_slice(&bytes[..bytes.len().min(4)]);
    u32::from_le_bytes(tmp)
}

/// Walks a HID report descriptor for Generic Desktop X/Y Logical Min/Max.
///
/// Returns `None` if the descriptor has no GD Usage X+Y Input items with a
/// usable range. The fallback is [`FALLBACK_ABS_MAX`], not a measured max.
///
/// Physical Maximum, Unit and Unit Exponent determine units per millimetre
/// for userspace pointer acceleration, scrolling and gesture thresholds.
fn parse_hid_xy_range(desc: &[u8]) -> Option<HidXy> {
    let mut i = 0;
    let mut page = 0u32;
    let mut lmin = 0i32;
    let mut lmax = 0i32;
    let mut usage = 0u32;
    let mut pmax = 0i32;
    let mut unit = 0u32;
    let mut unit_exp = 0i32;
    let mut x = None;
    let mut y = None;

    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xfe {
            if i + 2 >= desc.len() {
                break;
            }
            let size = desc[i + 1] as usize;
            i = i.saturating_add(3).saturating_add(size);
            continue;
        }
        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let ty = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;
        if i + 1 + size > desc.len() {
            break;
        }
        let data = &desc[i + 1..i + 1 + size];
        i += 1 + size;

        match (ty, tag) {
            (1, 0) => page = hid_item_u32(data),
            (1, 1) => lmin = hid_item_s32(data),
            (1, 2) => {
                lmax = if lmin < 0 {
                    hid_item_s32(data)
                } else {
                    i32::try_from(hid_item_u32(data)).unwrap_or(0)
                }
            }
            // Physical Maximum (0x4), Unit Exponent (0x5), Unit (0x6). Global
            // items, so they persist across the repeated Finger collections
            // exactly as the descriptor intends.
            (1, 4) => pmax = i32::try_from(hid_item_u32(data)).unwrap_or(0),
            (1, 5) => unit_exp = hid_unit_exponent(hid_item_u32(data)),
            (1, 6) => unit = hid_item_u32(data),
            (2, 0) => usage = hid_item_u32(data),
            (0, 8) => {
                // Input. Local usage state is consumed here.
                if page == 0x01 {
                    let res = hid_resolution(lmax, pmax, unit, unit_exp);
                    if usage == 0x30 {
                        x = Some((lmin, lmax, res));
                    } else if usage == 0x31 {
                        y = Some((lmin, lmax, res));
                    }
                }
                usage = 0;
            }
            (0, 10) | (0, 12) => {
                // Collection / End Collection — drop local state.
                usage = 0;
            }
            _ => {}
        }
    }

    match (x, y) {
        (Some((xmin, xmax, xres)), Some((ymin, ymax, yres))) if xmax > xmin && ymax > ymin => {
            Some(HidXy {
                xmin,
                xmax,
                xres,
                ymin,
                ymax,
                yres,
            })
        }
        _ => None,
    }
}

/// X/Y ranges from the report descriptor, with resolution in units per mm
/// (0 when the descriptor does not say).
#[derive(Clone, Copy, Debug)]
struct HidXy {
    xmin: i32,
    xmax: i32,
    xres: i32,
    ymin: i32,
    ymax: i32,
    yres: i32,
}

/// HID Unit Exponent is a 4-bit two's-complement nibble: 0x0e is -2.
fn hid_unit_exponent(raw: u32) -> i32 {
    let nibble = (raw & 0xf) as i32;
    if nibble > 7 { nibble - 16 } else { nibble }
}

/// Units per millimetre from Logical Maximum, Physical Maximum and the unit.
///
/// Only SI-linear length is accepted: Unit nibble 0 selects the system (1 =
/// SI Linear) and nibble 1 is the length exponent, so 0x11 means centimetres.
/// Anything else returns 0, which evdev reads as "no resolution given" --
/// better than a number derived from a unit we did not understand.
fn hid_resolution(lmax: i32, pmax: i32, unit: u32, unit_exp: i32) -> i32 {
    if lmax <= 0 || pmax <= 0 || unit & 0xff != 0x11 {
        return 0;
    }
    // Physical is `pmax * 10^unit_exp` cm, so millimetres are
    // `pmax * 10^(unit_exp + 1)`. Only negative exponents occur in practice
    // here (-2 on this pad); a positive one would make the pad metres wide.
    let shift = unit_exp + 1;
    if !(-6..=0).contains(&shift) {
        return 0;
    }
    let divisor = 10i64.pow((-shift) as u32);
    let mm = pmax as i64 * 1000 / divisor / 1000;
    if mm <= 0 {
        return 0;
    }
    // Round to nearest: 1626 units over 118 mm is 13.78, and 14 is closer to
    // the truth than truncating to 13.
    ((lmax as i64 * 2 / mm + 1) / 2) as i32
}

/// A contact id of 0 means this pad does not number its contacts, so the
/// report position is the only identity available. See [`ContactKey`].
fn contact_key(index: usize, contact: &Contact) -> ContactKey {
    if contact.id == 0 {
        ContactKey::Index(index)
    } else {
        ContactKey::Hw(contact.id)
    }
}

fn tool_for_count(n: u8) -> Option<KeyCode> {
    match n {
        0 => None,
        1 => Some(KeyCode::BtnToolFinger),
        2 => Some(KeyCode::BtnToolDoubleTap),
        3 => Some(KeyCode::BtnToolTripleTap),
        4 => Some(KeyCode::BtnToolQuadTap),
        _ => Some(KeyCode::BtnToolQuintTap),
    }
}

fn key_event(code: KeyCode, down: bool) -> InputEvent {
    InputEvent::from_key_and_status(
        code,
        if down {
            KeyStatus::Pressed
        } else {
            KeyStatus::Released
        },
    )
}

impl ContactTracker {
    const fn new() -> Self {
        Self {
            have_pos: false,
            last_x: 0,
            last_y: 0,
            last_tip: false,
            last_tick: 0,
        }
    }

    /// Ingests one absolute sample taken at `tick`.
    ///
    /// Returns a relative `(dx, dy)` only for a contact that is *continuous*
    /// with the previous sample. Three things break continuity, and all three
    /// have to, because none of them is reliable alone:
    ///
    ///  - a report with the tip bit clear, i.e. a lift the driver saw;
    ///  - a report saying no contacts, which the caller folds into `tip`;
    ///  - more than [`CONTACT_STALE_TICKS`] since the last sample, which is
    ///    the only one that catches a lift whose report was never read.
    fn ingest(&mut self, x: i32, y: i32, tip: bool, tick: u32) -> Option<(i32, i32)> {
        let fresh = tick.wrapping_sub(self.last_tick) > CONTACT_STALE_TICKS;
        let delta = if tip && self.have_pos && self.last_tip && !fresh {
            relative_delta(self.last_x, self.last_y, x, y)
        } else {
            None
        };
        if tip {
            self.last_x = x;
            self.last_y = y;
            self.have_pos = true;
        } else {
            self.have_pos = false;
        }
        self.last_tip = tip;
        self.last_tick = tick;
        delta
    }
}

impl Default for MtSlot {
    fn default() -> Self {
        Self {
            down: false,
            key: ContactKey::Index(usize::MAX),
            x: 0,
            y: 0,
        }
    }
}

impl MtTracker {
    fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| MtSlot::default()),
            next_tracking_id: 1,
            last_tick: 0,
            have_tick: false,
            last_touch: false,
            last_tool: 0,
        }
    }

    fn lift_slot(&mut self, slot: usize, events: &mut Vec<InputEvent>) {
        if !self.slots[slot].down {
            return;
        }
        events.push(InputEvent::from_absolute(AbsCode::MtSlot, slot as i32));
        events.push(InputEvent::from_absolute(AbsCode::MtTrackingId, -1));
        self.slots[slot] = MtSlot::default();
    }

    fn lift_all(&mut self, events: &mut Vec<InputEvent>) {
        for slot in 0..CONTACT_SLOTS {
            self.lift_slot(slot, events);
        }
    }

    fn emit_tools(&mut self, n_down: u8, events: &mut Vec<InputEvent>) {
        let touch = n_down > 0;
        if touch != self.last_touch {
            events.push(key_event(KeyCode::BtnTouch, touch));
            self.last_touch = touch;
        }
        if n_down != self.last_tool {
            if let Some(old) = tool_for_count(self.last_tool) {
                events.push(key_event(old, false));
            }
            if let Some(new) = tool_for_count(n_down) {
                events.push(key_event(new, true));
            }
            self.last_tool = n_down;
        }
    }

    /// Applies one parsed report. Stale-before-apply lifts every slot so a
    /// missed lift cannot leak a tracking id into the next touch.
    fn ingest(
        &mut self,
        contacts: [Option<Contact>; CONTACT_SLOTS],
        count: u8,
        tick: u32,
    ) -> Vec<InputEvent> {
        let mut events = Vec::new();
        let stale = self.have_tick && tick.wrapping_sub(self.last_tick) > CONTACT_STALE_TICKS;
        if stale || count == 0 {
            self.lift_all(&mut events);
            if count == 0 {
                self.emit_tools(0, &mut events);
                self.last_tick = tick;
                self.have_tick = true;
                return events;
            }
        }

        // Slots follow the *contact id*, not the position in the report.
        //
        // A Precision touchpad repacks: lift finger A of an A,B pair and the
        // next report carries B at report index 0. Treating the index as the
        // slot hands B's coordinates to the slot that was following A, and
        // userspace interprets that as a discontinuous position change.
        // Track by contact ID to preserve each finger's slot across repacking.
        let mut n_down = 0u8;
        let mut first_xy = None;

        // Lift anything whose key is no longer in the report. Done first so a
        // slot freed here can be reused below in the same packet.
        for slot in 0..CONTACT_SLOTS {
            if !self.slots[slot].down {
                continue;
            }
            let key = self.slots[slot].key;
            let still_there = contacts
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.as_ref().map(|c| contact_key(i, c)))
                .any(|k| k == key);
            if !still_there {
                self.lift_slot(slot, &mut events);
            }
        }

        for (index, contact) in contacts
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().map(|c| (i, c)))
        {
            let key = contact_key(index, contact);
            n_down = n_down.saturating_add(1);
            if first_xy.is_none() {
                first_xy = Some((contact.x, contact.y));
            }

            if let Some(slot) =
                (0..CONTACT_SLOTS).find(|&s| self.slots[s].down && self.slots[s].key == key)
            {
                if self.slots[slot].x != contact.x || self.slots[slot].y != contact.y {
                    events.push(InputEvent::from_absolute(AbsCode::MtSlot, slot as i32));
                    events.push(InputEvent::from_absolute(AbsCode::MtPositionX, contact.x));
                    events.push(InputEvent::from_absolute(AbsCode::MtPositionY, contact.y));
                    self.slots[slot].x = contact.x;
                    self.slots[slot].y = contact.y;
                }
                continue;
            }

            let Some(slot) = (0..CONTACT_SLOTS).find(|&s| !self.slots[s].down) else {
                // More simultaneous contacts than slots. Dropping the extra is
                // better than evicting a finger userspace is already tracking.
                continue;
            };
            let tid = self.next_tracking_id;
            self.next_tracking_id = self.next_tracking_id.wrapping_add(1).max(1);
            events.push(InputEvent::from_absolute(AbsCode::MtSlot, slot as i32));
            events.push(InputEvent::from_absolute(AbsCode::MtTrackingId, tid));
            events.push(InputEvent::from_absolute(AbsCode::MtPositionX, contact.x));
            events.push(InputEvent::from_absolute(AbsCode::MtPositionY, contact.y));
            self.slots[slot] = MtSlot {
                down: true,
                key,
                x: contact.x,
                y: contact.y,
            };
        }

        if let Some((x, y)) = first_xy {
            events.push(InputEvent::from_absolute(AbsCode::X, x));
            events.push(InputEvent::from_absolute(AbsCode::Y, y));
        }
        self.emit_tools(n_down, &mut events);
        self.last_tick = tick;
        self.have_tick = true;
        events
    }
}

/// Converts two absolute samples into an evdev relative step.
///
/// Acceleration belongs in userspace (`libinput`); this function does not
/// apply gain. Y is not inverted: `y - last_y` already has the evdev sign.
fn relative_delta(last_x: i32, last_y: i32, x: i32, y: i32) -> Option<(i32, i32)> {
    let dx = x.saturating_sub(last_x);
    let dy = y.saturating_sub(last_y);
    if dx == 0 && dy == 0 {
        return None;
    }
    if dx.unsigned_abs() >= MAX_STEP as u32 || dy.unsigned_abs() >= MAX_STEP as u32 {
        return None;
    }
    Some((dx, dy))
}

fn touchpad_is_rel() -> bool {
    matches!(TOUCHPAD_MODE.get(), Some(TouchpadMode::Rel))
        || ostd::boot::boot_info()
            .kernel_cmdline
            .split_whitespace()
            .any(|tok| tok.eq_ignore_ascii_case("k1.touchpad=rel"))
}

static TP: Mutex<Option<TouchpadState>> = Mutex::new(None);
static TP_TICK: AtomicU32 = AtomicU32::new(0);
/// Whether the INT line has ever read low, i.e. whether it is a usable gate.
///
/// A pin whose MFPR mux is not in GPIO mode, or a node naming the wrong
/// line, reads a constant. Gating reads on such a pin makes the pad
/// silent, so [`should_read`] stops gating once this has stayed false for
/// [`GPIO_SILENT_WARN_TICKS`].
static TP_IRQ_SEEN: AtomicBool = AtomicBool::new(false);
static TP_BH: Once<Arc<Taskless>> = Once::new();
static TP_IRQ_GPIO: AtomicU32 = AtomicU32::new(GPIO_TP_IRQ);

pub(super) fn probe_touchpad(bus: &TwsiBus) {
    log::info!(
        "[k1-tp] probe on {} addr=0x{:02x} descr@{:#06x}",
        bus.name(),
        TP_ADDR,
        HID_DESC_REG
    );

    k1_twsi::delay_ms(200);

    match bus.read_all(TP_ADDR, &mut [0u8; 1]) {
        Ok(()) => log::info!("[k1-tp] device ACK @0x{:02x}", TP_ADDR),
        Err(e) => {
            log::info!("[k1-tp] no ACK @0x{:02x}: {}", TP_ADDR, e);
            return;
        }
    }

    let mut raw = [0u8; 30];
    let reg = HID_DESC_REG.to_le_bytes();
    if let Err(e) = bus.write_then_read(TP_ADDR, &reg, &mut raw) {
        log::warn!("[k1-tp] fetch HID descriptor failed: {}", e);
        return;
    }

    let desc = parse_hid_desc(&raw);
    if desc.w_hid_desc_length != 30 || desc.bcd_version != 0x0100 {
        log::info!(
            "[k1-tp] unexpected HID descriptor len={} bcd={:#06x} — abort",
            desc.w_hid_desc_length,
            desc.bcd_version
        );
        return;
    }

    if let Err(e) = set_power(bus, &desc, I2C_HID_PWR_ON) {
        log::info!("[k1-tp] SET_POWER(ON) failed: {}", e);
        return;
    }
    k1_twsi::delay_ms(60);
    if let Err(e) = hid_reset(bus, &desc) {
        log::warn!("[k1-tp] RESET failed: {}", e);
    }
    k1_twsi::delay_ms(50);

    let desc_len = desc.w_report_desc_length as usize;
    let rd_len = desc_len.min(REPORT_DESC_DUMP_CAP);
    let mut xy = None;
    if rd_len > 0 {
        let mut rdesc = vec![0u8; rd_len];
        let rreg = desc.w_report_desc_register.to_le_bytes();
        match bus.write_then_read(TP_ADDR, &rreg, &mut rdesc) {
            Ok(()) => {
                xy = parse_hid_xy_range(&rdesc);
            }
            Err(e) => log::warn!("[k1-tp] reportDesc read failed: {}", e),
        }
    }

    let geom = xy.unwrap_or(HidXy {
        xmin: 0,
        xmax: FALLBACK_ABS_MAX,
        xres: 0,
        ymin: 0,
        ymax: FALLBACK_ABS_MAX,
        yres: 0,
    });
    if xy.is_none() {
        log::info!(
            "[k1-tp] no GD X/Y Logical Maximum in HID descriptor; \
             using 0..{} (12-bit ceiling, not a board-measured max)",
            FALLBACK_ABS_MAX
        );
    } else {
        log::info!(
            "[k1-tp] HID Logical X={}..{} Y={}..{} res={}/{} units per mm              (0 = descriptor gave no physical size)",
            geom.xmin,
            geom.xmax,
            geom.ymin,
            geom.ymax,
            geom.xres,
            geom.yres
        );
    }

    let mode = if touchpad_is_rel() {
        TouchpadMode::Rel
    } else {
        TouchpadMode::Abs
    };
    let max_in = desc.w_max_input_length.clamp(4, 64);
    let registered = register_touchpad(desc.w_vendor_id, desc.w_product_id, mode, geom);

    *TP.lock() = Some(TouchpadState {
        max_input: max_in,
        registered,
        mode,
        contact: ContactTracker::new(),
        mt: MtTracker::new(),
        last_left: false,
        last_right: false,
        unknown_id_logged: false,
    });

    let irq_gpio = touchpad_irq_gpio();
    TP_IRQ_GPIO.store(irq_gpio, Ordering::Relaxed);
    TP_BH.call_once(|| Taskless::new(drain_reports));

    // INT is active-low; report its mux to distinguish an asserted line
    // from a pad that was not configured as GPIO.
    if let Some(pad) = k1_musebook_pinctrl::read_pad(irq_gpio) {
        log::info!(
            "[k1-tp] GPIO{} MFPR={:#010x} mux[2:0]={}",
            irq_gpio,
            pad,
            pad & 0x7
        );
    }
    match k1_musebook_gpio::register_irq(
        irq_gpio,
        k1_musebook_gpio::Edge::Falling,
        touchpad_gpio_irq,
    ) {
        Ok(()) => log::info!(
            "[k1-tp] GPIO{} falling → Taskless, idle level {:?} (mode {:?})",
            irq_gpio,
            k1_musebook_gpio::get_value(irq_gpio),
            mode
        ),
        Err(err) => log::info!(
            "[k1-tp] GPIO{} IRQ unavailable ({:?}); timer poll only, idle level {:?} (mode {:?})",
            irq_gpio,
            err,
            k1_musebook_gpio::get_value(irq_gpio),
            mode
        ),
    }

    ostd::timer::register_callback_on_cpu(touchpad_timer_poll);
    log::info!(
        "[k1-tp] {:?} id={:#06x}:{:#06x} ready",
        mode,
        desc.w_vendor_id,
        desc.w_product_id
    );
}

/// First `interrupts` cell, big-endian, as the raw property carries it.
///
/// Deliberately not `FdtNode::interrupts()`. That helper switches on the
/// parent's `#interrupt-cells`, and for two cells `fdt` 0.1.5 reads the
/// pair as one `u64` (`src/node.rs:258`). The GPIO controller uses
/// `<pin flags>`, so narrowing that value to `u32` would select the flags
/// as the pin number. Decode the first cell directly.
fn first_interrupt_cell(bytes: &[u8]) -> Option<u32> {
    let cell = bytes.get(..4)?;
    Some(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
}

fn touchpad_irq_gpio() -> u32 {
    let Some(dt) = DEVICE_TREE.get() else {
        return GPIO_TP_IRQ;
    };
    if let Some(node) = dt.find_compatible(&["hid-over-i2c"])
        && let Some(prop) = node.property("interrupts")
        && let Some(pin) = first_interrupt_cell(prop.value)
    {
        return pin;
    }
    GPIO_TP_IRQ
}

fn touchpad_gpio_irq(_gpio: u32) {
    if let Some(bh) = TP_BH.get() {
        bh.schedule();
    }
}

fn register_touchpad(vid: u16, pid: u16, mode: TouchpadMode, geom: HidXy) -> RegisteredInputDevice {
    let device = Arc::new(I2cHidTouchpad::new(vid, pid, mode, geom));
    aster_input::register_device(device)
}

#[derive(Debug)]
struct I2cHidTouchpad {
    name: String,
    phys: String,
    uniq: String,
    id: InputId,
    capability: InputCapability,
}

impl I2cHidTouchpad {
    fn new(vid: u16, pid: u16, mode: TouchpadMode, geom: HidXy) -> Self {
        let mut capability = InputCapability::new();
        capability.set_supported_event_type(EventTypes::SYN);
        capability.set_supported_key(KeyCode::BtnLeft);
        // `BTN_RIGHT` only in the relative fallback, which presents as an
        // ordinary two-button mouse.
        //
        // INPUT_PROP_BUTTONPAD has one physical button. Userspace derives
        // secondary clicks from button areas or the contact count.
        if matches!(mode, TouchpadMode::Rel) {
            capability.set_supported_key(KeyCode::BtnRight);
        }
        match mode {
            TouchpadMode::Rel => {
                capability.set_supported_event_type(EventTypes::REL);
                capability.set_supported_relative_axis(RelCode::X);
                capability.set_supported_relative_axis(RelCode::Y);
            }
            TouchpadMode::Abs => {
                // Resolution matters as much as the range: it is how
                // userspace converts units to millimetres.
                let mut xy = AbsInfo::range(geom.xmin, geom.xmax);
                xy.resolution = geom.xres;
                let mut yy = AbsInfo::range(geom.ymin, geom.ymax);
                yy.resolution = geom.yres;
                capability.set_supported_absolute_axis(AbsCode::X, xy);
                capability.set_supported_absolute_axis(AbsCode::Y, yy);
                capability.set_supported_absolute_axis(
                    AbsCode::MtSlot,
                    AbsInfo::range(0, (CONTACT_SLOTS - 1) as i32),
                );
                capability.set_supported_absolute_axis(AbsCode::MtPositionX, xy);
                capability.set_supported_absolute_axis(AbsCode::MtPositionY, yy);
                // Tracking IDs span the generated non-negative i32 range; -1
                // remains the MT protocol's release sentinel outside this range.
                capability.set_supported_absolute_axis(
                    AbsCode::MtTrackingId,
                    AbsInfo::range(0, i32::MAX),
                );
                capability.set_supported_key(KeyCode::BtnTouch);
                capability.set_supported_key(KeyCode::BtnToolFinger);
                capability.set_supported_key(KeyCode::BtnToolDoubleTap);
                capability.set_supported_key(KeyCode::BtnToolTripleTap);
                capability.set_supported_key(KeyCode::BtnToolQuadTap);
                capability.set_supported_key(KeyCode::BtnToolQuintTap);
                capability.set_property(InputProp::Pointer);
                capability.set_property(InputProp::ButtonPad);
            }
        }
        Self {
            name: "i2c-hid-touchpad".to_string(),
            phys: "i2c-twsi4-0x2d/input0".to_string(),
            uniq: "".to_string(),
            id: InputId::new(InputId::BUS_I2C, vid, pid, 0x0001),
            capability,
        }
    }
}

impl InputDevice for I2cHidTouchpad {
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

fn parse_hid_desc(raw: &[u8; 30]) -> HidDesc {
    let u16_at = |o: usize| u16::from_le_bytes([raw[o], raw[o + 1]]);
    HidDesc {
        w_hid_desc_length: u16_at(0),
        bcd_version: u16_at(2),
        w_report_desc_length: u16_at(4),
        w_report_desc_register: u16_at(6),
        w_input_register: u16_at(8),
        w_max_input_length: u16_at(10),
        w_output_register: u16_at(12),
        w_max_output_length: u16_at(14),
        w_command_register: u16_at(16),
        w_data_register: u16_at(18),
        w_vendor_id: u16_at(20),
        w_product_id: u16_at(22),
        w_version_id: u16_at(24),
    }
}

fn set_power(bus: &TwsiBus, desc: &HidDesc, power: u8) -> Result<(), &'static str> {
    let mut msg = [0u8; 4];
    let cmd = desc.w_command_register.to_le_bytes();
    msg[0] = cmd[0];
    msg[1] = cmd[1];
    msg[2] = power;
    msg[3] = I2C_HID_OPCODE_SET_POWER;
    bus.write_all(TP_ADDR, &msg)
}

fn hid_reset(bus: &TwsiBus, desc: &HidDesc) -> Result<(), &'static str> {
    let mut msg = [0u8; 4];
    let cmd = desc.w_command_register.to_le_bytes();
    msg[0] = cmd[0];
    msg[1] = cmd[1];
    msg[2] = 0;
    msg[3] = I2C_HID_OPCODE_RESET;
    bus.write_all(TP_ADDR, &msg)
}

/// Whether this tick may read the input register.
///
/// HID-over-I2C defines a read only while INT is asserted: a read on an
/// idle line returns wLength 0 at best and races the device updating the
/// register at worst. So `Some(1)` normally means "skip".
///
/// The exception is a line that has never once read low. That is not a
/// gate, it is a constant — an unreadable pin (`None`) or a mux left off
/// GPIO mode — and honouring it would leave the pad permanently silent.
/// After [`GPIO_SILENT_WARN_TICKS`] the driver falls back to periodic reads.
fn should_read(level: Option<u32>, int_ever_low: bool, tick: u32) -> bool {
    match level {
        Some(0) => true,
        Some(_) => !int_ever_low && tick > GPIO_SILENT_WARN_TICKS,
        None => true,
    }
}

/// Timer path. Runs in both modes: the falling edge gives latency, this
/// gives a ceiling.
///
/// A missed edge can leave level-low INT asserted without another edge to
/// trigger the handler. Drain at [`POLL_EVERY_TICKS`] to bound latency and
/// avoid missing contacts shorter than a slow fallback interval.
fn touchpad_timer_poll() {
    let tick = TP_TICK.fetch_add(1, Ordering::Relaxed);
    if !tick.is_multiple_of(POLL_EVERY_TICKS) {
        return;
    }
    let gpio = TP_IRQ_GPIO.load(Ordering::Relaxed);
    let level = k1_musebook_gpio::get_value(gpio);
    if level == Some(0) {
        TP_IRQ_SEEN.store(true, Ordering::Relaxed);
    }
    let ever_low = TP_IRQ_SEEN.load(Ordering::Relaxed);
    if tick == GPIO_SILENT_WARN_TICKS && !ever_low {
        log::info!(
            "[k1-tp] GPIO{} never read low in {} ticks (reads {:?}); \
             polling blind every {} ms — check the pin's MFPR mux",
            gpio,
            GPIO_SILENT_WARN_TICKS,
            level,
            POLL_EVERY_TICKS
        );
    }
    if !should_read(level, ever_low, tick) {
        return;
    }
    // One bottom half is shared with the edge handler, so a drain already
    // queued is not queued twice, and the I2C transfer stays out of the
    // timer callback.
    if let Some(bh) = TP_BH.get() {
        bh.schedule();
    }
}

fn drain_reports() {
    let gpio = TP_IRQ_GPIO.load(Ordering::Relaxed);
    let ever_low = TP_IRQ_SEEN.load(Ordering::Relaxed);
    let tick = TP_TICK.load(Ordering::Relaxed);
    for _ in 0..IRQ_DRAIN_CAP {
        if !should_read(k1_musebook_gpio::get_value(gpio), ever_low, tick) {
            return;
        }
        if !read_one_report() {
            return;
        }
    }
}

/// Reads one i2c-hid input report. Returns `true` if a payload was parsed
/// (so a still-low INT is worth another pass).
fn read_one_report() -> bool {
    let Some(bus) = k1_twsi::twsi4() else {
        return false;
    };
    let tick = TP_TICK.load(Ordering::Relaxed);
    let mut guard = TP.lock();
    let Some(st) = guard.as_mut() else {
        return false;
    };

    let mut buf = [0u8; 64];
    let n = st.max_input as usize;
    if bus.read_all(TP_ADDR, &mut buf[..n]).is_err() {
        return false;
    }
    let sz = u16::from_le_bytes([buf[0], buf[1]]);
    // i2c-hid: wLength 0 or 2 means "no new report" / reset complete.
    if sz < 8 || sz as usize > n {
        return false;
    }

    let payload = &buf[2..sz as usize];
    if payload.len() <= BUTTON_OFF {
        return false;
    }

    if payload[0] != 0x01 {
        if !st.unknown_id_logged {
            st.unknown_id_logged = true;
            log::info!("[k1-tp] ignore report id={:#04x} sz={}", payload[0], sz);
        }
        return true;
    }

    let (count, contacts) = parse_contacts(payload);
    let (left, right) = parse_buttons(payload);
    let mut events = match st.mode {
        TouchpadMode::Rel => {
            let (x, y, tip) = match contacts[0] {
                Some(c) => (c.x, c.y, true),
                None => (0, 0, false),
            };
            let mut ev = Vec::new();
            if let Some((dx, dy)) = st.contact.ingest(x, y, tip, tick) {
                ev.push(InputEvent::from_relative_move(RelCode::X, dx));
                ev.push(InputEvent::from_relative_move(RelCode::Y, dy));
            }
            ev
        }
        TouchpadMode::Abs => st.mt.ingest(contacts, count, tick),
    };

    if left != st.last_left {
        events.push(key_event(KeyCode::BtnLeft, left));
        st.last_left = left;
    }
    // Only the relative fallback reports a right button; see the capability
    // set in `I2cHidTouchpad::new`. Sending an event for a code the device
    // never advertised is what libinput calls a kernel bug.
    if matches!(st.mode, TouchpadMode::Rel) && right != st.last_right {
        events.push(key_event(KeyCode::BtnRight, right));
        st.last_right = right;
    }

    if events.is_empty() {
        return true;
    }
    events.push(InputEvent::from_sync_event(SynEvent::Report));
    st.registered.submit_events(&events);
    true
}

#[cfg(ktest)]
mod test {
    use alloc::vec::Vec;

    use aster_input::input_dev::InputEvent;
    use ostd::prelude::ktest;

    use super::{
        AbsCode, BUTTON_OFF, CONTACT_COUNT_OFF, CONTACT_STALE_TICKS, Contact, ContactTracker,
        FALLBACK_ABS_MAX, GPIO_SILENT_WARN_TICKS, KeyCode, MAX_STEP, MtTracker,
        first_interrupt_cell, hid_resolution, hid_unit_exponent, parse_buttons, parse_contacts,
        parse_hid_xy_range, should_read,
    };

    /// `interrupts = <50 2>` on a two-cell GPIO parent is pin 50.
    ///
    /// A two-cell GPIO specifier contains a pin followed by flags. Narrowing
    /// its combined u64 would select the flags instead of the pin.
    #[ktest]
    fn a_two_cell_gpio_specifier_decodes_to_the_pin_not_the_flag() {
        let dts = [0, 0, 0, 50, 0, 0, 0, 2];
        assert_eq!(first_interrupt_cell(&dts), Some(50));
        assert_eq!(u64::from_be_bytes(dts) as u32, 2, "the trap being avoided");
        assert_eq!(first_interrupt_cell(&[0, 0, 50]), None);
    }

    /// An INT line that never reads low is a constant, not a gate.
    #[ktest]
    fn a_line_stuck_high_stops_gating_reads() {
        // Asserted: always read, whatever else is true.
        assert!(should_read(Some(0), false, 0));
        assert!(should_read(Some(0), true, GPIO_SILENT_WARN_TICKS + 1));
        // Idle on a line known good: skip, that is the whole point of the gate.
        assert!(!should_read(Some(1), true, GPIO_SILENT_WARN_TICKS + 1));
        // Idle on a line that has never been low: believe it for a while,
        // then poll blind rather than leave the pad silent.
        assert!(!should_read(Some(1), false, 1));
        assert!(should_read(Some(1), false, GPIO_SILENT_WARN_TICKS + 1));
        // Unreadable pin: no gate to honour.
        assert!(should_read(None, false, 0));
    }

    /// Every existing motion test drives a contact that never pauses, so
    /// they all pass the same tick and read as continuous.
    const T: u32 = 0;

    /// A captured one-finger report with no button pressed. Preserve the raw
    /// bytes as an independent check of the layout constants:
    /// `20 00 01 03 c3 04 39 01 00*20 01 00 00 00`, minus the two length
    /// bytes that `payload` does not include.
    fn board_payload() -> [u8; 30] {
        let mut p = [0u8; 30];
        p[0] = 0x01; // report ID
        p[1] = 0x03; // contact 0: tip | confidence, contact ID 0
        p[2..6].copy_from_slice(&[0xc3, 0x04, 0x39, 0x01]); // x=1219, y=313
        p[CONTACT_COUNT_OFF] = 0x01; // one contact
        p
    }

    fn two_finger_payload() -> [u8; 30] {
        let mut p = board_payload();
        p[6] = 0x23; // contact 1: tip | confidence, id 2 in the top nibble
        p[7..11].copy_from_slice(&[0x10, 0x00, 0x20, 0x00]); // 16, 32
        p[CONTACT_COUNT_OFF] = 0x02;
        p
    }

    fn codes_of(events: &[InputEvent]) -> Vec<(u16, i32)> {
        events
            .iter()
            .map(|e| {
                let (_ty, code, val) = e.to_raw();
                (code, val)
            })
            .collect()
    }

    #[ktest]
    fn the_board_layout_puts_the_contact_count_where_the_dump_shows_it() {
        let p = board_payload();
        assert_eq!(CONTACT_COUNT_OFF, 26);
        assert_eq!(p[CONTACT_COUNT_OFF], 1);
        assert_eq!(u16::from_le_bytes([p[2], p[3]]), 1219);
        assert_eq!(u16::from_le_bytes([p[4], p[5]]), 313);
    }

    /// A finger resting on the pad is not a click.
    ///
    /// The contact-count byte is 1 for any touch, so a button scan that
    /// included it would fire on every finger-down — which is the failure
    /// this pins.
    #[ktest]
    fn a_touch_without_a_press_reports_no_button() {
        assert_eq!(parse_buttons(&board_payload()), (false, false));
    }

    /// The exact byte and bit the board produced for a physical click.
    #[ktest]
    fn bit_zero_of_payload_27_is_the_left_button() {
        assert_eq!(BUTTON_OFF, 27);

        let mut p = board_payload();
        p[BUTTON_OFF] = 0x01;
        assert_eq!(parse_buttons(&p), (true, false));

        let mut p = board_payload();
        p[BUTTON_OFF] = 0x02;
        assert_eq!(parse_buttons(&p), (false, true));
    }

    /// The two bytes after the button are the scan time on a
    /// Windows-precision report. They read zero on this device, but a
    /// counter appearing there must never be mistaken for a click.
    #[ktest]
    fn the_scan_time_bytes_are_not_buttons() {
        for off in BUTTON_OFF + 1..30 {
            let mut p = board_payload();
            p[off] = 0xff;
            assert_eq!(
                parse_buttons(&p),
                (false, false),
                "payload offset {} must not read as a button",
                off
            );
        }
    }

    /// A report too short to hold the trailing fields must not panic, and
    /// must not interpret contact flags as a button.
    #[ktest]
    fn a_report_shorter_than_the_contact_array_reports_no_button() {
        assert_eq!(
            parse_buttons(&[0x01, 0x03, 0xc3, 0x04, 0x39, 0x01]),
            (false, false)
        );
    }

    #[ktest]
    fn a_finger_moving_down_the_pad_emits_positive_rel_y() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(100, 200, true, T), None);
        assert_eq!(tracker.ingest(100, 210, true, T), Some((0, 10)));
    }

    #[ktest]
    fn a_finger_moving_right_emits_positive_rel_x() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(100, 200, true, T), None);
        assert_eq!(tracker.ingest(115, 200, true, T), Some((15, 0)));
    }

    #[ktest]
    fn an_oversized_jump_is_rejected() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(0, 0, true, T), None);
        assert_eq!(tracker.ingest(MAX_STEP, 0, true, T), None);
        assert_eq!(tracker.ingest(MAX_STEP + 3, 1, true, T), Some((3, 1)));
    }

    /// The board symptom: a lift whose report the poll never read, then a
    /// finger landing somewhere else. Without the staleness rule the first
    /// report of the new touch is a delta from the old position, and the
    /// pointer jumps by up to `MAX_STEP`.
    #[ktest]
    fn a_touch_after_a_gap_starts_a_new_contact_even_without_a_lift_report() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(100, 100, true, T), None);
        assert_eq!(tracker.ingest(105, 100, true, T + 1), Some((5, 0)));

        // No `tip == false` ever arrives — the finger just stops being read.
        let later = T + 1 + CONTACT_STALE_TICKS + 1;
        assert_eq!(
            tracker.ingest(260, 240, true, later),
            None,
            "a report this long after the previous one must not be a delta"
        );
        // The new position is still adopted, so the touch tracks from there.
        assert_eq!(tracker.ingest(264, 240, true, later + 1), Some((4, 0)));
    }

    /// One tick under the threshold is still the same contact, or a slow
    /// drag would drop samples.
    #[ktest]
    fn a_gap_just_under_the_threshold_is_still_the_same_contact() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(100, 100, true, T), None);
        assert_eq!(
            tracker.ingest(110, 100, true, T + CONTACT_STALE_TICKS),
            Some((10, 0))
        );
    }

    #[ktest]
    fn the_first_report_after_recontact_emits_no_motion() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(10, 10, true, T), None);
        assert_eq!(tracker.ingest(12, 10, true, T), Some((2, 0)));
        assert_eq!(tracker.ingest(12, 10, false, T), None);
        assert_eq!(tracker.ingest(400, 400, true, T), None);
        assert_eq!(tracker.ingest(403, 400, true, T), Some((3, 0)));
    }

    #[ktest]
    fn a_zero_delta_emits_nothing() {
        let mut tracker = ContactTracker::new();
        assert_eq!(tracker.ingest(50, 50, true, T), None);
        assert_eq!(tracker.ingest(50, 50, true, T), None);
        assert_eq!(tracker.ingest(50, 51, true, T), Some((0, 1)));
    }

    #[ktest]
    fn parse_contacts_reads_every_slot_of_a_two_finger_report() {
        let (count, slots) = parse_contacts(&two_finger_payload());
        assert_eq!(count, 2);
        assert_eq!(
            slots[0],
            Some(Contact {
                id: 0,
                x: 1219,
                y: 313
            })
        );
        assert_eq!(
            slots[1],
            Some(Contact {
                id: 2,
                x: 16,
                y: 32
            })
        );
        assert!(slots[2].is_none());
    }

    #[ktest]
    fn a_zero_contact_count_is_a_lift_even_when_tip_is_still_set() {
        let mut p = board_payload();
        p[CONTACT_COUNT_OFF] = 0;
        let (count, slots) = parse_contacts(&p);
        assert_eq!(count, 0);
        assert!(slots.iter().all(|s| s.is_none()));
    }

    #[ktest]
    fn mt_b_reports_two_slots_then_lifts_them() {
        let mut mt = MtTracker::new();
        let (count, slots) = parse_contacts(&two_finger_payload());
        let ev = mt.ingest(slots, count, T);
        let codes = codes_of(&ev);
        assert!(codes.contains(&(AbsCode::MtSlot as u16, 0)));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 1)));
        assert!(codes.contains(&(AbsCode::MtSlot as u16, 1)));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 2)));
        assert!(codes.contains(&(AbsCode::MtPositionX as u16, 1219)));
        assert!(codes.contains(&(AbsCode::MtPositionX as u16, 16)));
        assert!(codes.contains(&(AbsCode::X as u16, 1219)));
        assert!(codes.contains(&(AbsCode::Y as u16, 313)));
        assert!(codes.contains(&(KeyCode::BtnTouch as u16, 1)));
        assert!(codes.contains(&(KeyCode::BtnToolDoubleTap as u16, 1)));

        let ev = mt.ingest([None; 5], 0, T + 1);
        let codes = codes_of(&ev);
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, -1)));
        assert!(codes.contains(&(KeyCode::BtnTouch as u16, 0)));
        assert!(codes.contains(&(KeyCode::BtnToolDoubleTap as u16, 0)));
    }

    #[ktest]
    fn tracking_ids_stay_in_the_advertised_range_after_255_touches() {
        let geom = super::HidXy {
            xmin: 0,
            xmax: 1000,
            xres: 10,
            ymin: 0,
            ymax: 1000,
            yres: 10,
        };
        let device = super::I2cHidTouchpad::new(0, 0, super::TouchpadMode::Abs, geom);
        let info = device.capability.absinfo(AbsCode::MtTrackingId).unwrap();
        assert_eq!(info.minimum, 0);
        assert_eq!(info.maximum, i32::MAX);
        let mut mt = MtTracker::new();
        let (count, contacts) = parse_contacts(&board_payload());
        for expected in 1..=300 {
            let codes = codes_of(&mt.ingest(contacts, count, T));
            assert!(codes.contains(&(AbsCode::MtTrackingId as u16, expected)));
            assert!((info.minimum..=info.maximum).contains(&expected));
            let codes = codes_of(&mt.ingest([None; 5], 0, T));
            assert!(codes.contains(&(AbsCode::MtTrackingId as u16, -1)));
        }
    }

    #[ktest]
    fn mt_stale_gap_lifts_before_the_next_touch() {
        let mut mt = MtTracker::new();
        let (count, slots) = parse_contacts(&board_payload());
        let _ = mt.ingest(slots, count, T);
        let later = T + CONTACT_STALE_TICKS + 1;
        let ev = mt.ingest(slots, count, later);
        let codes = codes_of(&ev);
        // Old slot is closed, then reopened — no leftover tracking id.
        let lift = codes
            .iter()
            .position(|&c| c == (AbsCode::MtTrackingId as u16, -1));
        // The reopened touch gets a *fresh* id, never the one just closed:
        // reusing it would tell userspace this is the same finger.
        let down = codes
            .iter()
            .position(|&c| c == (AbsCode::MtTrackingId as u16, 2));
        assert!(lift.is_some() && down.is_some());
        assert!(lift.unwrap() < down.unwrap());
    }

    /// This pad reports contact id 0 on every contact, so slots fall back to
    /// the report position. Two fingers must still be two slots, and a second
    /// tap must not inherit the first one's tracking id -- reusing it is what
    /// made the pointer jump by the distance between two taps.
    #[ktest]
    fn mt_unnumbered_contacts_use_the_report_position() {
        let mut mt = MtTracker::new();
        let mut two = [None; 5];
        two[0] = Some(Contact {
            id: 0,
            x: 100,
            y: 100,
        });
        two[1] = Some(Contact {
            id: 0,
            x: 900,
            y: 400,
        });
        let codes = codes_of(&mt.ingest(two, 2, T));
        assert!(codes.contains(&(AbsCode::MtSlot as u16, 0)));
        assert!(codes.contains(&(AbsCode::MtSlot as u16, 1)));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 1)));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 2)));

        let codes = codes_of(&mt.ingest([None; 5], 0, T + 1));
        assert_eq!(
            codes
                .iter()
                .filter(|&&c| c == (AbsCode::MtTrackingId as u16, -1))
                .count(),
            2
        );

        // Tap somewhere else: a fresh id, not 1 again.
        let mut again = [None; 5];
        again[0] = Some(Contact {
            id: 0,
            x: 400,
            y: 700,
        });
        let codes = codes_of(&mt.ingest(again, 1, T + 2));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 3)));
        assert!(!codes.contains(&(AbsCode::MtTrackingId as u16, 1)));
    }

    /// Repacking must preserve contact slots without duplicate lift events.
    #[ktest]
    fn mt_slot_follows_the_contact_id_across_a_repack() {
        let mut mt = MtTracker::new();
        let a = Contact {
            id: 5,
            x: 100,
            y: 100,
        };
        let b = Contact {
            id: 7,
            x: 900,
            y: 400,
        };

        let mut first = [None; 5];
        first[0] = Some(a);
        first[1] = Some(b);
        let codes = codes_of(&mt.ingest(first, 2, T));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 1)));
        assert!(codes.contains(&(AbsCode::MtTrackingId as u16, 2)));

        // Finger 5 lifts and the pad repacks, so 7 arrives at report index 0.
        let mut second = [None; 5];
        second[0] = Some(b);
        let codes = codes_of(&mt.ingest(second, 1, T + 1));
        // Slot 0 closes once, and finger 7 is not re-announced: it never
        // stopped touching, so it keeps its slot and its tracking id.
        assert_eq!(
            codes
                .iter()
                .filter(|&&c| c == (AbsCode::MtTrackingId as u16, -1))
                .count(),
            1
        );
        assert!(!codes.contains(&(AbsCode::MtTrackingId as u16, 3)));
        // The old code handed finger 7's position to slot 0 here, which is
        // what userspace saw as a jump.
        assert!(!codes.contains(&(AbsCode::MtPositionX as u16, 900)));

        // Finger 7 moves. It must still be slot 1, not the index it now
        // occupies in the report.
        let mut third = [None; 5];
        third[0] = Some(Contact {
            id: 7,
            x: 910,
            y: 400,
        });
        let codes = codes_of(&mt.ingest(third, 1, T + 2));
        assert!(codes.contains(&(AbsCode::MtSlot as u16, 1)));
        assert!(codes.contains(&(AbsCode::MtPositionX as u16, 910)));
        assert!(!codes.contains(&(AbsCode::MtSlot as u16, 0)));
    }

    #[ktest]
    fn a_click_does_not_depend_on_contact_parse() {
        let mut p = two_finger_payload();
        p[BUTTON_OFF] = 0x01;
        assert_eq!(parse_buttons(&p), (true, false));
        let (count, slots) = parse_contacts(&p);
        assert_eq!(count, 2);
        assert!(slots[0].is_some() && slots[1].is_some());
    }

    #[ktest]
    fn hid_descriptor_gd_xy_logical_max_is_walked() {
        // Usage Page GD, Usage X, Logical Min 0, Logical Max 4095, Input,
        // Usage Y, Input.
        let desc = [
            0x05, 0x01, 0x09, 0x30, 0x15, 0x00, 0x26, 0xff, 0x0f, 0x81, 0x02, 0x09, 0x31, 0x81,
            0x02,
        ];
        let geom = parse_hid_xy_range(&desc).unwrap();
        assert_eq!((geom.xmin, geom.xmax), (0, FALLBACK_ABS_MAX));
        assert_eq!((geom.ymin, geom.ymax), (0, FALLBACK_ABS_MAX));
        // No Physical Maximum / Unit in this descriptor, so no resolution.
        assert_eq!((geom.xres, geom.yres), (0, 0));
        assert!(parse_hid_xy_range(&[0x05, 0x0d, 0x09, 0x05]).is_none());
    }

    /// The touchpad's X-axis report descriptor:
    /// Unit Exponent -2, Unit cm, Physical Max 1180, Logical Max 1626.
    #[ktest]
    fn hid_descriptor_resolution_comes_from_physical_size() {
        let desc = [
            0x05, 0x01, 0x09, 0x30, 0x75, 0x10, 0x55, 0x0e, 0x65, 0x11, 0x35, 0x00, 0x46, 0x9c,
            0x04, 0x27, 0x5a, 0x06, 0x00, 0x00, 0x81, 0x02, 0x09, 0x31, 0x46, 0x80, 0x02, 0x27,
            0x72, 0x03, 0x00, 0x00, 0x81, 0x02,
        ];
        let geom = parse_hid_xy_range(&desc).unwrap();
        assert_eq!(geom.xmax, 1626);
        assert_eq!(geom.ymax, 882);
        // 1180 * 10^-2 cm = 118 mm, 1626 / 118 = 13.8 -> 14.
        assert_eq!(geom.xres, 14);
        // 640 * 10^-2 cm = 64 mm, 882 / 64 = 13.8 -> 14.
        assert_eq!(geom.yres, 14);
    }

    #[ktest]
    fn hid_resolution_refuses_units_it_does_not_understand() {
        // Unit 0x13 is not SI-linear centimetres.
        assert_eq!(hid_resolution(1626, 1180, 0x13, -2), 0);
        // No physical size at all.
        assert_eq!(hid_resolution(1626, 0, 0x11, -2), 0);
        assert_eq!(hid_unit_exponent(0x0e), -2);
        assert_eq!(hid_unit_exponent(0x02), 2);
    }

    /// Tip Switch is bit 1 and the contact id is the top nibble, per the
    /// descriptor. Reading bit 0 (Confidence) as the touch is what kept
    /// fingers from ever lifting.
    #[ktest]
    fn contact_flags_follow_the_descriptor_layout() {
        let mut p = two_finger_payload();
        // Confidence only, no Tip Switch: not a touch.
        p[1] = 0x01;
        let (_, slots) = parse_contacts(&p);
        assert!(slots[0].is_none());
        // Confidence + Tip Switch, contact id 3 in the top nibble.
        p[1] = 0x33;
        let (_, slots) = parse_contacts(&p);
        assert_eq!(slots[0].map(|c| c.id), Some(3));
    }
}
