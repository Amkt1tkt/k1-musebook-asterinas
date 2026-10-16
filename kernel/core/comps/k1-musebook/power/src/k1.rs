// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SBS word reads on TWSI5 @ 0x0b.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ostd::{arch::boot::DEVICE_TREE, sync::WaitQueue, timer::TIMER_FREQ};
use spin::{Mutex, Once};

use crate::convert::{
    BatterySnapshot, CMD_BATTERY_MODE, CMD_CURRENT, CMD_FULL, CMD_REMAINING, CMD_SOC, CMD_STATUS,
    CMD_TEMPERATURE, CMD_TIME_TO_EMPTY, CMD_TIME_TO_FULL, CMD_VOLTAGE, Capacity, ChargeStatus,
    adjust_soc, battery_mode_is_watt, capacity_from_raw, current_word_to_ua, decode_charge_status,
    sbs_time_minutes, voltage_mv_to_uv,
};

static WAIT: WaitQueue = WaitQueue::new();
static PENDING: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicU64 = AtomicU64::new(0);
static SNAP: Mutex<Option<BatterySnapshot>> = Mutex::new(None);
// Only bootstrap and the battery worker/sysfs refresh call sample.
static SAMPLE: ostd::sync::Mutex<()> = ostd::sync::Mutex::new(());
static PUBLISHER: Once<fn() -> bool> = Once::new();
static PUBLISHED: AtomicBool = AtomicBool::new(false);
static PUBLISHING: AtomicBool = AtomicBool::new(false);

pub(crate) fn register_battery_publisher(callback: fn() -> bool) {
    PUBLISHER.call_once(|| callback);
    try_publish();
}

/// The bootstrap sample precedes sysfs initialization. Runtime samples retry
/// publication after success, without holding the controller or snapshot lock.
fn try_publish() {
    if ostd::task::Task::current().is_none() || PUBLISHED.load(Ordering::Acquire) {
        return;
    }
    let Some(callback) = PUBLISHER.get() else {
        return;
    };
    if battery().is_none() || PUBLISHING.swap(true, Ordering::AcqRel) {
        return;
    }
    if !PUBLISHED.load(Ordering::Acquire) && callback() {
        PUBLISHED.store(true, Ordering::Release);
    }
    PUBLISHING.store(false, Ordering::Release);
}

/// `TIMER_FREQ` is 1000 Hz; same 10 s cadence as `[k1-therm]`.
const TICKS_PER_PRINT: u64 = TIMER_FREQ * 10;
/// DTS `sbs,i2c-retry-count = <2>`.
const I2C_RETRIES: u32 = 2;
/// Muse Book DTS `battery@b`.
const SBS_ADDR: u8 = 0x0b;
/// Repeat an unchanged failure at most this often. Success prints every 10 s.
const FAIL_HEARTBEAT_TICKS: u64 = TIMER_FREQ * 60;

