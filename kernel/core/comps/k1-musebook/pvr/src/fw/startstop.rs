// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: ordered GPU firmware reset and startup sequences.

//! Starting and stopping the firmware processor.
//!
//! Ports `pvr_fw_startstop.c` (355 lines).
//!
//! The sequence is order-sensitive and mostly undocumented outside the code, so
//! the port must follow upstream step for step rather than being tidied up.
//!
//! Register sequencing follows the pinned Linux driver.
//! Simulated registers can validate issued values and error paths, but only hardware
//! can validate idle acknowledgements, reset effects and processor execution.
//! Poll failures identify the expected and observed values; [`super::BootDiagnosis`]
//! interprets firmware progress after startup.

use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    fw::{abi, mips},
    rogue::{cr, defs},
};

/// Boots the firmware processor.
///
/// Ports upstream `pvr_fw_start()`. Broadly: put everything in soft reset, take
/// the fixed-function blocks out of it, configure the SLC and the firmware
/// wrapper, then release the firmware core last. The caller then waits for the
/// core to report itself started — see [`super::wait_for_boot`].
///
/// The two `udelay(3)`s around releasing the core are upstream's, and their
/// comment says why: the core needs at least sixteen cycles either side.
///
/// On failure the GPU is left in full soft reset, which is upstream's
/// `err_reset` path and the only safe state for a half-configured GPU.
pub fn start(device: &PvrDevice) -> PvrResult<()> {
    // From the first startup write onward, DMA quiescence must be established again.
    device
        .quiesced
        .store(false, core::sync::atomic::Ordering::Release);
    let fw = device.fw.get().ok_or_else(|| {
        ostd::error!("cannot start the firmware before the image has been loaded");
        PvrError::InvalidArgument
    })?;

    // Upstream picks between two full-write masks on a feature bit, because
    // PBE2-in-XE parts have reset bits the others do not.
    let soft_reset_mask = if device.info.features.pbe2_in_xe {
        cr::soft_reset::PBE2_XE__MASKFULL
    } else {
        cr::soft_reset::MASKFULL
    };
    let has_reset2 = device.info.features.xe_tpu2;

    ostd::debug!(
        "starting firmware: reset mask {:#x}{}",
        soft_reset_mask,
        if has_reset2 { ", with SOFT_RESET2" } else { "" }
    );

    if let Err(err) = start_inner(device, soft_reset_mask, has_reset2) {
        ostd::error!("firmware start failed ({err:?}); returning the GPU to full soft reset");
        // Upstream `err_reset`. Ignore a failure here: there is nothing better to
        // do, and the original error is the one worth reporting.
        let _ = device.write64(cr::soft_reset::OFFSET, soft_reset_mask);
        return Err(err);
    }

    // The bootloader's progress counter, read before the core has had time to
    // move it. Logging the pre-boot value makes the post-boot reading in
    // `BootDiagnosis` interpretable: if they are equal, nothing ran.
    if let Ok(stage) = fw.boot_stage() {
        ostd::debug!("firmware released; boot stage counter reads {stage:#x}");
    }
    Ok(())
}

