// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: USB timeout ownership regression using real BIOs and DMA pools.
// No MMIO or USB device is accessed. Completion classification is injected;
// transfer-event matching is tested separately in xhci::completion. The two
// permanent-retention cases deliberately retain three DMA pages in total.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, Ordering};

use aster_block::{
    BlockDevice, BlockDeviceMeta,
    bio::{
        Bio, BioDirection, BioEnqueueError, BioSegment, BioStatus, BioType, SubmittedBio,
        bio_segment_pool_init,
    },
    id::Sid,
};
use device_id::DeviceId;
use io_util::{IoError, batch::IoBatch};
use ostd::{
    mm::{FrameAllocOptions, HasDaddr, dma::DmaStream},
    prelude::ktest,
};
use spin::Mutex;

use crate::{
    block_device::{complete_or_retain, transfer_segments},
    xhci::{
        Error,
        ownership::{DmaRetention, FailureState},
    },
};

#[derive(Debug)]
struct SubmitSink(Mutex<Option<SubmittedBio>>);

impl BlockDevice for SubmitSink {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        let mut slot = self.0.lock();
        assert!(slot.is_none());
        *slot = Some(bio);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: 2,
            nr_sectors: 128,
        }
    }

    fn name(&self) -> &str {
        "usb-failure-fixture"
    }

    fn id(&self) -> DeviceId {
        unreachable!("fixture is never registered")
    }
}

struct SubmittedFixture {
    bio: SubmittedBio,
    batch: IoBatch,
    callbacks: Arc<AtomicU32>,
    dma: Vec<Weak<dyn core::any::Any + Send + Sync>>,
}

fn submitted(
    type_: BioType,
    segments: Vec<BioSegment>,
    expected_status: BioStatus,
    expect_dma_release: bool,
) -> SubmittedFixture {
    let dma: Vec<_> = segments
        .iter()
        .map(|segment| segment.weak_owner_for_test())
        .collect();
    let callback_dma = dma.clone();
    let callbacks = Arc::new(AtomicU32::new(0));
    let callback_counter = callbacks.clone();
    let bio = Bio::new(
        type_,
        Sid::from_offset(0),
        segments,
        Some(Box::new(move |status| {
            assert_eq!(status, expected_status);
            if expect_dma_release {
                // Actual SubmittedBio::complete drops unpooled segments before
                // invoking the callback and publishing the completion status.
                assert!(callback_dma.iter().all(|owner| owner.upgrade().is_none()));
            }
            assert_eq!(callback_counter.fetch_add(1, Ordering::Relaxed), 0);
        })),
    );
    let sink = SubmitSink(Mutex::new(None));
    let mut batch = IoBatch::with_capacity(1);
    bio.submit(&sink, &mut batch).unwrap();
    let bio = sink.0.lock().take().unwrap();
    assert_eq!(bio.status(), BioStatus::Submit);
    assert_eq!(batch.len(), 1);
    SubmittedFixture {
        bio,
        batch,
        callbacks,
        dma,
    }
}

fn unpooled_segments(direction: BioDirection) -> Vec<BioSegment> {
    (0..2)
        .map(|_| {
            let raw = FrameAllocOptions::new().alloc_segment(1).unwrap().into();
            BioSegment::new_from_segment(raw, direction)
        })
        .collect()
}

