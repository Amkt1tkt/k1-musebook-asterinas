// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: polled xHCI host (single interrupter).

//! Minimal polling xHCI host (single interrupter, no PLIC).
//!
//! Implemented commands: Enable Slot, Address Device, Configure Endpoint
//! (bulk + interrupt IN), Evaluate Context and Disable Slot.
//!
//! Unsupported: streams, isoch, Stop/Reset Endpoint recovery,
//! multiple interrupters, MSI/PLIC (PLIC 125 on K1), USB3 LPM, bandwidth
//! negotiation, Force Header.

mod completion;
pub(crate) mod ownership;

pub(crate) mod context;
pub(crate) mod regs;
pub(crate) mod ring;
pub(crate) mod trb;

mod host;

pub(crate) use host::{Error, Host, PortInfo};
