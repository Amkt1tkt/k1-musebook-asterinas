// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: xHCI Transfer Request Block encode/decode.

//! xHCI TRB encode/decode (spec 4.11 / 6.4). No MMIO.

/// TRB size in bytes (xHCI 6.4).
pub(crate) const TRB_SIZE: usize = 16;

// Transfer TRB types — xHCI 6.4.6 Table 6-91 / Linux `xhci.h` `TRB_*`.
pub(crate) const TRB_NORMAL: u32 = 1;
pub(crate) const TRB_SETUP: u32 = 2;
pub(crate) const TRB_DATA: u32 = 3;
pub(crate) const TRB_STATUS: u32 = 4;
pub(crate) const TRB_LINK: u32 = 6;
pub(crate) const TRB_ENABLE_SLOT: u32 = 9;
pub(crate) const TRB_DISABLE_SLOT: u32 = 10;
pub(crate) const TRB_ADDR_DEV: u32 = 11;
pub(crate) const TRB_CONFIG_EP: u32 = 12;
pub(crate) const TRB_EVAL_CONTEXT: u32 = 13;
pub(crate) const TRB_TRANSFER: u32 = 32;
pub(crate) const TRB_COMPLETION: u32 = 33;
pub(crate) const TRB_PORT_STATUS: u32 = 34;

// Control dword bits — xHCI 6.4.1.
pub(crate) const TRB_CYCLE: u32 = 1 << 0;
pub(crate) const TRB_ISP: u32 = 1 << 2;
pub(crate) const TRB_IOC: u32 = 1 << 5;
pub(crate) const TRB_IDT: u32 = 1 << 6;
pub(crate) const TRB_TC: u32 = 1 << 1; // Link TRB Toggle Cycle (same bit as ENT)
pub(crate) const TRB_BSR: u32 = 1 << 9; // Address Device: Block Set Address Request

/// Setup Stage TRT (bits 17:16) — xHCI 6.4.1.2.1.
pub(crate) const TRT_NO_DATA: u32 = 0;
pub(crate) const TRT_OUT_DATA: u32 = 2;
pub(crate) const TRT_IN_DATA: u32 = 3;

/// Completion codes used on the happy path — xHCI 6.4.5 Table 6-90.
pub(crate) const COMP_SUCCESS: u8 = 1;
pub(crate) const COMP_SHORT_PACKET: u8 = 13;
pub(crate) const COMP_STALL: u8 = 6;

pub(crate) fn pack(param: u64, status: u32, control: u32) -> [u32; 4] {
    [param as u32, (param >> 32) as u32, status, control]
}

pub(crate) fn param(words: &[u32; 4]) -> u64 {
    words[0] as u64 | ((words[1] as u64) << 32)
}

pub(crate) fn trb_type(control: u32) -> u32 {
    (control >> 10) & 0x3f
}

pub(crate) fn with_type(control: u32, ty: u32) -> u32 {
    (control & !(0x3f << 10)) | ((ty & 0x3f) << 10)
}

pub(crate) fn cycle(control: u32) -> bool {
    control & TRB_CYCLE != 0
}

pub(crate) fn set_cycle(control: u32, c: bool) -> u32 {
    (control & !TRB_CYCLE) | u32::from(c)
}

pub(crate) fn slot_id(control: u32) -> u8 {
    (control >> 24) as u8
}

/// Endpoint ID / DCI on a Transfer Event TRB (xHCI 6.4.2.1 bits 20:16).
pub(crate) fn endpoint_id(control: u32) -> u8 {
    ((control >> 16) & 0x1f) as u8
}

pub(crate) fn completion_code(status: u32) -> u8 {
    (status >> 24) as u8
}

pub(crate) fn residual(status: u32) -> u32 {
    status & 0xff_ffff
}

pub(crate) fn link_trb(ring_addr: u64, c: bool, toggle: bool) -> [u32; 4] {
    let mut ctrl = with_type(0, TRB_LINK);
    ctrl = set_cycle(ctrl, c);
    if toggle {
        ctrl |= TRB_TC;
    }
    pack(ring_addr, 0, ctrl)
}

pub(crate) fn cmd_enable_slot(c: bool) -> [u32; 4] {
    pack(0, 0, set_cycle(with_type(0, TRB_ENABLE_SLOT), c))
}

pub(crate) fn cmd_disable_slot(slot: u8, c: bool) -> [u32; 4] {
    pack(
        0,
        0,
        set_cycle(with_type(0, TRB_DISABLE_SLOT), c) | ((slot as u32) << 24),
    )
}

pub(crate) fn cmd_address_device(input_ctx: u64, slot: u8, bsr: bool, c: bool) -> [u32; 4] {
    let mut ctrl = with_type(0, TRB_ADDR_DEV) | ((slot as u32) << 24);
    if bsr {
        ctrl |= TRB_BSR;
    }
    pack(input_ctx, 0, set_cycle(ctrl, c))
}

pub(crate) fn cmd_configure_ep(input_ctx: u64, slot: u8, c: bool) -> [u32; 4] {
    pack(
        input_ctx,
        0,
        set_cycle(with_type(0, TRB_CONFIG_EP), c) | ((slot as u32) << 24),
    )
}

