// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: per-context firmware ring accounting and synchronization.

//! Per-client circular command buffers.
//!
//! Ports `pvr_cccb.c` and `pvr_cccb.h` (377 lines).
//!
//! One per context per job type. Unlike the KCCB, space here is reserved before
//! a job is committed and released only once the firmware has run it, so the
//! reservation has to be part of the same all-or-nothing step as the rest of
//! submission.
//!
//! # The ring, and why a command may not straddle its end
//!
//! The firmware reads commands out of this buffer sequentially and decodes each
//! one in place. It has no notion of a command split across the end of the
//! allocation, so the driver must never write one: if the remaining tail cannot
//! hold the whole command *and* a padding header behind it, the tail is filled
//! with a `PADDING` command and the real command is written from offset zero.
//!
//! Three consequences, all of them load-bearing:
//!
//! - **The tail always has room for a padding header.** After any write, the
//!   space between the write offset and the end of the ring is either zero
//!   because the write wrapped — in which case the offset is small, not
//!   large — or at least [`PADDING_COMMAND_SIZE`]. [`Cccb::write_command`]
//!   maintains that and [`Cccb::tail_invariant_holds`] states it, so a test can
//!   assert it rather than trust it.
//! - **A full ring reports one byte less than its size.** `write_offset ==
//!   read_offset` is the firmware's empty condition, so the last byte is never
//!   used; that is what the `& (size - 1)` in [`space`] arranges.
//! - **A command sequence larger than half the ring is refused outright.**
//!   Upstream's `pvr_cccb_cmdseq_can_fit()` halves the capacity so that a
//!   sequence is guaranteed to fit either in the tail or, after padding, at the
//!   start. Without that, a sequence could need the padding command to be
//!   consumed by the firmware before it could be written, which is a wait in the
//!   middle of what has to be an atomic step.
//!
//! # Who publishes the write offset
//!
//! Not the host. The control structure's `write_offset` is initialised to zero
//! here and thereafter updated by the *firmware*, from the `client_woff_update`
//! field of the `KICK` command that [`crate::sched::queue`] sends. So committing
//! a command means making its bytes visible to the firmware — nothing more — and
//! the kick is what tells the firmware they are there. Writing the control
//! structure from both sides would be a data race with no lock to take.

use core::mem::offset_of;

use ostd_pod::FromZeros;

use crate::{
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwHeap, FwObject},
    rogue::fwif::{FwAddr, RogueFwifCcbCmdHeader, RogueFwifCccbCtl},
    sched::abi,
};

/// Size of a padding command, which is a bare header with no payload.
///
/// Upstream `PADDING_COMMAND_SIZE`.
pub const PADDING_COMMAND_SIZE: u32 = size_of::<RogueFwifCcbCmdHeader>() as u32;

/// How many commands [`Cccb::report`] prints before giving up.
///
/// One job's sequence is at most a handful, so a walk that goes past this is
/// walking corruption.
const REPORT_MAX_COMMANDS: usize = 16;

/// A client's command buffer.
pub struct Cccb {
    /// `rogue_fwif_cccb_ctl`, which the firmware updates.
    ctrl: FwObject,
    /// The ring itself.
    ring: FwObject,
    /// Ring size in bytes, a power of two.
    size: u32,
    /// `size - 1`, which the firmware needs in every kick.
    wrap_mask: u32,
    /// The host's idea of where the next command goes.
    ///
    /// Authoritative: the control structure's copy lags until the firmware
    /// applies a kick's `client_woff_update`.
    write_offset: u32,
}

