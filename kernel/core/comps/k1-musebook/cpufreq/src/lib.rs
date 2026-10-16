// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 DVFS (OPP + PLL mux + SPM8821 dcdc1 + TSEN cap).

//! SpacemiT K1 cpufreq / DVFS for the Muse Book.
//!
//! Feature-gated at `aster-core` (`cpufreq`). Runtime
//! `k1.cpufreq=off` keeps the thermal sysfs and the sampler but never
//! writes a PLL, mux, or DCDC1 vsel. Any I2C / PLL-lock / FC timeout
//! freezes further writes so a bad step cannot hang the cores.
//!
//! Clock registers live in the ostd CCU `IoMem` window; this crate does
//! not map APMU/APBS itself (a second acquire would fail). Voltage uses
//! the existing TWSI8 `k1_musebook_i2c::{pmic_read,pmic_write}` path.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use core::{
    str::FromStr,
    sync::atomic::{AtomicBool, Ordering},
};

use component::{ComponentInitError, init_component};
use ostd::sync::Mutex;
use spin::Once;

macro_rules! __log_prefix {
    () => {
        "cpufreq: "
    };
}

mod clock;
mod opp;
mod policy;
mod sysfs;
mod thermal;
mod voltage;

pub use opp::{K1_OPP_TABLE0, STABLE_KHZ, allowed_max_khz, allowed_min_khz};
pub use policy::{Governor, Policy, THERMAL_CRITICAL_MC};
pub use sysfs::AttrSnap;

/// `k1.cpufreq=off` disables DVFS writes. Absent / `on` leaves them enabled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CpufreqCmd {
    On,
    Off,
}

impl FromStr for CpufreqCmd {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "off" | "0" | "false" | "no" => Ok(Self::Off),
            "on" | "1" | "true" | "yes" => Ok(Self::On),
            _ => Err(()),
        }
    }
}

static CPUFREQ_CMD: Once<CpufreqCmd> = Once::new();
aster_cmdline::define_kv_param!("k1.cpufreq", CPUFREQ_CMD);

struct State {
    policy: Policy,
    /// Hardware writes allowed. Cleared by cmdline or a failed step.
    dvfs_ok: bool,
    thermal_level: u8,
    last_temp_mc: Option<i32>,
}

static STATE: Mutex<State> = Mutex::new(State {
    policy: Policy {
        gov: Governor::Userspace,
        min_khz: 614_400,
        max_khz: 1_600_000,
        setspeed_khz: STABLE_KHZ,
        cur_khz: 0,
        thermal_cap_khz: 1_600_000,
    },
    dvfs_ok: false,
    thermal_level: 0,
    last_temp_mc: None,
});

static LOGGED_ENABLE: AtomicBool = AtomicBool::new(false);
static LOGGED_DEFAULT: AtomicBool = AtomicBool::new(false);
static LOGGED_FIRST_SET: AtomicBool = AtomicBool::new(false);
static LOGGED_FAIL: AtomicBool = AtomicBool::new(false);
static LOGGED_THERM: AtomicBool = AtomicBool::new(false);
static LOGGED_CRIT: AtomicBool = AtomicBool::new(false);

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    probe();
    Ok(())
}

fn cmdline_off() -> bool {
    CPUFREQ_CMD.get().copied() == Some(CpufreqCmd::Off)
}

fn probe() {
    let off = cmdline_off();
    let k1 = is_k1();

    if !LOGGED_ENABLE.swap(true, Ordering::AcqRel) {
        log::info!(
            "[k1-cpufreq] probe k1={} cmdline={}",
            k1,
            if off { "off" } else { "on" }
        );
    }

    // Component initialization precedes all policy readers and workers.
    let mut st = STATE.try_lock().expect("cpufreq probe is exclusive");
    st.policy = Policy::conservative_default();
    st.policy.cur_khz = 0;
    st.dvfs_ok = k1 && !off;

    if !k1 {
        return;
    }

    match clock::read_cur_khz().filter(|khz| opp::lookup(*khz).is_some()) {
        Some(khz) => {
            st.policy.cur_khz = khz;
            if opp::is_allowed(khz) {
                st.policy.setspeed_khz = khz;
            }
            log::info!(
                "[k1-cpufreq] live {} kHz voltage={:?} mV",
                khz,
                voltage::get_dcdc1_mv()
            );
        }
        None => {
            st.dvfs_ok = false;
            log::warn!("[k1-cpufreq] unknown or unequal cluster clocks; DVFS disabled");
        }
    }

    if off {
        log::info!("[k1-cpufreq] dvfs disabled (k1.cpufreq=off); thermal still sampled");
        return;
    }

    if !st.dvfs_ok {
        return;
    }

    if !LOGGED_DEFAULT.swap(true, Ordering::AcqRel) {
        log::info!(
            "[k1-cpufreq] default userspace {} kHz (BSP STABLE_FREQUENCY)",
            STABLE_KHZ
        );
    }

    let _ = apply_locked(&mut st);
}

fn is_k1() -> bool {
    ostd::arch::boot::DEVICE_TREE.get().is_some_and(|dt| {
        dt.find_compatible(&["spacemit,k1-x", "spacemit,k1"])
            .is_some()
    })
}