/// The body of [`start`], so that every early return goes through the reset path.
fn start_inner(device: &PvrDevice, soft_reset_mask: u64, has_reset2: bool) -> PvrResult<()> {
    let fw = device.fw.get().ok_or(PvrError::InvalidArgument)?;

    if device.info.features.sys_bus_secure_reset {
        // Drop the default sys-bus protection so the minimal setup below can
        // reach the registers it needs.
        ostd::debug!("fw start 1/9: clearing SYS_BUS_SECURE");
        device.write32(cr::sys_bus_secure::OFFSET, 0)?;
        // Upstream reads it back to fence the write.
        let _ = device.read32(cr::sys_bus_secure::OFFSET)?;
    }

    // Divergence: upstream clears ROGUE_CR_FWCORE_BOOT here for a RISC-V
    // firmware core, and sets it after the reset is released. `pvr/UPSTREAM`
    // excludes the RISC-V path, and `PvrDevice::probe` refuses such a device, so
    // both writes are unreachable and omitted.

    ostd::debug!("fw start 2/9: asserting full soft reset");
    device.write64(cr::soft_reset::OFFSET, soft_reset_mask)?;
    if has_reset2 {
        device.write64(cr::soft_reset2::OFFSET, cr::soft_reset2::MASKFULL)?;
    }
    // Read back to fence the write and drain the SOCIF pipeline.
    fence_soft_reset(device, has_reset2)?;

    ostd::debug!("fw start 3/9: releasing Rascal and the dusts");
    device.write64(
        cr::soft_reset::OFFSET,
        soft_reset_mask ^ defs::ROGUE_CR_SOFT_RESET_RASCALDUSTS_EN,
    )?;
    if has_reset2 {
        device.write64(cr::soft_reset2::OFFSET, 0)?;
    }
    fence_soft_reset(device, has_reset2)?;

    ostd::debug!("fw start 4/9: releasing everything but the firmware core");
    device.write64(cr::soft_reset::OFFSET, cr::soft_reset::GARTEN_EN)?;
    if has_reset2 {
        device.write64(cr::soft_reset2::OFFSET, 0)?;
    }
    fence_soft_reset(device, has_reset2)?;

    ostd::debug!("fw start 5/9: configuring the SLC");
    slc_init(device)?;

    ostd::debug!("fw start 6/9: programming the MIPS wrapper and remap regions");
    mips::wrapper_init(device, &fw.mips)?;

    // Must happen before the first BIF transaction.
    ostd::debug!("fw start 7/9: configuring the AXI-ACE interface");
    axi_ace_list_init(device)?;

    // Divergence: upstream calls `rogue_bif_init()` here for non-MIPS
    // processors, which programs the kernel page catalogue into
    // `ROGUE_CR_BIF_CAT_BASE0`. A MIPS firmware core reaches memory through its
    // own page table (`crate::mm::vm_mips`), not the Rogue MMU, so upstream skips
    // it for MIPS and so does this.

    // At least sixteen cycles before and after releasing the core.
    super::delay_us(3);
    ostd::debug!("fw start 8/9: releasing the firmware core");
    device.write64(cr::soft_reset::OFFSET, 0)?;
    let _ = device.read64(cr::soft_reset::OFFSET)?;
    super::delay_us(3);

    // Divergence: upstream writes ROGUE_CR_FWCORE_BOOT here to boot a RISC-V
    // core. MIPS starts fetching the moment reset is released, from the boot
    // vector the remap registers point at.
    ostd::debug!("fw start 9/9: firmware core released");
    Ok(())
}

/// Reads the soft-reset registers back to fence the preceding write.
fn fence_soft_reset(device: &PvrDevice, has_reset2: bool) -> PvrResult<()> {
    let _ = device.read64(cr::soft_reset::OFFSET)?;
    if has_reset2 {
        let _ = device.read64(cr::soft_reset2::OFFSET)?;
    }
    Ok(())
}

/// Configures the system level cache.
///
/// Ports upstream `rogue_slc_init()`. Only the low 32 bits are written: the top
/// half of the register is `SCRAMBLE_BITS`, which must keep its hardware default.
fn slc_init(device: &PvrDevice) -> PvrResult<()> {
    if !device.info.features.slc_cache_line_size_bits {
        // Upstream propagates the feature-lookup error from here, which aborts
        // the start sequence.
        ostd::error!("firmware image does not report the SLC cache line size");
        return Err(PvrError::Unsupported);
    }
    let line_size_bits = device.info.values.slc_cache_line_size_bits;

    let current = device.read32(cr::slc_ctrl_misc::OFFSET)? as u64;
    let mut reg_val = (current & cr::slc_ctrl_misc::ENABLE_PSG_HAZARD_CHECK_EN)
        | cr::slc_ctrl_misc::ADDR_DECODE_MODE_PVR_HASH1;

    // Bypass the burst combiner when a cache line is narrower than 1,024 bits.
    if line_size_bits < 1024 {
        reg_val |= cr::slc_ctrl_misc::BYPASS_BURST_COMBINER_EN;
    }
    if device.info.quirks.brn71242 && !device.info.features.gpu_multicore_support {
        reg_val |= cr::slc_ctrl_misc::LAZYWB_OVERRIDE_EN;
    }

    ostd::debug!("SLC control {reg_val:#x} for a {line_size_bits}-bit cache line");
    device.write32(cr::slc_ctrl_misc::OFFSET, reg_val as u32)
}

