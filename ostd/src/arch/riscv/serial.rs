// SPDX-License-Identifier: MPL-2.0

//! The console I/O.

// K1 MUSE Book --- (write CRLF without recursively taking the serial lock)
// use core::fmt;
use core::fmt::{self, Write};

// K1 MUSE Book ---
use spin::Once;

use crate::{
    boot::EarlyCmdline,
    sync::{LocalIrqDisabled, SpinLock},
};

/// The primary serial port, which serves as an early console.
pub(crate) static SERIAL_PORT: Once<SpinLock<SbiSerial, LocalIrqDisabled>> =
    Once::initialized(SpinLock::new(SbiSerial::new()));

/// A serial port that is implemented via RISC-V SBI.
pub(crate) struct SbiSerial {
    _private: (),
}

impl SbiSerial {
    const fn new() -> Self {
        Self { _private: () }
    }
}

// K1 MUSE Book --- (write CRLF without recursively taking the serial lock)
// impl fmt::Write for SbiSerial {
//     fn write_str(&mut self, s: &str) -> fmt::Result {
//         for c in s.as_bytes() {
//             sbi_rt::console_write_byte(*c);
//         }
//         Ok(())
//     }
// }
impl Write for SbiSerial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str_raw(s);
        Ok(())
    }
}
// K1 MUSE Book ---

// K1 MUSE Book --- (re-entrant raw write for nested early_print / panic during write_fmt)
/// Writes without taking [`SERIAL_PORT`].
///
/// Only used when this CPU already holds that lock — a nested `early_print`
/// or a panic during `write_fmt`. `SbiSerial` has no state, so the bytes
/// are the same either way.
pub(crate) fn write_fmt_raw(args: fmt::Arguments) {
    let _ = SbiSerial::new().write_fmt(args);
}

fn write_str_raw(s: &str) {
    for c in s.as_bytes() {
        if *c == b'\n' {
            sbi_rt::console_write_byte(b'\r');
        }
        sbi_rt::console_write_byte(*c);
    }
}
// K1 MUSE Book ---

/// Initializes the serial port.
pub(crate) fn init(_early_cmdline: &EarlyCmdline) {}
