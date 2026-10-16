// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: kernel firmware command ring reservation and completion.

//! Kernel/firmware circular command buffers.
//!
//! Ports `pvr_ccb.c` and `pvr_ccb.h` (738 lines).
//!
//! # Two rings, one structure
//!
//! The KCCB carries kernel-to-firmware commands and the FWCCB carries
//! firmware-to-kernel notifications. Both are arrays of fixed-size slots with a
//! `rogue_fwif_ccb_ctl` in front, so [`Ccb`] serves both; what differs is who
//! advances which offset. The host writes the KCCB's `write_offset` and reads its
//! `read_offset`; for the FWCCB it is the other way round.
//!
//! This is *not* the client CCB ([`crate::sched::cccb`]), which is a byte ring of
//! variable-length commands with entirely different wrap rules.
//!
//! # Why a KCCB slot is reserved before it is filled
//!
//! A command sequence that has begun cannot be abandoned halfway: a `KICK` that
//! never reaches the firmware leaves a client CCB with commands in it and nothing
//! to run them. So the slot is reserved first — a counter, not a position — and
//! only then is the rest of the submission allowed to proceed. Upstream reserves
//! through a `dma_fence` a job can wait on. This port reserves synchronously
//! before publishing command bytes, with a bounded one-second timeout. Foreign
//! fence callbacks only wake the service thread; they do not reserve here.

use alloc::vec::Vec;
use core::{
    mem::offset_of,
    sync::atomic::{Ordering, fence},
};

use aster_softirq::BottomHalfDisabled;
use ostd::sync::SpinLock;
use ostd_pod::FromZeros;

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::heap::{FwCache, FwHeap, FwObject},
    rogue::{
        cr,
        fwif::{FwAddr, RogueFwifCcbCtl, RogueFwifFwccbCmd, RogueFwifKccbCmd},
    },
    sched::abi,
};

/// How long to wait for a free KCCB slot before giving up.
///
/// Upstream `RESERVE_SLOT_TIMEOUT`, one second, with a floor of
/// `RESERVE_SLOT_MIN_RETRIES` attempts so that a slow clock cannot turn the wait
/// into no wait at all.
const RESERVE_SLOT_TIMEOUT_US: u64 = 1_000_000;
/// Upstream `RESERVE_SLOT_MIN_RETRIES`.
const RESERVE_SLOT_MIN_RETRIES: u32 = 10;
/// Microseconds between attempts. Upstream `usleep_range(1, 50)`.
const RESERVE_SLOT_INTERVAL_US: u64 = 20;

/// How long to wait for the firmware to execute a KCCB command.
///
/// Upstream passes `HZ` — one second — to `pvr_kccb_wait_for_completion()` from
/// every call site.
const COMPLETION_TIMEOUT_US: u64 = 1_000_000;

/// A circular command buffer shared with the firmware.
///
/// Both directions use the same structure: the KCCB carries kernel-to-firmware
/// commands, the FWCCB firmware-to-kernel notifications.
pub struct Ccb {
    /// `rogue_fwif_ccb_ctl`.
    ctrl: FwObject,
    /// The slot array.
    ccb: FwObject,
    /// Number of slots, a power of two.
    num_cmds: u32,
    /// Size of one slot in bytes.
    cmd_size: u32,
}

impl Ccb {
    /// Creates a buffer with `2^num_cmds_log2` slots of `cmd_size` bytes.
    ///
    /// Ports upstream `pvr_ccb_init()`. The control structure is always
    /// [`FwCache::POLLED`]: both sides spin on its offsets. `ring` is the CPU
    /// mapping for the slot array — [`FwCache::POLLED`] for the FWCCB (firmware
    /// writes commands, CPU reads them) and [`FwCache::DEVICE_UNCACHED`] for
    /// the KCCB (CPU writes, firmware reads). Shared with both rings so a
    /// caller cannot forget that the two halves of one CCB can disagree.
    pub fn new(
        heap: &mut FwHeap,
        num_cmds_log2: u32,
        cmd_size: usize,
        ring: FwCache,
    ) -> PvrResult<Self> {
        if num_cmds_log2 == 0 || num_cmds_log2 > 16 {
            return Err(PvrError::InvalidArgument);
        }
        let num_cmds = 1u32 << num_cmds_log2;
        let cmd_size = u32::try_from(cmd_size).map_err(|_| PvrError::InvalidArgument)?;
        let total = num_cmds
            .checked_mul(cmd_size)
            .ok_or(PvrError::InvalidArgument)?;

        let mut ctrl = heap.alloc_guard(size_of::<RogueFwifCcbCtl>(), FwCache::POLLED)?;
        let (ctrl_object, heap) = ctrl.parts();
        let ccb = heap.alloc_guard(total as usize, ring)?;

        let mut control = RogueFwifCcbCtl::new_zeroed();
        control.write_offset = 0;
        control.read_offset = 0;
        control.wrap_mask = num_cmds - 1;
        control.cmd_size = cmd_size;
        ctrl_object.write_val(0, &control)?;
        let ccb = ccb.commit();
        let ctrl = ctrl.commit();

        Ok(Self {
            ctrl,
            ccb,
            num_cmds,
            cmd_size,
        })
    }

