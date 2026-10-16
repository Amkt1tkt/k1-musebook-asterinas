// SPDX-License-Identifier: MPL-2.0

//! The framebuffer of Asterinas.
#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "framebuffer: "
    };
}

mod ansi_escape;
pub mod console;
pub mod font;
pub mod framebuffer;
pub mod mode;
pub mod pixel;
// K1 MUSE Book --- (kernel boot splash public API; no-op on non-riscv)
pub mod splash;
// K1 MUSE Book ---

use component::{ComponentInitError, init_component};
// K1 MUSE Book --- (re-exports used by /dev/fb0, backlight sysfs, and K1 eDP)
pub use framebuffer::{ColorMapEntry, FRAMEBUFFER, FrameBuffer, MAX_CMAP_SIZE};
pub use pixel::{Pixel, PixelFormat, RenderedPixel};
// K1 MUSE Book ---

// K1 MUSE Book --- (K1 eDP/KMS when boot_info has no framebuffer; PWM backlight)
pub mod sysfs;

#[cfg_attr(target_arch = "x86_64", path = "arch/x86/mod.rs")]
#[cfg_attr(target_arch = "riscv64", path = "arch/riscv/mod.rs")]
#[cfg_attr(target_arch = "loongarch64", path = "arch/loongarch/mod.rs")]
mod arch;

#[cfg(ktest)]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(ktest)]
static KTEST_PRESENT: AtomicBool = AtomicBool::new(false);

/// PWM14 came up on this board. Non-K1 is always false.
pub fn backlight_present() -> bool {
    #[cfg(ktest)]
    if KTEST_PRESENT.load(Ordering::Acquire) {
        return true;
    }
    arch::backlight_present()
}

pub fn backlight_brightness() -> u32 {
    arch::backlight_brightness()
}

pub fn backlight_bl_power() -> u32 {
    arch::backlight_bl_power()
}

/// Invalid input for a backlight update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BacklightError {
    /// The requested brightness exceeds the hardware brightness range.
    BrightnessOutOfRange,
}

pub fn set_backlight_brightness(value: u32) -> Result<(), BacklightError> {
    arch::set_backlight_brightness(value)
}

pub fn set_backlight_bl_power(value: u32) -> Result<(), BacklightError> {
    arch::set_backlight_bl_power(value)
}

/// ktests publish the sysfs node without PWM MMIO.
#[cfg(ktest)]
pub fn force_backlight_present_for_ktest() {
    KTEST_PRESENT.store(true, Ordering::Release);
}
// K1 MUSE Book ---

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    framebuffer::init();
    // K1 MUSE Book --- (platform FB / K1 eDP when boot_info has no framebuffer)
    if FRAMEBUFFER.get().is_none() {
        arch::init();
    }
    // K1 MUSE Book ---
    Ok(())
}
