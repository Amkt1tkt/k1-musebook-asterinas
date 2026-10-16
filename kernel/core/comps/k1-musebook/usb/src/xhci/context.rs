// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: xHCI device / input / output context layouts.

//! Device / slot / endpoint context encode (xHCI 6.2). 32-byte contexts;
//! 64-byte (CSZ=1) is the same fields plus reserved padding.

/// Slot Context speed (xHCI 7.2.2.1.1 / Table 7-13), also PORTSC Port Speed.
pub(crate) const SPEED_FS: u8 = 1;
pub(crate) const SPEED_LS: u8 = 2;
pub(crate) const SPEED_HS: u8 = 3;
pub(crate) const SPEED_SS: u8 = 4;
pub(crate) const SPEED_SSP: u8 = 5;

/// Endpoint Type (xHCI 6.2.3 Table 6-7).
pub(crate) const EP_BULK_OUT: u8 = 2;
pub(crate) const EP_CONTROL: u8 = 4;
pub(crate) const EP_BULK_IN: u8 = 6;
pub(crate) const EP_INT_IN: u8 = 7;

/// DCI of the default control endpoint.
pub(crate) const EP0_DCI: u8 = 1;

/// DCI for endpoint address `ep` (USB `bEndpointAddress`).
pub(crate) fn dci_of(ep_addr: u8) -> u8 {
    let num = ep_addr & 0x0f;
    if num == 0 {
        return EP0_DCI;
    }
    if ep_addr & 0x80 != 0 {
        num * 2 + 1
    } else {
        num * 2
    }
}

pub(crate) fn context_bytes(csz64: bool) -> usize {
    if csz64 { 64 } else { 32 }
}

/// Input Control Context (6.2.5.1): D0 = Drop flags, D1 = Add flags.
/// A0 = Slot, A1 = EP0, An = EP DCI n.
pub(crate) fn input_control(add: u32, drop: u32) -> [u32; 8] {
    let mut w = [0u32; 8];
    w[0] = drop;
    w[1] = add;
    w
}

/// Slot Context dwords 0..3 (6.2.2). Remaining words stay 0.
#[expect(
    clippy::too_many_arguments,
    reason = "arguments encode the independent xHCI 6.2.2 Slot Context fields"
)]
pub(crate) fn slot_context(
    speed: u8,
    context_entries: u8,
    root_port: u8,
    route: u32,
    hub: bool,
    nports: u8,
    tt_hub_slot: u8,
    tt_port: u8,
) -> [u32; 8] {
    let mut w = [0u32; 8];
    w[0] = (route & 0x0f_ffff)
        | ((speed as u32 & 0xf) << 20)
        | (u32::from(hub) << 26)
        | ((context_entries as u32 & 0x1f) << 27);
    w[1] = ((root_port as u32) << 16) | ((nports as u32) << 24);
    w[2] = (tt_hub_slot as u32) | ((tt_port as u32) << 8);
    w
}

/// Endpoint Context (6.2.3). `tr_deq` already includes DCS in bit 0.
pub(crate) fn ep_context(
    ep_type: u8,
    max_packet: u16,
    tr_deq: u64,
    interval: u8,
    max_burst: u8,
    avg_trb: u16,
    cerr: u8,
) -> [u32; 8] {
    let mut w = [0u32; 8];
    w[0] = (interval as u32) << 16;
    w[1] = ((cerr as u32 & 3) << 1)
        | ((ep_type as u32 & 7) << 3)
        | ((max_burst as u32) << 8)
        | ((max_packet as u32) << 16);
    w[2] = tr_deq as u32;
    w[3] = (tr_deq >> 32) as u32;
    w[4] = avg_trb as u32;
    w
}

/// EP0 max packet from PORTSC speed (before the device descriptor is read).
pub(crate) fn ep0_max_packet(speed: u8) -> u16 {
    match speed {
        SPEED_LS => 8,
        SPEED_FS => 64, // start 64; Evaluate Context if bMaxPacketSize0 differs
        SPEED_HS => 64,
        SPEED_SS | SPEED_SSP => 512,
        _ => 64,
    }
}

