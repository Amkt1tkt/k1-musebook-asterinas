// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SBS smart battery on TWSI5 (Muse Book `battery@b`).

//! Muse Book Smart Battery (SBS over TWSI5).
//!
//! The gauge is a standard `sbs,sbs-battery` at 0x0b on I2C5. Call
//! [`battery`] for the last successful sample. [`sysfs`] renders that
//! sample as Linux `/sys/class/power_supply` text; the kernel crate
//! hangs `BAT0` on the systree after Bootstrap component init.
//!
//! Silent on non-K1. Bus failures never panic and are not printed every
//! poll — a missing or sleeping pack is normal.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use component::{ComponentInitError, init_component};

macro_rules! __log_prefix {
    () => {
        "power: "
    };
}

mod k1;

pub mod convert;
pub mod sysfs;

pub use convert::{BatterySnapshot, Capacity, ChargeStatus};

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    k1::init();
    Ok(())
}

/// Last successful sample. `None` if this is not a K1, TWSI5 never
/// came up, or every read so far has failed.
///
/// A failed poll does not clear a previous success — the pack going
/// to sleep is normal, and wiping the last reading would make sysfs
/// flap between "present" and I/O errors.
pub fn battery() -> Option<BatterySnapshot> {
    k1::battery()
}

/// One more I2C sample, used just before sysfs is shown to user space.
///
/// Retries the initial sample before BAT0 sysfs registration. A failed refresh
/// keeps the previous successful sample, if any.
pub fn refresh() -> Option<BatterySnapshot> {
    k1::refresh()
}

/// Overwrites the cached sample. ktests use this to drive sysfs
/// without TWSI5; production code does not call it.
#[cfg(ktest)]
pub fn inject_snapshot_for_ktest(snap: Option<BatterySnapshot>) {
    k1::inject_snapshot(snap);
}

/// Periodic battery sampler. Spawned from the first kernel thread
/// next to `k1_musebook_thermal::run_loop`; silent on non-K1.
pub fn run_loop() {
    k1::run_loop();
}

/// Registers the kernel's BAT0 publisher once the device model is ready.
///
/// Call in a sleepable task. The callback runs after successful battery samples,
/// outside all driver locks, until it reports success. Failed registration is
/// retried by the existing sampler; no additional worker is created.
pub fn register_battery_publisher(callback: fn() -> bool) {
    k1::register_battery_publisher(callback);
}
