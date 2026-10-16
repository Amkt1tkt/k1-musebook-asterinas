// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SPM8821/P1 shutdown through the already initialized TWSI8 bus.

//! Power control register facts come from Linux's
//! drivers/power/reset/spacemit-p1-reboot.c: PWR_CTRL2 at 0x7e,
//! shutdown request bit 2 and reset request bit 1. No regulator voltage
//! or persistent configuration is changed here. The request bits self-clear.
//!
//! Call only after userspace has stopped writing and filesystem shutdown sync
//! has completed. This function does not itself establish that condition.

use ostd::arch::boot::DEVICE_TREE;

use crate::{k1_twsi, pmic_try_lock};

const PWR_CTRL2: u8 = 0x7e;
const SHUTDOWN_REQUEST: u8 = 1 << 2;
const RESET_REQUEST: u8 = 1 << 1;

/// Whether the DT explicitly names the PMIC supported by this implementation.
pub fn is_supported() -> bool {
    DEVICE_TREE.get().is_some_and(|dt| {
        dt.find_compatible(&["spacemit,spm8821", "spacemit,p1"])
            .is_some_and(|node| {
                node.property("status")
                    .and_then(|p| p.as_str())
                    .is_none_or(|s| matches!(s, "okay" | "ok"))
            })
    })
}

/// Requests power-off or restart and reports failure if the CPU keeps running.
///
/// All I2C polling and lock acquisition are bounded. On return the caller must
/// halt: filesystems might already be marked clean, so resuming userspace is
/// invalid even when the PMIC did not acknowledge the request.
pub fn request(restart: bool) -> Result<(), &'static str> {
    if !is_supported() {
        return Err("SPM8821/P1 is not enabled in the device tree");
    }
    let guard = (0..100)
        .find_map(|_| {
            let guard = pmic_try_lock();
            if guard.is_none() {
                k1_twsi::delay_ms(1);
            }
            guard
        })
        .ok_or("PMIC bus unavailable or busy for 100 ms")?;
    let before = guard.read(PWR_CTRL2).ok_or("PWR_CTRL2 read failed")?;
    let request = if restart {
        RESET_REQUEST
    } else {
        SHUTDOWN_REQUEST
    };
    log::debug!(
        "[k1-pmic] request restart={} reg=0x7e before={:#04x} write={:#04x}",
        restart,
        before,
        before | request
    );
    if !guard.write(PWR_CTRL2, before | request) {
        return Err("PWR_CTRL2 write failed");
    }
    drop(guard);
    log::info!("[k1-pmic] power request acknowledged; waiting for hardware");
    k1_twsi::delay_ms(1000);
    Err("PMIC acknowledged the request but the CPU is still running after 1000 ms")
}