/// Configures the AXI-ACE interface.
///
/// Ports upstream `rogue_axi_ace_list_init()`: every domain and cache attribute
/// set to outer-cacheable. The literal 2s and 3s are upstream's; they are ACE
/// encodings, not indices, which is why they are written as shifted constants
/// rather than named.
fn axi_ace_list_init(device: &PvrDevice) -> PvrResult<()> {
    use cr::axi_ace_lite_configuration as f;

    let reg_val = (3u64 << f::AWDOMAIN_NON_SNOOPING.shift)
        | (3u64 << f::ARDOMAIN_NON_SNOOPING.shift)
        | (2u64 << f::ARDOMAIN_CACHE_MAINTENANCE.shift)
        | (2u64 << f::AWDOMAIN_COHERENT.shift)
        | (2u64 << f::ARDOMAIN_COHERENT.shift)
        | (2u64 << f::AWCACHE_COHERENT.shift)
        | (2u64 << f::ARCACHE_COHERENT.shift)
        | (2u64 << f::ARCACHE_CACHE_MAINTENANCE.shift);

    device.write64(f::OFFSET, reg_val)
}

/// Halts the firmware processor.
///
/// Ports upstream `pvr_fw_stop()`. Waits for each block to report itself idle,
/// unhooks the MTS threads, then asserts full soft reset.
///
/// Must tolerate a firmware that is already wedged: this runs on the teardown
/// path after [`PvrDevice::declare_lost`]. Upstream returns on the first poll
/// timeout, leaving the GPU running; that is a leak of a live GPU on a path whose
/// caller is about to drop the device, so this port logs the failure, carries on
/// through the remaining steps, and asserts the reset regardless — then reports
/// the first error. Recorded in `ABI.md` compatibility limits.
///
/// [`PvrDevice::declare_lost`]: crate::device::PvrDevice::declare_lost
pub fn stop(device: &PvrDevice) -> PvrResult<()> {
    // Upstream masks out the three blocks that are never idle while the host is
    // polling them: the Garten wrapper holds the firmware core, and SOCIF and
    // HOSTIF are busy servicing this very read.
    let sidekick_idle_mask = cr::sidekick_idle::MASKFULL
        & !(cr::sidekick_idle::GARTEN_EN
            | cr::sidekick_idle::SOCIF_EN
            | cr::sidekick_idle::HOSTIF_EN);

    // On a part whose firmware core sits in the MARS domain, the firmware has
    // already powered SIDEKICK and the SLC down, so polling them would time out.
    let layout_mars = device.info.features.layout_mars && device.info.values.layout_mars > 0;

    ostd::debug!(
        "stopping firmware ({} domain)",
        if layout_mars { "MARS" } else { "SIDEKICK" }
    );

    let mut first_error = Ok(());
    let mut record = |result: PvrResult<()>| {
        if first_error.is_ok() {
            first_error = result;
        }
    };

    if !layout_mars {
        record(poll_masked32(
            device,
            cr::sidekick_idle::OFFSET,
            sidekick_idle_mask,
            sidekick_idle_mask,
            "SIDEKICK_IDLE (except Garten)",
        ));
        record(poll_masked32(
            device,
            cr::slc_idle::OFFSET,
            cr::slc_idle::MASKFULL,
            cr::slc_idle::MASKFULL,
            "SLC_IDLE",
        ));
    }

    // Unhook the data-master association so no new work can be scheduled.
    // Upstream writes `MASKFULL & DM_ASSOC_CLRMSK`, i.e. every bit of the
    // register except the association field, which for these registers is zero.
    let cleared_assoc =
        cr::mts_intctx_thread0_dm_assoc::MASKFULL & !cr::mts_intctx_thread0_dm_assoc::DM_ASSOC.mask;
    record(device.write32(
        cr::mts_intctx_thread0_dm_assoc::OFFSET,
        cleared_assoc as u32,
    ));
    record(device.write32(
        cr::mts_bgctx_thread0_dm_assoc::OFFSET,
        (cr::mts_bgctx_thread0_dm_assoc::MASKFULL & !cr::mts_bgctx_thread0_dm_assoc::DM_ASSOC.mask)
            as u32,
    ));

    // Divergence: upstream also clears the thread-1 associations, but only for a
    // META firmware core, which `pvr/UPSTREAM` excludes. A MIPS core uses one
    // thread pair.

    // Extra idle checks. Each wants the register to read zero under its full
    // mask.
    for (offset, mask, what) in [
        (
            cr::bif_status_mmu::OFFSET,
            cr::bif_status_mmu::MASKFULL,
            "BIF_STATUS_MMU",
        ),
        (
            cr::bifpm_status_mmu::OFFSET,
            cr::bifpm_status_mmu::MASKFULL,
            "BIFPM_STATUS_MMU",
        ),
    ] {
        record(poll_masked32(device, offset, 0, mask, what));
    }

    if !device.info.features.xt_top_infrastructure {
        record(poll_masked32(
            device,
            cr::bif_reads_ext_status::OFFSET,
            0,
            cr::bif_reads_ext_status::MASKFULL,
            "BIF_READS_EXT_STATUS",
        ));
    }
    record(poll_masked32(
        device,
        cr::bifpm_reads_ext_status::OFFSET,
        0,
        cr::bifpm_reads_ext_status::MASKFULL,
        "BIFPM_READS_EXT_STATUS",
    ));
    record(poll_masked64(
        device,
        cr::slc_status1::OFFSET,
        0,
        cr::slc_status1::MASKFULL,
        "SLC_STATUS1",
    ));

    if !layout_mars {
        record(poll_masked32(
            device,
            cr::slc_idle::OFFSET,
            cr::slc_idle::MASKFULL,
            cr::slc_idle::MASKFULL,
            "SLC_IDLE (second pass)",
        ));
        record(poll_masked32(
            device,
            cr::sidekick_idle::OFFSET,
            sidekick_idle_mask,
            sidekick_idle_mask,
            "SIDEKICK_IDLE (second pass)",
        ));
    }

    if layout_mars {
        // The firmware core has moved to the MARS domain. SOCIF there is never
        // idle while the host polls it, so only the CPU and system arbiter bits
        // are checked.
        let mars_mask = cr::mars_idle::CPU_EN | cr::mars_idle::MH_SYSARB0_EN;
        record(poll_masked32(
            device,
            cr::mars_idle::OFFSET,
            mars_mask,
            mars_mask,
            "MARS_IDLE (CPU and arbiter)",
        ));
    } else {
        // Divergence: upstream can skip this wait when a debugger has halted a
        // META core (`META_CR_TxVECINT_BHALT` non-zero). There is no MIPS
        // equivalent and the META path is excluded, so the wait is unconditional.
        record(poll_masked32(
            device,
            cr::sidekick_idle::OFFSET,
            cr::sidekick_idle::GARTEN_EN,
            cr::sidekick_idle::GARTEN_EN,
            "SIDEKICK_IDLE (Garten)",
        ));
    }

    let soft_reset_mask = if device.info.features.pbe2_in_xe {
        cr::soft_reset::PBE2_XE__MASKFULL
    } else {
        cr::soft_reset::MASKFULL
    };
    ostd::debug!("fw stop: asserting full soft reset ({soft_reset_mask:#x})");
    record(device.write64(cr::soft_reset::OFFSET, soft_reset_mask));
    record(device.read64(cr::soft_reset::OFFSET).map(|_| ()));
    if first_error.is_ok() {
        device
            .quiescence_epoch
            .fetch_add(1, core::sync::atomic::Ordering::Release);
        device
            .quiesced
            .store(true, core::sync::atomic::Ordering::Release);
    }

    first_error
}

