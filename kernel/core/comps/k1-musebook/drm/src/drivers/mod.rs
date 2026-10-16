// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Drivers that ship with the DRM core itself.
//!
//! Hardware drivers do not belong here. The K1 eDP pipeline is registered by
//! `aster-framebuffer`, which owns the DPU registers; see `../../ARCHITECTURE.md`
//! for why that ownership matters.
//!
//! What does belong here is [`soft`], a driver with no hardware behind it, so
//! that the core can be exercised under QEMU where there is no display engine to
//! talk to. It supports ioctl and mode-setting regression tests.

pub mod soft;