    /// Firmware address of the control structure.
    pub fn ctrl_fw_addr(&self) -> FwAddr {
        self.ctrl.fw_addr()
    }

    /// Firmware address of the slot array.
    pub fn ccb_fw_addr(&self) -> FwAddr {
        self.ccb.fw_addr()
    }

    /// Number of slots.
    pub fn num_cmds(&self) -> u32 {
        self.num_cmds
    }

    /// The offset the producer will write next.
    pub fn write_offset(&self) -> PvrResult<u32> {
        self.ctrl
            .read_val::<u32>(offset_of!(RogueFwifCcbCtl, write_offset))
    }

    /// The offset the consumer will read next.
    pub fn read_offset(&self) -> PvrResult<u32> {
        self.ctrl
            .read_val::<u32>(offset_of!(RogueFwifCcbCtl, read_offset))
    }

    /// Publishes a new write offset.
    fn set_write_offset(&self, offset: u32) -> PvrResult<()> {
        self.ctrl
            .write_val(offset_of!(RogueFwifCcbCtl, write_offset), &offset)
    }

    /// Publishes a new read offset.
    fn set_read_offset(&self, offset: u32) -> PvrResult<()> {
        self.ctrl
            .write_val(offset_of!(RogueFwifCcbCtl, read_offset), &offset)
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> PvrResult<bool> {
        Ok(self.read_offset()? == self.write_offset()?)
    }

    /// Slots the producer may fill without catching up with the consumer.
    ///
    /// Ports upstream `pvr_kccb_capacity()`: one less than the slot count,
    /// because `write_offset == read_offset` is how the firmware recognises an
    /// empty ring and a completely full one would look identical.
    pub fn capacity(&self) -> u32 {
        self.num_cmds - 1
    }

    /// Slots currently holding unconsumed commands.
    ///
    /// Ports upstream `pvr_kccb_used_slot_count_locked()`.
    pub fn used(&self) -> PvrResult<u32> {
        let write = self.write_offset()?;
        let read = self.read_offset()?;
        Ok(if write >= read {
            write - read
        } else {
            write + self.num_cmds - read
        })
    }
}

/// Owns a completed CCB while its return slots are still being constructed.
struct PendingCcb<'a> {
    heap: &'a mut FwHeap,
    ccb: Option<Ccb>,
}

impl Drop for PendingCcb<'_> {
    fn drop(&mut self) {
        if let Some(ccb) = &self.ccb {
            self.heap.free(&ccb.ccb);
            self.heap.free(&ccb.ctrl);
        }
    }
}

/// The kernel-to-firmware command buffer and its return slots.
///
/// Ports the `kccb` sub-structure of upstream `struct pvr_device`.
pub struct Kccb {
    /// The ring.
    ccb: Ccb,
    /// One `u32` per slot, which the firmware writes when it finishes a command.
    ///
    /// Upstream `pvr_device::kccb::rtn_obj`. Its firmware address goes into
    /// `rogue_fwif_osinit::kernel_ccb_rtn_slots_fw_addr`.
    rtn: FwObject,
    /// Slots promised to callers but not yet written.
    ///
    /// Upstream `pvr_device::kccb::reserved_count`. Counted separately from the
    /// used slots so that two concurrent submissions cannot both be told the
    /// last slot is theirs.
    reserved: u32,
}

impl Kccb {
    /// Creates the KCCB. Ports upstream `pvr_kccb_init()`.
    pub fn new(heap: &mut FwHeap) -> PvrResult<Self> {
        let ccb = Ccb::new(
            heap,
            abi::KCCB_NUMCMDS_LOG2,
            size_of::<RogueFwifKccbCmd>(),
            FwCache::DEVICE_UNCACHED,
        )?;
        let slots_size = ccb.num_cmds() as usize * size_of::<u32>();
        let mut pending = PendingCcb {
            heap,
            ccb: Some(ccb),
        };
        let rtn = pending.heap.alloc_guard(slots_size, FwCache::POLLED)?;
        rtn.object().zero()?;
        let rtn = rtn.commit();
        let ccb = pending
            .ccb
            .take()
            .expect("CCB owned until return slots are ready");
        Ok(Self {
            ccb,
            rtn,
            reserved: 0,
        })
    }

    /// The underlying ring.
    pub fn ccb(&self) -> &Ccb {
        &self.ccb
    }

    /// Firmware address of the return-slot array.
    pub fn rtn_fw_addr(&self) -> FwAddr {
        self.rtn.fw_addr()
    }

    /// The return-slot allocation, for the CPU-cache call-site table.
    #[cfg(ktest)]
    pub(crate) fn rtn_object(&self) -> &FwObject {
        &self.rtn
    }

