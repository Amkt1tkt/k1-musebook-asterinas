// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: DPU → MIPI-DSI → LT8911 eDP bring-up, KMS, vblank, cursor plane.
// HDMI (`k1_hdmi`) is not ported.

mod k1_cursor;
mod k1_edp;
mod k1_kms;
pub(crate) mod k1_splash;
mod k1_vblank;

pub(crate) fn init() {
    k1_edp::init();
}

pub(crate) fn backlight_present() -> bool {
    k1_edp::backlight_present()
}

pub(crate) fn backlight_brightness() -> u32 {
    k1_edp::backlight_brightness()
}

pub(crate) fn backlight_bl_power() -> u32 {
    k1_edp::backlight_bl_power()
}

pub(crate) fn set_backlight_brightness(value: u32) -> Result<(), crate::BacklightError> {
    k1_edp::set_backlight_brightness(value)
}

pub(crate) fn set_backlight_bl_power(value: u32) -> Result<(), crate::BacklightError> {
    k1_edp::set_backlight_bl_power(value)
}
