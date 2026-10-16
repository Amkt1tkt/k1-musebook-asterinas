// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: MFPR / padconf for the SpacemiT K1. riscv64-only crate.

//! Pin mux / padconf (MFPR) for the SpacemiT K1 MUSE Book.
//!
//! Owns the MFPR MMIO window, exposes read/write and `set_mux`, and probes
//! the board pads. Full Linux `pinctrl-single` semantics (gpio-ranges,
//! pin groups, IRQ wake) are not implemented.

#![no_std]
#![deny(unsafe_code)]

use component::{ComponentInitError, init_component};

macro_rules! __log_prefix {
    () => {
        "pinctrl: "
    };
}

mod k1;

// Clock/reset control lives in ostd (`ostd::arch::ccu`, mapped once in `late_init_on_bsp`);
// re-exported so the platform crates depending on pinctrl have a single import.
pub use k1::{MuxMode, or_mfpr_abs, read_pad, set_mux, write_mfpr_abs, write_pad};
pub use ostd::arch::ccu::{Periph, acgr_rmw, apbs_rmw, enable};

/// Pad access failures distinguish an unavailable controller, invalid offset
/// and MMIO access failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadError {
    /// The MFPR window is not mapped.
    Unmapped,
    /// The pin number or absolute address lies outside the mapped window.
    OutOfRange,
    /// The MMIO access itself failed.
    Io,
}

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    k1::init();
    Ok(())
}
