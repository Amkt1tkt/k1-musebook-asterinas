// SPDX-License-Identifier: MPL-2.0

use ostd::arch::boot::DEVICE_TREE;

mod ns16550a;
mod sifive;

// K1 MUSE Book --- (RustSBI owns the K1 UART and its interrupt source)
mod k1;
// K1 MUSE Book ---

pub(super) fn init() {
    let device_tree = DEVICE_TREE.get().unwrap();

    // K1 MUSE Book --- (keep direct UART IRQ handling on other RISC-V platforms)
    if device_tree
        .find_compatible(&["spacemit,k1-x", "spacemit,k1"])
        .is_some()
    {
        k1::init();
        return;
    }
    // K1 MUSE Book ---

    if let Some(ns16550a_node) = device_tree.find_compatible(&ns16550a::FDT_COMPATIBLES) {
        ns16550a::init(ns16550a_node);
        return;
    }

    if let Some(sifive_node) = device_tree.find_compatible(&sifive::FDT_COMPATIBLES) {
        sifive::init(sifive_node);
    }
}
