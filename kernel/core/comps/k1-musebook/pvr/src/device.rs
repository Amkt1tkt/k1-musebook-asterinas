// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR device lifetime and checked register access.

//! The PowerVR device.
//!
//! Ports `pvr_device.c` and `pvr_device.h` (1,712 lines).

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aster_softirq::BottomHalfDisabled;
use ostd::{
    arch::{read_tsc, tsc_freq},
    io::IoMem,
    mm::VmIoOnce,
    sync::{Mutex, RwMutex, SpinLock},
};
use spin::Once;

use crate::{
    device_info::DeviceInfo,
    error::{PvrError, PvrResult},
    fw::{FwDevice, ValidatedFirmware, info::pack_bvnc},
    mm::vm::VmContext,
    power::ResetRequest,
    render::free_list::FreeListRegistry,
    rogue::cr,
    sched::{
        ccb::{Ccb, Kccb},
        queue::SchedState,
    },
};

/// The hardware GPU identifier, read from `RGX_CR_CORE_ID`.
///
/// Written `B.V.N.C`. The K1's BXE-2-32 reports `36.29.52.182`, which is also
/// the BVNC recorded in the firmware image's info header — [`PvrDevice::probe`]
/// refuses to continue if the two disagree, because a mismatch means we are
/// about to run firmware built for different hardware.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuId {
    /// Branch ID.
    pub b: u16,
    /// Version ID.
    pub v: u16,
    /// Number of scalable units.
    pub n: u16,
    /// Config ID.
    pub c: u16,
}

impl GpuId {
    /// The BVNC of the SpacemiT K1's GPU, for the sanity check in
    /// [`PvrDevice::probe`].
    pub const K1_BXE_2_32: Self = Self {
        b: 36,
        v: 29,
        n: 52,
        c: 182,
    };

    /// Decodes the packed form found in `RGX_CR_CORE_ID__PBVNC`.
    pub fn from_packed(packed: u64) -> Self {
        use cr::core_id_pbvnc as f;
        Self {
            b: f::BRANCH_ID.get(packed) as u16,
            v: f::VERSION_ID.get(packed) as u16,
            n: f::NUMBER_OF_SCALABLE_UNITS.get(packed) as u16,
            c: f::CONFIG_ID.get(packed) as u16,
        }
    }

    /// Packs into the layout the firmware info header and
    /// `drm_pvr_dev_query_gpu_info::gpu_id` both use.
    ///
    /// Delegates to [`pack_bvnc`] rather than repeating the shifts, so that the
    /// hardware register decode and the ABI encode cannot drift apart; the
    /// `packed_bvnc_round_trips` test pins them together.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.h>
    /// (`pvr_gpu_id_to_packed_bvnc`).
    pub fn to_packed(self) -> u64 {
        pack_bvnc(self.b, self.v, self.n, self.c)
    }
}

impl core::fmt::Display for GpuId {
    /// Formats as `B.V.N.C`, the spelling upstream uses for the firmware file
    /// name and for its log messages.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.b, self.v, self.n, self.c)
    }
}

/// Firmware version, from the image's info header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FwVersion {
    /// Major version. The driver rejects anything above 1.
    pub major: u16,
    /// Minor version.
    pub minor: u16,
}

/// A memory-backed stand-in for the GPU's control-register window.
///
/// Registers are held as a little-endian byte array indexed by offset, which is
/// what makes overlapping 32- and 64-bit accesses behave the way they do on real
/// MMIO. Seeding a register is an ordinary [`RegisterWindow::write64`].
pub struct SimulatedRegisters {
    bytes: SpinLock<Vec<u8>>,
}

impl SimulatedRegisters {
    /// Creates a zeroed window of `len` bytes.
    pub fn new(len: usize) -> Self {
        Self {
            bytes: SpinLock::new(alloc::vec![0u8; len]),
        }
    }

