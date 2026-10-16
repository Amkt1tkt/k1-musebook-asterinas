// SPDX-License-Identifier: MPL-2.0

//! Virtual Dynamic Shared Object (vDSO).
//!
//! vDSO enables user space applications to execute routines that access kernel space data without
//! the need for user mode and kernel mode switching. This is particularly useful for frequently
//! invoked, read-only operations such as obtaining the current time, which can be efficiently and
//! securely handled within the user space.
//!
//! This module manages the vDSO mechanism through the [`Vdso`] structure, which contains a
//! [`VdsoData`] instance with necessary time-related information, and a Virtual Memory Object
//! ([`Vmo`]) that encapsulates both the data and the vDSO routines. The VMO is intended to be
//! mapped into the address space of every user space process for efficient access.

use alloc::sync::Arc;
// K1 MUSE Book --- (order shared vDSO data around its sequence publication)
use core::sync::atomic::{Ordering, fence};
use core::{mem::ManuallyDrop, time::Duration};

// K1 MUSE Book ---
use aster_time::{Instant, read_monotonic_time};
use aster_util::coeff::Coeff;
use ostd::{
    const_assert,
    mm::{PAGE_SIZE, UFrame, VmIo, VmIoOnce, VmReader},
    // K1 MUSE Book --- (clock setters and timer callbacks share vDSO data)
    // sync::SpinLock,
    sync::{LocalIrqDisabled, SpinLock},
    // K1 MUSE Book ---
};
use ostd_pod::IntoBytes;
use spin::Once;

use crate::{
    syscall::ClockId,
    time::{
        // K1 MUSE Book --- (the immutable boot sample is not the realtime origin)
        // START_TIME, SystemTime,
        // K1 MUSE Book ---
        clocks::MonotonicClock,
        timer::{Timeout, TimerGuard},
    },
    vm::page_cache::{Vmo, VmoMapMode, VmoOptions},
};

const CLOCK_TAI: usize = 11;
const VDSO_BASES: usize = CLOCK_TAI + 1;
const DEFAULT_CLOCK_MODE: VdsoClockMode = VdsoClockMode::Tsc;

// K1 MUSE Book --- (the syscall wall origin also supplies vDSO realtime)
// static START_SECS_COUNT: Once<u64> = Once::new();
// K1 MUSE Book ---
static VDSO: Once<Arc<Vdso>> = Once::new();

#[derive(Clone, Copy, Debug)]
enum VdsoClockMode {
    None = 0,
    Tsc = 1,
}

/// An instant used in [`VdsoData`]
///
/// This contains information that describes the current time with respect to a certain clock (see
/// [`ClockId`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
struct VdsoInstant {
    /// Seconds.
    secs: u64,
    /// Nanoseconds (for [`CLOCK_REALTIME_COARSE`] and [`CLOCK_MONOTONIC_COARSE`]) or shifted
    /// nanoseconds (for other high-resolution clocks).
    ///
    /// [`CLOCK_REALTIME_COARSE`]: ClockId::CLOCK_REALTIME_COARSE
    /// [`CLOCK_MONOTONIC_COARSE`]: ClockId::CLOCK_MONOTONIC_COARSE
    nanos_info: u64,
}

