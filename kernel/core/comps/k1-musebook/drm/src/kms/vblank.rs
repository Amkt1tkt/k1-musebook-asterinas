// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: generation-tagged display completion and scanout ownership.

//! Per-CRTC vblank sequences, flip acknowledgment, and scanout retirement.
//!
//! Drivers start an update before publishing shadow registers and record tagged
//! hardware observations under their register/IRQ lock. Flip acknowledgment and
//! safe DMA retirement are independent; an early acknowledgment survives software
//! publication and event parking without releasing old scanout storage.
//!
//! The three-tick event fallback preserves client progress but is not hardware
//! confirmation. Old scanout objects remain retained until a confirmed update.

use alloc::{sync::Arc, vec::Vec};

use ostd::{
    sync::{LocalIrqDisabled, SpinLock},
    timer::Jiffies,
};

use super::{AtomicState, ObjectId};
use crate::{
    error::{DrmError, DrmResult},
    file::{DrmEvent, EventSink, FileId},
};

/// Facts acknowledged by a display driver while its register/IRQ lock is held.
/// Presentation follows the driver's event contract. Retirement independently
/// proves that old scanout reads have finished; coalesced status is not enough.
#[derive(Clone, Copy, Debug, Default)]
pub struct HardwareObservation {
    /// Whether this acknowledgment contains a real vertical blank.
    pub vblank: bool,
    /// The update whose flip event may be delivered, independent of retirement.
    pub flipped_update: Option<u64>,
    /// The latest update for which old DMA-backed state may be released.
    /// This can precede the currently programmed generation.
    pub retired_update: Option<u64>,
}

/// When a successful commit's user event is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipTiming {
    /// Queue the event after software-state publication.
    ///
    /// Software drivers need no hardware confirmation. A hardware driver using
    /// this fallback must still track updates and retain old DMA owners.
    Immediate,
    /// Queue the event after the matching latch and vertical blank.
    ///
    /// Drivers call [`Vblank::begin_update`] before register publication, record
    /// [`Vblank::observe_hardware`] under their IRQ lock, and dispatch in the
    /// bottom half. Software drivers can use the tagged arm/tick pair.
    AtVblank,
}

/// Shared display-completion state of one device.
///
/// The short lock disables local interrupts and is shared with the DPU bottom
/// half. Allocation, event delivery, and final-reference destruction run outside
/// it. Hardware interrupt handlers record observations; event-producing dispatch
/// and software tick calls run in process or bottom-half context.
/// Retained scanouts are collected only by the process-context commit/close path.
#[derive(Clone, Debug)]
pub struct Vblank {
    inner: Arc<SpinLock<Inner, LocalIrqDisabled>>,
}

#[derive(Debug)]
struct Inner {
    crtcs: Vec<CrtcVblank>,
}

#[derive(Debug)]
struct CrtcVblank {
    id: ObjectId,
    sequence: u32,
    generation: u64,
    flip_complete: u64,
    retirement_complete: u64,
    completion: Completion,
    armed: bool,
    pending: Option<PendingFlip>,
    // Two unconfirmed transitions bound retained DMA memory even if event
    // fallback continues. Later commits return Busy until confirmation arrives.
    retired: [Option<RetiredScanout>; 2],
}

#[derive(Clone, Copy, Debug, Default)]
struct Completion {
    sequence: u32,
    tv_sec: u32,
    tv_usec: u32,
}

#[derive(Debug)]
struct RetiredScanout {
    generation: u64,
    _state: Arc<AtomicState>,
}

#[derive(Debug)]
struct PendingFlip {
    sink: EventSink,
    user_data: u64,
    crtc_id: u32,
    generation: u64,
    ticks_waiting: u32,
}

// Keep the established event-progress policy. This is not DMA retirement proof.
const MISSED_LATCH_TICKS: u32 = 3;

impl Vblank {
    pub(super) fn new(crtcs: impl IntoIterator<Item = ObjectId>) -> Self {
        let crtcs = crtcs
            .into_iter()
            .map(|id| CrtcVblank {
                id,
                sequence: 0,
                generation: 0,
                flip_complete: 0,
                retirement_complete: 0,
                completion: Completion::default(),
                armed: false,
                pending: None,
                retired: [None, None],
            })
            .collect();
        Self {
            inner: Arc::new(SpinLock::new(Inner { crtcs })),
        }
    }