    /// The firmware's answer for a slot, or [`abi::KCCB_RTN_SLOT_NO_RESPONSE`].
    pub fn rtn_slot(&self, slot: u32) -> PvrResult<u32> {
        self.rtn.read_val::<u32>(slot as usize * size_of::<u32>())
    }

    /// The last command written into the ring, for the tests.
    ///
    /// Reads it back out of the shared memory the firmware would read it from,
    /// rather than from anything the driver kept, so that a test asserting on a
    /// kick is asserting on what the firmware will see. `None` before anything has
    /// been sent.
    #[cfg(ktest)]
    pub(crate) fn last_command(&self) -> PvrResult<Option<RogueFwifKccbCmd>> {
        let write = self.ccb.write_offset()?;
        if write == self.ccb.read_offset()? {
            return Ok(None);
        }
        let slot = write.wrapping_sub(1) & (self.ccb.num_cmds - 1);
        self.ccb
            .ccb
            .read_val::<RogueFwifKccbCmd>(slot as usize * self.ccb.cmd_size as usize)
            .map(Some)
    }

    /// Takes one slot's worth of headroom, if any is free.
    ///
    /// Ports upstream `pvr_kccb_try_reserve_slot()`.
    fn try_reserve(&mut self) -> PvrResult<bool> {
        let used = self.ccb.used()?;
        if self.reserved + used < self.ccb.capacity() {
            self.reserved += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Gives back a reservation that will not be used.
    ///
    /// Ports upstream `pvr_kccb_release_slot()`. Must be called on every path
    /// between a successful reservation and a command that then fails to be
    /// written, or the ring leaks capacity until the device is reset.
    fn release(&mut self) {
        if self.reserved == 0 {
            ostd::error!("KCCB reservation released without one being held");
            return;
        }
        self.reserved -= 1;
    }

    /// Writes a command into a reserved slot and advances the write offset.
    ///
    /// Ports upstream `pvr_kccb_send_cmd_reserved_powered()`. The return slot is
    /// cleared before the command becomes visible, so that a caller that polls it
    /// cannot see a stale answer from the previous occupant of the slot.
    fn write_reserved(&mut self, command: &RogueFwifKccbCmd, want_slot: bool) -> PvrResult<u32> {
        if self.reserved == 0 {
            ostd::error!("KCCB command written without a reserved slot");
            return Err(PvrError::Busy);
        }

        let slot = self.ccb.write_offset()?;
        let next = (slot + 1) & (self.ccb.num_cmds - 1);
        if next == self.ccb.read_offset()? {
            // The reservation should have guaranteed this cannot happen.
            ostd::error!("KCCB is full despite a reserved slot");
            return Err(PvrError::Busy);
        }

        self.ccb
            .ccb
            .write_val(slot as usize * self.ccb.cmd_size as usize, command)?;
        if want_slot {
            self.rtn.write_val(
                slot as usize * size_of::<u32>(),
                &abi::KCCB_RTN_SLOT_NO_RESPONSE,
            )?;
        }

        // Upstream's `mb()`: the command and the cleared return slot must be
        // visible before the offset that publishes them. The `sync_to_device`
        // inside each write has already pushed the bytes out; this orders the
        // offset store after them.
        fence(Ordering::SeqCst);
        self.ccb.set_write_offset(next)?;
        self.reserved -= 1;

        Ok(slot)
    }
}

/// Creates the KCCB and the FWCCB and publishes them on the device.
///
/// Ports upstream `pvr_kccb_init()` and `pvr_fwccb_init()`, which
/// `pvr_device_gpu_init()` calls before the firmware is started — the addresses
/// have to be in `rogue_fwif_osinit` by then, which is why
/// [`crate::fw::FwDevice::init_structures`] runs after this.
///
/// Installing twice is [`PvrError::Busy`]: a second set of rings would leave the
/// firmware reading the first set forever.
pub fn init(device: &PvrDevice) -> PvrResult<()> {
    if device.kccb.is_completed() || device.fwccb.is_completed() {
        return Err(PvrError::Busy);
    }
    let fw = device.fw.get().ok_or(PvrError::Unsupported)?;

    let mut pending = PendingRings {
        fw,
        kccb: None,
        fwccb: None,
        mapped: 0,
    };
    {
        let mut heap = fw.heap.lock();
        pending.kccb = Some(Kccb::new(&mut heap)?);
        pending.fwccb = Some(Ccb::new(
            &mut heap,
            abi::FWCCB_NUMCMDS_LOG2,
            size_of::<RogueFwifFwccbCmd>(),
            FwCache::POLLED,
        )?);
    }

    // Preserve the original allocation and mapping order. The MIPS mapper
    // rolls back its current object; this owner rolls back completed objects.
    for index in 0..5 {
        fw.mips.page_table.map(pending.objects()[index])?;
        pending.mapped += 1;
    }
    let (kccb, fwccb) = pending.commit();

    device.kccb.call_once(|| SpinLock::new(kccb));
    device.fwccb.call_once(|| SpinLock::new(fwccb));
    Ok(())
}

/// Initialization-only ownership before the device's Once cells take over.
/// Dropping this after any error leaves firmware ranges and PTEs unchanged.
struct PendingRings<'a> {
    fw: &'a crate::fw::FwDevice,
    kccb: Option<Kccb>,
    fwccb: Option<Ccb>,
    mapped: usize,
}

impl PendingRings<'_> {
    fn objects(&self) -> [&FwObject; 5] {
        let kccb = self.kccb.as_ref().expect("KCCB constructed before mapping");
        let fwccb = self
            .fwccb
            .as_ref()
            .expect("FWCCB constructed before mapping");
        [
            kccb.ccb.ctrl_object(),
            kccb.ccb.ccb_object(),
            &kccb.rtn,
            fwccb.ctrl_object(),
            fwccb.ccb_object(),
        ]
    }

