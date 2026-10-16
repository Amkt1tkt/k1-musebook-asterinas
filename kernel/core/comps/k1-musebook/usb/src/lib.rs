// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: USB host stack (EHCI1 + optional DWC3/xHCI).

//! USB host drivers for the SpacemiT K1 MUSE Book.
//!
//! - **EHCI1 (USB2 Port1)**: always initialized. Enumerates the onboard hub
//!   and HID boot keyboard/mouse → [`aster_input`].
//! - **DWC3 / xHCI (USB3)**: compiled when the crate `xhci` feature is on
//!   (kernel `FEATURES="… usb …"`). Mass-storage disks become `usbblk*`
//!   block devices; HID boot devices are wired into evdev the same way.
//!
//! Two independent switches:
//!
//! 1. **Compile-time** `xhci` cargo feature (forwarded from `aster-core`'s
//!    `usb` feature). Without it the DWC3/xHCI code is still type-checked
//!    via `cfg!(feature = "xhci")` in a few places, but PHY writes are
//!    never issued.
//! 2. **Runtime** cmdline `k1.usb3=off`. A kernel built with `usb` can
//!    skip DWC3/xHCI initialization from the DTB alone. EHCI1 is unaffected.
//!
//! The crate is always linked on `riscv64` so EHCI1 comes up even when
//! the `usb` feature is off.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use component::{ComponentInitError, init_component};

macro_rules! __log_prefix {
    () => {
        "usb: "
    };
}

mod arch;
mod delay;
mod desc;
mod hid;
mod scsi;
mod xhci;

mod block_device;
mod enumerate;
mod msc;

pub use block_device::UsbBlockDevice;

#[init_component]
fn init() -> Result<(), ComponentInitError> {
    arch::init();
    Ok(())
}

// DmaStream + xHCI start + enum. IRQs must be on (`DmaStream::alloc`).
#[init_component(kthread)]
fn init_kthread() -> Result<(), ComponentInitError> {
    arch::scan_and_register();
    Ok(())
}

/// Poll for a newly attached MSC disk. Returns devices registered this pass.
///
/// Used by the block registry's hotplug loop so boot-attached and later
/// disks get a `handle_requests` kthread.
pub fn poll_hotplug() -> alloc::vec::Vec<alloc::sync::Arc<UsbBlockDevice>> {
    arch::poll_hotplug()
}

#[cfg(ktest)]
mod failure_test;
