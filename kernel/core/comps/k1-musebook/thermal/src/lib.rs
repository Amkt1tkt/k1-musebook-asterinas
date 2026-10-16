// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 on-die TSEN sensors and last-reset-cause dump.

//! Reset-reason + on-die temperature for the SpacemiT K1.
//!
//! Nothing here changes clocks, voltages, or trip behaviour. The
//! hardware emergency-reboot threshold is read, never written.

#![no_std]
#![deny(unsafe_code)]

use component::{ComponentInitError, init_component};

macro_rules! __log_prefix {
    () => {
        "thermal: "
    };
}

mod k1;
mod k1_rst;
mod k1_tsen;

pub mod convert;

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    k1::init();
    Ok(())
}

/// Periodic TSEN sampler. Spawned from the first kernel thread; silent
/// on non-K1.
pub fn run_loop() {
    k1::run_loop();
}

/// Hottest of cluster0 / cluster1 BJTs, millidegrees Celsius.
///
/// Linux `k1x_thermal_get_temp` reports `(raw - TEMPERATURE_OFFSET) * 1000`.
/// `None` on non-K1 or if TSEN never mapped. Consumed by `k1-musebook-cpufreq`
/// (TSEN `IoMem` is exclusive to this crate).
pub fn cluster_hot_temp_mc() -> Option<i32> {
    k1::cluster_hot_temp_mc()
}

/// Cluster0 BJT (bjt3) in millidegrees Celsius. Linux thermal-sensor `<&thermal 3>`.
pub fn cluster0_temp_mc() -> Option<i32> {
    k1::cluster0_temp_mc()
}

/// Cluster1 BJT (bjt4) in millidegrees Celsius. Linux thermal-sensor `<&thermal 4>`.
pub fn cluster1_temp_mc() -> Option<i32> {
    k1::cluster1_temp_mc()
}