    fn load<const N: usize>(&self, offset: u32) -> PvrResult<[u8; N]> {
        let bytes = self.bytes.lock();
        let start = offset as usize;
        let end = start.checked_add(N).ok_or(PvrError::InvalidArgument)?;
        let slice = bytes.get(start..end).ok_or(PvrError::InvalidArgument)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn store(&self, offset: u32, value: &[u8]) -> PvrResult<()> {
        let mut bytes = self.bytes.lock();
        let start = offset as usize;
        let end = start
            .checked_add(value.len())
            .ok_or(PvrError::InvalidArgument)?;
        let slice = bytes.get_mut(start..end).ok_or(PvrError::InvalidArgument)?;
        slice.copy_from_slice(value);
        Ok(())
    }
}

/// The GPU's control-register window.
///
/// Two things differ from upstream's bare `void __iomem *`:
///
/// - [`IoMem`] bounds-checks every access, so these accessors return
///   [`PvrResult`] where upstream's `pvr_cr_read32()` and friends are infallible.
///   A failure means the port asked for a register outside the mapped window,
///   which is a bug here rather than a hardware condition — hence the error log
///   at each site.
/// - There is a second, memory-backed variant. It is what lets the driver be
///   exercised without the silicon: a `#[ktest]` seeds `ROGUE_CR_CORE_ID__PBVNC`
///   and checks that the BVNC decodes to 36.29.52.182, and a QEMU build can
///   register a device whose registers simply read back what was written. No
///   code above this type may branch on which variant it holds.
pub enum RegisterWindow {
    /// A real MMIO window, as mapped from the platform's register resource.
    Mmio(IoMem),
    /// An in-memory simulation, for `#[ktest]` and for bring-up under QEMU.
    Simulated(SimulatedRegisters),
}

impl RegisterWindow {
    /// The window size the simulated variant uses by default.
    ///
    /// Large enough for every register in [`crate::rogue::cr`], whose highest
    /// allow-listed offset is `0xf3b8`.
    pub const SIMULATED_LEN: usize = 0x1_0000;

    /// Wraps a mapped MMIO window.
    pub fn mmio(regs: IoMem) -> Self {
        Self::Mmio(regs)
    }

    /// Creates a memory-backed window of [`Self::SIMULATED_LEN`] bytes.
    pub fn simulated() -> Self {
        Self::Simulated(SimulatedRegisters::new(Self::SIMULATED_LEN))
    }

    /// Reads a 32-bit control register.
    pub fn read32(&self, offset: u32) -> PvrResult<u32> {
        match self {
            Self::Mmio(regs) => regs
                .read_once(offset as usize)
                .map_err(|_| out_of_window(offset)),
            Self::Simulated(regs) => Ok(u32::from_le_bytes(regs.load::<4>(offset)?)),
        }
    }

    /// Reads a 64-bit control register.
    pub fn read64(&self, offset: u32) -> PvrResult<u64> {
        match self {
            Self::Mmio(regs) => regs
                .read_once(offset as usize)
                .map_err(|_| out_of_window(offset)),
            Self::Simulated(regs) => Ok(u64::from_le_bytes(regs.load::<8>(offset)?)),
        }
    }

    /// Writes a 32-bit control register.
    pub fn write32(&self, offset: u32, value: u32) -> PvrResult<()> {
        match self {
            Self::Mmio(regs) => regs
                .write_once(offset as usize, &value)
                .map_err(|_| out_of_window(offset)),
            Self::Simulated(regs) => regs.store(offset, &value.to_le_bytes()),
        }
    }