static LAST_FAIL: Mutex<Option<FailKind>> = Mutex::new(None);
static LAST_FAIL_TICK: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailKind {
    NoBus,
    Xfer(&'static str),
}

fn is_k1() -> bool {
    DEVICE_TREE.get().is_some_and(|dt| {
        dt.find_compatible(&["spacemit,k1-x", "spacemit,k1"])
            .is_some()
    })
}

pub(crate) fn init() {
    if !is_k1() {
        return;
    }
    tick(true);
}

fn on_timer() {
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_multiple_of(TICKS_PER_PRINT) {
        PENDING.store(true, Ordering::Release);
        WAIT.wake_all();
    }
}

pub(crate) fn run_loop() {
    if !is_k1() {
        // Return rather than park. Same reason as `k1-musebook-thermal`: a
        // queue that nothing wakes would strand this thread on every
        // non-K1 riscv target.
        return;
    }
    ostd::timer::register_callback_on_cpu(on_timer);
    loop {
        WAIT.wait_until(|| PENDING.swap(false, Ordering::AcqRel).then_some(()));
        tick(false);
    }
}

pub(crate) fn battery() -> Option<BatterySnapshot> {
    *SNAP.lock()
}

pub(crate) fn refresh() -> Option<BatterySnapshot> {
    if !is_k1() {
        return battery();
    }
    match sample() {
        Ok(snap) => {
            let mut slot = SNAP.lock();
            let first = slot.is_none();
            *slot = Some(snap);
            drop(slot);
            try_publish();
            if first {
                print_ok(snap);
            }
            Some(snap)
        }
        Err(_) => battery(),
    }
}

#[cfg(ktest)]
pub(crate) fn inject_snapshot(snap: Option<BatterySnapshot>) {
    *SNAP.lock() = snap;
}

fn tick(first: bool) {
    match sample() {
        Ok(snap) => {
            *SNAP.lock() = Some(snap);
            *LAST_FAIL.lock() = None;
            try_publish();
            // The first good read always prints (it is the "gauge answers"
            // line of the boot log); the 10 s cadence after that is
            // `k1.diag=batt`. `/sys/class/power_supply` is fed either way.
            if first || aster_cmdline::diag_enabled("batt") {
                print_ok(snap);
            }
        }
        Err(kind) => print_fail(kind, first),
    }
}

fn sample() -> Result<BatterySnapshot, FailKind> {
    let _guard = if ostd::task::Task::current().is_none() {
        SAMPLE
            .try_lock()
            .ok_or(FailKind::Xfer("battery sample busy during bootstrap"))?
    } else {
        SAMPLE.lock()
    };
    if !k1_musebook_i2c::twsi5_up() {
        return Err(FailKind::NoBus);
    }

    let mode = read_word(CMD_BATTERY_MODE)?;
    let temp = read_word(CMD_TEMPERATURE)?;
    let volt = read_word(CMD_VOLTAGE)?;
    let curr = read_word(CMD_CURRENT)?;
    let soc = read_word(CMD_SOC)?;
    let rem = read_word(CMD_REMAINING)?;
    let full = read_word(CMD_FULL)?;
    let tte = read_word(CMD_TIME_TO_EMPTY)?;
    let ttf = read_word(CMD_TIME_TO_FULL)?;
    let stat = read_word(CMD_STATUS)?;

    let current_ma = curr as i16;
    let status = decode_charge_status(stat, current_ma);
    let mode_watt = battery_mode_is_watt(mode);

    Ok(BatterySnapshot {
        soc_percent: adjust_soc(i32::from(soc), status),
        voltage_uv: voltage_mv_to_uv(volt),
        current_ua: current_word_to_ua(curr),
        temp_decikelvin: temp,
        status,
        remaining: capacity_from_raw(rem, mode_watt),
        full: capacity_from_raw(full, mode_watt),
        time_to_empty_min: sbs_time_minutes(tte),
        time_to_full_min: sbs_time_minutes(ttf),
        battery_status_raw: stat,
        capacity_mode_watt: mode_watt,
    })
}

fn read_word(cmd: u8) -> Result<u16, FailKind> {
    let mut last = "no bus";
    for _ in 0..I2C_RETRIES {
        match k1_musebook_i2c::twsi5_read_word(SBS_ADDR, cmd) {
            Ok(w) => return Ok(w),
            Err(e) => last = e,
        }
    }
    Err(FailKind::Xfer(last))
}

fn print_ok(snap: BatterySnapshot) {
    let v = snap.voltage_uv;
    let v_int = v / 1_000_000;
    let v_frac = (v / 10_000) % 100;

    let a = snap.current_ua;
    let a_neg = a < 0;
    let a_abs = a.unsigned_abs();
    let a_int = a_abs / 1_000_000;
    let a_frac = (a_abs / 10_000) % 100;
    let a_sign = if a_neg { "-" } else { "" };

    let (unit, rem, full) = match (snap.remaining, snap.full) {
        (Capacity::MilliAmpHour(r), Capacity::MilliAmpHour(f)) => {
            ("mAh", u32::from(r), u32::from(f))
        }
        (Capacity::MilliWattHour(r), Capacity::MilliWattHour(f)) => ("mWh", r, f),
        _ => ("?", 0, 0),
    };

    let eta = match snap.status {
        ChargeStatus::Discharging => snap.time_to_empty_min,
        ChargeStatus::Charging => snap.time_to_full_min,
        _ => None,
    };

    match eta {
        Some(min) => log::info!(
            "[k1-batt] soc={}% {}.{:02}V {}{}.{:02}A {}C status={} rem={}{}/{}{} eta={}min",
            snap.soc_percent,
            v_int,
            v_frac,
            a_sign,
            a_int,
            a_frac,
            snap.temp_celsius_trunc(),
            snap.status.as_str(),
            rem,
            unit,
            full,
            unit,
            min
        ),
        None => log::info!(
            "[k1-batt] soc={}% {}.{:02}V {}{}.{:02}A {}C status={} rem={}{}/{}{} eta=?",
            snap.soc_percent,
            v_int,
            v_frac,
            a_sign,
            a_int,
            a_frac,
            snap.temp_celsius_trunc(),
            snap.status.as_str(),
            rem,
            unit,
            full,
            unit
        ),
    }
}

fn print_fail(kind: FailKind, first: bool) {
    let ticks = TICKS.load(Ordering::Relaxed);
    let mut last = LAST_FAIL.lock();
    let same = *last == Some(kind);
    let due = ticks.saturating_sub(LAST_FAIL_TICK.load(Ordering::Relaxed)) >= FAIL_HEARTBEAT_TICKS;
    // First failure always logs; later repeats only under `k1.diag=batt`.
    if !first && same && !(due && aster_cmdline::diag_enabled("batt")) {
        return;
    }
    *last = Some(kind);
    LAST_FAIL_TICK.store(ticks, Ordering::Relaxed);
    drop(last);

    match kind {
        FailKind::NoBus => {
            log::warn!("[k1-batt] twsi5 down (clock/pinctrl/mmio) — not a battery NAK")
        }
        FailKind::Xfer("no ACK") => {
            log::warn!("[k1-batt] no ACK @0x0b (pack absent, asleep, or not this addr)")
        }
        FailKind::Xfer(e) => log::warn!(
            "[k1-batt] twsi5 xfer failed: {} (controller/wiring; not a clean NAK)",
            e
        ),
    }
}
