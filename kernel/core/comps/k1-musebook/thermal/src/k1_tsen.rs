// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 on-die TSEN (`thermal@d4018000`).

//! K1 on-die TSEN (`thermal@d4018000`).
//!
//! Initialization follows Linux `k1x-thermal.c` `init_sensors` + `enable_sensors`
//! All five BJTs are enabled: the MUSE Book DT `sensor_range = <3 4>`
//! only cools the clusters, but GPU / top / local are the other three
//! on-die sensors.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};

use ostd::{
    arch::{ccu, read_tsc, tsc_freq},
    io::IoMem,
    mm::VmIoOnce,
};
use spin::Once;

use crate::convert::{
    EMERGENT_REBOOT_C, TEMPERATURE_OFFSET, TRIP_CRITICAL_C, TRIP_PASSIVE_0_C, raw_to_celsius,
};

const TSEN_BASE: usize = 0xd401_8000;
const TSEN_SIZE: usize = 0x100;

const REG_PCTRL: usize = 0x00;
const REG_PCTRL2: usize = 0x04;
const REG_BJT_EN: usize = 0x08;
const REG_TIME_CTRL: usize = 0x0c;
const REG_INT_MASK: usize = 0x14;
const REG_DATA0: usize = 0x20;
const REG_DATA1: usize = 0x24;
const REG_DATA2: usize = 0x28;
const REG_EMERG_THR: usize = 0x68;

const BIT_EN_SENSOR: u32 = 1 << 0;
const BIT_TEMP_MODE: u32 = 1 << 3;
const BIT_TSEN_RAW_SEL: u32 = 1 << 7;
const BIT_HW_AUTO_MODE: u32 = 1 << 23;
const BITS_CTUNE: u32 = 0xf00;
const BITS_SW_CTRL: u32 = 0x3c_0000;
const BITS_TIME_CTRL: u32 = 0xff_ffff;
const VALUE_FILTER_PERIOD: u32 = 0x3000 << 8;
const BITS_RST_ADC_CNT: u32 = 0xf0;
const VALUE_WAIT_REF_CNT: u32 = 0xf;
const BITS_SDM_CLK_SEL: u32 = 3 << 14;

const SENSOR_NAMES: [&str; 5] = ["loc", "top", "gpu", "c0", "c1"];

struct Tsen {
    io: IoMem,
}

static TSEN: Once<Tsen> = Once::new();
static PEAK_C: AtomicI32 = AtomicI32::new(i32::MIN);
static PEAK_WHICH: AtomicU8 = AtomicU8::new(0xff);

