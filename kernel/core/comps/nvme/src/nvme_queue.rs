// SPDX-License-Identifier: MPL-2.0

//! NVMe Submission and Completion Queue implementation.
//!
//! Refer to NVM Express Base Specification Revision 2.0, Section 3.3 (Queue Mechanism).

use alloc::vec::Vec;
use core::{
    ops::DerefMut,
    sync::atomic::{Ordering, fence},
};

use aster_util::{field_ptr, safe_ptr::SafePtr};
use ostd::{
    // K1 MUSE Book --- (explicit cache maintenance for non-coherent DMA)
    // mm::{HasDaddr, dma::DmaCoherent},
    mm::{HasDaddr, dma::DmaStream},
    // K1 MUSE Book ---
    warn,
};

use crate::{
    nvme_regs::NvmeDoorbellRegs,
    nvme_spec::{NvmeCommand, NvmeCompletion},
    transport::pci::transport::DbregAccess,
};

/// Number of entries in each submission and completion ring.
pub(crate) const QUEUE_DEPTH: usize = 64;

/// Number of queue pairs the driver allocates (admin plus I/O).
//
// TODO: This value should be changed when supporting more than 1 I/O queue pairs.
pub(crate) const QUEUE_NUM: usize = 2;

/// Completion Queue.
#[derive(Debug)]
pub(crate) struct NvmeCompletionQueue {
    // K1 MUSE Book --- (streaming queue memory)
    // cqueue: SafePtr<Cqring, DmaCoherent>,
    cqueue: SafePtr<Cqring, DmaStream>,
    // K1 MUSE Book ---
    head: u16,
    phase: bool,
}

struct Cqring {
    ring: [NvmeCompletion; QUEUE_DEPTH],
}

impl NvmeCompletionQueue {
    /// Creates a new completion ring.
    ///
    /// Returns `None` if DMA memory for the completion ring cannot be allocated.
    pub(crate) fn new() -> Option<Self> {
        // K1 MUSE Book --- (CQ is device-written; streaming DMA + flush the zeroed ring)
        // let dma = DmaCoherent::alloc(1, true).ok()?;
        // Some(Self {
        //     cqueue: SafePtr::new(dma, 0),
        //     head: 0,
        //     phase: true,
        // })
        let dma = DmaStream::alloc(1, false).ok()?;
        let cqueue = SafePtr::new(dma, 0);
        cqueue.sync_to_device().ok()?;
        Some(Self {
            cqueue,
            head: 0,
            phase: true,
        })
        // K1 MUSE Book ---
    }

    /// Returns the DMA physical address of the completion ring.
    pub(crate) fn cq_daddr(&self) -> usize {
        self.cqueue.daddr()
    }

    /// Consumes the next completion entry if its phase tag matches the expected phase.
    ///
    /// Returns the new head index (for the CQ head doorbell) and the completion, or `None` if no
    /// entry is ready.
    fn complete(&mut self) -> Option<(u16, NvmeCompletion)> {
        // K1 MUSE Book --- (invalidate the slot so we read the device's DMA write)
        let off = self.head as usize * size_of::<NvmeCompletion>();
        let _ = self
            .cqueue
            .vm()
            .sync_from_device(off..off + size_of::<NvmeCompletion>());
        // K1 MUSE Book ---

        // K1 MUSE Book --- (CQ backing store is DmaStream)
        // let ring_ptr: SafePtr<[NvmeCompletion; QUEUE_DEPTH], &DmaCoherent> =
        let ring_ptr: SafePtr<[NvmeCompletion; QUEUE_DEPTH], &DmaStream> =
            // K1 MUSE Book ---
            field_ptr!(&self.cqueue, Cqring, ring);
        let mut ring_slot_ptr = ring_ptr.cast::<NvmeCompletion>();
        ring_slot_ptr.add(self.head as usize);

        let phase_tag = NvmeCompletion::read_phase_tag(&ring_slot_ptr);
        if phase_tag != self.phase {
            return None;
        }

        // Read barrier.
        fence(Ordering::SeqCst);

        let entry = ring_slot_ptr
            .read()
            .expect("CQ slot pointer must be valid within allocated DMA ring");
        self.head = (self.head + 1) % (QUEUE_DEPTH as u16);
        if self.head == 0 {
            self.phase = !self.phase;
        }
        Some((self.head, entry))
    }
}

/// Submission Queue.
///
/// The type parameter `C` is optional driver-side state for in-flight commands:
/// [`NoContext`] when none is needed (e.g., the admin queue),
/// or [`Context`] to keep one value per CID until the matching completion is taken.
pub(crate) struct NvmeSubmissionQueue<C = NoContext> {
    inner: NvmeSubmissionQueueInner,
    context: C,
}