/// Convert USB `bInterval` to the xHCI Endpoint Context Interval exponent
/// (xHCI 6.2.3.6): period = 125µs × 2^Interval.
///
/// HS/SS: `bInterval` is already 1..=16 (period = 125µs × 2^(bInterval-1)).
/// FS/LS interrupt: `bInterval` is milliseconds.
pub(crate) fn xhci_ep_interval(speed: u8, b_interval: u8) -> u8 {
    match speed {
        SPEED_HS | SPEED_SS | SPEED_SSP => b_interval.saturating_sub(1).min(15),
        _ => {
            let units = (b_interval as u32).max(1).saturating_mul(8);
            let exp = 31 - units.leading_zeros();
            (exp as u8).clamp(3, 10)
        }
    }
}

/// USB2 `bMaxPacketSize0` is the size; USB3 is a 2^N exponent (usually 9 → 512).
pub(crate) fn decode_ep0_mps(speed: u8, b_max_packet_size0: u8) -> u16 {
    if speed >= SPEED_SS {
        1u16.checked_shl(b_max_packet_size0 as u32).unwrap_or(512)
    } else {
        b_max_packet_size0 as u16
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn dci_mapping() {
        assert_eq!(dci_of(0x00), 1);
        assert_eq!(dci_of(0x01), 2); // EP1 OUT
        assert_eq!(dci_of(0x81), 3); // EP1 IN
        assert_eq!(dci_of(0x02), 4);
        assert_eq!(dci_of(0x82), 5);
    }

    #[ktest]
    fn input_control_add_slot_and_ep0() {
        let ic = input_control(0b11, 0);
        assert_eq!(ic[1], 0b11);
        assert_eq!(ic[0], 0);
    }

    #[ktest]
    fn slot_context_speed_port_route() {
        let s = slot_context(SPEED_HS, 1, 2, 0x5, false, 0, 0, 0);
        assert_eq!(s[0] & 0x0f_ffff, 0x5);
        assert_eq!((s[0] >> 20) & 0xf, SPEED_HS as u32);
        assert_eq!((s[0] >> 27) & 0x1f, 1);
        assert_eq!((s[1] >> 16) & 0xff, 2);
    }

    #[ktest]
    fn ep_context_control_deq_and_mps() {
        let deq = 0x0000_0000_0001_2001u64; // addr | DCS
        let e = ep_context(EP_CONTROL, 64, deq, 0, 0, 8, 3);
        assert_eq!((e[1] >> 3) & 7, EP_CONTROL as u32);
        assert_eq!((e[1] >> 16) & 0xffff, 64);
        assert_eq!((e[1] >> 1) & 3, 3);
        assert_eq!(e[2] as u64 | ((e[3] as u64) << 32), deq);
    }

    #[ktest]
    fn ep0_mps_usb2_vs_usb3() {
        assert_eq!(ep0_max_packet(SPEED_HS), 64);
        assert_eq!(ep0_max_packet(SPEED_SS), 512);
        assert_eq!(decode_ep0_mps(SPEED_HS, 64), 64);
        assert_eq!(decode_ep0_mps(SPEED_SS, 9), 512);
    }

    #[ktest]
    fn interrupt_interval_hs_vs_fs() {
        // HS bInterval=4 → 2^3 × 125µs = 1ms.
        assert_eq!(xhci_ep_interval(SPEED_HS, 4), 3);
        assert_eq!(xhci_ep_interval(SPEED_SS, 1), 0);
        // FS 8ms → 8×8 = 64 = 2^6 × 125µs.
        assert_eq!(xhci_ep_interval(SPEED_FS, 8), 6);
        // FS 1ms is the USB2 floor (Interval=3).
        assert_eq!(xhci_ep_interval(SPEED_FS, 1), 3);
    }
}