fn timeout_keeps_pool_slots_and_rejects_later_bios(fail_at: usize) {
    bio_segment_pool_init();
    let segments = vec![
        BioSegment::alloc(1, BioDirection::FromDevice),
        BioSegment::alloc(1, BioDirection::FromDevice),
    ];
    // Verify that the fixture exercises actual shared arena allocations.
    assert!(segments.iter().all(BioSegment::is_arena_allocated_for_test));
    let addresses = [
        segments[0].dma_slice().daddr(),
        segments[1].dma_slice().daddr(),
    ];
    assert_ne!(addresses[0], addresses[1]);
    let SubmittedFixture {
        bio,
        batch,
        callbacks,
        ..
    } = submitted(BioType::Read, segments, BioStatus::IoError, false);
    // Inject failure in each of the two real segments through the helper used
    // by both production I/O entry points. A first-segment timeout must prevent
    // the second submission; a second-segment timeout must retain both owners.
    let mut calls = Vec::new();
    let result = transfer_segments(&bio, 17, |segment, lba| {
        calls.push((segment.dma_slice().daddr(), lba));
        if calls.len() == fail_at {
            Err(Error::Timeout)
        } else {
            Ok(())
        }
    });
    assert_eq!(result, Err(Error::Timeout));
    assert_eq!(calls.len(), fail_at);
    let mut next_lba = 17;
    for ((address, lba), segment) in calls.iter().zip(bio.segments()) {
        assert_eq!(*address, segment.dma_slice().daddr());
        assert_eq!(*lba, next_lba);
        next_lba += segment.nsectors().to_raw();
    }
    let mut failure = FailureState::new();
    assert_eq!(failure.check_running(), Ok(()));
    failure.fail();
    complete_or_retain(bio, result, |bio| failure.retain(bio));
    let pending = failure.retained().unwrap();
    assert_eq!(pending.status(), BioStatus::Submit);
    assert_eq!(pending.segments().len(), 2);
    for (segment, address) in pending.segments().iter().zip(addresses) {
        assert_eq!(segment.dma_slice().daddr(), address);
    }
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    assert_eq!(failure.check_running(), Err(Error::ControllerFailed));

    let other = BioSegment::alloc(1, BioDirection::FromDevice);
    assert!(other.is_arena_allocated_for_test());
    assert!(!addresses.contains(&other.dma_slice().daddr()));

    // This request has not reached hardware. Its error must not overwrite the
    // one uncertain owner, and callers must receive a normal failed completion.
    let SubmittedFixture {
        bio: next,
        batch: next_batch,
        callbacks: next_callbacks,
        dma: next_dma,
    } = submitted(
        BioType::Read,
        unpooled_segments(BioDirection::FromDevice),
        BioStatus::IoError,
        true,
    );
    complete_or_retain(next, failure.check_running(), |_| {
        panic!("a rejected request cannot enter quarantine")
    });
    assert_eq!(next_callbacks.load(Ordering::Relaxed), 1);
    assert!(next_dma.iter().all(|owner| owner.upgrade().is_none()));
    assert_eq!(next_batch.wait_all(), Err(IoError::Failed));
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    let pending = failure.retained().unwrap();
    assert_eq!(pending.status(), BioStatus::Submit);
    assert_eq!(pending.segments()[0].dma_slice().daddr(), addresses[0]);

    // There is no device in this fixture, so test-only teardown establishes
    // quiescence. Production has no API that drains an uncertain controller.
    let pending = failure.take_retained_for_test().unwrap();
    pending.complete(BioStatus::IoError);
    assert_eq!(callbacks.load(Ordering::Relaxed), 1);
    assert_eq!(batch.wait_all(), Err(IoError::Failed));
    assert!(failure.retained().is_none());
    assert_eq!(failure.check_running(), Err(Error::ControllerFailed));
    drop(other);
}

fn completed_transfers_release_segments_once() {
    for (type_, direction, result, status, expected) in [
        (
            BioType::Read,
            BioDirection::FromDevice,
            Ok(()),
            BioStatus::Complete,
            Ok(()),
        ),
        (
            BioType::Write,
            BioDirection::ToDevice,
            Err(Error::Stall),
            BioStatus::IoError,
            Err(IoError::Failed),
        ),
    ] {
        let SubmittedFixture {
            bio,
            batch,
            callbacks,
            dma,
        } = submitted(type_, unpooled_segments(direction), status, true);
        complete_or_retain(bio, result, |_| panic!("completed transfer"));
        assert_eq!(callbacks.load(Ordering::Relaxed), 1);
        assert!(dma.iter().all(|owner| owner.upgrade().is_none()));
        assert_eq!(batch.wait_all(), expected);
    }

    let SubmittedFixture {
        bio,
        batch,
        callbacks,
        ..
    } = submitted(BioType::Flush, vec![], BioStatus::Complete, true);
    complete_or_retain(bio, Ok(()), |_| panic!("completed flush"));
    assert_eq!(callbacks.load(Ordering::Relaxed), 1);
    assert_eq!(batch.wait_all(), Ok(()));
}

fn dropping_failed_state_preserves_the_uncompleted_bio() {
    let SubmittedFixture {
        bio,
        batch,
        callbacks,
        dma,
    } = submitted(
        BioType::Read,
        unpooled_segments(BioDirection::FromDevice),
        BioStatus::IoError,
        true,
    );
    let mut failure = FailureState::new();
    failure.fail();
    complete_or_retain(bio, Err(Error::Timeout), |bio| failure.retain(bio));
    assert_eq!(failure.retained().unwrap().status(), BioStatus::Submit);
    drop(failure);
    drop(batch); // A waiter handle never owns the DMA or completes the request.
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    assert!(dma.iter().all(|owner| owner.upgrade().is_some()));
}

struct DmaOwner {
    _dma: Arc<DmaStream>,
    drops: Arc<AtomicU32>,
}

impl Drop for DmaOwner {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn published_dma_outlives_its_container() {
    for published in [false, true] {
        let dma = Arc::new(DmaStream::alloc_uninit(1, false).unwrap());
        let weak = Arc::downgrade(&dma);
        let drops = Arc::new(AtomicU32::new(0));
        let mut owner = DmaRetention::new(DmaOwner {
            _dma: dma,
            drops: drops.clone(),
        });
        if published {
            owner.publish();
        }
        drop(owner);
        assert_eq!(drops.load(Ordering::Relaxed), u32::from(!published));
        assert_eq!(weak.upgrade().is_some(), published);
    }
}

#[ktest]
fn usb_failure_ownership_regression() {
    timeout_keeps_pool_slots_and_rejects_later_bios(1);
    timeout_keeps_pool_slots_and_rejects_later_bios(2);
    completed_transfers_release_segments_once();
    dropping_failed_state_preserves_the_uncompleted_bio();
    published_dma_outlives_its_container();
}