#[derive(Default)]
pub(crate) struct NoContext;

pub(crate) struct Context<T>([Option<T>; QUEUE_DEPTH]);

impl<T> Default for Context<T> {
    fn default() -> Self {
        Self(core::array::from_fn(|_| None))
    }
}

impl<C> core::fmt::Debug for NvmeSubmissionQueue<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NvmeSubmissionQueue")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<C: Default> NvmeSubmissionQueue<C> {
    /// Creates a new submission queue.
    ///
    /// Returns `None` if DMA memory for the submission ring cannot be allocated.
    pub(crate) fn new() -> Option<Self> {
        Some(Self {
            inner: NvmeSubmissionQueueInner::new()?,
            context: Default::default(),
        })
    }
}

impl<C> NvmeSubmissionQueue<C> {
    /// Updates the mirrored SQ head from the SQ head pointer in `completion`.
    pub(crate) fn update_sq_head(&mut self, completion: &NvmeCompletion) {
        self.inner.head = completion.sq_head() % (QUEUE_DEPTH as u16);
    }

    /// Returns the DMA physical address of the submission ring.
    pub(crate) fn sq_daddr(&self) -> usize {
        self.inner.squeue.daddr()
    }

    /// Returns the number of SQ slots available for new commands starting at the current tail.
    pub(crate) fn free_slots(&self) -> usize {
        let used = if self.inner.tail >= self.inner.head {
            (self.inner.tail - self.inner.head) as usize
        } else {
            QUEUE_DEPTH - (self.inner.head - self.inner.tail) as usize
        };
        (QUEUE_DEPTH - 1) - used
    }
}

impl NvmeSubmissionQueue<NoContext> {
    /// Replaces the unused unit context slots with an empty context table of type `T`.
    pub(crate) fn with_items<T>(self) -> NvmeSubmissionQueue<Context<T>> {
        NvmeSubmissionQueue {
            inner: self.inner,
            context: Default::default(),
        }
    }
}

impl<T> NvmeSubmissionQueue<Context<T>> {
    // K1 MUSE Book --- (CID owners survive until their own completion)
    pub(crate) fn free_context_slots(&self) -> usize {
        (0..self.free_slots())
            .take_while(|offset| {
                let cid = (usize::from(self.inner.tail) + offset) % QUEUE_DEPTH;
                self.context.0[cid].is_none()
            })
            .count()
    }
    // K1 MUSE Book ---

    // K1 MUSE Book --- (share the CID ownership operation with queue regression tests)
    /// Publishes one command and its context; the caller rings the doorbell.
    pub(crate) fn enqueue(&mut self, mut entry: NvmeCommand, item: T) -> Option<u16> {
        let cid = self.inner.tail;
        if self.context.0[cid as usize].is_some() {
            return None;
        }
        entry.set_cid(cid);
        self.inner.submit(entry)?;
        self.context.0[cid as usize] = Some(item);
        Some(cid)
    }
    // K1 MUSE Book ---

    /// Removes and returns the outstanding context for `cid`.
    ///
    /// Returns `None` if `cid` is out of range or the slot has no outstanding context.
    pub(crate) fn take_item(&mut self, cid: u16) -> Option<T> {
        self.context.0.get_mut(cid as usize)?.take()
    }
}

/// Hardware submission ring.
#[derive(Debug)]
struct NvmeSubmissionQueueInner {
    // K1 MUSE Book --- (streaming queue memory)
    // squeue: SafePtr<Sqring, DmaCoherent>,
    squeue: SafePtr<Sqring, DmaStream>,
    // K1 MUSE Book ---
    tail: u16,
    head: u16,
}

struct Sqring {
    ring: [NvmeCommand; QUEUE_DEPTH],
}

impl NvmeSubmissionQueueInner {
    /// Creates a new submission ring.
    ///
    /// Returns `None` if DMA memory for the submission ring cannot be allocated.
    fn new() -> Option<Self> {
        // K1 MUSE Book --- (streaming submission ring)
        // let dma = DmaCoherent::alloc(1, true).ok()?;
        let dma = DmaStream::alloc(1, false).ok()?;
        // K1 MUSE Book ---
        Some(Self {
            squeue: SafePtr::new(dma, 0),
            tail: 0,
            head: 0,
        })
    }

