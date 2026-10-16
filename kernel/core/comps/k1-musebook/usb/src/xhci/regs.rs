// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: xHCI capability / operational / runtime registers.

//! xHCI register offsets (spec 5.3 / 5.4 / 5.5). Capability block is at
//! BAR+0; DWC3 host mode exposes this at `dwc3@c0a00000`.

// Capability (5.3)
pub(crate) const CAP_HCSPARAMS1: usize = 0x04;
pub(crate) const CAP_HCSPARAMS2: usize = 0x08;
pub(crate) const CAP_HCCPARAMS1: usize = 0x10;
pub(crate) const CAP_DBOFF: usize = 0x14;
pub(crate) const CAP_RTSOFF: usize = 0x18;

// Operational, relative to CAPLENGTH (5.4)
pub(crate) const OP_USBCMD: usize = 0x00;
pub(crate) const OP_USBSTS: usize = 0x04;
pub(crate) const OP_PAGESIZE: usize = 0x08;
pub(crate) const OP_DNCTRL: usize = 0x14;
pub(crate) const OP_CRCR: usize = 0x18;
pub(crate) const OP_DCBAAP: usize = 0x30;
pub(crate) const OP_CONFIG: usize = 0x38;
pub(crate) const OP_PORTSC: usize = 0x400; // port 1; +0x10 per port (5.4.8)

// USBCMD — 5.4.1
pub(crate) const CMD_RS: u32 = 1 << 0;
pub(crate) const CMD_HCRST: u32 = 1 << 1;

// USBSTS — 5.4.2
pub(crate) const STS_HCH: u32 = 1 << 0;
pub(crate) const STS_EINT: u32 = 1 << 3;
pub(crate) const STS_CNR: u32 = 1 << 11;

// CRCR — 5.4.5
pub(crate) const CRCR_RCS: u64 = 1;

// PORTSC — 5.4.8
pub(crate) const PORT_CCS: u32 = 1 << 0;
pub(crate) const PORT_PED: u32 = 1 << 1;
pub(crate) const PORT_PR: u32 = 1 << 4;
pub(crate) const PORT_PLS_MASK: u32 = 0xf << 5;
pub(crate) const PORT_PP: u32 = 1 << 9;
pub(crate) const PORT_SPEED_SHIFT: u32 = 10;
pub(crate) const PORT_SPEED_MASK: u32 = 0xf << 10;
pub(crate) const PORT_CSC: u32 = 1 << 17;
pub(crate) const PORT_PEC: u32 = 1 << 18;
pub(crate) const PORT_WRC: u32 = 1 << 19;
pub(crate) const PORT_OCC: u32 = 1 << 20;
pub(crate) const PORT_PRC: u32 = 1 << 21;
pub(crate) const PORT_PLC: u32 = 1 << 22;
pub(crate) const PORT_CEC: u32 = 1 << 23;
pub(crate) const PORT_WPR: u32 = 1 << 31;
/// Bits that must be written back as 0 in every PORTSC write: `PED` is
/// RW1CS (writing 1 *disables* the port), `PR`/`WPR` start a reset, and the
/// change bits are RW1C. Linux `xhci_port_state_to_neutral()` does the same
/// masking before every write to avoid disabling an enabled port.
pub(crate) const PORT_WRITE_CLEAR: u32 = PORT_PED | PORT_PR | PORT_WPR | PORT_CHANGE;

/// A PORTSC value safe to write back (clears nothing, starts nothing).
pub(crate) fn port_neutral(portsc: u32) -> u32 {
    portsc & !PORT_WRITE_CLEAR
}

/// RW1C change bits — write 1 to clear, must not be set when asserting PR.
pub(crate) const PORT_CHANGE: u32 =
    PORT_CSC | PORT_PEC | PORT_WRC | PORT_OCC | PORT_PRC | PORT_PLC | PORT_CEC;

// Interrupter 0, relative to RTSOFF + 0x20 (5.5.2)
pub(crate) const IR_IMAN: usize = 0x00;
pub(crate) const IR_ERSTSZ: usize = 0x08;
pub(crate) const IR_ERSTBA: usize = 0x10;
pub(crate) const IR_ERDP: usize = 0x18;
pub(crate) const ERDP_EHB: u64 = 1 << 3;

// HCCPARAMS1 — 5.3.6
pub(crate) const HCC_AC64: u32 = 1 << 0;
pub(crate) const HCC_CSZ: u32 = 1 << 2;
pub(crate) const HCC_PPC: u32 = 1 << 3;

pub(crate) fn max_slots(hcs1: u32) -> u8 {
    (hcs1 & 0xff) as u8
}

#[cfg(any(test, ktest))]
pub(crate) fn max_intrs(hcs1: u32) -> u16 {
    ((hcs1 >> 8) & 0x7ff) as u16
}

pub(crate) fn max_ports(hcs1: u32) -> u8 {
    ((hcs1 >> 24) & 0xff) as u8
}

pub(crate) fn max_scratchpad(hcs2: u32) -> u32 {
    let hi = (hcs2 >> 21) & 0x1f;
    let lo = (hcs2 >> 27) & 0x1f;
    (hi << 5) | lo
}

pub(crate) fn port_speed(portsc: u32) -> u8 {
    ((portsc & PORT_SPEED_MASK) >> PORT_SPEED_SHIFT) as u8
}

pub(crate) fn portsc_off(port: u8) -> usize {
    OP_PORTSC + (port as usize - 1) * 0x10
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn hcsparams1_fields() {
        // MaxSlots=64, MaxIntrs=16, MaxPorts=2
        let h = 64u32 | (16 << 8) | (2 << 24);
        assert_eq!(max_slots(h), 64);
        assert_eq!(max_intrs(h), 16);
        assert_eq!(max_ports(h), 2);
    }

    #[ktest]
    fn scratchpad_split_field() {
        // Hi=1 (bits 25:21), Lo=0 (bits 31:27) → 32
        let hcs2 = 1 << 21;
        assert_eq!(max_scratchpad(hcs2), 32);
        assert_eq!(max_scratchpad(1 << 27), 1);
    }

    #[ktest]
    fn portsc_index() {
        assert_eq!(portsc_off(1), 0x400);
        assert_eq!(portsc_off(2), 0x410);
        assert_eq!(port_speed(3 << 10), 3);
    }
}
