// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: /sys/class/backlight/lcd_backlight on the virtual-device model.

//! `/sys/class/backlight/lcd_backlight`, following the BAT0 / input_class
//! precedent.
//!
//! [`register_virtual_device`] puts a parentless class device at
//! `/sys/devices/virtual/backlight/<name>`, with `/sys/class/backlight/<name>`
//! a relative symlink. That is the shape `libudev` `match_subsystem("backlight")`
//! needs.
//!
//! Published only when [`aster_framebuffer::backlight_present`] is true
//! (PWM14 came up on K1). Non-K1 is a silent no-op.

use alloc::format;
use core::sync::atomic::{AtomicBool, Ordering};

use aster_systree::{Error as SysTreeError, Result as SysTreeResult, SysStr};

use super::{LiveAttr, LiveRwAttr, VirtualDeviceInfo, register_virtual_device};
use crate::prelude::*;

/// Linux class name. `libudev` / COSMIC look up this exact string.
const CLASS: &str = aster_framebuffer::sysfs::CLASS;

/// DT node name (`pwm_bl: lcd_backlight`).
const NAME: &str = aster_framebuffer::sysfs::NAME;

static PUBLISHED: AtomicBool = AtomicBool::new(false);

/// Publishes `lcd_backlight` if PWM14 is present. Idempotent.
pub(crate) fn publish() -> bool {
    if !aster_framebuffer::backlight_present() {
        return false;
    }
    if PUBLISHED.swap(true, Ordering::AcqRel) {
        return true;
    }

    match register() {
        Ok(()) => {
            info!("sysfs /sys/class/backlight/lcd_backlight");
            true
        }
        Err(err) => {
            PUBLISHED.store(false, Ordering::Release);
            warn!("failed to publish backlight sysfs: {:?}", err);
            false
        }
    }
}

fn register() -> Result<()> {
    let live_attrs: Vec<LiveAttr> = vec![
        (SysStr::from("actual_brightness"), attr_actual_brightness),
        (SysStr::from("uevent"), attr_uevent),
    ];
    let live_rw_attrs: Vec<LiveRwAttr> = vec![
        (
            SysStr::from("brightness"),
            attr_brightness,
            write_brightness,
        ),
        (SysStr::from("bl_power"), attr_bl_power, write_bl_power),
    ];

    register_virtual_device(VirtualDeviceInfo {
        class: SysStr::from(CLASS),
        name: SysStr::from(NAME),
        uevent: String::new(),
        attrs: vec![
            (
                SysStr::from("max_brightness"),
                format!("{}\n", aster_framebuffer::sysfs::MAX_BRIGHTNESS),
            ),
            (
                SysStr::from("type"),
                format!("{}\n", aster_framebuffer::sysfs::TYPE),
            ),
            (
                SysStr::from("scale"),
                format!("{}\n", aster_framebuffer::sysfs::SCALE),
            ),
        ],
        attr_groups: Vec::new(),
        live_attrs,
        live_rw_attrs,
    })?;
    Ok(())
}

fn snap() -> (u32, u32) {
    (
        aster_framebuffer::backlight_brightness(),
        aster_framebuffer::backlight_bl_power(),
    )
}

fn attr_brightness() -> Option<String> {
    let (brightness, bl_power) = snap();
    aster_framebuffer::sysfs::format_attr("brightness", brightness, bl_power)
}

fn attr_actual_brightness() -> Option<String> {
    let (brightness, bl_power) = snap();
    aster_framebuffer::sysfs::format_attr("actual_brightness", brightness, bl_power)
}

fn attr_bl_power() -> Option<String> {
    let (brightness, bl_power) = snap();
    aster_framebuffer::sysfs::format_attr("bl_power", brightness, bl_power)
}

fn attr_uevent() -> Option<String> {
    let (brightness, bl_power) = snap();
    Some(aster_framebuffer::sysfs::format_uevent(
        brightness, bl_power,
    ))
}

/// Linux `backlight_store_brightness`: `kstrtouint`, then `> max` → `-EINVAL`.
fn write_brightness(reader: &mut VmReader) -> SysTreeResult<usize> {
    let (value, n) = parse_u32(reader)?;
    aster_framebuffer::set_backlight_brightness(value)
        .map_err(|_| SysTreeError::InvalidOperation)?;
    Ok(n)
}

/// Linux `backlight_store_bl_power`: any `kstrtoul` value is stored.
fn write_bl_power(reader: &mut VmReader) -> SysTreeResult<usize> {
    let (value, n) = parse_u32(reader)?;
    aster_framebuffer::set_backlight_bl_power(value).map_err(|_| SysTreeError::InvalidOperation)?;
    Ok(n)
}

fn parse_u32(reader: &mut VmReader) -> SysTreeResult<(u32, usize)> {
    // Longest decimal `u32` plus newline: "4294967295\n".
    const MAX: usize = 16;
    let (cstr, n) = reader
        .read_cstring_until_end(MAX)
        .map_err(|_| SysTreeError::PageFault)?;
    let value = cstr
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or(SysTreeError::InvalidOperation)?;
    Ok((value, n))
}