    fn commit(mut self) -> (Kccb, Ccb) {
        self.mapped = 0;
        (
            self.kccb.take().expect("KCCB ready"),
            self.fwccb.take().expect("FWCCB ready"),
        )
    }
}

impl Drop for PendingRings<'_> {
    fn drop(&mut self) {
        // A constructor's local heap guard has already gone out of scope.
        // Match other firmware owners: heap -> MIPS write lock, then DMA drop.
        let mut heap = self.fw.heap.lock();
        if self.mapped != 0 {
            for object in self.objects()[..self.mapped].iter().rev() {
                let _ = self.fw.mips.page_table.unmap(object);
            }
        }
        if let Some(ccb) = &self.fwccb {
            heap.free(&ccb.ccb);
            heap.free(&ccb.ctrl);
        }
        if let Some(kccb) = &self.kccb {
            heap.free(&kccb.rtn);
            heap.free(&kccb.ccb.ccb);
            heap.free(&kccb.ccb.ctrl);
        }
    }
}

/// Puts both rings back to the state [`init`] left them in, after a hard reset.
///
/// Upstream gets this for free: `pvr_fw_hard_reset()` zeroes every firmware
/// object that is not marked `PVR_BO_FW_NO_CLEAR_ON_RESET` — which includes both
/// control structures and both slot arrays — and then re-runs each object's
/// initialiser. This port's [`crate::fw::FwDevice::hard_reset`] owns only the
/// driver's own structures, so the rings are put back here instead.
///
/// What has to be true afterwards is one sentence: **the firmware restarts from
/// read offset zero, so the host has to write from zero as well.** Leaving the
/// old offsets in place would have the host writing commands into slots the
/// firmware has already walked past, and the firmware executing whatever bytes
/// happened to be in the slots before it.
///
/// The rings themselves are not re-allocated, so their firmware addresses do not
/// change; they are re-published anyway because `osinit` was zeroed by the hard
/// reset.
pub fn reinit(device: &PvrDevice) -> PvrResult<()> {
    let kccb = device.kccb.get().ok_or(PvrError::Unsupported)?;
    let fwccb = device.fwccb.get().ok_or(PvrError::Unsupported)?;

    {
        let mut kccb = kccb.lock();
        kccb.ccb.reset_offsets()?;
        kccb.rtn.zero()?;
        // Any slot promised to a caller that is no longer coming back. The
        // reset terminated every submission that could have used one.
        kccb.reserved = 0;
    }
    fwccb.lock().reset_offsets()?;

    Ok(())
}

impl Ccb {
    /// Puts the read and write offsets back to zero. See [`reinit`].
    fn reset_offsets(&self) -> PvrResult<()> {
        self.set_read_offset(0)?;
        self.set_write_offset(0)
    }

    /// The control structure's allocation, for the firmware page table.
    pub(crate) fn ctrl_object(&self) -> &FwObject {
        &self.ctrl
    }

    /// The slot array's allocation, for the firmware page table.
    pub(crate) fn ccb_object(&self) -> &FwObject {
        &self.ccb
    }
}

impl Ccb {
    /// Writes one command into the ring as the *producer* would, for tests.
    ///
    /// The FWCCB's producer is the firmware, so a test that wants to exercise
    /// [`process_fwccb`] has to stand in for it. Deliberately not available
    /// outside tests: the host writing the FWCCB in production would be a data
    /// race with the firmware.
    #[cfg(ktest)]
    pub(crate) fn write_for_test<T: ostd_pod::Pod>(&self, command: &T) -> PvrResult<()> {
        let write = self.write_offset()?;
        self.ccb
            .write_val(write as usize * self.cmd_size as usize, command)?;
        self.set_write_offset((write + 1) & (self.num_cmds - 1))
    }
}