    /// Writes a 64-bit control register.
    pub fn write64(&self, offset: u32, value: u64) -> PvrResult<()> {
        match self {
            Self::Mmio(regs) => regs
                .write_once(offset as usize, &value)
                .map_err(|_| out_of_window(offset)),
            Self::Simulated(regs) => regs.store(offset, &value.to_le_bytes()),
        }
    }
}

/// Reports an access outside the mapped register window.
///
/// Separated out so that every accessor logs the offending offset: the failure
/// is a driver bug, and the offset is the only clue to which register definition
/// is wrong.
fn out_of_window(offset: u32) -> PvrError {
    ostd::error!(
        "control register {:#06x} is outside the mapped window",
        offset
    );
    PvrError::InvalidArgument
}

/// A PowerVR GPU.
///
/// One per device. Held by `Arc` and referenced from every context, buffer object
/// and job, so it must not contain anything whose destruction order matters.
///
/// # Why three fields are lazily initialized
///
/// Firmware state, the kernel VM, and FWCCB are initialized in separate steps.
/// Their [`Once`] cells distinguish allocated resources from stages not yet
/// reached. Cell presence does not mean firmware is currently running: runtime
/// PM can stop and restart it without replacing those owners.
pub struct PvrDevice {
    /// BVNC read from the hardware.
    pub gpu_id: GpuId,
    /// Features and quirks, taken from the firmware image rather than a
    /// compiled-in table. See [`DeviceInfo`].
    pub info: DeviceInfo,
    /// Version of the loaded firmware.
    pub fw_version: FwVersion,
    /// The GPU's register window.
    pub regs: RegisterWindow,
    /// Firmware state: image, heap, trace buffer, and MIPS page tables.
    pub fw: Once<FwDevice>,
    /// The kernel's GPU address space, holding firmware structures.
    pub kernel_vm: Once<Arc<VmContext>>,
    /// Firmware-to-kernel command circular buffer.
    ///
    /// Both command rings are shared with the taskless IRQ handler. Their guards
    /// defer local bottom halves so an interrupt cannot re-enter its own ring lock.
    pub fwccb: Once<SpinLock<Ccb, BottomHalfDisabled>>,
    /// Kernel-to-firmware command circular buffer, and its return slots.
    ///
    /// [`Self::fwccb`]'s counterpart, and filled at the same moment by
    /// [`crate::sched::init`]. Upstream keeps both in `struct pvr_device`, which
    /// is what this pair is.
    pub kccb: Once<SpinLock<Kccb, BottomHalfDisabled>>,
    /// Queues, contexts, and submission id allocators.
    ///
    /// [`crate::sched::queue::SchedState`] owns their shared invariants.
    pub sched: SchedState,
    /// Free lists by the id the firmware names them with in FWCCB messages.
    ///
    /// Upstream `pvr_device::free_list_ids`. Entries are weak; see
    /// [`FreeListRegistry`].
    pub free_lists: FreeListRegistry,
    /// Pending MMU cache invalidation flags, coalesced until the next flush.
    pub mmu_flush_flags: AtomicU32,
    /// Serializes invalidation acknowledgements across submitting threads.
    pub(crate) mmu_flush_lock: Mutex<()>,
    /// Removed backing retained until invalidation or a confirmed hardware stop.
    pub(crate) retired_mappings: SpinLock<crate::mm::mmu::RetiredMappings, BottomHalfDisabled>,
    /// Serializes a GPU reset against the paths that talk to the firmware.
    ///
    /// Mirrors upstream's `pvr_device::reset_sem`, including the rule stated
    /// there: a reset takes it for writing, and "any code that submits commands
    /// to the firmware that isn't in an IRQ handler or on the scheduler
    /// workqueue must lock this for reading". This port has no scheduler
    /// workqueue — submission runs in the ioctl thread — so the readers are the
    /// ioctls themselves; see [`crate::power::reset`] for the lock order.
    ///
    /// Sleeping, and therefore never taken from the interrupt handler's
    /// deferred half. That is the same exemption upstream grants its IRQ
    /// handler, and the reason [`crate::power::request_reset`] exists.
    pub reset_lock: RwMutex<()>,
    /// A reset asked for from a context that may not sleep.
    ///
    /// Serviced by the next caller that may: see [`crate::power::ResetRequest`].
    pub reset_request: ResetRequest,
    /// State for the thread that does the driver's sleepable work.
    ///
    /// See [`crate::service`]: the reset a deferred half could only ask for, the
    /// firmware structures a closing file could not release, the timeout sweep and
    /// the watchdog all live on it.
    pub service: crate::service::ServiceState,
    /// Set when the GPU has been declared unrecoverable.
    ///
    /// Sticky: GPU operations afterwards fail with [`PvrError::DeviceLost`] until
    /// reboot. Mirrors upstream's `pvr_device::lost`.
    pub lost: AtomicBool,
    /// Whether the complete firmware-stop sequence confirmed idle and reset.
    /// A lost flag alone does not permit releasing memory still reachable by DMA.
    pub(crate) quiesced: AtomicBool,
    /// Successful full-stop generation for deferred references spanning a restart.
    pub(crate) quiescence_epoch: AtomicU64,
}

impl PvrDevice {
    /// Builds a device from an already-known identity.
    ///
    /// [`Self::probe`] is the way in for real bring-up; this exists for callers
    /// that have no register window to interrogate yet.
    pub fn new(
        regs: RegisterWindow,
        gpu_id: GpuId,
        info: DeviceInfo,
        fw_version: FwVersion,
    ) -> Arc<Self> {
        Arc::new(Self {
            gpu_id,
            info,
            fw_version,
            regs,
            fw: Once::new(),
            kernel_vm: Once::new(),
            fwccb: Once::new(),
            kccb: Once::new(),
            sched: SchedState::default(),
            free_lists: FreeListRegistry::new(),
            mmu_flush_flags: AtomicU32::new(0),
            mmu_flush_lock: Mutex::new(()),
            retired_mappings: SpinLock::new(crate::mm::mmu::RetiredMappings::default()),
            reset_lock: RwMutex::new(()),
            reset_request: ResetRequest::new(),
            service: crate::service::ServiceState::new(),
            lost: AtomicBool::new(false),
            quiesced: AtomicBool::new(false),
            quiescence_epoch: AtomicU64::new(0),
        })
    }