    /// Enqueues a command into the submission ring.
    ///
    /// Does nothing when the queue is full (`(tail + 1) % size == head`).
    ///
    /// Returns the new tail index for the SQ Tail doorbell, or `None` if full.
    fn submit(&mut self, entry: NvmeCommand) -> Option<u16> {
        let next_tail = (self.tail + 1) % (QUEUE_DEPTH as u16);
        if next_tail == self.head {
            return None;
        }

        // K1 MUSE Book --- (SQ backing store is DmaStream; flush the new entry)
        // let ring_ptr: SafePtr<[NvmeCommand; QUEUE_DEPTH], &DmaCoherent> =
        let slot = self.tail;
        let ring_ptr: SafePtr<[NvmeCommand; QUEUE_DEPTH], &DmaStream> =
            // K1 MUSE Book ---
            field_ptr!(&self.squeue, Sqring, ring);
        let mut ring_slot_ptr = ring_ptr.cast::<NvmeCommand>();
        // K1 MUSE Book --- (use the saved SQ slot for both writing and cache synchronization)
        // ring_slot_ptr.add(self.tail as usize);
        ring_slot_ptr.add(slot as usize);
        // K1 MUSE Book ---
        ring_slot_ptr
            .write(&entry)
            .expect("SQ slot pointer must be valid within allocated DMA ring");

        // K1 MUSE Book --- (flush so the device fetches the command, not stale cache)
        let off = slot as usize * size_of::<NvmeCommand>();
        let _ = self
            .squeue
            .vm()
            .sync_to_device(off..off + size_of::<NvmeCommand>());
        // K1 MUSE Book ---

        self.tail = next_tail;
        Some(self.tail)
    }
}

pub(crate) struct NvmeCompletionQueueAccess<'a, Q> {
    qid: u16,
    dstrd: u16,
    queue: Q,
    dbregs: DbregAccess<'a>,
}

impl<'a, Q> NvmeCompletionQueueAccess<'a, Q>
where
    Q: DerefMut<Target = NvmeCompletionQueue>,
{
    /// Binds queue `qid` and doorbell stride `dstrd` to `queue` and `dbregs` for locked poll.
    pub(crate) fn new(qid: u16, dstrd: u16, queue: Q, dbregs: DbregAccess<'a>) -> Self {
        Self {
            qid,
            dstrd,
            queue,
            dbregs,
        }
    }

    /// Consumes all ready completions and updates the CQ head doorbell once.
    ///
    /// Returns an empty vector when no completion is ready.
    pub(crate) fn complete(&mut self) -> Vec<NvmeCompletion> {
        let mut entries = Vec::new();
        let mut last_head = None;
        // K1 MUSE Book --- (bounded completion batches preserve polling deadlines)
        // while let Some((entry, new_head)) = self.poll() {
        //     last_head = Some(new_head);
        //     entries.push(entry);
        // }
        // A broken controller must not keep one drain call alive indefinitely.
        for _ in 0..QUEUE_DEPTH {
            let Some((entry, new_head)) = self.poll() else {
                break;
            };
            last_head = Some(new_head);
            entries.push(entry);
        }
        // K1 MUSE Book ---
        if let Some(new_head) = last_head {
            self.ring_doorbell(new_head);
        }
        entries
    }

    /// Polls one completion without ringing the CQ doorbell.
    fn poll(&mut self) -> Option<(NvmeCompletion, u16)> {
        let (new_head, entry) = self.queue.complete()?;
        if entry.has_error() {
            warn!(
                "completion queue {}: command failed (CID={}, status={:04X}, SC={:#04x}, SQID={})",
                self.qid,
                entry.cid(),
                entry.status(),
                entry.status_code(),
                entry.sq_id(),
            );
        }
        Some((entry, new_head))
    }

    /// Rings the CQ head doorbell after one or more [`Self::poll`] calls.
    fn ring_doorbell(&mut self, new_head: u16) {
        // K1 MUSE Book --- (CQ reads must precede the device-I/O slot release)
        // fence(Ordering::SeqCst);
        ostd::mm::dma::dma_io_fence();
        // K1 MUSE Book ---
        self.dbregs.write_racy(
            NvmeDoorbellRegs::Cqhdbl,
            self.qid,
            self.dstrd,
            new_head as u32,
        );
    }
}

pub(crate) struct NvmeSubmissionQueueAccess<'a, Q> {
    qid: u16,
    dstrd: u16,
    queue: Q,
    dbregs: DbregAccess<'a>,
}

impl<'a, Q, C> NvmeSubmissionQueueAccess<'a, Q>
where
    Q: DerefMut<Target = NvmeSubmissionQueue<C>>,
{
    /// Binds queue `qid` and doorbell stride `dstrd` to `queue` and `dbregs` for locked submit.
    pub(crate) fn new(qid: u16, dstrd: u16, queue: Q, dbregs: DbregAccess<'a>) -> Self {
        Self {
            qid,
            dstrd,
            queue,
            dbregs,
        }
    }

    // K1 MUSE Book --- (Context queues also account for occupied CIDs)
    // /// Returns the number of free SQ slots available for new commands.
    // pub(crate) fn free_slots(&self) -> usize {
    //     self.queue.free_slots()
    // }
    // K1 MUSE Book ---

    /// Rings the SQ tail doorbell after one or more enqueued commands.
    fn ring_doorbell(&mut self) {
        let new_tail = self.queue.inner.tail;
        // Write barrier: do not update the doorbell until the submit entry writes finish.
        // K1 MUSE Book --- (publish SQ entries, contexts and PRPs before device I/O)
        // fence(Ordering::SeqCst);
        ostd::mm::dma::dma_io_fence();
        // K1 MUSE Book ---
        self.dbregs.write_racy(
            NvmeDoorbellRegs::Sqtdbl,
            self.qid,
            self.dstrd,
            new_tail as u32,
        );
    }
}