/// Sends a command to the firmware and returns without waiting.
///
/// Ports upstream `pvr_kccb_send_cmd_powered()`: reserve a slot, write, kick the
/// firmware's scheduler. Returns the slot number, which is the handle
/// [`wait_for_completion`] needs; a caller that does not intend to wait may
/// ignore it.
///
/// Fails with [`PvrError::Busy`] rather than blocking indefinitely when the
/// buffer is full; the caller decides whether to retry.
///
/// Divergence: upstream's `pvr_kccb_send_cmd()` wraps this in
/// `pvr_power_get()`/`pvr_power_put()`; this is its
/// `pvr_kccb_send_cmd_powered()` instead, and the caller must already hold a
/// power reference. The ioctl handlers that start work
/// (`SUBMIT_JOBS`, the destroy paths that issue cleanup) take
/// [`crate::power::PowerRef`] for that reason. Two constraints keep the get out
/// of this function:
///
/// - every production caller here runs under a held reference, so a second get
///   would only add nesting;
/// - [`crate::sched::queue::Queue::advance`] can be reached from a fence
///   callback, which may run in interrupt context. `Pm::get` takes a lock, can
///   power the GPU up and can boot the firmware, none of which is legal there.
///   Taking a PM reference on this path would be wrong even if it were free.
///
/// Recorded in `ABI.md` compatibility limits.
pub fn send(device: &PvrDevice, command: &RogueFwifKccbCmd) -> PvrResult<u32> {
    send_inner(device, command, true)
}

/// Sends a command and waits for the firmware to consume it.
///
/// A timeout is returned as [`PvrError::Timeout`] and the device is *not*
/// declared lost — see [`wait_for_completion`], which explains why that decision
/// belongs to the caller, which chooses whether to reset and retry.
///
/// Returns the firmware's answer for the slot, whose bits are the
/// `abi::KCCB_RTN_SLOT_*` constants. A caller that asked for a cleanup checks
/// [`abi::KCCB_RTN_SLOT_CLEANUP_BUSY`] in it.
pub fn send_and_wait(device: &PvrDevice, command: &RogueFwifKccbCmd) -> PvrResult<u32> {
    let slot = send_inner(device, command, true)?;
    wait_for_completion(device, slot)
}

/// Reserves a slot, writes the command and kicks the firmware.
fn send_inner(device: &PvrDevice, command: &RogueFwifKccbCmd, want_slot: bool) -> PvrResult<u32> {
    reserve_slot(device)?;
    match send_reserved_inner(device, command, want_slot) {
        Ok(slot) => Ok(slot),
        Err(err) => {
            release_slot(device);
            Err(err)
        }
    }
}

/// Takes one KCCB slot's worth of headroom, waiting for one if need be.
///
/// Ports upstream `pvr_kccb_reserve_slot_sync()`. Split out from [`send`] because
/// [`crate::sched::queue`] has to reserve *before* it takes its own lock: this
/// busy-waits, and holding a queue lock across it would be the lock-order
/// inversion that module's lock order forbids.
///
/// Every successful reservation must be matched by exactly one
/// [`send_reserved`] or [`release_slot`], or the ring leaks capacity until the
/// device is reset.
pub fn reserve_slot(device: &PvrDevice) -> PvrResult<()> {
    device.check_alive()?;
    let kccb = device.kccb.get().ok_or(PvrError::Unsupported)?;
    reserve_slot_sync(kccb)
}

/// Gives a reservation back unused. Ports upstream `pvr_kccb_release_slot()`.
pub fn release_slot(device: &PvrDevice) {
    if let Some(kccb) = device.kccb.get() {
        kccb.lock().release();
    }
}

/// What the kernel CCB looks like, for the hang report.
///
/// Upstream has no equivalent because it does not need one: its watchdog reports
/// a stall and resets, and a developer reads the ring out of a crash dump. There
/// is no dump here, so the report has to carry the evidence.
#[derive(Clone, Copy, Debug)]
pub struct KccbSnapshot {
    /// Where the firmware will read next.
    pub read_offset: u32,
    /// Where the driver will write next.
    pub write_offset: u32,
    /// Commands written and not yet consumed.
    pub used: u32,
    /// Slots promised to callers but not yet written.
    pub reserved: u32,
    /// `cmd_type` of the command at [`Self::read_offset`] — the one the firmware
    /// would consume next, which on a stalled ring is the one it is not
    /// consuming. `None` when the ring is empty, so nothing is being refused.
    pub next_cmd_type: Option<u32>,
    /// `cmd_type` of unconsumed commands, oldest first, up to [`SNAPSHOT_PENDING`].
    /// The pending sequence preserves command ordering as well as the current head.
    pub pending: [u32; SNAPSHOT_PENDING],
    /// How many of [`Self::pending`] are filled in.
    pub pending_len: usize,
}

