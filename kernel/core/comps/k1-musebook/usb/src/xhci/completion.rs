// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: associate xHCI events with a submitted transfer descriptor.

use super::trb::{COMP_SHORT_PACKET, COMP_SUCCESS};

#[derive(Clone, Copy, Debug)]
pub(super) struct TransferEvent {
    pub slot: u8,
    pub dci: u8,
    pub trb_dma: u64,
    pub event_data: bool,
    pub code: u8,
    pub resid: u32,
}

/// Control transfers complete at Status Stage. An ISP Data Stage event only
/// records the short packet's residue; failures may identify any TRB in the TD.
pub(super) struct ExpectedTransfer {
    pub slot: u8,
    pub dci: u8,
    terminal: u64,
    setup: Option<u64>,
    data: Option<u64>,
    short_residue: Option<u32>,
}

impl ExpectedTransfer {
    pub(super) fn normal(slot: u8, dci: u8, trb_dma: u64) -> Self {
        Self {
            slot,
            dci,
            terminal: trb_dma,
            setup: None,
            data: None,
            short_residue: None,
        }
    }

    pub(super) fn control(slot: u8, dci: u8, setup: u64, data: Option<u64>, status: u64) -> Self {
        Self {
            slot,
            dci,
            terminal: status,
            setup: Some(setup),
            data,
            short_residue: None,
        }
    }

    /// Returns a terminal completion code and transfer residue. Event Data TRBs
    /// are not emitted by this driver, so their opaque payload is never a pointer.
    pub(super) fn observe(&mut self, event: TransferEvent) -> Option<(u8, u32)> {
        if event.slot != self.slot || event.dci != self.dci || event.event_data {
            return None;
        }
        if event.trb_dma == self.terminal {
            let residue = if event.code == COMP_SUCCESS || event.code == COMP_SHORT_PACKET {
                self.short_residue.unwrap_or(event.resid)
            } else {
                event.resid
            };
            return Some((event.code, residue));
        }
        if Some(event.trb_dma) != self.setup && Some(event.trb_dma) != self.data {
            return None;
        }
        if event.code != COMP_SUCCESS && event.code != COMP_SHORT_PACKET {
            return Some((event.code, event.resid));
        }
        if Some(event.trb_dma) == self.data && event.code == COMP_SHORT_PACKET {
            self.short_residue = Some(event.resid);
        }
        None
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::ktest;

    use super::*;

    fn event(pointer: u64, code: u8, resid: u32) -> TransferEvent {
        TransferEvent {
            slot: 1,
            dci: 3,
            trb_dma: pointer,
            event_data: false,
            code,
            resid,
        }
    }

    #[ktest]
    fn normal_completion_requires_identity() {
        let mut expected = ExpectedTransfer::normal(1, 3, 0x1000);
        assert_eq!(expected.observe(event(0x2000, COMP_SUCCESS, 0)), None);
        assert_eq!(
            expected.observe(TransferEvent {
                slot: 2,
                ..event(0x1000, 6, 0)
            }),
            None
        );
        assert_eq!(
            expected.observe(TransferEvent {
                dci: 4,
                ..event(0x1000, 6, 0)
            }),
            None
        );
        assert_eq!(
            expected.observe(TransferEvent {
                event_data: true,
                ..event(0x1000, 6, 0)
            }),
            None
        );
        assert_eq!(expected.observe(event(0x1000, 6, 0)), Some((6, 0)));
        assert_eq!(
            expected.observe(event(0x1000, COMP_SHORT_PACKET, 7)),
            Some((COMP_SHORT_PACKET, 7))
        );
    }

    #[ktest]
    fn control_short_packet_waits_for_status() {
        let mut expected = ExpectedTransfer::control(1, 3, 0x1000, Some(0x1010), 0x1020);
        assert_eq!(expected.observe(event(0x1010, COMP_SHORT_PACKET, 9)), None);
        assert_eq!(expected.observe(event(0x0990, COMP_SUCCESS, 0)), None);
        assert_eq!(
            expected.observe(event(0x1020, COMP_SUCCESS, 0)),
            Some((COMP_SUCCESS, 9))
        );
    }

    #[ktest]
    fn control_failure_can_identify_any_stage() {
        for pointer in [0x1000, 0x1010, 0x1020] {
            let mut expected = ExpectedTransfer::control(1, 3, 0x1000, Some(0x1010), 0x1020);
            assert_eq!(expected.observe(event(pointer, 6, 0)), Some((6, 0)));
        }
    }

    #[ktest]
    fn nodata_control_requires_status() {
        let mut expected = ExpectedTransfer::control(1, 3, 0x1000, None, 0x1010);
        assert_eq!(expected.observe(event(0x1000, COMP_SUCCESS, 0)), None);
        assert_eq!(
            expected.observe(event(0x1010, COMP_SUCCESS, 0)),
            Some((COMP_SUCCESS, 0))
        );
    }
}
