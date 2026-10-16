// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: publish evdev nodes under /sys/class/input so libudev/libinput can enumerate them.

//! The sysfs entries of the `input` class.
//!
//! An evdev node that exists only in `/dev` is invisible to `libudev`, and thus
//! to every user space input stack that enumerates through it. `libinput`'s udev
//! backend, which is the only backend `cosmic-comp` uses, asks `libudev` for the
//! devices in the `input` subsystem and opens what it gets back; with no
//! `/sys/class/input` it gets back nothing and reports no failure.
//!
//! # What user space reads
//!
//! `libudev`'s enumerator scans `/sys/class/<subsystem>`, resolves each entry,
//! and refuses any device under `/sys/devices` that has no `uevent` file
//! (`sd_device_new_from_syspath`); it then decides the device's subsystem by
//! reading the `subsystem` link, and treats every unrecognized `uevent` key as a
//! device property (`handle_uevent_line`). So the `uevent` file and the
//! `subsystem` link are what make a device exist and be findable at all.
//!
//! The properties that decide whether `libinput` will touch a device —
//! `ID_INPUT`, `ID_INPUT_KEYBOARD`, `ID_INPUT_TOUCHPAD` and the rest — are not
//! kernel properties on Linux either: `60-input-id.rules` computes them from
//! sysfs with the `input_id` builtin. This module therefore publishes exactly the
//! inputs that computation needs, and nothing that pretends to be its output:
//!
//! - `capabilities/{ev,key,rel,abs}` and `properties` on the input device, which
//!   is where `input_id` reads the bitmaps from;
//! - `id/{bustype,vendor,product,version}`, which it reads the device ids from;
//! - the same bitmaps as `EV=`, `KEY=` and `REL=` `uevent` keys, which is where a
//!   udev implementation without a database — `libudev-zero` — reads them from.
//!
//! The matching userspace implementations are systemd's `udev-builtin-input_id`
//! and libudev-zero's input capability parser.
//!
//! # The shape of the tree
//!
//! Two devices per evdev node, as in Linux: the input device that owns the
//! capabilities, and the evdev handle that owns the device node. Both are in the
//! `input` class, and the handle is a child of the input device.
//!
//! ```text
//! /sys/devices/virtual/input/input0/
//!     uevent  name  phys  uniq  properties
//!     id/{bustype,vendor,product,version}
//!     capabilities/{ev,key,rel,abs,msc,led,snd,ff,sw}
//!     subsystem -> ../../../../class/input
//!     event0/
//!         uevent  dev
//!         device -> ../../input0
//!         subsystem -> ../../../../../class/input
//! /sys/class/input/input0 -> ../../devices/virtual/input/input0
//! /sys/class/input/event0 -> ../../devices/virtual/input/input0/event0
//! /sys/dev/char/13:0 -> ../../devices/virtual/input/input0/event0
//! ```
//!
//! Linux parents an input device on whatever physical device produced it, so a
//! USB keyboard's input device really lives under its HID interface. This kernel
//! has no sysfs entries for those buses, so the input devices are parentless,
//! which puts them under `/sys/devices/virtual/input` — the same place Linux puts
//! a parentless class device, `uinput`'s devices among them. Nothing in the input
//! stack walks above the input device.

use alloc::format;
use core::fmt::Write;

use aster_input::{event_type_codes::EventTypes, input_dev::InputDevice};
use aster_systree::SysStr;
use device_id::DeviceId;

use crate::{
    fs::sysfs::{self, AttrFile},
    prelude::*,
};

/// The name of the input class, as it appears in `/sys/class`.
const INPUT_CLASS: &str = "input";

/// The number of bits in a byte of a bitmap.
const BITS_PER_BYTE: usize = 8;

/// The highest input device property code.
///
/// This and the constants below are the `*_MAX` values of the event code ABI, and
/// decide how wide each printed bitmap is.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/linux/input-event-codes.h>.
const INPUT_PROP_MAX: usize = 0x1f;
/// The highest event type code.
const EV_MAX: usize = 0x1f;
/// The highest key code.
const KEY_MAX: usize = 0x2ff;
/// The highest relative axis code.
const REL_MAX: usize = 0x0f;
/// The highest absolute axis code.
const ABS_MAX: usize = 0x3f;
/// The highest miscellaneous event code.
const MSC_MAX: usize = 0x07;
/// The highest LED code.
const LED_MAX: usize = 0x0f;
/// The highest sound code.
const SND_MAX: usize = 0x07;
/// The highest force feedback code.
const FF_MAX: usize = 0x7f;
/// The highest switch code.
const SW_MAX: usize = 0x10;