impl Cccb {
    /// Creates a buffer with `2^size_log2` bytes of command space.
    ///
    /// Ports upstream `pvr_cccb_init()`. Both allocations are device-uncached
    /// (MIPS-side). The control structure is also CPU-uncached: the firmware
    /// writes `read_offset` and the host polls it. The ring stays CPU-cached —
    /// the host writes commands, the firmware reads them.
    pub fn new(heap: &mut FwHeap, size_log2: u32) -> PvrResult<Self> {
        // Below 256 bytes, half the ring cannot hold a command header plus the
        // padding header `cmdseq_can_fit` reserves, so no command would ever be
        // accepted; above 2^31 the offset arithmetic in `space` overflows.
        // Upstream has no such check because its only caller passes
        // [`abi::CCCB_SIZE_LOG2`].
        if !(8..=31).contains(&size_log2) {
            return Err(PvrError::InvalidArgument);
        }
        let size = 1u32 << size_log2;

        let mut ctrl = heap.alloc_guard(size_of::<RogueFwifCccbCtl>(), FwCache::POLLED)?;
        let (ctrl_object, heap) = ctrl.parts();
        let ring = heap.alloc_guard(size as usize, FwCache::DEVICE_UNCACHED)?;

        // Keep the original alloc/alloc/init sequence while guards cover errors.
        let mut initial = RogueFwifCccbCtl::new_zeroed();
        initial.write_offset = 0;
        initial.read_offset = 0;
        initial.dep_offset = 0;
        initial.wrap_mask = size - 1;
        ctrl_object.write_val(0, &initial)?;

        let ring = ring.commit();
        let ctrl = ctrl.commit();
        Ok(Self {
            ctrl,
            ring,
            size,
            wrap_mask: size - 1,
            write_offset: 0,
        })
    }

    /// Both allocations must be mapped in the firmware processor's page table.
    ///
    /// [`Self::ctrl_fw_addr`] and [`Self::ring_fw_addr`] are published in the common
    /// context, so firmware may dereference either independently.
    pub fn fw_objects(&self) -> [&FwObject; 2] {
        [&self.ctrl, &self.ring]
    }

    /// Firmware address of the control structure.
    pub fn ctrl_fw_addr(&self) -> FwAddr {
        self.ctrl.fw_addr()
    }

    /// Firmware address of the ring.
    pub fn ring_fw_addr(&self) -> FwAddr {
        self.ring.fw_addr()
    }

    /// `size - 1`, which every kick carries as `client_wrap_mask_update`.
    pub fn wrap_mask(&self) -> u32 {
        self.wrap_mask
    }

    /// Ring size in bytes.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// The host's write offset, which a kick publishes to the firmware.
    pub fn write_offset(&self) -> u32 {
        self.write_offset
    }

    /// How far the firmware has read.
    ///
    /// Invalidates before reading: the firmware writes this and, on the K1,
    /// nothing makes its write visible to the CPU by itself. See
    /// `CONVENTIONS.md` DMA contract.
    pub fn read_offset(&self) -> PvrResult<u32> {
        self.ctrl
            .read_val::<u32>(offset_of!(RogueFwifCccbCtl, read_offset))
    }

    /// Bytes of a command plus its header, rounded to the firmware's alignment.
    ///
    /// Ports upstream `pvr_cccb_get_size_of_cmd_with_hdr()`. A command whose
    /// length is not a multiple of eight is a driver bug — upstream `WARN_ON`s
    /// it — so this reports it rather than rounding silently.
    pub fn size_of_cmd_with_hdr(cmd_size: u32) -> PvrResult<u32> {
        if !cmd_size.is_multiple_of(8) {
            ostd::error!("client CCB command of {cmd_size} bytes is not 8-byte aligned");
            return Err(PvrError::InvalidArgument);
        }
        PADDING_COMMAND_SIZE
            .checked_add(cmd_size)
            .ok_or(PvrError::InvalidArgument)
    }

    /// Whether a command sequence of `len` bytes could *ever* fit.
    ///
    /// Ports upstream `pvr_cccb_cmdseq_can_fit()`, including the halving. A
    /// sequence that fails this is [`PvrError::TooBig`] at job-creation time and
    /// no amount of waiting will help, which is why it is a different question
    /// from [`Self::cmdseq_fits`].
    pub fn cmdseq_can_fit(&self, len: u32) -> bool {
        len.checked_add(PADDING_COMMAND_SIZE)
            .is_some_and(|needed| needed <= self.size / 2)
    }

    /// Whether a command sequence of `len` bytes fits right now.
    ///
    /// Ports upstream `pvr_cccb_cmdseq_fits()`. When the sequence would not fit
    /// in the tail, the tail is counted as consumed too, because it will have to
    /// become a padding command.
    pub fn cmdseq_fits(&self, len: u32) -> PvrResult<bool> {
        let read_offset = self.read_offset()?;
        let remaining = self.size - self.write_offset;

        let len = if len
            .checked_add(PADDING_COMMAND_SIZE)
            .is_some_and(|needed| needed > remaining)
        {
            len.checked_add(remaining)
                .ok_or(PvrError::InvalidArgument)?
        } else {
            len
        };

        Ok(space(self.write_offset, read_offset, self.size) >= len)
    }

