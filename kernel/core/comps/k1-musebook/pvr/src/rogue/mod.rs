// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: module declarations for generated PowerVR definitions.

//! Hardware register and firmware-interface definitions.
//!
//! The seven child modules are generated from headers pinned in UPSTREAM.
//! This module declaration is handwritten. Use the host-side regen crate;
//! ordinary kernel builds do not run it. The parser supports the pinned input
//! subset, not arbitrary C. See CONVENTIONS.md for coverage requirements.
//!
//! | Module | Upstream header |
//! | --- | --- |
//! | [`cr`] | `pvr_rogue_cr_defs.h`, `pvr_rogue_cr_defs_client.h` |
//! | [`fwif`] | `pvr_rogue_fwif*.h` |
//! | [`sf`] | `pvr_rogue_fwif_sf.h` |
//! | [`mips`] | `pvr_rogue_mips.h` |
//! | [`mmu`] | `pvr_rogue_mmu_defs.h` |
//! | [`heap`] | `pvr_rogue_heap_config.h` |
//! | [`defs`] | `pvr_rogue_defs.h` |
//!
//! `pvr_rogue_meta.h` and `pvr_rogue_riscv.h` are deliberately absent: BXE-2-32
//! has a MIPS firmware processor, so those paths are unreachable for us.
//!
//! # The register subset rule
//!
//! Generate the registers used by the port. The allow-list keeps the reviewed
//! hardware interface explicit and makes missing required definitions fatal.
//! Add names when implementing their consumers instead of copying the full header.

pub mod cr;
pub mod defs;
pub mod fwif;
pub mod heap;
pub mod mips;
pub mod mmu;
pub mod sf;