/// Publishes an evdev node and the input device behind it in sysfs.
///
/// The two devices are named after the evdev minor number: minor `0` becomes
/// `input0` holding `event0`. Linux numbers input devices and evdev handles from
/// two counters that happen to agree when every input device has an evdev handle
/// and nothing else creates input devices, which is the case here.
pub(super) fn register(id: DeviceId, device: &dyn InputDevice) -> Result<()> {
    let index = id.minor().get();
    let event_name = format!("event{}", index);

    let input_device = sysfs::register_virtual_device(sysfs::VirtualDeviceInfo {
        class: SysStr::from(INPUT_CLASS),
        name: SysStr::from(format!("input{}", index)),
        uevent: input_uevent(device),
        attrs: input_attrs(device),
        attr_groups: vec![
            (SysStr::from("id"), id_attrs(device)),
            (SysStr::from("capabilities"), capability_attrs(device)),
        ],
        live_attrs: Vec::new(),
        live_rw_attrs: Vec::new(),
    })?;

    // A failure here leaves an input device with no evdev handle under it. That
    // is inert rather than misleading: `libinput` ignores every device whose name
    // does not begin with `event`.
    input_device.register_char_device(sysfs::CharDeviceInfo {
        class: SysStr::from(INPUT_CLASS),
        name: SysStr::from(event_name.clone()),
        // Has to agree with the device node `EvdevDevice::devtmpfs_meta` asks
        // devtmpfs for, since user space opens `/dev/<DEVNAME>`.
        dev_name: format!("input/{}", event_name),
        // Linux's evdev handle has no `device_type`, so it emits no `DEVTYPE`.
        dev_type: None,
        id,
    })?;

    Ok(())
}

/// Builds the `uevent` contents of an input device.
///
/// The keys, their order, and the condition under which each appears follow
/// Linux's `input_dev_uevent`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/input/input.c#L1683>.
fn input_uevent(device: &dyn InputDevice) -> String {
    // Writing into a `String` cannot fail, hence the discarded results.
    let mut text = String::new();

    let id = device.id();
    let _ = writeln!(
        text,
        "PRODUCT={:x}/{:x}/{:x}/{:x}",
        id.bustype(),
        id.vendor(),
        id.product(),
        id.version()
    );
    // Linux emits these three only for a device that has them, and the quotes are
    // part of the value.
    for (key, value) in [
        ("NAME", device.name()),
        ("PHYS", device.phys()),
        ("UNIQ", device.uniq()),
    ] {
        if !value.is_empty() {
            let _ = writeln!(text, "{}=\"{}\"", key, value);
        }
    }

    let capability = device.capability();
    let _ = writeln!(
        text,
        "PROP={}",
        print_bitmap(capability.properties_bitmap(), INPUT_PROP_MAX)
    );
    let _ = writeln!(
        text,
        "EV={}",
        print_bitmap(&capability.event_types_bits().to_le_bytes(), EV_MAX)
    );
    if capability.support_event_type(EventTypes::KEY) {
        let _ = writeln!(
            text,
            "KEY={}",
            print_bitmap(capability.supported_keys_bitmap(), KEY_MAX)
        );
    }
    if capability.support_event_type(EventTypes::REL) {
        let _ = writeln!(
            text,
            "REL={}",
            print_bitmap(capability.supported_relative_axes_bitmap(), REL_MAX)
        );
    }
    if capability.support_event_type(EventTypes::ABS) {
        let _ = writeln!(
            text,
            "ABS={}",
            print_bitmap(capability.supported_absolute_axes_bitmap(), ABS_MAX)
        );
    }
    if capability.support_event_type(EventTypes::SW) {
        let _ = writeln!(
            text,
            "SW={}",
            print_bitmap(capability.supported_switches_bitmap(), SW_MAX)
        );
    }

    // Divergence: Linux ends with a `MODALIAS` that spells out every capability
    // bit, some 800 bytes for a keyboard. Nothing here would read it: there is no
    // module autoloading, no hwdb, and `libinput`'s quirks match on the evdev
    // ioctls rather than on udev properties.
    text
}