impl KccbSnapshot {
    /// Prints the ring, in the shape [`crate::sched::cccb::Cccb::report`] uses.
    ///
    /// A method rather than a `Display`, because the interesting part is not the
    /// numbers but what they mean together: `used` non-zero with `read_offset`
    /// unmoved says the firmware is not consuming what it was given, and `used`
    /// zero says the driver never gave it anything -- and those are the two
    /// halves of the question "did the kick arrive".
    pub fn report(&self, label: &str) {
        ostd::debug!(
            "[k1-gpu] {} kccb: roff={:#x} woff={:#x} used={} reserved={} next={:?}",
            label,
            self.read_offset,
            self.write_offset,
            self.used,
            self.reserved,
            self.next_cmd_type
        );
        for index in 0..self.pending_len {
            ostd::debug!(
                "[k1-gpu] {} kccb: pending[{}] type={:#x}",
                label,
                index,
                self.pending[index]
            );
        }
    }
}

/// How many unconsumed commands a [`KccbSnapshot`] records.
///
/// The ring holds far more, but a stall that involves more than a handful is not
/// diagnosed by reading them all off a serial console.
pub const SNAPSHOT_PENDING: usize = 8;

/// Reads the kernel CCB's state without waiting for the lock.
///
/// `None` when there is no KCCB yet, when another CPU holds it, or when the
/// firmware memory could not be read. Never blocks, for the reason
/// [`crate::power::kccb_is_idle`] does not either: this is reached from the
/// interrupt handler's deferred half, which may have interrupted the holder.
///
/// A contended lock giving `None` is acceptable here in a way it would not be for
/// a decision: this only feeds a diagnostic, and the next poll is 500 ms away.
pub(crate) fn snapshot(device: &PvrDevice) -> Option<KccbSnapshot> {
    let kccb = device.kccb.get()?.try_lock()?;
    let ccb = kccb.ccb();

    let read_offset = ccb.read_offset().ok()?;
    let write_offset = ccb.write_offset().ok()?;

    let cmd_type_at = |slot: u32| -> Option<u32> {
        Some(
            ccb.ccb
                .read_val::<RogueFwifKccbCmd>(slot as usize * ccb.cmd_size as usize)
                .ok()?
                .cmd_type,
        )
    };

    let next_cmd_type = if read_offset == write_offset {
        None
    } else {
        Some(cmd_type_at(read_offset)?)
    };

    let mut pending = [0u32; SNAPSHOT_PENDING];
    let mut pending_len = 0;
    let mut slot = read_offset;
    while slot != write_offset && pending_len < SNAPSHOT_PENDING {
        pending[pending_len] = cmd_type_at(slot)?;
        pending_len += 1;
        slot = (slot + 1) & (ccb.num_cmds - 1);
    }

    Some(KccbSnapshot {
        read_offset,
        write_offset,
        used: ccb.used().ok()?,
        reserved: kccb.reserved,
        next_cmd_type,
        pending,
        pending_len,
    })
}

/// Writes a command into an already-reserved slot and kicks the firmware.
///
/// Ports upstream `pvr_kccb_send_cmd_reserved_powered()`. Does not ask for a
/// return slot: the one caller that uses this — a job kick — has nothing to wait
/// for, because the job's completion is reported through its timeline UFO rather
/// than through the KCCB.
pub fn send_reserved(device: &PvrDevice, command: &RogueFwifKccbCmd) -> PvrResult<()> {
    send_reserved_inner(device, command, false).map(|_| ())
}

/// The body shared by [`send_reserved`] and [`send_inner`].
fn send_reserved_inner(
    device: &PvrDevice,
    command: &RogueFwifKccbCmd,
    want_slot: bool,
) -> PvrResult<u32> {
    device.check_alive()?;
    let kccb = device.kccb.get().ok_or(PvrError::Unsupported)?;
    let slot = kccb.lock().write_reserved(command, want_slot)?;
    kick_mts(device)?;
    Ok(slot)
}

/// Spins until a KCCB slot is free, or a second has passed.
///
/// Ports upstream `pvr_kccb_reserve_slot_sync()`, including the "at least ten
/// tries" floor. The lock is released between attempts. This bounded polling
/// path is also used by queue processing; foreign callbacks merely wake its worker.
fn reserve_slot_sync(kccb: &SpinLock<Kccb, BottomHalfDisabled>) -> PvrResult<()> {
    let mut retries = 0u32;
    let mut waited_us = 0u64;
    loop {
        if kccb.lock().try_reserve()? {
            return Ok(());
        }
        if waited_us >= RESERVE_SLOT_TIMEOUT_US && retries >= RESERVE_SLOT_MIN_RETRIES {
            ostd::error!("no KCCB slot became free in {RESERVE_SLOT_TIMEOUT_US} us");
            return Err(PvrError::Busy);
        }
        crate::fw::delay_us(RESERVE_SLOT_INTERVAL_US);
        waited_us += RESERVE_SLOT_INTERVAL_US;
        retries = retries.saturating_add(1);
    }
}

