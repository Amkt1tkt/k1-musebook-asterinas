// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Linux /sys/class/backlight attribute text (PWM14, 0..=255).

//! Linux `/sys/class/backlight` attribute text.
//!
//! Names and spellings follow `Documentation/ABI/stable/sysfs-class-backlight`
//! and `Documentation/ABI/testing/sysfs-class-backlight` (`type`, `scale`)
//! plus `drivers/video/backlight/backlight.c`. This module only formats
//! and converts; the kernel crate hangs the virtual class device.
//!
//! `max_brightness` is 255, not the PWM period. The Muse Book DT
//! (`k1-x_MUSE-Book.dts` `&pwm_bl`) publishes a 256-entry
//! `brightness-levels` table, so Linux `pwm-backlight` advertises
//! `max_brightness = 255`. COSMIC / `brightnessctl` treat that as the
//! slider range. PWM14 after `k1_edp` init is clamped to
//! `PWMDCR_FD - 1` = 1023; we scale 0..=255 linearly onto 0..=1023 so
//! `echo 255` stays full duty (the boot default) and `echo 0` is off.

use alloc::{format, string::String};
use core::fmt::Write;

/// Linux class directory name. `libudev` `match_subsystem("backlight")`.
pub const CLASS: &str = "backlight";

/// DT node name (`pwm_bl: lcd_backlight` in `k1-x.dtsi`).
pub const NAME: &str = "lcd_backlight";

/// `BACKLIGHT_RAW` in `include/linux/backlight.h`. We poke PWMDCR.
pub const TYPE: &str = "raw";

/// `BACKLIGHT_SCALE_LINEAR`. Duty is `brightness * max_duty / 255`.
pub const SCALE: &str = "linear";

/// Vendor `brightness-levels` length minus one.
pub const MAX_BRIGHTNESS: u32 = 255;

/// `PWMDCR_FD - 1` after `k1x,pwm-disable-fd` (`pwm-pxa.c`).
pub const PWM_MAX_DUTY: u32 = 1023;

/// `FB_BLANK_UNBLANK` — backlight on, use the stored brightness.
pub const FB_BLANK_UNBLANK: u32 = 0;

/// `FB_BLANK_POWERDOWN`. Any non-zero `bl_power` blanks, matching
/// `backlight_is_blank` in `include/linux/backlight.h`.
pub const FB_BLANK_POWERDOWN: u32 = 4;

const ATTRS: [&str; 6] = [
    "brightness",
    "actual_brightness",
    "max_brightness",
    "bl_power",
    "type",
    "scale",
];

/// Stored brightness if `bl_power` is UNBLANK, otherwise 0.
///
/// Linux `backlight_get_brightness`: a blanked device reports 0 here
/// even when the `brightness` file still holds the last non-zero value.
pub fn effective_brightness(brightness: u32, bl_power: u32) -> u32 {
    if bl_power != FB_BLANK_UNBLANK {
        0
    } else {
        brightness
    }
}

/// Linear map onto the PWM duty register. `brightness == 0` is duty 0
/// (`pwm_backlight_update_status` when `backlight_get_brightness` is 0).
pub fn brightness_to_duty(brightness: u32, max_brightness: u32, max_duty: u32) -> u32 {
    if brightness == 0 || max_brightness == 0 {
        return 0;
    }
    let brightness = brightness.min(max_brightness);
    ((u64::from(brightness) * u64::from(max_duty)) / u64::from(max_brightness)) as u32
}

pub fn format_attr(name: &str, brightness: u32, bl_power: u32) -> Option<String> {
    match name {
        "brightness" => Some(line_u32(brightness)),
        "actual_brightness" => Some(line_u32(effective_brightness(brightness, bl_power))),
        "max_brightness" => Some(line_u32(MAX_BRIGHTNESS)),
        "bl_power" => Some(line_u32(bl_power)),
        "type" => Some(line_str(TYPE)),
        "scale" => Some(line_str(SCALE)),
        _ => None,
    }
}

/// Class uevent. `libudev` refuses a device with no `uevent` file
/// (`sd_device_new_from_syspath`).
pub fn format_uevent(brightness: u32, bl_power: u32) -> String {
    let mut text = format!("BACKLIGHT_NAME={}\n", NAME);
    for name in ATTRS {
        let Some(value) = format_attr(name, brightness, bl_power) else {
            continue;
        };
        let value = value.trim_end_matches('\n');
        let _ = writeln!(text, "BACKLIGHT_{}={}", name.to_ascii_uppercase(), value);
    }
    text
}

fn line_str(value: &str) -> String {
    format!("{}\n", value)
}

fn line_u32(value: u32) -> String {
    format!("{}\n", value)
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Full sysfs brightness must program the same duty `k1_edp` writes
    /// at boot (`PWMDCR_FD - 1`), so publishing the node does not dim
    /// the panel.
    #[ktest]
    fn full_brightness_is_full_duty() {
        assert_eq!(
            brightness_to_duty(MAX_BRIGHTNESS, MAX_BRIGHTNESS, PWM_MAX_DUTY),
            PWM_MAX_DUTY
        );
        assert_eq!(brightness_to_duty(0, MAX_BRIGHTNESS, PWM_MAX_DUTY), 0);
        assert_eq!(
            brightness_to_duty(128, MAX_BRIGHTNESS, PWM_MAX_DUTY),
            (128 * PWM_MAX_DUTY) / 255
        );
        assert_eq!(
            brightness_to_duty(1, MAX_BRIGHTNESS, PWM_MAX_DUTY),
            PWM_MAX_DUTY / 255
        );
    }

    /// `bl_power != 0` blanks, including the Linux `FB_BLANK_POWERDOWN = 4`
    /// and the other `FB_BLANK_*` values. The stored brightness is
    /// unchanged; only `actual_brightness` drops to 0.
    #[ktest]
    fn bl_power_blanks_actual_not_stored() {
        assert_eq!(effective_brightness(200, FB_BLANK_UNBLANK), 200);
        assert_eq!(effective_brightness(200, FB_BLANK_POWERDOWN), 0);
        assert_eq!(effective_brightness(200, 1), 0);
        assert_eq!(
            format_attr("brightness", 200, FB_BLANK_POWERDOWN).unwrap(),
            "200\n"
        );
        assert_eq!(
            format_attr("actual_brightness", 200, FB_BLANK_POWERDOWN).unwrap(),
            "0\n"
        );
        assert_eq!(format_attr("type", 0, 0).unwrap(), "raw\n");
        assert_eq!(format_attr("scale", 0, 0).unwrap(), "linear\n");
    }

    #[ktest]
    fn uevent_carries_backlight_keys() {
        let text = format_uevent(80, 0);
        assert!(text.starts_with("BACKLIGHT_NAME=lcd_backlight\n"));
        assert!(text.contains("BACKLIGHT_BRIGHTNESS=80\n"));
        assert!(text.contains("BACKLIGHT_MAX_BRIGHTNESS=255\n"));
        assert!(text.contains("BACKLIGHT_TYPE=raw\n"));
        assert!(text.contains("BACKLIGHT_SCALE=linear\n"));
        assert!(text.contains("BACKLIGHT_BL_POWER=0\n"));
    }
}
