// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SpacemiT K1 MFPR / padconf.

//! SpacemiT K1 MFPR / padconf.
//!
//! Cross-checked against:
//! - Linux `include/dt-bindings/pinctrl/k1-x-pinctrl.h`
//! - Vendor SPL `k1spl/src/soc/pinctrl.rs`
//! - DT `pinctrl@d401e000` (`reg` size 0x27c for the main MFPR block)
//!
//! Address: `MFPR(gpio) = 0xd401e000 + (gpio + 1) * 4` (`PINID = gpio + 1`).
//! Bits `[2:0]` = mux mode (`MUX_MODE0` = GPIO).

use ostd::{
    arch::boot::DEVICE_TREE,
    io::IoMem,
    mm::{VmIoOnce, dma::dma_io_fence},
    sync::{LocalIrqDisabled, SpinLock},
};
use spin::Once;

use crate::PadError;

/// MFPR base; size covers the vendor 0x27c window plus the TWSI pad regs at
/// `0xd401e228` / `0xd401e22c` that U-Boot `i2c_init_board` touches.
const MFPR_BASE: usize = 0xd401_e000;
const MFPR_SIZE: usize = 0x300;

/// Function select in bits `[2:0]` (`k1-x-pinctrl.h` `MUX_MODE*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MuxMode {
    Mode0 = 0, // typically GPIO
    Mode1 = 1,
    Mode2 = 2, // e.g. UART0 on GPIO68/69
    Mode3 = 3,
    Mode4 = 4,
    Mode5 = 5,
    Mode6 = 6,
    Mode7 = 7,
}

struct Mfpr {
    io: IoMem,
}

static MFPR: Once<Mfpr> = Once::new();
static UPDATE: SpinLock<(), LocalIrqDisabled> = SpinLock::new(());

pub(crate) fn init() {
    let Some(dt) = DEVICE_TREE.get() else {
        return;
    };
    if dt
        .find_compatible(&["spacemit,k1-x", "spacemit,k1"])
        .is_none()
    {
        return;
    }

    let Ok(io) = IoMem::acquire(MFPR_BASE..MFPR_BASE + MFPR_SIZE) else {
        log::warn!(
            "[k1-pinctrl] failed to map MFPR at {:#x} (size {:#x})",
            MFPR_BASE,
            MFPR_SIZE
        );
        return;
    };
    log::info!("[k1-pinctrl] MFPR mapped at {:#x}", MFPR_BASE);
    MFPR.call_once(|| Mfpr { io });
    log::info!("[k1-pinctrl] probe OK");
}

fn pin_offset(gpio: u32) -> Option<usize> {
    // PINID = gpio + 1; byte offset = PINID * 4.
    let off = (gpio as usize + 1).checked_mul(4)?;
    if off + 4 > MFPR_SIZE {
        return None;
    }
    Some(off)
}

/// Read the MFPR word for `gpio` (0..127-ish).
pub fn read_pad(gpio: u32) -> Option<u32> {
    let mfpr = MFPR.get()?;
    let off = pin_offset(gpio)?;
    mfpr.io.read_once(off).ok()
}

/// Write the full MFPR word for `gpio`.
pub fn write_pad(gpio: u32, val: u32) -> Result<(), PadError> {
    let mfpr = MFPR.get().ok_or(PadError::Unmapped)?;
    let off = pin_offset(gpio).ok_or(PadError::OutOfRange)?;
    let _guard = UPDATE.lock();
    let result = mfpr.io.write_once(off, &val).map_err(|_| PadError::Io);
    dma_io_fence();
    result
}

/// Set only bits `[2:0]` (mux mode), leave the rest of the pad word intact.
pub fn set_mux(gpio: u32, mode: MuxMode) -> Result<(), PadError> {
    let mfpr = MFPR.get().ok_or(PadError::Unmapped)?;
    let off = pin_offset(gpio).ok_or(PadError::OutOfRange)?;
    let _guard = UPDATE.lock();
    let cur: u32 = mfpr.io.read_once(off).map_err(|_| PadError::Io)?;
    mfpr.io
        .write_once(off, &((cur & !0x7) | mode as u32))
        .map_err(|_| PadError::Io)?;
    dma_io_fence();
    Ok(())
}

/// Write an absolute MFPR-region address.
pub fn write_mfpr_abs(addr: usize, val: u32) -> Result<(), PadError> {
    let mfpr = MFPR.get().ok_or(PadError::Unmapped)?;
    if !addr.is_multiple_of(4) || !(MFPR_BASE..=MFPR_BASE + MFPR_SIZE - 4).contains(&addr) {
        return Err(PadError::OutOfRange);
    }
    let _guard = UPDATE.lock();
    let result = mfpr
        .io
        .write_once(addr - MFPR_BASE, &val)
        .map_err(|_| PadError::Io);
    dma_io_fence();
    result
}

/// RMW-OR a bit at an absolute MFPR-region address (U-Boot TWSI pad enables).
pub fn or_mfpr_abs(addr: usize, bit: u32) -> Result<(), PadError> {
    let mfpr = MFPR.get().ok_or(PadError::Unmapped)?;
    if !addr.is_multiple_of(4) || !(MFPR_BASE..=MFPR_BASE + MFPR_SIZE - 4).contains(&addr) {
        return Err(PadError::OutOfRange);
    }
    let _guard = UPDATE.lock();
    let cur: u32 = mfpr
        .io
        .read_once(addr - MFPR_BASE)
        .map_err(|_| PadError::Io)?;
    mfpr.io
        .write_once(addr - MFPR_BASE, &(cur | bit))
        .map_err(|_| PadError::Io)?;
    dma_io_fence();
    Ok(())
}