    /// Reads the hardware identity and checks it against the firmware image.
    ///
    /// Mirrors `pvr_device_gpu_init()`: read the BVNC, compare it with the one
    /// the image was built for, then pick the firmware processor type from the
    /// feature bits.
    ///
    /// Two failures are worth naming:
    ///
    /// - A BVNC mismatch is [`PvrError::BadFirmware`], because continuing would
    ///   run firmware built for other silicon. Upstream rejects the image at the
    ///   same point, inside `pvr_fw_validate()`.
    /// - A device whose firmware reports a META or RISC-V processor is
    ///   [`PvrError::Unsupported`]. Divergence: upstream would take those paths;
    ///   `pvr/UPSTREAM` excludes them because BXE-2-32's image carries only
    ///   `MIPS_*` sections, so reaching them here means the exclusion was wrong
    ///   and must be revisited rather than silently worked around.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
    /// (`pvr_device_gpu_init`, `pvr_load_gpu_id`).
    pub fn probe(regs: RegisterWindow, firmware: &ValidatedFirmware) -> PvrResult<Arc<Self>> {
        let gpu_id = read_gpu_id(&regs)?;

        if gpu_id != firmware.gpu_id {
            ostd::error!(
                "hardware reports BVNC {} but the firmware image is for {}",
                gpu_id,
                firmware.gpu_id
            );
            return Err(PvrError::BadFirmware);
        }

        let features = &firmware.info.features;
        if !features.mips {
            ostd::error!(
                "BVNC {} reports a {} firmware processor; only MIPS is ported (see pvr/UPSTREAM)",
                gpu_id,
                if features.meta {
                    "META"
                } else if features.riscv_fw_processor {
                    "RISC-V"
                } else {
                    "n unrecognised"
                }
            );
            return Err(PvrError::Unsupported);
        }

        ostd::info!(
            "IMG BVNC {} with firmware v{}.{}",
            gpu_id,
            firmware.version.major,
            firmware.version.minor
        );

        Ok(Self::new(regs, gpu_id, firmware.info, firmware.version))
    }

    /// Reads a 32-bit control register.
    pub fn read32(&self, offset: u32) -> PvrResult<u32> {
        self.regs.read32(offset)
    }

    /// Reads a 64-bit control register.
    pub fn read64(&self, offset: u32) -> PvrResult<u64> {
        self.regs.read64(offset)
    }

    /// Writes a 32-bit control register.
    pub fn write32(&self, offset: u32, value: u32) -> PvrResult<()> {
        self.regs.write32(offset, value)
    }

    /// Writes a 64-bit control register.
    pub fn write64(&self, offset: u32, value: u64) -> PvrResult<()> {
        self.regs.write64(offset, value)
    }

    /// Polls a register until `predicate` holds or the timeout expires.
    ///
    /// Upstream reaches for `readl_poll_timeout` in a dozen places; giving that
    /// one home here keeps the timeout and the relaxed-read discipline consistent.
    ///
    /// The predicate is evaluated once before the deadline is consulted, so a
    /// zero timeout still performs one read — that is `readl_poll_timeout`'s
    /// behaviour, and several upstream call sites rely on it.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.h>
    /// (`pvr_cr_poll_reg32`).
    pub fn poll32(
        &self,
        offset: u32,
        predicate: impl Fn(u32) -> bool,
        timeout_us: u64,
    ) -> PvrResult<u32> {
        poll(timeout_us, || self.read32(offset), predicate)
    }

    /// Polls a 64-bit register until `predicate` holds or the timeout expires.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.h>
    /// (`pvr_cr_poll_reg64`).
    pub fn poll64(
        &self,
        offset: u32,
        predicate: impl Fn(u64) -> bool,
        timeout_us: u64,
    ) -> PvrResult<u64> {
        poll(timeout_us, || self.read64(offset), predicate)
    }

    /// Reads the BVNC from the hardware.
    pub fn read_gpu_id(&self) -> PvrResult<GpuId> {
        read_gpu_id(&self.regs)
    }