    /// Starts one hardware update while the driver's register/IRQ lock is held.
    ///
    /// The driver must exclude or tag old interrupt status before this call and
    /// preserve this generation when acknowledging status and scheduling a bottom
    /// half. Each successful hardware commit starts exactly one update per CRTC.
    pub fn begin_update(&self, crtc: ObjectId) -> DrmResult<u64> {
        let index = self.slot(crtc).ok_or(DrmError::InvalidArgument)?;
        let mut inner = self.inner.lock();
        let slot = &mut inner.crtcs[index];
        let generation = slot.generation.checked_add(1).ok_or(DrmError::NoSpace)?;
        slot.generation = generation;
        slot.armed = false;
        Ok(generation)
    }

    /// Returns the generation most recently started for this CRTC.
    pub fn generation(&self, crtc: ObjectId) -> u64 {
        self.slot(crtc)
            .map_or(0, |index| self.inner.lock().crtcs[index].generation)
    }

    /// Whether the old scanout state replaced by this update is safe to retire.
    pub fn is_update_retired(&self, crtc: ObjectId, generation: u64) -> bool {
        self.slot(crtc)
            .is_some_and(|index| generation <= self.inner.lock().crtcs[index].retirement_complete)
    }

    /// Records that the matching shadow register image has been consumed.
    pub fn arm_update(&self, crtc: ObjectId, generation: u64) {
        let Some(index) = self.slot(crtc) else {
            return;
        };
        let mut inner = self.inner.lock();
        let slot = &mut inner.crtcs[index];
        if slot.generation == generation && generation > slot.retirement_complete {
            slot.armed = true;
        }
    }

    /// Confirms an update whose register image is already scanning out.
    ///
    /// Only use this for a proven unchanged live image or stopped engine. Merely
    /// seeing a consumed shadow is insufficient: old DMA reads may be in flight.
    /// This method records state only, so it is safe under the driver's IRQ lock.
    pub fn complete_update(&self, crtc: ObjectId, generation: u64) {
        self.observe_hardware(
            crtc,
            HardwareObservation {
                vblank: false,
                flipped_update: Some(generation),
                retired_update: Some(generation),
            },
        );
    }

    #[cfg(ktest)]
    pub(super) fn has_pending(&self, crtc: ObjectId) -> bool {
        self.slot(crtc)
            .is_some_and(|index| self.inner.lock().crtcs[index].pending.is_some())
    }

    pub(super) fn can_update(&self, crtc: ObjectId, retires_scanout: bool) -> bool {
        self.slot(crtc).is_some_and(|index| {
            let inner = self.inner.lock();
            let slot = &inner.crtcs[index];
            slot.pending.is_none() && (!retires_scanout || slot.retired.iter().any(Option::is_none))
        })
    }

    /// Retains the previous state until the hardware no longer reads its buffers.
    /// Admission is checked before register writes while commits are serialized.
    pub(super) fn retain_scanout(&self, crtc: ObjectId, generation: u64, state: Arc<AtomicState>) {
        let index = self.slot(crtc).expect("registered CRTC");
        let mut retained = Some(RetiredScanout {
            generation,
            _state: state,
        });
        {
            let mut inner = self.inner.lock();
            let slot = &mut inner.crtcs[index];
            if generation > slot.retirement_complete {
                let target = slot
                    .retired
                    .iter_mut()
                    .find(|entry| entry.is_none())
                    .expect("scanout retirement space was checked before commit");
                *target = retained.take();
            }
        }
        drop(retained);
    }

    /// Releases confirmed old states in process context, after unlocking.
    pub(super) fn retire_completed(&self) {
        let count = self.inner.lock().crtcs.len();
        for index in 0..count {
            for entry in 0..2 {
                let retired = {
                    let mut inner = self.inner.lock();
                    let slot = &mut inner.crtcs[index];
                    if slot.retired[entry]
                        .as_ref()
                        .is_some_and(|old| old.generation <= slot.retirement_complete)
                    {
                        slot.retired[entry].take()
                    } else {
                        None
                    }
                };
                drop(retired);
            }
        }
    }