fn apply_locked(st: &mut State) -> Result<(), &'static str> {
    if !st.dvfs_ok {
        return Err("dvfs frozen");
    }
    let new = st.policy.target_opp();
    let Some(old) = opp::lookup(st.policy.cur_khz) else {
        return freeze(st, "unknown current frequency");
    };
    let Some(mut live_mv) = voltage::get_dcdc1_mv() else {
        return freeze(st, "cannot read DCDC1 voltage");
    };
    // The bootloader's voltage need not equal the old OPP's nominal value.
    // Keep both old and new operating points powered until the clock changes.
    let required_mv = old.mv.max(new.mv);
    if live_mv < required_mv {
        if !set_voltage(live_mv, required_mv) {
            return freeze(st, "DCDC1 raise failed");
        }
        live_mv = required_mv;
    }
    if new.khz == old.khz {
        return Ok(());
    }
    if let Err(e) = clock::set_rate(old, new) {
        // A partially applied mux transition cannot be reported as the old OPP.
        st.policy.cur_khz = clock::read_cur_khz().unwrap_or(0);
        return freeze(st, e);
    }
    st.policy.cur_khz = new.khz;
    if live_mv > new.mv && !set_voltage(live_mv, new.mv) {
        return freeze(st, "DCDC1 lower failed");
    }
    if !LOGGED_FIRST_SET.swap(true, Ordering::AcqRel) {
        log::info!(
            "[k1-cpufreq] set {} kHz {} mV gov={}",
            new.khz,
            new.mv,
            st.policy.gov.as_str()
        );
    }
    Ok(())
}

fn set_voltage(from_mv: u32, to_mv: u32) -> bool {
    if !voltage::set_dcdc1_mv(to_mv) {
        return false;
    }
    delay_us(voltage::ramp_delay_us(from_mv, to_mv).max(1));
    voltage::get_dcdc1_mv() == Some(to_mv)
}

fn freeze(st: &mut State, why: &'static str) -> Result<(), &'static str> {
    st.dvfs_ok = false;
    if !LOGGED_FAIL.swap(true, Ordering::AcqRel) {
        log::warn!(
            "[k1-cpufreq] fail: {}; frozen (no more PLL/DCDC1 writes)",
            why
        );
    }
    Err(why)
}

fn delay_us(us: u64) {
    let ticks = (ostd::arch::tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = ostd::arch::read_tsc();
    while ostd::arch::read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}

/// One thermal + governor step. Called from the kernel thread.
pub fn tick() {
    let Some(temp) = thermal::cluster_hot_temp_mc() else {
        return;
    };
    let mut st = STATE.lock();
    st.last_temp_mc = Some(temp);
    if temp >= THERMAL_CRITICAL_MC && !LOGGED_CRIT.swap(true, Ordering::AcqRel) {
        log::warn!(
            "[k1-cpufreq] critical {} mC (dtsi 115 C); staying at thermal min",
            temp
        );
    }
    let (lvl, cap) = policy::thermal_update(st.thermal_level, temp);
    if lvl != st.thermal_level {
        if !LOGGED_THERM.swap(true, Ordering::AcqRel) {
            log::info!(
                "[k1-cpufreq] thermal level {}→{} cap={} kHz temp={} mC",
                st.thermal_level,
                lvl,
                cap,
                temp
            );
        }
        st.thermal_level = lvl;
        st.policy.thermal_cap_khz = cap;
        if st.dvfs_ok {
            let _ = apply_locked(&mut st);
        }
    }
}

/// Live policy for sysfs `cat`.
pub fn attr_snap() -> AttrSnap {
    AttrSnap {
        policy: STATE.lock().policy,
    }
}

pub fn last_temp_mc() -> Option<i32> {
    STATE.lock().last_temp_mc.or_else(thermal::zone0_temp_mc)
}

pub fn zone0_temp_mc() -> Option<i32> {
    thermal::zone0_temp_mc()
}

pub fn dvfs_enabled() -> bool {
    STATE.lock().dvfs_ok
}

pub fn set_governor(name: &str) -> Result<(), &'static str> {
    let gov = Governor::parse(name).ok_or("bad governor")?;
    let mut st = STATE.lock();
    st.policy.gov = gov;
    apply_locked(&mut st)
}

pub fn set_setspeed(khz: u32) -> Result<(), &'static str> {
    if !opp::is_allowed(khz) {
        return Err("freq not in table");
    }
    let mut st = STATE.lock();
    if st.policy.gov != Governor::Userspace {
        return Err("setspeed needs userspace");
    }
    st.policy.setspeed_khz = khz;
    apply_locked(&mut st).map(|_| ())
}

pub fn set_min_khz(khz: u32) -> Result<(), &'static str> {
    if !opp::is_allowed(khz) {
        return Err("freq not in table");
    }
    let mut st = STATE.lock();
    if khz > st.policy.max_khz {
        return Err("min > max");
    }
    st.policy.min_khz = khz;
    apply_locked(&mut st)
}

pub fn set_max_khz(khz: u32) -> Result<(), &'static str> {
    if !opp::is_allowed(khz) {
        return Err("freq not in table");
    }
    let mut st = STATE.lock();
    if khz < st.policy.min_khz {
        return Err("max < min");
    }
    st.policy.max_khz = khz;
    apply_locked(&mut st)
}

/// Format helpers the kernel crate uses to hang sysfs.
pub mod nodes {
    pub use crate::{sysfs::*, thermal::format_temp_mc};
}