    /// Marks the GPU unrecoverable.
    ///
    /// Publishes an unrecoverable error; it does not itself stop DMA.
    /// Production callers must stop firmware and prevent further submissions first.
    /// Idempotent. Resource cleanup also checks the independent quiescence state.
    pub(crate) fn declare_lost(&self) {
        // Publish the lost state for `check_alive` and firmware wait loops.
        if !self.lost.swap(true, Ordering::Release) {
            ostd::error!(
                "GPU {} declared lost; further GPU operations are disabled",
                self.gpu_id
            );
        }
    }

    /// Returns an error if the GPU has been declared lost.
    ///
    /// The first thing every ioctl handler does.
    pub fn check_alive(&self) -> PvrResult<()> {
        if self.lost.load(Ordering::Acquire) {
            return Err(PvrError::DeviceLost);
        }
        Ok(())
    }

    /// Formats the GPU identifier as `B.V.N.C`.
    ///
    /// This is the spelling that goes into the firmware file name
    /// (`powervr/rogue_36.29.52.182_v1.fw`) and into log messages.
    /// `DRM_PVR_DEV_QUERY_GPU_INFO_GET` reports the *packed* form instead — see
    /// [`GpuId::to_packed`].
    pub fn gpu_id_string(&self) -> String {
        alloc::format!("{}", self.gpu_id)
    }
}

/// Reads the BVNC out of a register window.
///
/// `RGX_CR_CORE_ID__PBVNC` at `0x20` holds all four values packed when the GPU
/// has the `pbvnc_coreid_reg` feature. Older parts leave branch zero there, and
/// the identity has to be assembled from `CORE_REVISION` and `CORE_ID` instead.
/// The feature bit cannot be consulted: this runs before the firmware — and
/// therefore the feature mask — has been read, which is exactly why upstream
/// keys off a zero branch rather than off the feature.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_device.c>
/// (`pvr_load_gpu_id`).
fn read_gpu_id(regs: &RegisterWindow) -> PvrResult<GpuId> {
    let packed = regs.read64(cr::core_id_pbvnc::OFFSET)?;
    let gpu_id = GpuId::from_packed(packed);
    if gpu_id.b != 0 {
        return Ok(gpu_id);
    }

    let revision = regs.read32(cr::core_revision::OFFSET)? as u64;
    let core_id = regs.read32(cr::core_id::OFFSET)? as u64;
    let config = cr::core_id::CONFIG.get(core_id) as u16;

    Ok(GpuId {
        b: cr::core_revision::MAJOR.get(revision) as u16,
        v: cr::core_revision::MINOR.get(revision) as u16,
        n: config >> 8,
        c: config & 0xff,
    })
}

/// The shared body of [`PvrDevice::poll32`] and [`PvrDevice::poll64`].
fn poll<T: Copy>(
    timeout_us: u64,
    read: impl Fn() -> PvrResult<T>,
    predicate: impl Fn(T) -> bool,
) -> PvrResult<T> {
    // `tsc_freq()` is in Hz; the product cannot overflow a u128 for any timeout
    // a driver would plausibly ask for, and the result is clamped to at least one
    // tick so that a sub-tick timeout still bounds the loop.
    let ticks = ((tsc_freq() as u128 * timeout_us as u128) / 1_000_000).max(1) as u64;
    let start = read_tsc();

    loop {
        let value = read()?;
        if predicate(value) {
            return Ok(value);
        }
        if read_tsc().wrapping_sub(start) >= ticks {
            return Err(PvrError::Timeout);
        }
        core::hint::spin_loop();
    }
}

/// Tests that need neither hardware nor firmware.
///
/// The register window is the simulated one, seeded with the values
/// `rogue_36.29.52.182_v1.fw` was verified against offline (see `pvr/UPSTREAM`).
#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;
    use crate::{device_info::Features, fw::info::unpack_bvnc};

    /// A window whose `CORE_ID__PBVNC` reads back BXE-2-32's identity.
    fn bxe_window() -> RegisterWindow {
        let regs = RegisterWindow::simulated();
        regs.write64(cr::core_id_pbvnc::OFFSET, GpuId::K1_BXE_2_32.to_packed())
            .unwrap();
        regs
    }

