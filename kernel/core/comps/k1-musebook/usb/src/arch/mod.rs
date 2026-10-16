// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: riscv64-only USB arch glue (EHCI1 + optional DWC3/xHCI).

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::block_device::UsbBlockDevice;

mod k1_dwc3;
mod k1_ehci1;
mod k1_hid_kbd;

/// `k1.usb3=off` holds the DWC3 / xHCI initialization back at run time, so a
/// kernel built with the `usb` feature can skip USB3 from the DTB alone.
/// EHCI1 (USB2) is unaffected. Default on: the feature flag already says
/// the caller asked for USB3.
static USB3: AtomicBool = AtomicBool::new(true);
aster_cmdline::define_flag_param!("k1.usb3", USB3);

fn xhci_wanted() -> bool {
    // `cfg!`, not `#[cfg]`: the xHCI port stays compiled (and ktested)
    // even when the feature is off; only its boot-time PHY writes are held
    // back.
    cfg!(feature = "xhci") && USB3.load(Ordering::Relaxed)
}

pub(crate) fn init() {
    k1_ehci1::init();
    if cfg!(feature = "xhci") && !USB3.load(Ordering::Relaxed) {
        log::info!("[k1-usb] dwc3/xhci built in but k1.usb3=off; not touching the ComboPHY");
    }
    if xhci_wanted() {
        // Component order puts aster-pci before this crate (Cargo.toml edge),
        // so the RC's Rterm calibration is done before the PHY mode flips.
        log::info!("[k1-usb] dwc3/xhci initialization (after PCIe RC init)");
        k1_dwc3::init();
    }
}

pub(crate) fn scan_and_register() {
    if xhci_wanted() {
        k1_dwc3::scan_and_register();
    }
}

pub(crate) fn poll_hotplug() -> Vec<Arc<UsbBlockDevice>> {
    if xhci_wanted() {
        k1_dwc3::poll_hotplug()
    } else {
        Vec::new()
    }
}