    /// Parks after software publication, preserving a confirmation that arrived
    /// between register publication and this call. Event reservations remain live
    /// until delivery or the caller releases them after an error.
    pub(super) fn park(&self, crtc: ObjectId, sink: EventSink, user_data: u64) -> DrmResult<()> {
        let index = self.slot(crtc).ok_or(DrmError::InvalidArgument)?;
        let completion;
        let mut pending = Some(PendingFlip {
            sink,
            user_data,
            crtc_id: crtc.get(),
            generation: 0,
            ticks_waiting: 0,
        });
        {
            let mut inner = self.inner.lock();
            let slot = &mut inner.crtcs[index];
            if slot.pending.is_some() {
                return Err(DrmError::Busy);
            }
            pending.as_mut().unwrap().generation = slot.generation;
            completion = (slot.generation <= slot.flip_complete).then_some(slot.completion);
            if completion.is_none() {
                slot.pending = pending.take();
            }
        }
        if let Some(completion) = completion {
            deliver(pending.unwrap(), completion);
        }
        Ok(())
    }

    /// Records hardware facts without callbacks or resource destruction.
    ///
    /// The driver calls this under its register/IRQ lock. Presentation and old
    /// DMA retirement are distinct: an event acknowledgment must not release
    /// storage unless `retired_update` supplies independent hardware proof.
    /// Retirement of an earlier generation survives continuous later commits.
    pub fn observe_hardware(&self, crtc: ObjectId, observation: HardwareObservation) -> u32 {
        let Some(index) = self.slot(crtc) else {
            return 0;
        };
        let (tv_sec, tv_usec) = monotonic_timestamp();
        let mut inner = self.inner.lock();
        let slot = &mut inner.crtcs[index];
        if observation.vblank {
            slot.sequence = slot.sequence.wrapping_add(1);
        }
        if let Some(retired) = observation.retired_update
            && retired <= slot.generation
        {
            slot.retirement_complete = slot.retirement_complete.max(retired);
        }
        if let Some(flipped) = observation.flipped_update
            && flipped == slot.generation
            && flipped > slot.flip_complete
        {
            slot.flip_complete = flipped;
            slot.completion = Completion {
                sequence: slot.sequence,
                tv_sec,
                tv_usec,
            };
        }
        slot.sequence
    }

    /// Delivers an acknowledged vblank in bottom-half context.
    /// This does not count it twice or infer any additional DMA completion.
    pub fn dispatch_observed_vblank(&self, crtc: ObjectId, generation: u64) -> u32 {
        self.tick_inner(crtc, generation, false, false)
    }

    /// Completes an armed software update and counts its vblank.
    /// Hardware drivers instead record independently proven flip and retirement
    /// observations. Stale tags cannot complete a newer update or its fallback.
    pub fn tick_update(&self, crtc: ObjectId, generation: u64) -> u32 {
        self.tick_inner(crtc, generation, true, true)
    }

    /// Advances only the event-progress fallback when the hardware clock stalls.
    /// A synthetic tick must never confirm a latch or retire DMA owners, even
    /// when a real shadow-consumed observation already armed the update.
    pub fn fallback_tick(&self, crtc: ObjectId) -> u32 {
        self.tick_inner(crtc, self.generation(crtc), false, true)
    }

