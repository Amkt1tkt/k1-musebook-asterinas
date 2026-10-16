// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: backlight stubs so aster-framebuffer keeps building on loongarch.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::sysfs;

static BRIGHTNESS: AtomicU32 = AtomicU32::new(sysfs::MAX_BRIGHTNESS);
static BL_POWER: AtomicU32 = AtomicU32::new(sysfs::FB_BLANK_UNBLANK);

pub(crate) fn init() {}

pub(crate) fn backlight_present() -> bool {
    false
}

pub(crate) fn backlight_brightness() -> u32 {
    BRIGHTNESS.load(Ordering::Relaxed)
}

pub(crate) fn backlight_bl_power() -> u32 {
    BL_POWER.load(Ordering::Relaxed)
}

pub(crate) fn set_backlight_brightness(value: u32) -> Result<(), crate::BacklightError> {
    if value > sysfs::MAX_BRIGHTNESS {
        return Err(crate::BacklightError::BrightnessOutOfRange);
    }
    BRIGHTNESS.store(value, Ordering::Relaxed);
    Ok(())
}

pub(crate) fn set_backlight_bl_power(value: u32) -> Result<(), crate::BacklightError> {
    BL_POWER.store(value, Ordering::Relaxed);
    Ok(())
}