    /// Writes a command and its header, padding the tail first if it must wrap.
    ///
    /// Ports upstream `pvr_cccb_write_command_with_header()`. The caller must
    /// have established that the whole sequence fits, with
    /// [`Self::cmdseq_fits`]; a sequence that turns out not to fit here is a
    /// driver bug and is reported as [`PvrError::Busy`] rather than truncated,
    /// because the alternative is handing the firmware half a command.
    ///
    /// `cmd` must be a multiple of eight bytes long. Every caller in this port
    /// passes either a firmware command structure or an array of
    /// [`crate::rogue::fwif::RogueFwifUfo`], both of which are.
    pub fn write_command(
        &mut self,
        cmd_type: u32,
        cmd: &[u8],
        ext_job_ref: u32,
        int_job_ref: u32,
    ) -> PvrResult<()> {
        let cmd_size = u32::try_from(cmd.len()).map_err(|_| PvrError::InvalidArgument)?;
        let sz_with_hdr = Self::size_of_cmd_with_hdr(cmd_size)?;
        let remaining = self.size - self.write_offset;

        let required = if sz_with_hdr
            .checked_add(PADDING_COMMAND_SIZE)
            .is_some_and(|needed| needed > remaining)
        {
            // The command would straddle the end of the ring, so the tail
            // becomes a padding command and the real command starts at zero.
            sz_with_hdr
                .checked_add(remaining)
                .ok_or(PvrError::InvalidArgument)?
        } else {
            sz_with_hdr
        };

        let read_offset = self.read_offset()?;
        if space(self.write_offset, read_offset, self.size) < required {
            ostd::error!(
                "client CCB overflow: {} bytes needed, {} free (w {:#x}, r {:#x})",
                required,
                space(self.write_offset, read_offset, self.size),
                self.write_offset,
                read_offset
            );
            return Err(PvrError::Busy);
        }

        if required != sz_with_hdr {
            let mut pad = RogueFwifCcbCmdHeader::new_zeroed();
            pad.cmd_type = abi::CCB_CMD_TYPE_PADDING;
            // The payload the firmware is to skip: everything left in the tail
            // beyond this header.
            pad.cmd_size = remaining - PADDING_COMMAND_SIZE;
            self.ring.write_val(self.write_offset as usize, &pad)?;
            self.write_offset = 0;
        }

        let mut header = RogueFwifCcbCmdHeader::new_zeroed();
        header.cmd_type = cmd_type;
        header.cmd_size = cmd_size;
        header.ext_job_ref = ext_job_ref;
        header.int_job_ref = int_job_ref;

        let at = self.write_offset as usize;
        self.ring.write_val(at, &header)?;
        self.ring
            .write_bytes(at + PADDING_COMMAND_SIZE as usize, cmd)?;
        self.write_offset += sz_with_hdr;

        debug_assert!(self.tail_invariant_holds());
        Ok(())
    }

    /// Whether the tail of the ring can still hold a padding command.
    ///
    /// The invariant [`Self::write_command`] maintains, and the reason a command
    /// never has to be split. Diagnostic, for tests and for a driver chasing a
    /// firmware that has stopped decoding.
    pub fn tail_invariant_holds(&self) -> bool {
        self.write_offset <= self.size && self.size - self.write_offset >= PADDING_COMMAND_SIZE
    }

    /// Returns both allocations to the firmware heap.
    ///
    /// Called from [`crate::sched::queue::Queue::release`], and only once the
    /// firmware is known not to be reading the ring — which is what draining the
    /// queue and cleaning up the firmware context establish.
    pub fn release(&self, heap: &mut FwHeap) {
        heap.free(&self.ring);
        heap.free(&self.ctrl);
    }