pub(super) fn init() {
    ccu::enable(ccu::Periph::Tsen);

    let Ok(io) = IoMem::acquire(TSEN_BASE..TSEN_BASE + TSEN_SIZE) else {
        log::warn!("[k1-therm] failed to map TSEN at {:#x}", TSEN_BASE);
        return;
    };

    // Mask every IRQ first. Temperature polling feeds the cpufreq policy; the
    // BSP emergency-IRQ unmask is inside `#if 0` for the same reason
    // we will not write `REG_EMERGENT_REBOOT_TEMP_THR`.
    io.write_once(REG_INT_MASK, &0xffff_ffffu32).ok();

    let time = io.read_once::<u32>(REG_TIME_CTRL).unwrap_or(0);
    let time =
        (time & !BITS_TIME_CTRL) | VALUE_FILTER_PERIOD | BITS_RST_ADC_CNT | VALUE_WAIT_REF_CNT;
    io.write_once(REG_TIME_CTRL, &time).ok();

    let mut pctrl = io.read_once::<u32>(REG_PCTRL).unwrap_or(0);
    pctrl |= BIT_TSEN_RAW_SEL | BIT_TEMP_MODE | BIT_EN_SENSOR;
    pctrl &= !BITS_SW_CTRL;
    pctrl &= !BITS_CTUNE;
    io.write_once(REG_PCTRL, &pctrl).ok();

    let pctrl2 = io.read_once::<u32>(REG_PCTRL2).unwrap_or(0) & !BITS_SDM_CLK_SEL;
    io.write_once(REG_PCTRL2, &pctrl2).ok();

    // All five BJTs. DT cooling only uses 3 and 4.
    io.write_once(REG_BJT_EN, &0x1fu32).ok();

    let int_msk = io.read_once::<u32>(REG_INT_MASK).unwrap_or(0) | 1;
    io.write_once(REG_INT_MASK, &int_msk).ok();
    let pctrl = io.read_once::<u32>(REG_PCTRL).unwrap_or(0) | BIT_HW_AUTO_MODE;
    io.write_once(REG_PCTRL, &pctrl).ok();

    let emerg = io.read_once::<u32>(REG_EMERG_THR).unwrap_or(0) & 0xffff;
    let emerg_c = raw_to_celsius(emerg as u16);
    log::info!(
        "[k1-therm] emerg_thr raw={} (~{}C) commented_dt={}C linux_critical={}C passive0={}C offset={}",
        emerg,
        emerg_c,
        EMERGENT_REBOOT_C,
        TRIP_CRITICAL_C,
        TRIP_PASSIVE_0_C,
        TEMPERATURE_OFFSET
    );

    TSEN.call_once(|| Tsen { io });

    // First conversion is not instant; 2 ms is well above one filter
    // period at the BSP `0x3000<<8` setting.
    delay_us(2_000);
    tick();
}

/// One BJT in millidegrees. `id` is the Linux sensor index (0..=4).
pub(super) fn cluster_temp_mc(id: usize) -> Option<i32> {
    TSEN.get()
        .map(|tsen| tsen.read_celsius(id).saturating_mul(1000))
}

static PRINTED_ONCE: AtomicBool = AtomicBool::new(false);

pub(super) fn tick() {
    let Some(tsen) = TSEN.get() else {
        return;
    };
    let mut vals = [0i32; 5];
    for (i, v) in vals.iter_mut().enumerate() {
        *v = tsen.read_celsius(i);
        update_peak(i, *v);
    }
    // The samples above still feed the peak trackers and the sysfs
    // thermal zones. The console line prints once at probe; after that
    // it is opt-in (`k1.diag=therm`).
    if PRINTED_ONCE.swap(true, Ordering::Relaxed) && !aster_cmdline::diag_enabled("therm") {
        return;
    }
    let peak = PEAK_C.load(Ordering::Relaxed);
    let which = PEAK_WHICH.load(Ordering::Relaxed) as usize;
    let peak_name = SENSOR_NAMES.get(which).copied().unwrap_or("?");
    log::info!(
        "[k1-therm] loc={} top={} gpu={} c0={} c1={} peak={}@{} (C; trip {}/{}/{})",
        vals[0],
        vals[1],
        vals[2],
        vals[3],
        vals[4],
        peak,
        peak_name,
        TRIP_PASSIVE_0_C,
        crate::convert::TRIP_PASSIVE_3_C,
        TRIP_CRITICAL_C
    );
}

impl Tsen {
    fn read_celsius(&self, id: usize) -> i32 {
        let (reg, shift) = match id {
            0 => (REG_DATA0, 0),
            1 => (REG_DATA0, 16),
            2 => (REG_DATA1, 0),
            3 => (REG_DATA1, 16),
            4 => (REG_DATA2, 0),
            _ => return i32::MIN,
        };
        let word = self.io.read_once::<u32>(reg).unwrap_or(0);
        let raw = ((word >> shift) & 0xffff) as u16;
        raw_to_celsius(raw)
    }
}

fn update_peak(id: usize, c: i32) {
    let prev = PEAK_C.load(Ordering::Relaxed);
    if c > prev {
        PEAK_C.store(c, Ordering::Relaxed);
        PEAK_WHICH.store(id as u8, Ordering::Relaxed);
    }
}

fn delay_us(us: u64) {
    ostd::mm::dma::dma_io_fence();
    let ticks = (tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}
