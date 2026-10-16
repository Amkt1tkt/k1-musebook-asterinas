// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: sysfs write path used by the login1 brightness shim.

//! Sysfs write path used by the login1 brightness shim.
//!
//! The sysfs parser and write policy are independent of D-Bus so tests can
//! run against temporary directories. The binary claims the service name
//! only after [`probe_backlight`] succeeds.

mod sysfs;

pub use sysfs::{
    clamp_brightness, device_dir, parse_sysfs_u32, probe_backlight, set_brightness, validate_name,
    validate_subsystem, Absent, ParseError, Probe, Reject, SetError, ALLOWED_SUBSYSTEMS,
};
