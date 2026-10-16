// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: NVMe failure lifecycle tests using real BIO/DMA owners.
// Hardware stages and clock/CSTS reads are injected. No PCI device is accessed.
// The unconfirmed-init cases deliberately retain a bounded set of queue pages,
// as production does; they do not claim device reset or recovery was tested.

use alloc::{boxed::Box, vec};
use core::cell::{Cell, RefCell};

use aster_block::{
    BlockDevice,
    bio::{Bio, BioDirection, BioSegment},
    id::Sid,
};
use io_util::{IoError, batch::IoBatch};
use ostd::{mm::FrameAllocOptions, prelude::ktest};

use super::*;

#[derive(Debug)]
struct SubmitSink(Mutex<Option<SubmittedBio>>);

impl BlockDevice for SubmitSink {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        *self.0.lock() = Some(bio);
        Ok(())
    }
    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: 128,
        }
    }
    fn name(&self) -> &str {
        "nvme-failure-fixture"
    }
    fn id(&self) -> DeviceId {
        unreachable!("fixture is never registered")
    }
}

fn submitted(
    callbacks: Arc<AtomicU32>,
) -> (
    SubmittedBio,
    IoBatch,
    Vec<Weak<dyn core::any::Any + Send + Sync>>,
    PrpPointers,
) {
    let mut segments = Vec::new();
    let mut weak = Vec::new();
    for pages in [4, 2] {
        // Unpooled backing makes final owner release observable.
        let raw = FrameAllocOptions::new()
            .alloc_segment(pages)
            .unwrap()
            .into();
        let segment = BioSegment::new_from_segment(raw, BioDirection::FromDevice);
        weak.push(segment.weak_owner_for_test());
        segments.push(segment);
    }
    let first = segments[0].dma_slice();
    let prp = PrpPointers::build_prp(
        first.daddr() as u64,
        NonZeroUsize::new(first.size()).unwrap(),
    )
    .unwrap();
    assert_ne!(prp.prp2(), 0); // Four pages need an actual owned PRP list.
    let bio = Bio::new(
        BioType::Read,
        Sid::from_offset(0),
        segments,
        Some(Box::new(move |status| {
            assert_eq!(status, BioStatus::IoError);
            callbacks.fetch_add(1, Ordering::Relaxed);
        })),
    );
    let sink = SubmitSink(Mutex::new(None));
    let mut batch = IoBatch::with_capacity(1);
    bio.submit(&sink, &mut batch).unwrap();
    let bio = sink.0.lock().take().unwrap();
    (bio, batch, weak, prp)
}

fn polling_and_stop_bounds() {
    let mut deadline = Duration::from_secs(3);
    // A large request that completes one CID every two seconds must not time out.
    for second in (2..=20).step_by(2) {
        assert!(!polling_stalled(
            &mut deadline,
            Duration::from_secs(second),
            true
        ));
        assert_eq!(deadline, Duration::from_secs(second + 3));
    }
    // Empty CQ and wrong-CID traffic cannot extend the last valid completion.
    assert!(!polling_stalled(
        &mut deadline,
        Duration::from_secs(22),
        false
    ));
    assert!(polling_stalled(
        &mut deadline,
        Duration::from_secs(23),
        false
    ));
    // The drain is checked first: a matching completion exactly at the boundary wins.
    assert!(!polling_stalled(
        &mut deadline,
        Duration::from_secs(23),
        true
    ));
    assert_eq!(deadline, Duration::from_secs(26));

    let polls = Cell::new(0);
    let timeouts = Cell::new(0);
    // Every step represents either empty CQ or one wrong CID. Both must check time.
    let result: Result<(), u8> = poll_command(
        || {
            polls.set(polls.get() + 1);
            None
        },
        || polls.get() >= 4,
        || {
            timeouts.set(timeouts.get() + 1);
            7
        },
    );
    assert_eq!(result, Err(7));
    assert_eq!(polls.get(), 4);
    assert_eq!(timeouts.get(), 1);

    // A matching completion at the deadline is allowed; no false timeout.
    let result = poll_command(|| Some(Ok::<_, u8>(23)), || true, || panic!("matching CQE"));
    assert_eq!(result, Ok(23));
    let result = poll_command(|| Some(Err::<(), _>(9)), || true, || panic!("CQE error"));
    assert_eq!(result, Err(9));

    let reads = Cell::new(0);
    assert!(wait_for_disabled(
        || {
            reads.set(reads.get() + 1);
            if reads.get() < 3 {
                InitContext::CSTS_RDY
            } else {
                InitContext::CSTS_CFS
            }
        },
        || reads.get() >= 3,
    ));
    assert_eq!(reads.get(), 3); // CFS set with RDY clear still establishes stopped.
    let reads = Cell::new(0);
    assert!(!wait_for_disabled(
        || {
            reads.set(reads.get() + 1);
            InitContext::CSTS_RDY | InitContext::CSTS_CFS
        },
        || reads.get() >= 5,
    ));
    assert_eq!(reads.get(), 5);
}