    /// Dumps the command sequence visible to firmware for a stalled job.
    ///
    /// The walk stops after [`REPORT_MAX_COMMANDS`]. A malformed zero-sized command
    /// must not make a diagnostic loop indefinitely.
    pub fn report(&self, label: &str) {
        match self.ctrl.read_val::<RogueFwifCccbCtl>(0) {
            Ok(ctrl) => ostd::debug!(
                "[k1-gpu] {} cccb: host_woff={:#x} fw_woff={:#x} roff={:#x} dep={:#x} \
                 wrap={:#x} size={:#x}",
                label,
                self.write_offset,
                ctrl.write_offset,
                ctrl.read_offset,
                ctrl.dep_offset,
                ctrl.wrap_mask,
                self.size
            ),
            Err(err) => {
                ostd::debug!(
                    "[k1-gpu] {} cccb: control structure unreadable ({:?})",
                    label,
                    err
                );
                return;
            }
        }

        let mut at = 0u32;
        let mut seen = 0;
        while at < self.write_offset {
            if seen == REPORT_MAX_COMMANDS {
                ostd::debug!(
                    "[k1-gpu] {} cccb: stopping after {} commands, still at {:#x}",
                    label,
                    REPORT_MAX_COMMANDS,
                    at
                );
                return;
            }
            let Ok(header) = self.ring.read_val::<RogueFwifCcbCmdHeader>(at as usize) else {
                ostd::debug!("[k1-gpu] {} cccb: header at {:#x} unreadable", label, at);
                return;
            };
            ostd::debug!(
                "[k1-gpu] {} cccb: [{:#x}] type={:#010x} size={:#x} ext={} int={}",
                label,
                at,
                header.cmd_type,
                header.cmd_size,
                header.ext_job_ref,
                header.int_job_ref
            );
            // The condition that would trap the firmware, named rather than
            // left for the reader to spot in the numbers.
            if header.cmd_size == 0 && header.cmd_type != abi::CCB_CMD_TYPE_PADDING {
                ostd::debug!(
                    "[k1-gpu] {} cccb: zero-length command at {:#x}: a walker that advances \
                     by the header's size cannot get past this",
                    label,
                    at
                );
                return;
            }
            at = at + PADDING_COMMAND_SIZE + header.cmd_size;
            seen += 1;
        }
    }

    /// The commands written into the ring so far, as `(type, payload)` pairs.
    ///
    /// Walks the ring from the start the way the firmware does, which is what
    /// makes it evidence: a test that reads fields out of the [`FwObject`] at
    /// offsets it computed itself would agree with a driver that had written them
    /// wrong. Stops at the write offset, and skips a `PADDING` command's payload
    /// as the firmware would.
    #[cfg(ktest)]
    pub(crate) fn commands(&self) -> PvrResult<alloc::vec::Vec<(u32, alloc::vec::Vec<u8>)>> {
        let mut out = alloc::vec::Vec::new();
        let mut at = 0u32;
        while at < self.write_offset {
            let header: RogueFwifCcbCmdHeader = self.ring.read_val(at as usize)?;
            let payload_at = at + PADDING_COMMAND_SIZE;
            if header.cmd_type != abi::CCB_CMD_TYPE_PADDING {
                let mut payload = alloc::vec![0u8; header.cmd_size as usize];
                self.ring.read_bytes(payload_at as usize, &mut payload)?;
                out.push((header.cmd_type, payload));
            }
            at = payload_at + header.cmd_size;
        }
        Ok(out)
    }

    /// Restores a previously captured write offset.
    ///
    /// Used to unwind a partially written command sequence. Nothing needs to
    /// undo the bytes themselves: the firmware only ever reads as far as the
    /// write offset a kick published, and no kick has been sent.
    pub(crate) fn rewind_to(&mut self, write_offset: u32) {
        self.write_offset = write_offset;
    }
}

/// Free space in a ring, in the firmware's arithmetic.
///
/// Ports upstream `get_ccb_space()`. Wrapping subtraction on purpose: `r - w`
/// underflows whenever the reader is behind the writer, and the mask turns that
/// into the distance the long way round. One byte is always unusable, because
/// `w == r` is how the firmware recognises an empty ring.
///
/// `size` must be a power of two, which [`Cccb::new`] enforces.
fn space(write_offset: u32, read_offset: u32, size: u32) -> u32 {
    read_offset
        .wrapping_sub(write_offset)
        .wrapping_add(size - 1)
        & (size - 1)
}