impl VdsoInstant {
    const fn zero() -> Self {
        Self {
            secs: 0,
            nanos_info: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
struct ArchVdsoData {}

/// Plain-old-data vDSO data that will be mapped to userspace.
///
/// Since we currently use the vDSO shared library directly from Linux, the layout of this
/// structure must match what is specified in the Linux library.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.2.10/source/include/vdso/datapage.h#L90>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct VdsoData {
    seq: u32,

    clock_mode: i32,
    last_cycles: u64,
    mask: u64,
    mult: u32,
    shift: u32,
    basetime: [VdsoInstant; VDSO_BASES],

    tz_minuteswest: i32,
    tz_dsttime: i32,
    hrtimer_res: u32,
    __unused: u32,

    arch_data: ArchVdsoData,
}

const HIGH_RES_CLOCK_IDS: [ClockId; 4] = [
    ClockId::CLOCK_REALTIME,
    ClockId::CLOCK_MONOTONIC,
    ClockId::CLOCK_MONOTONIC_RAW,
    ClockId::CLOCK_BOOTTIME,
];

const COARSE_RES_CLOCK_IDS: [ClockId; 2] = [
    ClockId::CLOCK_REALTIME_COARSE,
    ClockId::CLOCK_MONOTONIC_COARSE,
];

impl VdsoData {
    const fn empty() -> Self {
        VdsoData {
            seq: 0,
            clock_mode: VdsoClockMode::None as i32,
            last_cycles: 0,
            mask: u64::MAX,
            mult: 0,
            shift: 0,
            basetime: [VdsoInstant::zero(); VDSO_BASES],
            tz_minuteswest: 0,
            tz_dsttime: 0,
            hrtimer_res: 0,
            __unused: 0,
            arch_data: ArchVdsoData {},
        }
    }

    /// Initializes vDSO data based on the default clock source.
    fn init(&mut self) {
        let clocksource = aster_time::default_clocksource();
        let coeff = clocksource.coeff();
        self.set_clock_mode(DEFAULT_CLOCK_MODE);
        self.set_coeff(coeff);

        let (last_instant, last_cycles) = clocksource.last_record();
        self.update_high_res_instant(last_instant, last_cycles);
        self.update_coarse_res_instant(last_instant);
    }

    fn set_clock_mode(&mut self, mode: VdsoClockMode) {
        self.clock_mode = mode as i32;
    }

    fn set_coeff(&mut self, coeff: &Coeff) {
        self.mult = coeff.mult();
        self.shift = coeff.shift();
    }

    fn update_clock_instant(&mut self, clockid: usize, secs: u64, nanos_info: u64) {
        self.basetime[clockid].secs = secs;
        self.basetime[clockid].nanos_info = nanos_info;
    }

    // K1 MUSE Book --- (one precise encoding for timer and setter updates)
    fn update_sample(
        &mut self,
        clock_id: ClockId,
        monotonic: Duration,
        origin: Duration,
        shifted: bool,
    ) {
        let sample = match clock_id {
            ClockId::CLOCK_REALTIME | ClockId::CLOCK_REALTIME_COARSE => {
                origin.checked_add(monotonic).unwrap()
            }
            _ => monotonic,
        };
        let nanos = u64::from(sample.subsec_nanos());
        let encoded = if shifted { nanos << self.shift } else { nanos };
        self.update_clock_instant(clock_id as usize, sample.as_secs(), encoded);
    }

    fn restamp_realtime(&mut self, origin: Duration) {
        for (mono_id, real_id, shifted) in [
            (ClockId::CLOCK_MONOTONIC, ClockId::CLOCK_REALTIME, true),
            (
                ClockId::CLOCK_MONOTONIC_COARSE,
                ClockId::CLOCK_REALTIME_COARSE,
                false,
            ),
        ] {
            let mono = self.basetime[mono_id as usize];
            let nanos = if shifted {
                mono.nanos_info >> self.shift
            } else {
                mono.nanos_info
            };
            self.update_sample(
                real_id,
                Duration::new(mono.secs, nanos as u32),
                origin,
                shifted,
            );
        }
    }
    // K1 MUSE Book ---

    // K1 MUSE Book --- (preserve fractional realtime in the vDSO snapshot)
    // fn update_high_res_instant(&mut self, instant: Instant, instant_cycles: u64) {
    //     self.last_cycles = instant_cycles;
    //     for clock_id in HIGH_RES_CLOCK_IDS {
    //         let secs = if clock_id == ClockId::CLOCK_REALTIME {
    //             instant.secs() + START_SECS_COUNT.get().unwrap()
    //         } else {
    //             instant.secs()
    //         };
    //
    //         self.update_clock_instant(
    //             clock_id as usize,
    //             secs,
    //             (instant.nanos() as u64) << self.shift as u64,
    //         );
    //     }
    // }
    fn update_high_res_instant(&mut self, instant: Instant, instant_cycles: u64) {
        self.last_cycles = instant_cycles;
        let monotonic = Duration::new(instant.secs(), instant.nanos());
        let origin = crate::time::wall_origin();
        for clock_id in HIGH_RES_CLOCK_IDS {
            self.update_sample(clock_id, monotonic, origin, true);
        }
    }
    // K1 MUSE Book ---

    // K1 MUSE Book --- (preserve fractional realtime in the vDSO snapshot)
    // fn update_coarse_res_instant(&mut self, instant: Instant) {
    //     for clock_id in COARSE_RES_CLOCK_IDS {
    //         let secs = if clock_id == ClockId::CLOCK_REALTIME_COARSE {
    //             instant.secs() + START_SECS_COUNT.get().unwrap()
    //         } else {
    //             instant.secs()
    //         };
    //         self.update_clock_instant(clock_id as usize, secs, instant.nanos() as u64);
    //     }
    // }
    fn update_coarse_res_instant(&mut self, instant: Instant) {
        let monotonic = Duration::new(instant.secs(), instant.nanos());
        let origin = crate::time::wall_origin();
        for clock_id in COARSE_RES_CLOCK_IDS {
            self.update_sample(clock_id, monotonic, origin, false);
        }
    }
    // K1 MUSE Book ---
}

macro_rules! vdso_data_field_offset {
    ($field:ident) => {
        VDSO_VMO_LAYOUT.data_offset + core::mem::offset_of!(VdsoData, $field)
    };
}

/// The vDSO singleton.
///
/// See [the module-level documentations](self) for more about the vDSO mechanism.
struct Vdso {
    /// A `VdsoData` instance.
    // K1 MUSE Book --- (exclude local timer callbacks while a setter updates data)
    // data: SpinLock<VdsoData>,
    data: SpinLock<VdsoData, LocalIrqDisabled>,
    // K1 MUSE Book ---
    /// A VMO that contains the entire vDSO, including the library text and the vDSO data.
    vmo: Arc<Vmo>,
    /// A frame that contains the vDSO data. This frame is contained in and will not be removed
    /// from the vDSO VMO.
    ///
    /// Note: This frame should only be updated while holding the spin lock on [`Self::data`].
    data_frame: UFrame,
}

/// The binary of a prebuilt Linux vDSO library.
///
/// These binaries can be found in [this repo](https://github.com/asterinas/linux_vdso).
/// The development environment should download these binaries before compiling the kernel.
/// and provide the path of the local copy of the repo by setting the `VDSO_LIBRARY_DIR` env var.
//
// TODO: Remove this dependency of a Linux's prebuilt vDSO library.
// Asterinas can implement vDSO library independently.
// As long as our vDSO provides the same symbols as Linux does,
// the libc will work just fine.
#[cfg(target_arch = "x86_64")]
const PREBUILT_VDSO_LIB: &[u8] =
    include_bytes!(concat!(env!("VDSO_LIBRARY_DIR"), "/vdso_x86_64.so"));
#[cfg(target_arch = "riscv64")]
const PREBUILT_VDSO_LIB: &[u8] =
    include_bytes!(concat!(env!("VDSO_LIBRARY_DIR"), "/vdso_riscv64.so"));
#[cfg(target_arch = "aarch64")]
const PREBUILT_VDSO_LIB: &[u8] =
    include_bytes!(concat!(env!("VDSO_LIBRARY_DIR"), "/vdso_aarch64.so"));

/// The offset from the vDSO base to the `__vdso_rt_sigreturn` function.
///
/// This constant is specific to the prebuilt vDSO library and can be obtained from
/// `readelf -s vdso_riscv64.so | grep '__vdso_rt_sigreturn'` for RISC-V and
/// `readelf -s vdso_aarch64.so | grep '__kernel_rt_sigreturn'` for ARM.
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub(crate) const __VDSO_RT_SIGRETURN_OFFSET: usize = cfg_select! {
    target_arch = "riscv64" => 0x5b0,
    target_arch = "aarch64" => 0x538,
};

impl Vdso {
    /// Constructs a new `Vdso`, including an initialized `VdsoData` and a VMO of the vDSO.
    fn new() -> Self {
        let mut vdso_data = VdsoData::empty();
        vdso_data.init();

        let (vdso_vmo, data_frame) = {
            let vmo_options = VmoOptions::new(VDSO_VMO_LAYOUT.size);
            let vdso_vmo = vmo_options.alloc().unwrap();
            // Write vDSO data to vDSO VMO.
            let mut reader = VmReader::from(vdso_data.as_bytes()).to_fallible();
            vdso_vmo
                .write(VDSO_VMO_LAYOUT.data_offset, &mut reader)
                .unwrap();

            // Write vDSO library to vDSO VMO.
            let mut reader = VmReader::from(
                &PREBUILT_VDSO_LIB[..VDSO_VMO_LAYOUT
                    .text_segment_size
                    .min(PREBUILT_VDSO_LIB.len())],
            )
            .to_fallible();
            vdso_vmo
                .write(VDSO_VMO_LAYOUT.text_segment_offset, &mut reader)
                .unwrap();

            let (data_frame, _) = vdso_vmo
                .try_commit_page(0, VmoMapMode::SharedWrite)
                .unwrap();
            (vdso_vmo, data_frame)
        };

        Self {
            data: SpinLock::new(vdso_data),
            vmo: vdso_vmo,
            data_frame: data_frame.into(),
        }
    }

    fn update_high_res_instant(&self, instant: Instant, instant_cycles: u64) {
        let mut data = self.data.lock();

        data.update_high_res_instant(instant, instant_cycles);

        // Update begins.
        self.begin_update_data_frame(&mut data);

        self.data_frame
            .write_val(vdso_data_field_offset!(last_cycles), &instant_cycles)
            .unwrap();
        for clock_id in HIGH_RES_CLOCK_IDS {
            self.update_data_frame_instant(clock_id, &mut data);
        }

        // Update finishes.
        self.finish_update_data_frame(&mut data);
    }

    fn update_coarse_res_instant(&self, instant: Instant) {
        let mut data = self.data.lock();

        data.update_coarse_res_instant(instant);

        // Update begins.
        self.begin_update_data_frame(&mut data);

        for clock_id in COARSE_RES_CLOCK_IDS {
            self.update_data_frame_instant(clock_id, &mut data);
        }

        // Update finishes.
        self.finish_update_data_frame(&mut data);
    }

    fn begin_update_data_frame(&self, data: &mut VdsoData) {
        debug_assert!(data.seq.is_multiple_of(2));

        data.seq = data.seq.wrapping_add(1);
        self.data_frame
            .write_once(vdso_data_field_offset!(seq), &data.seq)
            .unwrap();
        // K1 MUSE Book --- (Linux vdso_write_begin: odd seq precedes data stores)
        fence(Ordering::Release);
        // K1 MUSE Book ---
    }

    /// Updates the requisite fields of the vDSO data in the frame.
    fn update_data_frame_instant(&self, clockid: ClockId, data: &mut VdsoData) {
        let clock_index = clockid as usize;

        let secs_offset =
            vdso_data_field_offset!(basetime) + clock_index * size_of::<VdsoInstant>();
        let nanos_info_offset = vdso_data_field_offset!(basetime)
            + core::mem::offset_of!(VdsoInstant, nanos_info)
            + clock_index * size_of::<VdsoInstant>();
        self.data_frame
            .write_val(secs_offset, &data.basetime[clock_index].secs)
            .unwrap();
        self.data_frame
            .write_val(nanos_info_offset, &data.basetime[clock_index].nanos_info)
            .unwrap();
    }

    fn finish_update_data_frame(&self, data: &mut VdsoData) {
        debug_assert!(!data.seq.is_multiple_of(2));

        data.seq = data.seq.wrapping_add(1);
        // K1 MUSE Book --- (Linux vdso_write_end: data stores precede even seq)
        // Original upstream FIXME:
        // FIXME: To synchronize with the vDSO library, this needs to be an atomic write with the
        // Release memory order.
        // The aligned sequence write is indivisible; this fence supplies its
        // publication ordering and pairs with the Linux vDSO reader barriers.
        fence(Ordering::Release);
        // K1 MUSE Book ---
        self.data_frame
            .write_once(vdso_data_field_offset!(seq), &data.seq)
            .unwrap();
    }
}

/// Updates instants with respect to high-resolution clocks in vDSO data.
fn update_vdso_high_res_instant(instant: Instant, instant_cycles: u64) {
    VDSO.get()
        .unwrap()
        .update_high_res_instant(instant, instant_cycles);
}

/// Updates instants with respect to coarse-resolution clocks in vDSO data.
fn update_vdso_coarse_res_instant(_guard: TimerGuard) {
    let instant = Instant::from(read_monotonic_time());
    VDSO.get().unwrap().update_coarse_res_instant(instant);
}

// K1 MUSE Book --- (refresh existing realtime samples immediately after clock_settime)
// /// Initializes the time duration from 1970-01-01 00:00:00 to the start time.
// fn init_start_secs_count() {
//     let time_duration = START_TIME
//         .get()
//         .unwrap()
//         .duration_since(&SystemTime::UNIX_EPOCH)
//         .unwrap();
//     START_SECS_COUNT.call_once(|| time_duration.as_secs());
// }
/// Restamps realtime using the existing monotonic samples and cycle anchor.
/// Monotonic values, coefficients, and the vDSO ABI layout remain unchanged.
pub(crate) fn refresh_realtime() {
    let Some(vdso) = VDSO.get() else {
        return;
    };
    let mut data = vdso.data.lock();
    data.restamp_realtime(crate::time::wall_origin());
    vdso.begin_update_data_frame(&mut data);
    vdso.update_data_frame_instant(ClockId::CLOCK_REALTIME, &mut data);
    vdso.update_data_frame_instant(ClockId::CLOCK_REALTIME_COARSE, &mut data);
    vdso.finish_update_data_frame(&mut data);
}
// K1 MUSE Book ---

// K1 MUSE Book --- (gettimeofday's vDSO path shares kernel timezone metadata)
pub(crate) fn refresh_timezone() {
    let Some(vdso) = VDSO.get() else {
        return;
    };
    let mut data = vdso.data.lock();
    let timezone = crate::time::timezone();
    data.tz_minuteswest = timezone.tz_minuteswest;
    data.tz_dsttime = timezone.tz_dsttime;
    vdso.begin_update_data_frame(&mut data);
    vdso.data_frame
        .write_once(
            vdso_data_field_offset!(tz_minuteswest),
            &data.tz_minuteswest,
        )
        .unwrap();
    vdso.data_frame
        .write_once(vdso_data_field_offset!(tz_dsttime), &data.tz_dsttime)
        .unwrap();
    vdso.finish_update_data_frame(&mut data);
}
// K1 MUSE Book ---

/// Initializes the vDSO singleton.
fn init_vdso() {
    let vdso = Vdso::new();
    VDSO.call_once(|| Arc::new(vdso));
}

pub(super) fn init_in_first_kthread() {
    // K1 MUSE Book --- (realtime is initialized by time::system_time)
    // init_start_secs_count();
    // K1 MUSE Book ---
    init_vdso();
    // K1 MUSE Book --- (preserve timezone state if vDSO setup follows a setter)
    refresh_timezone();
    // K1 MUSE Book ---

    aster_time::VDSO_DATA_HIGH_RES_UPDATE_FN.call_once(|| update_vdso_high_res_instant);

    // Coarse resolution clock IDs directly read the instant stored in vDSO data without
    // using coefficients for calculation, thus the related instant requires more frequent updating.
    let coarse_instant_timer = ManuallyDrop::new(
        MonotonicClock::timer_manager().create_timer(update_vdso_coarse_res_instant),
    );
    let mut timer_guard = coarse_instant_timer.lock();
    timer_guard.set_interval(Duration::from_millis(100));
    timer_guard.set_timeout(Timeout::After(Duration::from_millis(100)));
}

/// Returns the vDSO VMO.
///
/// This function will return `None` if vDSO does not exist (e.g., if it has not been initialized).
pub(crate) fn vdso_vmo() -> Option<Arc<Vmo>> {
    VDSO.get().map(|vdso| vdso.vmo.clone())
}

#[cfg(target_arch = "x86_64")]
pub(crate) const VDSO_VMO_LAYOUT: VdsoVmoLayout = VdsoVmoLayout {
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/x86/entry/vdso/vdso-layout.lds.S#L20
    data_segment_offset: 0,
    data_segment_size: PAGE_SIZE,
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/x86/entry/vdso/vdso-layout.lds.S#L19
    text_segment_offset: 4 * PAGE_SIZE,
    text_segment_size: PAGE_SIZE,
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/x86/include/asm/vvar.h#L51
    data_offset: 0x80,

    size: 5 * PAGE_SIZE,
};

#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub(crate) const VDSO_VMO_LAYOUT: VdsoVmoLayout = VdsoVmoLayout {
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/riscv/kernel/vdso.c#L247
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/arm64/kernel/vdso.c#L211
    data_segment_offset: 0,
    data_segment_size: PAGE_SIZE,
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/riscv/kernel/vdso.c#L256
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/arm64/kernel/vdso.c#L222
    text_segment_offset: 2 * PAGE_SIZE,
    text_segment_size: PAGE_SIZE,
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/riscv/kernel/vdso.c#L47
    // https://elixir.bootlin.com/linux/v6.2.10/source/arch/arm64/kernel/vdso.c#L73
    data_offset: 0,

    size: 3 * PAGE_SIZE,
};

pub(crate) struct VdsoVmoLayout {
    pub data_segment_offset: usize,
    pub data_segment_size: usize,
    pub text_segment_offset: usize,
    pub text_segment_size: usize,
    pub data_offset: usize,
    pub size: usize,
}

const_assert!(
    VDSO_VMO_LAYOUT
        .data_segment_offset
        .is_multiple_of(PAGE_SIZE)
);
const_assert!(VDSO_VMO_LAYOUT.data_segment_size.is_multiple_of(PAGE_SIZE));
const_assert!(
    VDSO_VMO_LAYOUT
        .text_segment_offset
        .is_multiple_of(PAGE_SIZE)
);
const_assert!(VDSO_VMO_LAYOUT.text_segment_size.is_multiple_of(PAGE_SIZE));
const_assert!(VDSO_VMO_LAYOUT.size.is_multiple_of(PAGE_SIZE));

// Ensure that the vDSO data at `VDSO_VMO_LAYOUT.data_offset` is in the data segment.
//
// `VDSO_VMO_LAYOUT.data_segment_offset <= VDSO_VMO_LAYOUT.data_offset` should also hold, but we
// skipped that assertion due to the broken `clippy::absurd_extreme_comparisons` lint.
const_assert!(
    VDSO_VMO_LAYOUT.data_offset + size_of::<VdsoData>()
        <= VDSO_VMO_LAYOUT.data_segment_offset + VDSO_VMO_LAYOUT.data_segment_size
);

// K1 MUSE Book --- (vDSO uses the full syscall wall origin)
#[cfg(ktest)]
mod realtime_tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn realtime_samples_include_the_wall_origins_nanoseconds() {
        let monotonic = Duration::new(12, 900_000_000);
        let origin = Duration::new(1_700_000_000, 300_000_000);
        let mut data = VdsoData::empty();
        data.shift = 24;
        data.last_cycles = 123;
        for clock_id in HIGH_RES_CLOCK_IDS {
            data.update_sample(clock_id, monotonic, origin, true);
        }
        for clock_id in COARSE_RES_CLOCK_IDS {
            data.update_sample(clock_id, monotonic, origin, false);
        }
        let high = data.basetime[ClockId::CLOCK_REALTIME as usize];
        let coarse = data.basetime[ClockId::CLOCK_REALTIME_COARSE as usize];
        assert_eq!(
            (high.secs, high.nanos_info >> data.shift),
            (1_700_000_013, 200_000_000)
        );
        assert_eq!(
            (coarse.secs, coarse.nanos_info),
            (1_700_000_013, 200_000_000)
        );
        let before = data.basetime[ClockId::CLOCK_MONOTONIC as usize];
        data.restamp_realtime(Duration::new(1_600_000_000, 400_000_000));
        let after = data.basetime[ClockId::CLOCK_MONOTONIC as usize];
        assert_eq!(
            (after.secs, after.nanos_info),
            (before.secs, before.nanos_info)
        );
        assert_eq!(data.last_cycles, 123);
        let high = data.basetime[ClockId::CLOCK_REALTIME as usize];
        let coarse = data.basetime[ClockId::CLOCK_REALTIME_COARSE as usize];
        assert_eq!(
            (high.secs, high.nanos_info >> data.shift),
            (1_600_000_013, 300_000_000)
        );
        assert_eq!(
            (coarse.secs, coarse.nanos_info),
            (1_600_000_013, 300_000_000)
        );
    }
}
// K1 MUSE Book ---