/// Builds the attribute files an input device carries directly.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/input/input.c#L1488>.
fn input_attrs(device: &dyn InputDevice) -> Vec<AttrFile> {
    vec![
        (SysStr::from("name"), format!("{}\n", device.name())),
        (SysStr::from("phys"), format!("{}\n", device.phys())),
        (SysStr::from("uniq"), format!("{}\n", device.uniq())),
        (
            SysStr::from("properties"),
            format!(
                "{}\n",
                print_bitmap(device.capability().properties_bitmap(), INPUT_PROP_MAX)
            ),
        ),
    ]
}

/// Builds the `id` attribute group of an input device.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/input/input.c#L1502>.
fn id_attrs(device: &dyn InputDevice) -> Vec<AttrFile> {
    let id = device.id();

    [
        ("bustype", id.bustype()),
        ("vendor", id.vendor()),
        ("product", id.product()),
        ("version", id.version()),
    ]
    .into_iter()
    .map(|(name, value)| (SysStr::from(name), format!("{:04x}\n", value)))
    .collect()
}

/// Builds the `capabilities` attribute group of an input device.
///
/// Every one of the nine files exists on Linux whether or not the device supports
/// that event type, and an unsupported one reads as `0`. `input_id` reads four of
/// them and gives up on a device whose `capabilities/ev` it cannot read, so the
/// unsupported ones are not optional.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/input/input.c#L1582>.
fn capability_attrs(device: &dyn InputDevice) -> Vec<AttrFile> {
    let capability = device.capability();
    let event_types = capability.event_types_bits().to_le_bytes();

    // The event types this kernel has no bitmaps for read as an empty bitmap,
    // which is also what Linux prints for a device that supports none of them.
    let bitmaps: [(&str, &[u8], usize); 9] = [
        ("ev", &event_types, EV_MAX),
        ("key", capability.supported_keys_bitmap(), KEY_MAX),
        ("rel", capability.supported_relative_axes_bitmap(), REL_MAX),
        ("abs", capability.supported_absolute_axes_bitmap(), ABS_MAX),
        ("msc", &[], MSC_MAX),
        ("led", &[], LED_MAX),
        ("snd", &[], SND_MAX),
        ("ff", &[], FF_MAX),
        ("sw", capability.supported_switches_bitmap(), SW_MAX),
    ];

    bitmaps
        .into_iter()
        .map(|(name, bitmap, max_bit)| {
            (
                SysStr::from(name),
                format!("{}\n", print_bitmap(bitmap, max_bit)),
            )
        })
        .collect()
}

/// Renders a bitmap the way Linux prints one for user space: the machine words of
/// the bitmap in hexadecimal, most significant word first, separated by single
/// spaces, with the leading all-zero words left out and a lone `0` for a bitmap
/// that is empty.
///
/// `bitmap` holds the bits least significant byte first, as the evdev ioctls
/// report them, and may be shorter than `max_bit` implies; the missing bytes are
/// zero. `max_bit` is the highest code the bitmap can hold, which is what decides
/// how many words Linux prints.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/input/input.c#L1530>.
fn print_bitmap(bitmap: &[u8], max_bit: usize) -> String {
    const WORD_BYTES: usize = size_of::<usize>();

    let word_at = |index: usize| -> usize {
        let mut word = 0;
        for offset in 0..WORD_BYTES {
            let byte = bitmap
                .get(index * WORD_BYTES + offset)
                .copied()
                .unwrap_or(0);
            word |= (byte as usize) << (offset * BITS_PER_BYTE);
        }
        word
    };

    let mut text = String::new();
    for index in (0..max_bit.div_ceil(WORD_BYTES * BITS_PER_BYTE)).rev() {
        let word = word_at(index);
        // Leading zero words are not printed at all, but a zero word that follows
        // a printed one is.
        if word == 0 && text.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        let _ = write!(text, "{:x}", word);
    }
    if text.is_empty() {
        text.push('0');
    }

    text
}

#[cfg(ktest)]
mod test;
