// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: ktests for input_class bitmap/uevent formatting (libudev ABI).

use aster_input::{
    event_type_codes::{AbsCode, AbsInfo, InputProp, KeyCode, RelCode},
    input_dev::{InputCapability, InputId},
};
use ostd::prelude::ktest;

use super::*;

/// A stand-in for a driver's input device, carrying whatever the test needs it to
/// report.
#[derive(Debug)]
struct FakeInputDevice {
    name: String,
    phys: String,
    id: InputId,
    capability: InputCapability,
}

impl InputDevice for FakeInputDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn phys(&self) -> &str {
        &self.phys
    }

    fn uniq(&self) -> &str {
        ""
    }

    fn id(&self) -> InputId {
        self.id
    }

    fn capability(&self) -> &InputCapability {
        &self.capability
    }
}

/// A keyboard reporting `KEY_ESC`, `KEY_ENTER` and `KEY_A`, which are codes 1, 28
/// and 30 and thus all in the lowest word of the key bitmap.
fn keyboard() -> FakeInputDevice {
    let mut capability = InputCapability::new();
    capability.set_supported_event_type(EventTypes::SYN);
    for key in [KeyCode::Esc, KeyCode::Enter, KeyCode::A] {
        capability.set_supported_key(key);
    }

    FakeInputDevice {
        name: "Fake USB Keyboard".to_string(),
        phys: "usb-0000:00:01.0-1/input0".to_string(),
        id: InputId::new(InputId::BUS_USB, 0x1234, 0x5678, 0x0111),
        capability,
    }
}

/// A mouse reporting `REL_X`, `REL_Y` and `BTN_LEFT`, the last of which is code
/// 0x110 and thus in the fifth word of the key bitmap.
fn mouse() -> FakeInputDevice {
    let mut capability = InputCapability::new();
    capability.set_supported_event_type(EventTypes::SYN);
    capability.set_supported_key(KeyCode::BtnLeft);
    for axis in [RelCode::X, RelCode::Y] {
        capability.set_supported_relative_axis(axis);
    }

    FakeInputDevice {
        name: "Fake Touchpad".to_string(),
        phys: String::new(),
        id: InputId::new(InputId::BUS_I2C, 0x0000, 0x0000, 0x0000),
        capability,
    }
}

#[ktest]
fn an_empty_bitmap_prints_as_a_single_zero() {
    assert_eq!(print_bitmap(&[], KEY_MAX), "0");
    assert_eq!(print_bitmap(&[0; 96], KEY_MAX), "0");
}

/// Leading all-zero words are left out, but a zero word between two printed ones
/// is not. Getting this wrong shifts every bit in the map by a whole word, which
/// user space reads as a completely different device.
#[ktest]
fn a_bitmap_prints_its_words_from_the_top_and_skips_only_leading_zeros() {
    // Bit 1 in the lowest word, bit 64 in the next one.
    let mut bitmap = [0u8; 96];
    bitmap[0] = 0b10;
    bitmap[8] = 0b1;
    assert_eq!(print_bitmap(&bitmap, KEY_MAX), "1 2");

    // Only the highest word of a twelve-word map is set, so the eleven zero words
    // below it are all printed.
    let mut bitmap = [0u8; 96];
    bitmap[88] = 0b1;
    assert_eq!(print_bitmap(&bitmap, KEY_MAX), "1 0 0 0 0 0 0 0 0 0 0 0");
}

/// A bitmap shorter than its event type's code space is zero-extended, which is
/// how the relative axis map, sixteen bits wide, prints as a full word.
#[ktest]
fn a_bitmap_shorter_than_its_code_space_is_zero_extended() {
    assert_eq!(print_bitmap(&[0b11], REL_MAX), "3");
}

/// `libudev-zero` derives `ID_INPUT` and the device type entirely from these
/// keys, so their absence is the difference between a compositor that sees a
/// keyboard and one that sees nothing.
#[ktest]
fn the_uevent_of_a_keyboard_carries_the_bitmaps_a_udev_reads() {
    let text = input_uevent(&keyboard());

    assert!(text.contains("PRODUCT=3/1234/5678/111\n"));
    assert!(text.contains("NAME=\"Fake USB Keyboard\"\n"));
    assert!(text.contains("PHYS=\"usb-0000:00:01.0-1/input0\"\n"));
    assert!(text.contains("PROP=0\n"));
    // `EV_SYN` and `EV_KEY`, codes 0 and 1.
    assert!(text.contains("EV=3\n"));
    // `KEY_ESC`, `KEY_ENTER` and `KEY_A`, codes 1, 28 and 30.
    assert!(text.contains("KEY=50000002\n"));
    // Linux emits `REL=` only for a device that reports `EV_REL`.
    assert!(!text.contains("REL="));
}