/// Waits for firmware to report a slot executed by polling its return slot.
///
/// Upstream `pvr_kccb_wait_for_completion()` uses an IRQ-woken wait queue.
/// A timeout here returns [`PvrError::Timeout`] without declaring the device lost:
/// the caller determines recovery. [`crate::mm::mmu::flush_if_pending`] resets
/// and retries, while cleanup callers report the failure. Prematurely marking
/// the device lost would prevent those reset attempts.
pub fn wait_for_completion(device: &PvrDevice, slot: u32) -> PvrResult<u32> {
    let kccb = device.kccb.get().ok_or(PvrError::Unsupported)?;
    let mut waited_us = 0u64;
    loop {
        let rtn = kccb.lock().rtn_slot(slot)?;
        if rtn & abi::KCCB_RTN_SLOT_CMD_EXECUTED != 0 {
            if rtn & abi::KCCB_RTN_SLOT_POLL_FAILURE != 0 {
                ostd::warn!("firmware reports a failed poll for KCCB slot {slot}");
            }
            return Ok(rtn);
        }
        if waited_us >= COMPLETION_TIMEOUT_US {
            ostd::error!(
                "KCCB slot {slot} was not executed in {COMPLETION_TIMEOUT_US} us; \
                 the firmware is not draining the kernel CCB"
            );
            return Err(PvrError::Timeout);
        }
        crate::fw::delay_us(RESERVE_SLOT_INTERVAL_US);
        waited_us += RESERVE_SLOT_INTERVAL_US;
    }
}

/// Tells the firmware's scheduler there is something in the KCCB.
///
/// Ports upstream `pvr_fw_mts_schedule()` as called from
/// `pvr_kccb_send_cmd_reserved_powered()`, where the value is
/// `PVR_FWIF_DM_GP & ~ROGUE_CR_MTS_SCHEDULE_DM_CLRMSK` — the general-purpose
/// data master's number placed in the register's `DM` field, which for
/// [`abi::FWIF_DM_GP`] is zero. Written out rather than simplified to `0`, so
/// that the register field and the data master stay visibly connected.
fn kick_mts(device: &PvrDevice) -> PvrResult<()> {
    let value = abi::FWIF_DM_GP & cr::mts_schedule::DM.mask as u32;
    // Order cached rings and uncached control words against the MMIO doorbell.
    // DMA cache synchronization alone does not establish memory-to-I/O ordering.
    ostd::mm::dma::dma_io_fence();
    device.write32(cr::mts_schedule::OFFSET, value)?;
    ostd::mm::dma::dma_io_fence();
    Ok(())
}

/// Processes everything the firmware has posted to the FWCCB.
///
/// Ports upstream `pvr_fwccb_process()`. Runs from the interrupt handler's
/// deferred half. Handles hang reports, so it must never itself block on the
/// firmware — which is why every arm below either records something or logs, and
/// none of them sends a KCCB command and waits.
///
/// The read offset is advanced *before* the command is acted on, exactly as
/// upstream does: a command that makes the driver take a long path must not also
/// make the firmware think the ring is still full.
pub fn process_fwccb(device: &PvrDevice) {
    let Some(fwccb) = device.fwccb.get() else {
        return;
    };

    // Commands are drained under the lock and handled outside it. Upstream drops
    // its mutex around each `process_fwccb_command()` for the same reason: the
    // handlers touch contexts and queues, whose locks must not nest inside this
    // one.
    let commands = match drain_fwccb(fwccb) {
        Ok(commands) => commands,
        Err(err) => {
            ostd::error!("could not read the firmware CCB: {err:?}");
            return;
        }
    };

    for command in commands {
        handle_fwccb_command(device, &command);
    }
}

/// Copies out every pending FWCCB command and advances the read offset.
fn drain_fwccb(fwccb: &SpinLock<Ccb, BottomHalfDisabled>) -> PvrResult<Vec<RogueFwifFwccbCmd>> {
    let mut commands = Vec::new();
    let ccb = fwccb.lock();
    loop {
        let read = ccb.read_offset()?;
        if read == ccb.write_offset()? {
            break;
        }
        let command: RogueFwifFwccbCmd = ccb.ccb.read_val(read as usize * ccb.cmd_size as usize)?;
        ccb.set_read_offset((read + 1) & (ccb.num_cmds - 1))?;
        commands.push(command);
    }
    Ok(commands)
}

