// SPDX-License-Identifier: MPL-2.0

//! The assembler crate for the Asterinas kernel.

#![no_std]
#![no_main]
#![deny(unsafe_code)]

// High-level components as external crates.
//
// Keep this list in sync with the high-level component dependencies in `Cargo.toml`.
// Each high-level component crate must have an explicit `extern crate` declaration
// to ensure that its component registration and initialization code are linked into
// the kernel, because the assembler does not otherwise reference symbols from that crate.
// K1 MUSE Book --- (RISC-V uses the K1 DRM registry and its software fallback)
// extern crate aster_drm as _;
#[cfg(not(target_arch = "riscv64"))]
extern crate aster_drm as _;
// K1 MUSE Book ---
#[cfg(target_arch = "x86_64")]
extern crate aster_i8042 as _;
// K1 MUSE Book --- (RISC-V uses the K1 DRM registry and its software fallback)
// extern crate aster_simpledrm as _;
#[cfg(not(target_arch = "riscv64"))]
extern crate aster_simpledrm as _;
// K1 MUSE Book ---
extern crate aster_uart as _;

#[ostd::main]
fn main() {
    aster_core::boot();
}
