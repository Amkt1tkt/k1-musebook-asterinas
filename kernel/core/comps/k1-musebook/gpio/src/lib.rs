// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: four-bank K1 GPIO plus the shared PLIC 58 edge IRQ controller.

//! GPIO controllers for the SpacemiT K1 MUSE Book.
//!
//! Four PXA-style banks at `0xd4019000`, plus a shared PLIC interrupt (58)
//! demuxed via GRER/GFER/GEDR. Non-K1 platforms are silent no-ops.

#![no_std]
#![deny(unsafe_code)]

use component::{ComponentInitError, init_component};

macro_rules! __log_prefix {
    () => {
        "gpio: "
    };
}

mod k1_gpio;

/// Edge polarity for [`register_irq`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Rising,
    Falling,
    Both,
}

/// Why [`register_irq`] refused a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GpioIrqError {
    /// Not a K1, or the GPIO block never mapped.
    Unavailable,
    /// `gpio` is outside the four 32-bit banks (0..127).
    InvalidGpio,
    /// The shared PLIC line could not be claimed.
    IrqMapFailed,
    /// Another handler already owns this pin.
    Busy,
}

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    k1_gpio::init();
    Ok(())
}

/// Read the current level of `gpio` (0 or 1). Returns `None` if the K1 GPIO
/// controller is not initialized.
pub fn get_value(gpio: u32) -> Option<u32> {
    k1_gpio::get_value(gpio)
}

/// Drive `gpio` as an output to `value` (0 or 1). Returns `false` if the K1
/// GPIO controller is not initialized.
pub fn set_value(gpio: u32, value: u32) -> bool {
    k1_gpio::set_value(gpio, value)
}

/// Register a falling/rising/both-edge handler on `gpio`.
///
/// The shared PLIC 58 handler scans each bank's GEDR and calls `handler(gpio)`
/// for every pending unmasked bit. `handler` runs in interrupt context and
/// must not block. Non-K1 platforms return [`GpioIrqError::Unavailable`].
pub fn register_irq(gpio: u32, edge: Edge, handler: fn(u32)) -> Result<(), GpioIrqError> {
    k1_gpio::register_irq(gpio, edge, handler)
}