/// Acts on one FWCCB command.
///
/// Ports upstream `process_fwccb_command()`. The magic-value check comes first:
/// a command word without it is corruption of shared memory rather than a
/// command this driver has not heard of, and the two want different reactions —
/// the first means the firmware and the host disagree about the ring's contents,
/// the second is forward compatibility working as intended.
fn handle_fwccb_command(device: &PvrDevice, command: &RogueFwifFwccbCmd) {
    if command.cmd_type & abi::CMD_MAGIC_DWORD_MASK != abi::CMD_MAGIC_DWORD_SHIFTED {
        ostd::warn!(
            "firmware CCB command {:#010x} has no magic value; ignoring",
            command.cmd_type
        );
        return;
    }

    match command.cmd_type {
        abi::FWCCB_CMD_REQUEST_GPU_RESTART => {
            // Upstream calls `pvr_power_reset(pvr_dev, false)` from its threaded
            // IRQ handler — a *soft* reset, which is why that function comments
            // that it "is triggered as a response to a FW command to the Host".
            // This runs in the interrupt handler's deferred half, which may not
            // sleep, so the request is recorded and run by the next caller that
            // may; see `crate::power::ResetRequest`.
            ostd::debug!("[k1-gpu] firmware asked for a GPU restart; queueing a soft reset");
            ostd::error!("firmware asked for a GPU restart; a soft reset is queued");
            crate::power::request_reset(device, crate::power::ResetKind::Soft);
        }
        abi::FWCCB_CMD_FREELISTS_RECONSTRUCTION => {
            let data = command.cmd_data.cmd_freelists_reconstruction();
            let count = core::cmp::min(data.freelist_count as usize, data.freelist_ids.len());
            crate::render::free_list::process_reconstruct_request(
                device,
                &data.freelist_ids[..count],
            );
        }
        abi::FWCCB_CMD_FREELIST_GROW => {
            let data = command.cmd_data.cmd_free_list_gs();
            crate::render::free_list::process_grow_request(device, data.freelist_id);
        }
        abi::FWCCB_CMD_CONTEXT_RESET_NOTIFICATION => {
            // Upstream decodes the whole `pvr_dump_context_reset_notification()`
            // report. Reading the fields needs the union arm, which is what the
            // `reset_reason` and `dm` below are; the addresses are the two most
            // useful numbers in it.
            let data = command.cmd_data.cmd_context_reset_notification();
            ostd::error!(
                "firmware reset a context: id {}, reason {}, DM {}, job {}, \
                 page catalogue {:#x}, fault address {:#x}",
                data.server_common_context_id,
                data.reset_reason,
                data.dm,
                data.reset_job_ref,
                data.pc_address,
                data.fault_address
            );
            crate::sched::queue::mark_context_faulty(device, data.server_common_context_id);
        }
        abi::FWCCB_CMD_CONTEXT_FW_PF_NOTIFICATION => {
            // The firmware processor reports its own fault address. Correlate it with
            // the MIPS page table to identify the mapped object, if any.
            let data = command.cmd_data.cmd_fw_pagefault();
            ostd::debug!(
                "[k1-gpu] firmware page fault at {:#x}; the address is a firmware virtual \
                 address -- look it up in the MIPS page table",
                data.fw_fault_addr
            );
            crate::mm::vm_mips::report_fault(device, data.fw_fault_addr);
        }
        abi::FWCCB_CMD_DEBUG_DUMP => {
            ostd::warn!("firmware asked for a state dump");
        }
        // Upstream suppresses the warning for these rather than acting on them:
        // it has no infrastructure for the statistics, the clock-rate
        // notification is only interesting to a governor this port has none of,
        // and Z/S buffers are a render feature this port does not implement.
        abi::FWCCB_CMD_UPDATE_STATS
        | abi::FWCCB_CMD_CORE_CLK_RATE_CHANGE
        | abi::FWCCB_CMD_ZSBUFFER_BACKING
        | abi::FWCCB_CMD_ZSBUFFER_UNBACKING => {}
        other => {
            // Preserve unknown firmware command numbers in failure diagnostics.
            ostd::debug!(
                "[k1-gpu] unknown firmware CCB command {} ({:#010x})",
                other & !abi::CMD_MAGIC_DWORD_MASK,
                other
            );
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::fw::{heap::FwHeapInfo, mips};

    fn heap() -> FwHeap {
        FwHeap::new(
            FwHeapInfo::new(
                mips::FW_HEAP_MIPS_LOG2_SIZE,
                mips::FW_HEAP_MIPS_RESERVED_SIZE,
            )
            .expect("heap geometry"),
            mips::FW_HEAP_MIPS_BASE,
        )
    }

    /// [`Kccb::new`] hardcodes the three mappings. The expected column is the
    /// policy: flipping the return-value array to write-back, or the ring to
    /// uncacheable, turns this red. The FWCCB is built by [`super::init`], and
    /// is checked there via the device fixture — constructing it here with
    /// `FwCache::POLLED` and then asserting that flag would not be able to fail.
    #[ktest]
    fn kccb_cpu_mappings() {
        let mut heap = heap();
        let kccb = Kccb::new(&mut heap).expect("kccb");
        let rows = [
            (
                "kccb ctrl",
                kccb.ccb().ctrl_object().is_cpu_uncached(),
                true,
            ),
            (
                "kccb ring",
                kccb.ccb().ccb_object().is_cpu_uncached(),
                false,
            ),
            ("kccb rtn", kccb.rtn_object().is_cpu_uncached(), true),
        ];
        for (what, actual, expected) in rows {
            assert_eq!(actual, expected, "{what}");
        }
    }
}