fn touchpad() -> FakeInputDevice {
    let mut capability = InputCapability::new();
    capability.set_supported_event_type(EventTypes::SYN);
    capability.set_supported_key(KeyCode::BtnTouch);
    capability.set_supported_key(KeyCode::BtnToolFinger);
    capability.set_supported_absolute_axis(AbsCode::X, AbsInfo::range(0, 4095));
    capability.set_supported_absolute_axis(AbsCode::Y, AbsInfo::range(0, 4095));
    capability.set_property(InputProp::Pointer);
    capability.set_property(InputProp::ButtonPad);

    FakeInputDevice {
        name: "Fake MT Touchpad".to_string(),
        phys: String::new(),
        id: InputId::new(InputId::BUS_I2C, 0x0911, 0x5288, 0x0001),
        capability,
    }
}

#[ktest]
fn the_uevent_of_a_touchpad_carries_abs_and_pointer_prop() {
    let text = input_uevent(&touchpad());

    // SYN + KEY + ABS (bits 0, 1, 3).
    assert!(text.contains("EV=b\n"));
    assert!(text.contains("ABS="));
    // INPUT_PROP_POINTER | INPUT_PROP_BUTTONPAD = bits 0 and 2.
    assert!(text.contains("PROP=5\n"));
    assert_eq!(
        input_attrs(&touchpad())
            .iter()
            .find(|(name, _)| name.as_ref() == "properties")
            .map(|(_, v)| v.as_str()),
        Some("5\n")
    );
}

#[ktest]
fn the_uevent_of_a_pointer_carries_the_relative_axes() {
    let text = input_uevent(&mouse());

    // `EV_SYN`, `EV_KEY` and `EV_REL`, codes 0, 1 and 2.
    assert!(text.contains("EV=7\n"));
    assert!(text.contains("REL=3\n"));
    // `BTN_LEFT` is code 0x110, which lands in the fifth word.
    assert!(text.contains("KEY=10000 0 0 0 0\n"));
}

/// Linux omits `NAME`, `PHYS` and `UNIQ` for a device that has none rather than
/// emitting an empty value, and a udev takes an empty property as present.
#[ktest]
fn the_uevent_omits_the_strings_a_device_does_not_have() {
    let text = input_uevent(&mouse());

    assert!(text.contains("NAME=\"Fake Touchpad\"\n"));
    assert!(!text.contains("PHYS="));
    assert!(!text.contains("UNIQ="));
}

/// `input_id` gives up on a device whose `capabilities/ev` it cannot read, and
/// reads three more of these files, so the group has to be complete even though
/// this kernel tracks only three of the nine bitmaps.
#[ktest]
fn the_capabilities_group_has_a_file_for_every_event_type() {
    let attrs = capability_attrs(&mouse());

    let names: Vec<&str> = attrs.iter().map(|(name, _)| name.as_ref()).collect();
    assert_eq!(
        names,
        ["ev", "key", "rel", "abs", "msc", "led", "snd", "ff", "sw"]
    );

    let value_of = |wanted: &str| -> String {
        attrs
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.clone())
            .unwrap()
    };
    assert_eq!(value_of("ev"), "7\n");
    assert_eq!(value_of("rel"), "3\n");
    assert_eq!(value_of("key"), "10000 0 0 0 0\n");
    // The event types this kernel has no bitmaps for read as an empty bitmap.
    assert_eq!(value_of("abs"), "0\n");
    assert_eq!(value_of("sw"), "0\n");
}

/// `60-input-id.rules` matches on `ATTRS{id/bustype}=="0003"`, so the padding is
/// part of the interface rather than cosmetic.
#[ktest]
fn the_id_group_prints_four_hexadecimal_digits_per_field() {
    let attrs = id_attrs(&keyboard());

    assert_eq!(
        attrs,
        [
            ("bustype".into(), "0003\n".to_string()),
            ("vendor".into(), "1234\n".to_string()),
            ("product".into(), "5678\n".to_string()),
            ("version".into(), "0111\n".to_string()),
        ]
    );
}

#[ktest]
fn the_input_device_attributes_end_in_a_newline_like_every_sysfs_string() {
    let attrs = input_attrs(&keyboard());

    assert_eq!(
        attrs,
        [
            ("name".into(), "Fake USB Keyboard\n".to_string()),
            ("phys".into(), "usb-0000:00:01.0-1/input0\n".to_string()),
            ("uniq".into(), "\n".to_string()),
            ("properties".into(), "0\n".to_string()),
        ]
    );
}