/// Polls a 32-bit register until `(value & mask) == expected`.
///
/// Ports upstream `pvr_cr_poll_reg32()`, with the timeout logging upstream does
/// not have. `what` names the step in the log, because a bare `-ETIMEDOUT` out of
/// a nine-step sequence says nothing about which step it was.
fn poll_masked32(
    device: &PvrDevice,
    offset: u32,
    expected: u64,
    mask: u64,
    what: &str,
) -> PvrResult<()> {
    let expected = expected as u32;
    let mask = mask as u32;
    match device.poll32(offset, |v| v & mask == expected, abi::POLL_TIMEOUT_USEC) {
        Ok(_) => Ok(()),
        Err(err) => {
            let last = device.read32(offset);
            ostd::error!(
                "timed out waiting for {} ({:#06x}): wanted {:#x} under {:#x}, read {:?}",
                what,
                offset,
                expected,
                mask,
                last
            );
            Err(err)
        }
    }
}

/// [`poll_masked32`] for a 64-bit register. Upstream `pvr_cr_poll_reg64()`.
fn poll_masked64(
    device: &PvrDevice,
    offset: u32,
    expected: u64,
    mask: u64,
    what: &str,
) -> PvrResult<()> {
    match device.poll64(offset, |v| v & mask == expected, abi::POLL_TIMEOUT_USEC) {
        Ok(_) => Ok(()),
        Err(err) => {
            let last = device.read64(offset);
            ostd::error!(
                "timed out waiting for {} ({:#06x}): wanted {:#x} under {:#x}, read {:?}",
                what,
                offset,
                expected,
                mask,
                last
            );
            Err(err)
        }
    }
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// The masks the start and stop sequences write are the ones a mistake would
    /// be invisible in: a wrong reset mask leaves a block running, and the GPU
    /// answers register reads either way.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn reset_masks_match_the_upstream_headers() {
        assert_eq!(cr::soft_reset::MASKFULL, 0x00e7_ffff_ffff_fc3d);
        assert_eq!(cr::soft_reset::PBE2_XE__MASKFULL, 0xffef_ffff_ffff_fc3d);
        assert_eq!(cr::soft_reset2::MASKFULL, 0x001f_ffff);
        // Step 3 flips exactly the Rascal and dust bits out of the full mask.
        assert_eq!(
            cr::soft_reset::MASKFULL ^ defs::ROGUE_CR_SOFT_RESET_RASCALDUSTS_EN,
            0x00e7_fe07_1fff_fc3d
        );
        // Step 4 leaves only the firmware core held.
        assert_eq!(cr::soft_reset::GARTEN_EN, 1 << 32);
    }

    /// The three bits the idle wait must ignore, and the fact that ignoring them
    /// still leaves something to wait for.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn sidekick_idle_mask_excludes_the_blocks_that_are_never_idle() {
        let mask = cr::sidekick_idle::MASKFULL
            & !(cr::sidekick_idle::GARTEN_EN
                | cr::sidekick_idle::SOCIF_EN
                | cr::sidekick_idle::HOSTIF_EN);
        assert_eq!(mask, 0x78);
        assert_eq!(mask & cr::sidekick_idle::GARTEN_EN, 0);
        assert_eq!(mask & cr::sidekick_idle::SOCIF_EN, 0);
        assert_eq!(mask & cr::sidekick_idle::HOSTIF_EN, 0);
        assert_ne!(mask & cr::sidekick_idle::MMU_EN, 0);
    }

    /// Clearing the data-master association means writing every bit of the
    /// register except the association field — which, for these registers, is
    /// every bit, so the write is zero.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn clearing_the_dm_association_writes_zero() {
        assert_eq!(
            cr::mts_intctx_thread0_dm_assoc::MASKFULL
                & !cr::mts_intctx_thread0_dm_assoc::DM_ASSOC.mask,
            0
        );
    }

    /// The AXI-ACE word is eight fields at eight shifts; a transposed pair would
    /// silently configure coherency incorrectly, which on a non-coherent SoC is exactly the
    /// class of bug `CONVENTIONS.md` DMA contract is about.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn axi_ace_word_sets_every_domain_to_outer_cacheable() {
        use cr::axi_ace_lite_configuration as f;

        let reg_val = (3u64 << f::AWDOMAIN_NON_SNOOPING.shift)
            | (3u64 << f::ARDOMAIN_NON_SNOOPING.shift)
            | (2u64 << f::ARDOMAIN_CACHE_MAINTENANCE.shift)
            | (2u64 << f::AWDOMAIN_COHERENT.shift)
            | (2u64 << f::ARDOMAIN_COHERENT.shift)
            | (2u64 << f::AWCACHE_COHERENT.shift)
            | (2u64 << f::ARCACHE_COHERENT.shift)
            | (2u64 << f::ARCACHE_CACHE_MAINTENANCE.shift);

        assert_eq!(f::AWDOMAIN_NON_SNOOPING.get(reg_val), 3);
        assert_eq!(f::ARDOMAIN_NON_SNOOPING.get(reg_val), 3);
        assert_eq!(f::ARDOMAIN_CACHE_MAINTENANCE.get(reg_val), 2);
        assert_eq!(f::AWDOMAIN_COHERENT.get(reg_val), 2);
        assert_eq!(f::ARDOMAIN_COHERENT.get(reg_val), 2);
        assert_eq!(f::AWCACHE_COHERENT.get(reg_val), 2);
        assert_eq!(f::ARCACHE_COHERENT.get(reg_val), 2);
        assert_eq!(f::ARCACHE_CACHE_MAINTENANCE.get(reg_val), 2);
        // Nothing outside those eight fields.
        assert_eq!(reg_val & !f::MASKFULL, 0);
    }
}
