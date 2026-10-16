// SPDX-License-Identifier: MIT OR GPL-2.0-only

// K1 MUSE Book: safe-Rust PowerVR BXE-2-32 driver for the SpacemiT K1.
//! Imagination PowerVR support for the SpacemiT K1's IMG BXE-2-32.
//!
//! This ports Linux's open `drivers/gpu/drm/imagination` driver, whose module name
//! is `powervr`, for Mesa's PVR Vulkan userspace. It does not implement the DDK
//! `img-rogue`/`pvrsrvkm` bridge interface. Each module names its Linux source;
//! the fixed revision is recorded in this crate's `UPSTREAM` file.
//!
//! The GPU identity is read from hardware. Feature and quirk masks come from
//! the matching firmware image, rather than a kernel per-GPU table. K1 uses
//! `powervr/rogue_36.29.52.182_v1.fw`, BVNC 36.29.52.182, and a MIPS firmware
//! processor independently of the host's RISC-V architecture.
//!
//! The firmware is a separate redistributable binary under `LICENSE.powervr`.
//! `PVR_FW_FLAGS_OPEN_SOURCE` selects the supported protocol and does not assert
//! that firmware source is available.
//!
//! Shared layouts are checked using generated `sizeof`/`offsetof` assertions in
//! [`rogue::fwif`] and explicit assertions for handwritten structures. These
//! checks enforce the encoded layouts, not firmware or hardware behavior.
#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

extern crate alloc;
// Overrides the built-in `derive` so that `#[derive(Pod)]` works on the uapi and
// firmware-shared structures. See the `ostd-pod` crate documentation.
#[macro_use]
extern crate ostd_pod;

// Set this crate's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "pvr: "
    };
}

pub mod device;
pub mod device_info;
pub mod driver;
pub mod fw;
mod knobs;
pub mod live;
pub mod mm;
pub mod platform;
pub mod power;
pub mod render;
pub mod rogue;
pub mod sched;
pub mod service;
pub mod sync;
pub mod uapi;

use component::{ComponentInitError, init_component};

pub use crate::{
    device::{GpuId, PvrDevice},
    device_info::DeviceInfo,
    error::{PvrError, PvrResult},
};

pub mod error;

// Probe registers and platform identity during component initialization.
// The kernel supplies firmware to `platform::attach` after mounting the root
// filesystem. Attachment registers the DRM device; runtime PM boots firmware.
#[init_component]
fn init() -> Result<(), ComponentInitError> {
    platform::probe()
}