/// Ring-wrap and space-accounting tests.
///
/// These need no GPU: a [`Cccb`] is a [`FwHeap`] allocation and the firmware's
/// side of the protocol is a single `read_offset` word, which a test can write
/// itself.
#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::fw::{heap::FwHeapInfo, mips};

    /// A heap big enough for a few rings.
    fn heap() -> FwHeap {
        let info = FwHeapInfo::new(
            mips::FW_HEAP_MIPS_LOG2_SIZE,
            mips::FW_HEAP_MIPS_RESERVED_SIZE,
        )
        .expect("heap geometry");
        FwHeap::new(info, mips::FW_HEAP_MIPS_BASE)
    }

    /// Pretends the firmware has consumed up to `read_offset`.
    fn set_read_offset(cccb: &Cccb, read_offset: u32) {
        cccb.ctrl
            .write_val(offset_of!(RogueFwifCccbCtl, read_offset), &read_offset)
            .expect("write read offset");
    }

    #[ktest]
    fn space_reserves_one_byte_so_full_is_never_empty() {
        // Empty ring: everything but one byte.
        assert_eq!(space(0, 0, 256), 255);
        // Reader one ahead of the writer: one byte less again.
        assert_eq!(space(0, 1, 256), 0);
        // Writer ahead of the reader by 16: the rest of the ring, less one.
        assert_eq!(space(16, 0, 256), 239);
        // Wrapped: writer at the top, reader at the bottom.
        assert_eq!(space(255, 0, 256), 0);
        assert_eq!(space(248, 8, 256), 15);
    }

    #[ktest]
    fn a_command_that_does_not_fit_the_tail_pads_and_wraps() {
        let mut heap = heap();
        // 256-byte ring: six 40-byte headers and change, so wrapping is
        // reachable in a handful of commands.
        let mut cccb = Cccb::new(&mut heap, 8).expect("cccb");
        assert_eq!(cccb.size(), 256);
        assert_eq!(cccb.wrap_mask(), 255);

        // An 8-byte payload is 48 bytes with its header.
        let cmd = [0u8; 8];
        let sz = Cccb::size_of_cmd_with_hdr(8).expect("size");
        assert_eq!(sz, 48);

        // The firmware is keeping up, so there is always space.
        set_read_offset(&cccb, 0);
        for expected in 1..=4u32 {
            // Pretend everything written so far has been consumed.
            set_read_offset(&cccb, cccb.write_offset());
            cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0)
                .expect("write");
            assert_eq!(cccb.write_offset(), expected * sz);
            assert!(cccb.tail_invariant_holds());
        }

        // At 192 the tail holds 64 bytes: 48 for the command but only 16 after
        // it, less than a padding header. So this one wraps.
        assert_eq!(cccb.write_offset(), 192);
        set_read_offset(&cccb, 192);
        cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0)
            .expect("write that wraps");
        assert_eq!(cccb.write_offset(), sz);
        assert!(cccb.tail_invariant_holds());

        // The tail really did become a padding command the firmware can skip.
        let pad: RogueFwifCcbCmdHeader = cccb.ring.read_val(192).expect("padding header");
        assert_eq!(pad.cmd_type, abi::CCB_CMD_TYPE_PADDING);
        assert_eq!(pad.cmd_size, 64 - PADDING_COMMAND_SIZE);
    }

    #[ktest]
    fn the_tail_always_has_room_for_a_padding_command() {
        let mut heap = heap();
        let mut cccb = Cccb::new(&mut heap, 8).expect("cccb");
        let cmd = [0u8; 8];

        // Thirty writes take the ring round several times. The invariant is what
        // makes "a command never straddles the end" true by construction, so it
        // is asserted after every single one.
        for _ in 0..30 {
            set_read_offset(&cccb, cccb.write_offset());
            cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0)
                .expect("write");
            assert!(cccb.tail_invariant_holds());
            assert!(cccb.write_offset() < cccb.size());
        }
    }

    #[ktest]
    fn a_full_ring_refuses_the_next_command() {
        let mut heap = heap();
        let mut cccb = Cccb::new(&mut heap, 8).expect("cccb");
        let cmd = [0u8; 8];

        // The firmware has read nothing, so after four commands the ring holds
        // 192 of 256 bytes and the fifth would need 64 more than it has.
        set_read_offset(&cccb, 0);
        for _ in 0..4 {
            cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0)
                .expect("write");
        }
        assert_eq!(cccb.write_offset(), 192);
        assert_eq!(cccb.cmdseq_fits(48), Ok(false));
        assert_eq!(
            cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0),
            Err(PvrError::Busy)
        );
        // Refused, not half-written.
        assert_eq!(cccb.write_offset(), 192);
    }

    #[ktest]
    fn fits_accounts_for_the_padding_the_wrap_will_need() {
        let mut heap = heap();
        let mut cccb = Cccb::new(&mut heap, 8).expect("cccb");
        let cmd = [0u8; 8];

        set_read_offset(&cccb, 0);
        for _ in 0..3 {
            set_read_offset(&cccb, cccb.write_offset());
            cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0)
                .expect("write");
        }
        assert_eq!(cccb.write_offset(), 144);

        // 112 bytes of tail. A 48-byte sequence fits there with room for a
        // padding header behind it, so no wrap is needed and the reader's
        // position is irrelevant.
        set_read_offset(&cccb, 144);
        assert_eq!(cccb.cmdseq_fits(48), Ok(true));

        // A 104-byte sequence needs 104 + 40 > 112, so it would wrap, and
        // `fits` must count the 112 bytes of tail as spent as well: 216 bytes
        // in a ring that has 255 free says yes.
        assert_eq!(cccb.cmdseq_fits(104), Ok(true));
        // With the reader only 200 bytes ahead there are 199 free, so no.
        set_read_offset(&cccb, 144 + 200 - 256);
        assert_eq!(cccb.cmdseq_fits(104), Ok(false));
    }

    #[ktest]
    fn a_sequence_larger_than_half_the_ring_can_never_fit() {
        let mut heap = heap();
        let cccb = Cccb::new(&mut heap, 8).expect("cccb");

        // 88 + 40 = 128, exactly half, which is the boundary case.
        assert!(cccb.cmdseq_can_fit(88));
        assert!(!cccb.cmdseq_can_fit(89));
        assert!(!cccb.cmdseq_can_fit(u32::MAX));
    }

    #[ktest]
    fn a_misaligned_command_length_is_refused() {
        assert_eq!(
            Cccb::size_of_cmd_with_hdr(12),
            Err(PvrError::InvalidArgument)
        );
        assert_eq!(Cccb::size_of_cmd_with_hdr(16), Ok(56));
    }

    #[ktest]
    fn a_rewind_puts_the_write_offset_back() {
        let mut heap = heap();
        let mut cccb = Cccb::new(&mut heap, 8).expect("cccb");
        let cmd = [0u8; 8];

        set_read_offset(&cccb, 0);
        let mark = cccb.write_offset();
        cccb.write_command(abi::CCB_CMD_TYPE_UPDATE, &cmd, 0, 0)
            .expect("write");
        assert_ne!(cccb.write_offset(), mark);
        cccb.rewind_to(mark);
        assert_eq!(cccb.write_offset(), mark);
    }

    #[ktest]
    fn a_ring_too_small_to_hold_a_command_is_refused_at_creation() {
        let mut heap = heap();
        assert_eq!(
            Cccb::new(&mut heap, 7).err(),
            Some(PvrError::InvalidArgument)
        );
        assert_eq!(
            Cccb::new(&mut heap, 32).err(),
            Some(PvrError::InvalidArgument)
        );
    }

    /// [`Cccb::new`] hardcodes the two mappings. The expected column is the
    /// policy, not a flag the test passed in: flipping the control structure
    /// to write-back, or the 32 KiB ring to uncacheable, turns this red.
    #[ktest]
    fn cccb_control_is_cpu_uncached_and_the_ring_is_not() {
        let mut heap = heap();
        let cccb = Cccb::new(&mut heap, 8).expect("cccb");
        let [ctrl, ring] = cccb.fw_objects();
        let rows = [
            ("cccb ctrl", ctrl.is_cpu_uncached(), true),
            ("cccb ring", ring.is_cpu_uncached(), false),
        ];
        for (what, actual, expected) in rows {
            assert_eq!(actual, expected, "{what}");
        }
    }
}
