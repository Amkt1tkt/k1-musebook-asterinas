// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: sysfs snapshot used by the UPower shim binary.

//! Sysfs snapshot used by the UPower shim binary.
//!
//! Keeps battery parsing independent of D-Bus. The binary publishes the
//! service only after [`read_battery`] returns a valid snapshot.

mod sysfs;

pub use sysfs::{read_battery, Absent, Snapshot, Status};