    fn tick_inner(
        &self,
        crtc: ObjectId,
        generation: u64,
        hardware_vblank: bool,
        advance_sequence: bool,
    ) -> u32 {
        let Some(index) = self.slot(crtc) else {
            return 0;
        };
        let (tv_sec, tv_usec) = monotonic_timestamp();
        let (sequence, pending, discarded, unarmed_ticks, completion) = {
            let mut inner = self.inner.lock();
            let slot = &mut inner.crtcs[index];
            if advance_sequence {
                slot.sequence = slot.sequence.wrapping_add(1);
            }
            let completion = Completion {
                sequence: slot.sequence,
                tv_sec,
                tv_usec,
            };
            let matches = generation == slot.generation;
            if matches && slot.armed && hardware_vblank {
                slot.armed = false;
                slot.retirement_complete = generation;
                if generation > slot.flip_complete {
                    slot.flip_complete = generation;
                    slot.completion = completion;
                }
            }
            let event_completion = if matches && generation <= slot.flip_complete {
                slot.completion
            } else {
                completion
            };
            let discarded = if slot.pending.as_ref().is_some_and(|p| p.sink.is_dead()) {
                slot.pending.take()
            } else {
                None
            };
            let mut unarmed_ticks = None;
            let deliver = slot.pending.as_mut().is_some_and(|p| {
                if !matches || p.generation != generation {
                    return false;
                }
                p.ticks_waiting = p.ticks_waiting.saturating_add(1);
                if generation <= slot.flip_complete {
                    return true;
                }
                if p.ticks_waiting >= MISSED_LATCH_TICKS {
                    unarmed_ticks = Some(p.ticks_waiting);
                    return true;
                }
                false
            });
            let pending = if deliver { slot.pending.take() } else { None };
            (
                slot.sequence,
                pending,
                discarded,
                unarmed_ticks,
                event_completion,
            )
        };
        drop(discarded);
        if let Some(ticks) = unarmed_ticks {
            ostd::warn!(
                "delivering a page-flip event on crtc {} after {} vblanks without hardware latch confirmation",
                crtc.get(),
                ticks
            );
        }
        if let Some(pending) = pending {
            deliver(pending, completion);
        }
        sequence
    }

    /// Counts an untagged software vblank. Hardware drivers use
    /// [`Self::observe_hardware`] and [`Self::dispatch_observed_vblank`].
    pub fn tick(&self, crtc: ObjectId) -> u32 {
        self.tick_update(crtc, self.generation(crtc))
    }

    /// Arms an untagged software update. Hardware drivers record observations.
    pub fn arm(&self, crtc: ObjectId) {
        self.arm_update(crtc, self.generation(crtc));
    }

    pub(super) fn next_immediate(&self, crtc: ObjectId) -> u32 {
        let Some(index) = self.slot(crtc) else {
            return 0;
        };
        let mut inner = self.inner.lock();
        let slot = &mut inner.crtcs[index];
        slot.sequence = slot.sequence.wrapping_add(1);
        slot.sequence
    }

    /// Returns the current vblank sequence without incrementing.
    pub fn sequence(&self, crtc: ObjectId) -> u32 {
        self.slot(crtc)
            .map_or(0, |index| self.inner.lock().crtcs[index].sequence)
    }

    /// Cancels a closing file's event, without releasing unconfirmed DMA owners.
    pub(super) fn drop_file(&self, owner: FileId) {
        let count = self.inner.lock().crtcs.len();
        for index in 0..count {
            let removed = {
                let mut inner = self.inner.lock();
                let slot = &mut inner.crtcs[index];
                if slot
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.sink.file_id() == owner)
                {
                    slot.pending.take()
                } else {
                    None
                }
            };
            drop(removed);
        }
    }

    fn slot(&self, crtc: ObjectId) -> Option<usize> {
        self.inner
            .lock()
            .crtcs
            .iter()
            .position(|slot| slot.id == crtc)
    }
}

fn deliver(pending: PendingFlip, completion: Completion) {
    pending.sink.queue_event(DrmEvent::FlipComplete {
        user_data: pending.user_data,
        tv_sec: completion.tv_sec,
        tv_usec: completion.tv_usec,
        sequence: completion.sequence,
        crtc_id: pending.crtc_id,
    });
}