struct ControllerOwner {
    failure: FailureState<PendingOwner>,
    drops: Arc<AtomicU32>,
    sq: SpinLock<NvmeSubmissionQueueWithRequests, LocalIrqDisabled>,
    _cq: NvmeCompletionQueue,
}
struct PendingOwner {
    controller: Arc<ControllerOwner>,
    request: Arc<InFlightRequest>,
}
impl Drop for ControllerOwner {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn request_from_bio(bio: SubmittedBio) -> Arc<InFlightRequest> {
    Arc::new(InFlightRequest {
        bio_request: SpinLock::new(Some(bio.into())),
        remaining: AtomicUsize::new(2),
        is_failed: AtomicBool::new(false),
    })
}

fn take_pending(controller: &ControllerOwner) -> Vec<SubmittedRequest> {
    let mut sq = controller.sq.lock();
    (0..QUEUE_DEPTH)
        .filter_map(|cid| sq.take_item(cid as u16))
        .collect()
}

fn runtime_owners_and_callback_order() {
    // Both branches exercise the actual production retirement decision and CID table.
    for stopped in [true, false] {
        let drops = Arc::new(AtomicU32::new(0));
        let controller = Arc::new(ControllerOwner {
            failure: FailureState::new(),
            drops: drops.clone(),
            sq: SpinLock::new(NvmeSubmissionQueue::new().unwrap().with_items()),
            _cq: NvmeCompletionQueue::new().unwrap(),
        });
        let weak_controller = Arc::downgrade(&controller);
        let callbacks = Arc::new(AtomicU32::new(0));
        let (bio, batch, dma, prp) = submitted(callbacks.clone());
        let request = request_from_bio(bio);
        {
            let mut sq = controller.sq.lock();
            assert_eq!(
                sq.enqueue(
                    nvme_cmd::io_flush(1),
                    SubmittedRequest {
                        request: request.clone(),
                        _prp: Some(prp),
                    }
                ),
                Some(0)
            );
            assert_eq!(
                sq.enqueue(
                    nvme_cmd::io_flush(1),
                    SubmittedRequest {
                        request: request.clone(),
                        _prp: None,
                    }
                ),
                Some(1)
            );
        }
        assert_eq!(controller.failure.fail_and_stop(|| stopped), stopped);
        retire_or_retain_request(
            request,
            stopped,
            || take_pending(&controller),
            |request| {
                controller.failure.retain(PendingOwner {
                    controller: controller.clone(),
                    request,
                });
            },
        );
        assert!(controller.failure.check_running().is_err());
        assert_eq!(callbacks.load(Ordering::Relaxed), u32::from(stopped));
        assert!(dma.iter().all(|owner| owner.upgrade().is_some() != stopped));
        if stopped {
            assert!(take_pending(&controller).is_empty());
            drop(controller);
        } else {
            // Subsequent requests were never published and may complete normally as errors.
            let next_callbacks = Arc::new(AtomicU32::new(0));
            let (next, next_batch, next_dma, next_prp) = submitted(next_callbacks.clone());
            drop(next_prp);
            next.complete(BioStatus::IoError);
            assert_eq!(next_batch.wait_all(), Err(IoError::Failed));
            assert!(next_dma.iter().all(|owner| owner.upgrade().is_none()));
            assert_eq!(callbacks.load(Ordering::Relaxed), 0);
            drop(controller);
            let retained = weak_controller
                .upgrade()
                .expect("uncertain controller owns SQ/CQ and BIOs");
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            {
                let slot = retained.failure.quarantine.lock();
                let request = slot.as_ref().unwrap().request.bio_request.lock();
                assert_eq!(request.as_ref().unwrap().num_segments(), 2);
            }
            // No hardware exists here: only the fixture may establish quiescence and tear down.
            let PendingOwner {
                controller,
                request,
            } = retained.failure.quarantine.lock().take().unwrap();
            retire_or_retain_request(
                request,
                true,
                || take_pending(&controller),
                |_| panic!("stopped"),
            );
            drop(controller);
            drop(retained);
        }
        assert_eq!(batch.wait_all(), Err(IoError::Failed));
        assert_eq!(callbacks.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(weak_controller.upgrade().is_none());
        assert!(dma.iter().all(|owner| owner.upgrade().is_none()));
    }

    // A real two-command context completes the BIO only after its last CQE.
    let callbacks = Arc::new(AtomicU32::new(0));
    let (bio, batch, dma, prp) = submitted(callbacks.clone());
    let request = request_from_bio(bio);
    let first = SubmittedRequest {
        request: request.clone(),
        _prp: Some(prp),
    };
    let last = SubmittedRequest {
        request: request.clone(),
        _prp: None,
    };
    assert!(first.complete(false).is_none());
    assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    assert!(dma.iter().all(|owner| owner.upgrade().is_some()));
    last.complete(true).expect("last CID").finish();
    assert_eq!(callbacks.load(Ordering::Relaxed), 1);
    assert_eq!(batch.wait_all(), Err(IoError::Failed));
    assert!(dma.iter().all(|owner| owner.upgrade().is_none()));
    request.finish(); // No duplicate callback even after a conservative stop sweep.
    assert_eq!(callbacks.load(Ordering::Relaxed), 1);
}

struct InitFixture {
    fail: u8,
    stop: bool,
    trace: Arc<Mutex<Vec<u8>>>,
    drops: Arc<AtomicU32>,
    active: Option<Arc<DmaStream>>,
    active_weak: Arc<Mutex<Option<Weak<DmaStream>>>>,
    _sq: NvmeSubmissionQueue,
    _cq: NvmeCompletionQueue,
}
impl InitFixture {
    fn stage(&mut self, at: u8) -> Result<(), NvmeDeviceError> {
        self.trace.lock().push(at);
        if self.fail == at {
            Err(NvmeDeviceError::CommandTimedOut)
        } else {
            Ok(())
        }
    }
    fn identify(&mut self, at: u8) -> Result<(), NvmeDeviceError> {
        let dma = begin_identify(&mut self.active)?;
        *self.active_weak.lock() = Some(Arc::downgrade(&dma));
        // The same local SafePtr owner as the three production Identify methods.
        let _local: SafePtr<IdentifyControllerData, Arc<DmaStream>> = SafePtr::new(dma, 0);
        self.stage(at)?;
        self.active = None;
        Ok(())
    }
}
impl Drop for InitFixture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}
impl ControllerInit for InitFixture {
    fn reset(&mut self) -> Result<(), NvmeDeviceError> {
        self.stage(1)
    }
    fn configure_admin(&mut self) {
        self.trace.lock().push(10);
    }
    fn set_entry_size(&mut self) {
        self.trace.lock().push(11);
    }
    fn enable(&mut self) -> Result<(), NvmeDeviceError> {
        self.stage(2)
    }
    fn identify_controller(&mut self) -> Result<(), NvmeDeviceError> {
        self.identify(3)
    }
    fn identify_ns_list(&mut self) -> Result<Vec<u32>, NvmeDeviceError> {
        self.identify(4)?;
        Ok(if self.fail == 5 { vec![] } else { vec![1] })
    }
    fn identify_ns(&mut self, nsid: u32) -> Result<NvmeNamespace, NvmeDeviceError> {
        assert_eq!(nsid, 1);
        self.identify(6)?;
        Ok(NvmeNamespace { id: 1, nsze: 128 })
    }
    fn create_io_queues(&mut self) -> Result<IoMsixVectors, NvmeDeviceError> {
        self.stage(7)?;
        Ok(IoMsixVectors([0; QUEUE_NUM - 1]))
    }
    fn disable(&mut self) -> bool {
        self.trace.lock().push(99);
        self.stop
    }
}
fn init_fixture(fail: u8, stop: bool) -> InitFixture {
    InitFixture {
        fail,
        stop,
        trace: Arc::new(Mutex::new(Vec::new())),
        drops: Arc::new(AtomicU32::new(0)),
        active: None,
        active_weak: Arc::new(Mutex::new(None)),
        _sq: NvmeSubmissionQueue::new().unwrap(),
        _cq: NvmeCompletionQueue::new().unwrap(),
    }
}

fn initialization_and_identify_owners() {
    // Every fallible post-allocation stage and the successful empty-list rejection.
    for fail in 1..=7 {
        for stop in [true, false] {
            let fixture = init_fixture(fail, stop);
            let drops = fixture.drops.clone();
            let trace = fixture.trace.clone();
            let active = fixture.active_weak.clone();
            assert!(run_initialization(fixture).is_err());
            assert_eq!(drops.load(Ordering::Relaxed), if stop { 1 } else { 0 });
            let trace = trace.lock();
            assert_eq!(trace.last(), Some(&99));
            assert_eq!(trace.iter().filter(|stage| **stage == 99).count(), 1);
            let active = active.lock();
            if [3, 4, 6].contains(&fail) {
                let weak = active
                    .as_ref()
                    .expect("Identify started before injected error");
                assert_eq!(weak.upgrade().is_some(), !stop);
            }
        }
    }
    let fixture = init_fixture(0, true);
    let trace = fixture.trace.clone();
    let drops = fixture.drops.clone();
    let (fixture, namespace, _) = run_initialization(fixture).expect("normal sequence");
    assert_eq!(namespace.id, 1);
    assert_eq!(*trace.lock(), [1, 10, 11, 2, 3, 4, 6, 7]);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(fixture);
    assert_eq!(drops.load(Ordering::Relaxed), 1);

    let mut active = None;
    let dma = begin_identify(&mut active).unwrap();
    let weak = Arc::downgrade(&dma);
    drop(dma);
    assert!(begin_identify(&mut active).is_err()); // Cannot overwrite a live command.
    assert!(weak.upgrade().is_some());
    drop(active);
    assert!(weak.upgrade().is_none());
}

struct RegistrationOwner(Arc<AtomicU32>);
impl Drop for RegistrationOwner {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn registration_failure_owners() {
    for stop in [true, false] {
        let drops = Arc::new(AtomicU32::new(0));
        let owner = Arc::new(RegistrationOwner(drops.clone()));
        let weak = Arc::downgrade(&owner);
        let state: FailureState<Arc<RegistrationOwner>> = FailureState::new();
        assert!(!register_or_stop(
            owner,
            |_| false,
            |_| state.fail_and_stop(|| stop),
            |owner| state.retain(owner),
        ));
        assert!(matches!(
            state.check_running(),
            Err(NvmeDeviceError::ControllerFailed)
        ));
        assert_eq!(weak.upgrade().is_some(), !stop);
        assert_eq!(drops.load(Ordering::Relaxed), if stop { 1 } else { 0 });
        drop(state); // Test-only release of the fake, quiescent owner.
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    let registered = RefCell::new(None);
    let owner = Arc::new(RegistrationOwner(Arc::new(AtomicU32::new(0))));
    let weak = Arc::downgrade(&owner);
    assert!(register_or_stop(
        owner,
        |owner| {
            *registered.borrow_mut() = Some(owner.clone());
            true
        },
        |_| panic!("successful registration must not stop"),
        |_| panic!("successful registration must not quarantine"),
    ));
    assert!(weak.upgrade().is_some());
    drop(registered);
    assert!(weak.upgrade().is_none());
}

#[ktest]
fn nvme_failure_ownership_regression() {
    polling_and_stop_bounds();
    runtime_owners_and_callback_order();
    initialization_and_identify_owners();
    registration_failure_owners();
}