pub(crate) fn cmd_eval_context(input_ctx: u64, slot: u8, c: bool) -> [u32; 4] {
    pack(
        input_ctx,
        0,
        set_cycle(with_type(0, TRB_EVAL_CONTEXT), c) | ((slot as u32) << 24),
    )
}

/// Setup Stage TRB (xHCI 6.4.1.2.1). `setup` is the 8-byte Chapter-9 packet.
pub(crate) fn setup_trb(setup: [u8; 8], trt: u32, c: bool) -> [u32; 4] {
    let lo = u32::from_le_bytes([setup[0], setup[1], setup[2], setup[3]]);
    let hi = u32::from_le_bytes([setup[4], setup[5], setup[6], setup[7]]);
    let status = 8u32; // TRB Transfer Length = 8
    let ctrl = set_cycle(with_type(0, TRB_SETUP), c) | TRB_IDT | ((trt & 3) << 16);
    [lo, hi, status, ctrl]
}

/// Data Stage TRB (xHCI 6.4.1.2.2). `dir_in` sets DIR (bit 16).
pub(crate) fn data_trb(buf: u64, len: u32, dir_in: bool, c: bool) -> [u32; 4] {
    let mut ctrl = set_cycle(with_type(0, TRB_DATA), c) | TRB_ISP;
    if dir_in {
        ctrl |= 1 << 16;
    }
    pack(buf, len & 0x1_ffff, ctrl)
}

/// Status Stage TRB (xHCI 6.4.1.2.3). DIR is opposite the data stage
/// (IN if there was no data / OUT-data; OUT if there was IN-data).
pub(crate) fn status_trb(dir_in: bool, c: bool) -> [u32; 4] {
    let mut ctrl = set_cycle(with_type(0, TRB_STATUS), c) | TRB_IOC;
    if dir_in {
        ctrl |= 1 << 16;
    }
    pack(0, 0, ctrl)
}

/// Normal TRB for bulk (xHCI 6.4.1.1).
pub(crate) fn normal_trb(buf: u64, len: u32, c: bool) -> [u32; 4] {
    pack(
        buf,
        len & 0x1_ffff,
        set_cycle(with_type(0, TRB_NORMAL), c) | TRB_IOC | TRB_ISP,
    )
}

/// Immediate-data Normal TRB (xHCI 6.4.1.1), limited to eight bytes.
/// The 31-byte BOT CBW uses the DMA-backed Normal TRB instead.

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn pack_roundtrip_param() {
        let t = pack(0x1122_3344_5566_7788, 0xaabbcc, 0x400);
        assert_eq!(param(&t), 0x1122_3344_5566_7788);
        assert_eq!(t[2], 0xaabbcc);
    }

    #[ktest]
    fn type_and_cycle_bits() {
        let t = cmd_enable_slot(true);
        assert_eq!(trb_type(t[3]), TRB_ENABLE_SLOT);
        assert!(cycle(t[3]));
        let t = cmd_enable_slot(false);
        assert!(!cycle(t[3]));
        assert_eq!(set_cycle(t[3], true) & 1, 1);
    }

    #[ktest]
    fn address_device_encodes_slot_and_bsr() {
        let t = cmd_address_device(0x2000, 3, true, true);
        assert_eq!(trb_type(t[3]), TRB_ADDR_DEV);
        assert_eq!(slot_id(t[3]), 3);
        assert_eq!(endpoint_id(0x0003_0000 | (32 << 10)), 3);
        assert_eq!(t[3] & TRB_BSR, TRB_BSR);
        assert_eq!(param(&t), 0x2000);
    }

    #[ktest]
    fn setup_trb_is_immediate_8_bytes() {
        let setup = [0x80, 6, 0, 1, 0, 0, 18, 0];
        let t = setup_trb(setup, TRT_IN_DATA, true);
        assert_eq!(trb_type(t[3]), TRB_SETUP);
        assert_eq!(t[3] & TRB_IDT, TRB_IDT);
        assert_eq!((t[3] >> 16) & 3, TRT_IN_DATA);
        assert_eq!(t[2] & 0x1_ffff, 8);
        assert_eq!(t[0].to_le_bytes(), [0x80, 6, 0, 1]);
        assert_eq!(t[1].to_le_bytes(), [0, 0, 18, 0]);
    }

    #[ktest]
    fn data_status_dir_bits() {
        let d = data_trb(0x1000, 18, true, true);
        assert_eq!(trb_type(d[3]), TRB_DATA);
        assert_eq!(d[3] & (1 << 16), 1 << 16);
        let s = status_trb(false, true);
        assert_eq!(trb_type(s[3]), TRB_STATUS);
        assert_eq!(s[3] & TRB_IOC, TRB_IOC);
        assert_eq!(s[3] & (1 << 16), 0);
    }

    #[ktest]
    fn link_trb_toggle() {
        let t = link_trb(0x4000, true, true);
        assert_eq!(trb_type(t[3]), TRB_LINK);
        assert_eq!(t[3] & TRB_TC, TRB_TC);
        assert_eq!(param(&t), 0x4000);
    }

    #[ktest]
    fn completion_code_and_residual() {
        assert_eq!(completion_code(1 << 24), COMP_SUCCESS);
        assert_eq!(completion_code(13 << 24), COMP_SHORT_PACKET);
        assert_eq!(residual(0x0d00_0012), 0x12);
    }
}
