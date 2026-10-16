// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: xHCI command / transfer / event rings.

//! Producer/consumer ring walk (xHCI 4.9.2). Cycle bit + Link TRB wrap.
//! Operates on indices only so ktests need no MMIO.

#[cfg(any(test, ktest))]
use super::trb::TRB_SIZE;

/// Software view of a single-segment ring whose last TRB is a Link TRB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Producer {
    /// Next data-TRB index (never the Link slot).
    pub index: usize,
    pub cycle: bool,
    /// Total TRBs including the Link TRB at `num - 1`.
    pub num: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// Write a data/command TRB at `index` with `cycle`.
    Slot {
        index: usize,
        cycle: bool,
        next_index: usize,
        next_cycle: bool,
    },
    /// First write the Link TRB at `link_index` with `link_cycle`, then the
    /// data TRB at index 0 with the toggled cycle.
    Wrap {
        link_index: usize,
        link_cycle: bool,
        index: usize,
        cycle: bool,
        next_index: usize,
        next_cycle: bool,
    },
}

impl Producer {
    pub(crate) fn new(num: usize) -> Self {
        assert!(num >= 2);
        Self {
            index: 0,
            cycle: true,
            num,
        }
    }

    /// Decide where the next producer TRB goes. Does not mutate; call
    /// [`apply`] after writing.
    pub(crate) fn place(self) -> Place {
        if self.index + 1 >= self.num {
            // `index` is the last data slot (`num-2`). After writing it the
            // next producer step must emit Link and wrap. `place` for the
            // *following* enqueue therefore returns Wrap when index == num-1
            // would be the Link — we treat `index == num-1` as "need wrap
            // before this enqueue".
            Place::Wrap {
                link_index: self.num - 1,
                link_cycle: self.cycle,
                index: 0,
                cycle: !self.cycle,
                next_index: 1,
                next_cycle: !self.cycle,
            }
        } else if self.index + 1 == self.num - 1 {
            // This write occupies the last data slot; next enqueue wraps.
            Place::Slot {
                index: self.index,
                cycle: self.cycle,
                next_index: self.num - 1,
                next_cycle: self.cycle,
            }
        } else {
            Place::Slot {
                index: self.index,
                cycle: self.cycle,
                next_index: self.index + 1,
                next_cycle: self.cycle,
            }
        }
    }

    pub(crate) fn apply(&mut self, p: Place) {
        match p {
            Place::Slot {
                next_index,
                next_cycle,
                ..
            }
            | Place::Wrap {
                next_index,
                next_cycle,
                ..
            } => {
                self.index = next_index;
                self.cycle = next_cycle;
            }
        }
    }

    #[cfg(any(test, ktest))]
    pub(crate) fn byte_offset(self, base: usize, index: usize) -> usize {
        base + index * TRB_SIZE
    }
}

/// Event-ring consumer (xHCI 4.9.4): own the TRB iff its cycle matches ours.
/// After consuming the last TRB of a single segment, toggle cycle and wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Consumer {
    pub index: usize,
    pub cycle: bool,
    pub num: usize,
}

impl Consumer {
    pub(crate) fn new(num: usize) -> Self {
        Self {
            index: 0,
            cycle: true,
            num,
        }
    }

    pub(crate) fn owns(self, trb_cycle: bool) -> bool {
        trb_cycle == self.cycle
    }

    pub(crate) fn advance(&mut self) {
        self.index += 1;
        if self.index >= self.num {
            self.index = 0;
            self.cycle = !self.cycle;
        }
    }
}

/// Byte offset of TRB `i` in a ring whose DMA base is `base`.
#[cfg(any(test, ktest))]
pub(crate) fn trb_offset(base: usize, i: usize) -> usize {
    base + i * TRB_SIZE
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn producer_walks_then_wraps() {
        let mut p = Producer::new(4); // data 0,1,2 + Link 3
        let a = p.place();
        assert_eq!(
            a,
            Place::Slot {
                index: 0,
                cycle: true,
                next_index: 1,
                next_cycle: true
            }
        );
        p.apply(a);
        p.apply(p.place());
        p.apply(p.place());
        // index is now 3 (Link slot) — next place wraps
        assert_eq!(p.index, 3);
        match p.place() {
            Place::Wrap {
                link_index,
                link_cycle,
                index,
                cycle,
                next_index,
                next_cycle,
            } => {
                assert_eq!(link_index, 3);
                assert!(link_cycle);
                assert_eq!(index, 0);
                assert!(!cycle);
                assert_eq!(next_index, 1);
                assert!(!next_cycle);
            }
            other => panic!("expected wrap, got {other:?}"),
        }
    }

    #[ktest]
    fn consumer_toggles_at_segment_end() {
        let mut c = Consumer::new(3);
        assert!(c.owns(true));
        assert!(!c.owns(false));
        c.advance();
        c.advance();
        c.advance();
        assert_eq!(c.index, 0);
        assert!(!c.cycle);
        assert!(c.owns(false));
    }

    #[ktest]
    fn trb_offset_is_16_byte_stride() {
        assert_eq!(trb_offset(0x1000, 2), 0x1020);
        assert_eq!(Producer::new(8).byte_offset(0x2000, 3), 0x2030);
    }
}