    /// The minimum device description [`PvrDevice::probe`] accepts.
    fn mips_info() -> DeviceInfo {
        DeviceInfo {
            features: Features {
                mips: true,
                ..Features::default()
            },
            ..DeviceInfo::default()
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn bvnc_decodes_to_36_29_52_182() {
        let regs = bxe_window();
        assert_eq!(read_gpu_id(&regs).unwrap(), GpuId::K1_BXE_2_32);
        let id = read_gpu_id(&regs).unwrap();
        assert_eq!((id.b, id.v, id.n, id.c), (36, 29, 52, 182));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn packed_bvnc_round_trips() {
        // The register decode and the ABI encode must agree; they are written
        // against different sources (the generated field masks and `pack_bvnc`).
        let packed = GpuId::K1_BXE_2_32.to_packed();
        assert_eq!(packed, 0x0024_001d_0034_00b6);
        assert_eq!(GpuId::from_packed(packed), GpuId::K1_BXE_2_32);
        assert_eq!(unpack_bvnc(packed), (36, 29, 52, 182));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn bvnc_falls_back_to_the_legacy_registers() {
        // Branch zero in the packed register means the GPU predates it.
        let regs = RegisterWindow::simulated();
        regs.write64(cr::core_id_pbvnc::OFFSET, 0).unwrap();
        let revision = cr::core_revision::MAJOR.set(0, 4) | cr::core_revision::MINOR.set(0, 40);
        regs.write32(cr::core_revision::OFFSET, revision as u32)
            .unwrap();
        // CONFIG packs N in the high byte and C in the low byte.
        regs.write32(
            cr::core_id::OFFSET,
            cr::core_id::CONFIG.set(0, 0x0203) as u32,
        )
        .unwrap();

        assert_eq!(
            read_gpu_id(&regs).unwrap(),
            GpuId {
                b: 4,
                v: 40,
                n: 2,
                c: 3
            }
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn register_access_outside_the_window_is_rejected() {
        let regs = RegisterWindow::simulated();
        let past_end = RegisterWindow::SIMULATED_LEN as u32;
        assert_eq!(regs.read32(past_end), Err(PvrError::InvalidArgument));
        assert_eq!(regs.read64(past_end - 4), Err(PvrError::InvalidArgument));
        assert_eq!(regs.write32(past_end, 1), Err(PvrError::InvalidArgument));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn poll_returns_the_matching_value_and_times_out_otherwise() {
        let device = PvrDevice::new(
            bxe_window(),
            GpuId::K1_BXE_2_32,
            DeviceInfo::default(),
            FwVersion { major: 1, minor: 1 },
        );
        device.write32(0x100, 0xa5).unwrap();

        assert_eq!(device.poll32(0x100, |v| v == 0xa5, 0).unwrap(), 0xa5);
        assert_eq!(
            device.poll32(0x100, |v| v == 0x5a, 100),
            Err(PvrError::Timeout)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn declare_lost_is_sticky_and_idempotent() {
        let device = PvrDevice::new(
            bxe_window(),
            GpuId::K1_BXE_2_32,
            DeviceInfo::default(),
            FwVersion { major: 1, minor: 1 },
        );
        assert_eq!(device.check_alive(), Ok(()));
        device.declare_lost();
        device.declare_lost();
        assert_eq!(device.check_alive(), Err(PvrError::DeviceLost));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn probe_rejects_firmware_for_other_silicon() {
        let matching = ValidatedFirmware {
            gpu_id: GpuId::K1_BXE_2_32,
            version: FwVersion { major: 1, minor: 1 },
            info: mips_info(),
            layout: Vec::new(),
        };
        let device = PvrDevice::probe(bxe_window(), &matching).expect("BVNCs agree");
        assert_eq!(device.gpu_id, GpuId::K1_BXE_2_32);
        assert_eq!(device.gpu_id_string(), "36.29.52.182");

        let other = ValidatedFirmware {
            gpu_id: GpuId {
                b: 33,
                v: 15,
                n: 11,
                c: 3,
            },
            ..matching
        };
        assert_eq!(
            PvrDevice::probe(bxe_window(), &other).err(),
            Some(PvrError::BadFirmware)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn probe_rejects_a_non_mips_firmware_processor() {
        let firmware = ValidatedFirmware {
            gpu_id: GpuId::K1_BXE_2_32,
            version: FwVersion { major: 1, minor: 1 },
            info: DeviceInfo {
                features: Features {
                    meta: true,
                    ..Features::default()
                },
                ..DeviceInfo::default()
            },
            layout: Vec::new(),
        };
        assert_eq!(
            PvrDevice::probe(bxe_window(), &firmware).err(),
            Some(PvrError::Unsupported)
        );
    }
}