fn monotonic_timestamp() -> (u32, u32) {
    let now = Jiffies::elapsed().as_duration();
    (now.as_secs() as u32, now.subsec_micros())
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;
    use ostd_pod::Pod;

    use super::*;
    use crate::{
        device::{DriverFeatures, DriverInfo, DrmDevice, DrmDriver, DrmMinor},
        file::{DrmFile, Privilege},
        uapi::DrmEventVblank,
    };

    /// A sink that records events without a real DRM file.
    ///
    /// [`EventSink`] is a `Weak` to a file's queue; building one here would
    /// need a whole device. The vblank state machine is tested through
    /// [`Vblank`] methods that do not queue, plus the ModeConfig tests in
    /// `kms/test.rs` that go through a real file.
    #[ktest]
    fn sequence_starts_at_zero_and_tick_increments() {
        let crtc = ObjectId(1);
        let vblank = Vblank::new([crtc]);

        assert_eq!(vblank.sequence(crtc), 0);
        assert_eq!(vblank.tick(crtc), 1);
        assert_eq!(vblank.tick(crtc), 2);
        assert_eq!(vblank.sequence(crtc), 2);
        assert_eq!(vblank.next_immediate(crtc), 3);
        assert_eq!(vblank.sequence(crtc), 3);
        assert!(!vblank.has_pending(crtc));
    }

    #[ktest]
    fn an_unknown_crtc_is_a_zero_sequence() {
        let vblank = Vblank::new([ObjectId(1)]);
        assert_eq!(vblank.tick(ObjectId(99)), 0);
        assert_eq!(vblank.sequence(ObjectId(99)), 0);
        assert!(!vblank.has_pending(ObjectId(99)));
    }

    #[ktest]
    fn wrapping_add_does_not_panic_at_u32_max() {
        let crtc = ObjectId(1);
        let vblank = Vblank::new([crtc]);
        {
            let mut inner = vblank.inner.lock();
            inner.crtcs[0].sequence = u32::MAX;
        }
        assert_eq!(vblank.tick(crtc), 0);
        assert_eq!(vblank.tick(crtc), 1);
    }

    /// Parks through a real file so the event-queue side of [`Vblank::tick`]
    /// is exercised: the event must not appear until a tick after [`Vblank::arm`].
    #[ktest]
    fn a_parked_flip_waits_for_arm_then_tick() {
        let (file, crtc, vblank) = file_and_vblank();
        assert!(file.reserve_events(1));
        vblank
            .park(crtc, file.event_sink(), 0xabc)
            .expect("first park");
        assert!(vblank.has_pending(crtc));
        assert!(read_one(&file).is_none());

        // A tick before arm counts the vblank but does not complete the flip.
        assert_eq!(vblank.tick(crtc), 1);
        assert!(vblank.has_pending(crtc));
        assert!(read_one(&file).is_none());

        vblank.arm(crtc);
        assert_eq!(vblank.tick(crtc), 2);
        assert!(!vblank.has_pending(crtc));

        let event = read_one(&file).expect("the flip completed");
        assert_eq!(event.base.type_, crate::uapi::event_type::FLIP_COMPLETE);
        assert_eq!(event.user_data, 0xabc);
        assert_eq!(event.sequence, 2);
        assert_eq!(event.crtc_id, crtc.get());
    }

    #[ktest]
    fn a_second_park_is_busy_while_one_is_pending() {
        let (file, crtc, vblank) = file_and_vblank();
        assert!(file.reserve_events(2));
        vblank.park(crtc, file.event_sink(), 1).expect("first park");
        assert_eq!(vblank.park(crtc, file.event_sink(), 2), Err(DrmError::Busy));
        file.release_events(1);
    }

    #[ktest]
    fn three_unarmed_ticks_deliver_as_a_safety_net() {
        let (file, crtc, vblank) = file_and_vblank();
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 7).expect("park");

        assert!(vblank.tick(crtc) == 1 && read_one(&file).is_none());
        assert!(vblank.tick(crtc) == 2 && read_one(&file).is_none());
        assert_eq!(vblank.tick(crtc), 3);

        let event = read_one(&file).expect("safety-net delivery");
        assert_eq!(event.user_data, 7);
        assert_eq!(event.sequence, 3);
        assert!(!vblank.has_pending(crtc));
    }

    #[ktest]
    fn dropping_the_file_clears_the_pending_slot() {
        let (file, crtc, vblank) = file_and_vblank();
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 1).expect("park");
        let id = file.id();
        drop(file);
        vblank.drop_file(id);
        assert!(!vblank.has_pending(crtc));
    }

    #[ktest]
    fn confirmation_before_park_is_preserved() {
        let (file, crtc, vblank) = file_and_vblank();
        let generation = vblank.generation(crtc);
        vblank.arm_update(crtc, generation);
        let sequence = vblank.tick_update(crtc, generation);
        vblank.arm_update(crtc, generation);
        vblank.tick_update(crtc, generation);
        vblank.tick_update(crtc, generation);
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 0x91).unwrap();
        let event = read_one(&file).expect("early confirmation survives delayed park");
        assert_eq!(event.sequence, sequence);
        assert_eq!(event.user_data, 0x91);
        assert!(!vblank.has_pending(crtc));
    }

    #[ktest]
    fn stale_latch_and_vsync_cannot_complete_a_new_update() {
        let (file, crtc, vblank) = file_and_vblank();
        let old = vblank.generation(crtc);
        let current = vblank.begin_update(crtc).unwrap();
        vblank.arm_update(crtc, old);
        vblank.tick_update(crtc, old);
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 0x92).unwrap();
        assert!(read_one(&file).is_none());
        vblank.arm_update(crtc, current);
        for _ in 0..4 {
            vblank.tick_update(crtc, old);
        }
        assert!(read_one(&file).is_none());
        assert!(!vblank.is_update_retired(crtc, current));
        let sequence = vblank.tick_update(crtc, current);
        assert_eq!(read_one(&file).unwrap().sequence, sequence);
        assert!(vblank.is_update_retired(crtc, current));
    }

    #[ktest]
    fn update_generation_exhaustion_preserves_the_current_update() {
        let crtc = ObjectId(1);
        let vblank = Vblank::new([crtc]);
        vblank.inner.lock().crtcs[0].generation = u64::MAX - 1;
        assert_eq!(vblank.begin_update(crtc), Ok(u64::MAX));
        vblank.arm_update(crtc, u64::MAX);
        assert_eq!(vblank.begin_update(crtc), Err(DrmError::NoSpace));
        assert!(vblank.inner.lock().crtcs[0].armed);
        assert_eq!(vblank.generation(crtc), u64::MAX);
    }

    #[ktest]
    fn synthetic_ticks_cannot_confirm_an_armed_scanout() {
        let (file, crtc, vblank) = file_and_vblank();
        let generation = vblank.generation(crtc);
        let state = Arc::new(AtomicState::initial(&[], &[]));
        let weak = Arc::downgrade(&state);
        vblank.retain_scanout(crtc, generation, state);
        vblank.arm_update(crtc, generation);
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 0x94).unwrap();
        for _ in 0..3 {
            vblank.fallback_tick(crtc);
        }
        assert!(read_one(&file).is_some());
        assert!(!vblank.is_update_retired(crtc, generation));
        vblank.retire_completed();
        assert!(weak.upgrade().is_some());
        vblank.tick_update(crtc, generation);
        assert!(vblank.is_update_retired(crtc, generation));
        vblank.retire_completed();
        assert!(weak.upgrade().is_none());
    }

    #[ktest]
    fn irq_flip_acknowledgment_does_not_retire_scanout_or_call_the_event_sink() {
        let (file, crtc, vblank) = file_and_vblank();
        let generation = vblank.generation(crtc);
        let state = Arc::new(AtomicState::initial(&[], &[]));
        let weak = Arc::downgrade(&state);
        vblank.retain_scanout(crtc, generation, state);
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 0x95).unwrap();
        assert!(!vblank.can_update(crtc, false));
        assert_eq!(
            vblank.observe_hardware(
                crtc,
                HardwareObservation {
                    vblank: true,
                    flipped_update: Some(generation),
                    retired_update: None,
                }
            ),
            1
        );
        assert!(!vblank.is_update_retired(crtc, generation));
        assert!(
            read_one(&file).is_none(),
            "IRQ observation never calls the event sink"
        );
        vblank.retire_completed();
        assert!(weak.upgrade().is_some());
        assert_eq!(vblank.dispatch_observed_vblank(crtc, generation), 1);
        assert_eq!(read_one(&file).unwrap().sequence, 1);
        assert!(vblank.can_update(crtc, false));
        vblank.observe_hardware(
            crtc,
            HardwareObservation {
                vblank: true,
                flipped_update: None,
                retired_update: Some(generation),
            },
        );
        assert!(vblank.is_update_retired(crtc, generation));
        assert!(
            weak.upgrade().is_some(),
            "IRQ retirement proof does not destroy driver resources"
        );
        vblank.retire_completed();
        assert!(weak.upgrade().is_none());
    }

    #[ktest]
    fn late_park_preserves_the_first_flip_acknowledgment_sequence() {
        let (file, crtc, vblank) = file_and_vblank();
        let generation = vblank.generation(crtc);
        for _ in 0..3 {
            vblank.observe_hardware(
                crtc,
                HardwareObservation {
                    vblank: true,
                    flipped_update: Some(generation),
                    retired_update: None,
                },
            );
        }
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 0x96).unwrap();
        assert_eq!(read_one(&file).unwrap().sequence, 1);
        assert_eq!(vblank.sequence(crtc), 3);
        assert!(!vblank.is_update_retired(crtc, generation));
    }

    #[ktest]
    fn an_older_irq_retirement_proof_cannot_retire_or_complete_a_newer_update() {
        let (file, crtc, vblank) = file_and_vblank();
        let old = vblank.generation(crtc);
        let state = Arc::new(AtomicState::initial(&[], &[]));
        let old_weak = Arc::downgrade(&state);
        vblank.retain_scanout(crtc, old, state);
        let new = vblank.begin_update(crtc).unwrap();
        let current_state = Arc::new(AtomicState::initial(&[], &[]));
        let new_weak = Arc::downgrade(&current_state);
        vblank.retain_scanout(crtc, new, current_state);
        assert!(file.reserve_events(1));
        vblank.park(crtc, file.event_sink(), 0x97).unwrap();
        vblank.observe_hardware(
            crtc,
            HardwareObservation {
                vblank: true,
                flipped_update: Some(old),
                retired_update: Some(old),
            },
        );
        vblank.dispatch_observed_vblank(crtc, old);
        assert!(read_one(&file).is_none());
        assert!(!vblank.is_update_retired(crtc, new));
        vblank.retire_completed();
        assert!(old_weak.upgrade().is_none());
        assert!(new_weak.upgrade().is_some());
        vblank.observe_hardware(
            crtc,
            HardwareObservation {
                vblank: true,
                flipped_update: None,
                retired_update: Some(new + 1),
            },
        );
        assert!(
            !vblank.is_update_retired(crtc, new),
            "future-generation proof is rejected"
        );
        vblank.observe_hardware(
            crtc,
            HardwareObservation {
                vblank: true,
                flipped_update: Some(new),
                retired_update: Some(new),
            },
        );
        vblank.dispatch_observed_vblank(crtc, new);
        assert!(read_one(&file).is_some());
        vblank.retire_completed();
        assert!(new_weak.upgrade().is_none());
    }

    fn file_and_vblank() -> (Arc<DrmFile>, ObjectId, Vblank) {
        let crtc = ObjectId(1);
        let vblank = Vblank::new([crtc]);
        let device = DrmDevice::new_unregistered(Arc::new(EmptyDriver), Some(0), None);
        let file = device
            .open(DrmMinor::Primary, Privilege::Privileged)
            .expect("open");
        vblank.begin_update(crtc).unwrap();
        (file, crtc, vblank)
    }

    fn read_one(file: &DrmFile) -> Option<DrmEventVblank> {
        let mut buffer = [0u8; size_of::<DrmEventVblank>()];
        let read = file
            .read_event::<()>(buffer.len(), |bytes| {
                buffer.copy_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        (read != 0).then(|| DrmEventVblank::from_bytes(&buffer))
    }

    #[derive(Debug)]
    struct EmptyDriver;

    impl DrmDriver for EmptyDriver {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
        fn info(&self) -> DriverInfo {
            DriverInfo {
                name: "vblank-test",
                date: "20260826",
                desc: "vblank unit test",
                major: 1,
                minor: 0,
                patchlevel: 0,
            }
        }
        fn features(&self) -> DriverFeatures {
            DriverFeatures::MODESET
        }
    }
}