impl<'a, Q> NvmeSubmissionQueueAccess<'a, Q>
where
    Q: DerefMut<Target = NvmeSubmissionQueue<NoContext>>,
{
    /// Submits a command and rings the SQ tail doorbell.
    ///
    /// Writes at the current tail, sets the command identifier to that slot index, advances the
    /// tail, then updates the doorbell.
    ///
    /// Returns the command identifier used (same as the tail before enqueue), or `None` if the
    /// queue is full.
    pub(crate) fn submit(&mut self, mut entry: NvmeCommand) -> Option<u16> {
        let cid = self.queue.inner.tail;
        entry.set_cid(cid);
        self.queue.inner.submit(entry)?;
        self.ring_doorbell();
        Some(cid)
    }
}

impl<'a, Q, T> NvmeSubmissionQueueAccess<'a, Q>
where
    Q: DerefMut<Target = NvmeSubmissionQueue<Context<T>>>,
{
    // K1 MUSE Book --- (SQ consumption does not imply CID completion)
    /// Counts contiguous empty CID slots without overwriting a live request.
    pub(crate) fn free_slots(&self) -> usize {
        self.queue.free_context_slots()
    }
    // K1 MUSE Book ---

    /// Submits a batch of commands with contexts and rings the SQ doorbell once.
    pub(crate) fn submit_with_items(
        &mut self,
        commands: impl IntoIterator<Item = (NvmeCommand, T)>,
    ) -> Option<usize> {
        let mut count = 0;
        for (entry, item) in commands {
            self.enqueue_one(entry, item)?;
            count += 1;
        }
        if count > 0 {
            self.ring_doorbell();
        }
        Some(count)
    }

    /// Enqueues one command and context without ringing the SQ doorbell.
    // K1 MUSE Book --- (one shared CID publication operation)
    // fn enqueue_one(&mut self, mut entry: NvmeCommand, item: T) -> Option<u16> {
    //     let cid = self.queue.inner.tail;
    //     if self.queue.context.0[cid as usize].is_some() {
    //         warn!(
    //             "submission queue {} slot {} is still outstanding",
    //             self.qid, cid
    //         );
    //         return None;
    //     }
    //     entry.set_cid(cid);
    //     self.queue.inner.submit(entry)?;
    //     self.queue.context.0[cid as usize] = Some(item);
    //     Some(cid)
    // }
    fn enqueue_one(&mut self, entry: NvmeCommand, item: T) -> Option<u16> {
        self.queue.enqueue(entry, item)
    }
    // K1 MUSE Book ---
}

// K1 MUSE Book --- (queue ownership regression without MMIO)
#[cfg(ktest)]
mod ownership_test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn consumed_sq_slot_does_not_reuse_live_cid() {
        let mut queue = NvmeSubmissionQueue::new().unwrap().with_items::<usize>();
        // Hardware may consume the whole SQ before any command finishes.
        for i in 0..QUEUE_DEPTH - 1 {
            assert_eq!(
                queue.enqueue(crate::nvme_cmd::io_flush(1), i),
                Some(i as u16)
            );
        }
        queue.inner.head = queue.inner.tail;
        assert_eq!(queue.free_slots(), QUEUE_DEPTH - 1);
        assert_eq!(queue.free_context_slots(), 1);
        assert_eq!(queue.enqueue(crate::nvme_cmd::io_flush(1), 63), Some(63));
        queue.inner.head = queue.inner.tail;
        assert_eq!(queue.free_context_slots(), 0);
        assert!(queue.enqueue(crate::nvme_cmd::io_flush(1), 64).is_none());
        assert_eq!(queue.take_item(0), Some(0));
        assert_eq!(queue.free_context_slots(), 1);
        assert_eq!(queue.enqueue(crate::nvme_cmd::io_flush(1), 64), Some(0));
        assert_eq!(queue.take_item(0), Some(64));
        for cid in 1..QUEUE_DEPTH {
            assert_eq!(queue.take_item(cid as u16), Some(cid));
        }
    }
}
// K1 MUSE Book ---
