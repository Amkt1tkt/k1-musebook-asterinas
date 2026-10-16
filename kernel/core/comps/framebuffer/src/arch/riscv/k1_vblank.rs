// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: Saturn DPU online2 vblank IRQ (vsync + cfg_rdy_clr).

//! DPU online2 vblank interrupt: register map and device-tree lookup.
//!
//! The Saturn DPU used on the K1 latches programmed layer addresses when the
//! composer consumes `ctl2_nml_cfg_rdy`. That consumption raises
//! `CFG_RDY_CLR`. A later `FRM_TIMING_VSYNC` confirms the consumed image.
//! Both bits are sticky: one acknowledgement containing both does not prove
//! their order. KMS keeps the BSP flip-event policy but separately requires a
//! latch from a preceding acknowledgement before VSYNC can retire old DMA.
//! That latch generation survives later commits, preserving per-frame events
//! without weakening buffer ownership. The vendor ISR in `dpu_saturn.c` uses
//! these same status and control fields.
//!
//! Sources:
//! - `official/bsp-src/linux-6.6/drivers/gpu/drm/spacemit/dpu/saturn_regs/reg_map.h`
//!   (`DPU_INT_BASE_ADDR = 0x900`, `DPU_CTRL_BASE_ADDR = 0x500`)
//! - `official/bsp-src/linux-6.6/drivers/gpu/drm/spacemit/dpu/saturn_regs/dpu_intp.h`
//!   (bit 0 = vsync, bit 4 = cfg_rdy_clr; online2 mask/status/raw at
//!   `dpu_int_reg_{4,14,24}`)
//! - `official/bsp-src/linux-6.6/arch/riscv/boot/dts/spacemit/k1-x-lcd.dtsi`
//!   (`interrupts = <90>, <89>`, names `ONLINE_IRQ`, `OFFLINE_IRQ`)
//!
//! `status = "disabled"` on the DPU node is honoured and equivalent to
//! deleting it. The board DTS keeps `dpu@c0340000` okay.

use ostd::{
    arch::{boot::DEVICE_TREE, irq::InterruptSourceInFdt},
    io::IoMem,
    mm::VmIoOnce,
};

/// Composer control block, from `reg_map.h`.
pub(crate) const DPU_CTRL_BASE: usize = 0x500;
/// Interrupt controller block, from `reg_map.h`.
pub(crate) const DPU_INT_BASE: usize = 0x900;

/// `ctl2_nml_cfg_rdy` — `dpu_ctl_reg_27`, i.e. `DPU_CTRL_BASE + 27 * 4`.
///
/// Writing 1 tells the composer the shadow registers are ready. Hardware
/// copies them at the next frame boundary and clears the bit, which is what
/// raises `CFG_RDY_CLR`. This is the "load" write `k1_edp` and `k1_kms`
/// already perform at `0x56c`.
pub(crate) const DPU_CTL2_CFG_RDY: usize = DPU_CTRL_BASE + 27 * 4;

/// `ctl2_sw_start` — `dpu_ctl_reg_35`, i.e. `DPU_CTRL_BASE + 35 * 4` = `0x58c`.
///
/// The vendor driver writes this only on the first frame, to start the
/// pipeline. Writing it on every flip would restart the controller.
#[cfg(ktest)]
pub(crate) const DPU_CTL2_SW_START: usize = DPU_CTRL_BASE + 35 * 4;

/// online2 normal mask — `dpu_int_reg_4`.
const DPU_INT_ONL2_MASK: usize = DPU_INT_BASE + 4 * 4;
/// online2 normal status, write-1-to-clear — `dpu_int_reg_14`.
const DPU_INT_ONL2_STATUS: usize = DPU_INT_BASE + 14 * 4;
/// online2 normal raw — `dpu_int_reg_24`.
const DPU_INT_ONL2_RAW: usize = DPU_INT_BASE + 24 * 4;

/// `DPU_INT_FRM_TIMING_VSYNC` in `dpu_intp.h`.
const INT_VSYNC: u32 = 1 << 0;
/// `DPU_INT_CFG_RDY_CLR` in `dpu_intp.h`.
const INT_CFG_RDY_CLR: u32 = 1 << 4;

/// `ONLINE_IRQ` in `k1-x-lcd.dtsi`.
///
/// The chip manual (`docs-chip/.../7.Interrupt_Assignments.md`) lists
/// `sys_int_ap[90] = dpu_onl2_int_sat`. The vendor DTS cell is `<90>`.
/// `sys_int_ap[91] = lcd_de_irq` is a different source. Offline writeback
/// is `<89>` and is not wired here.
pub(crate) const DPU_ONLINE_IRQ: u32 = 90;

/// Status bits read from the online2 raw register and acknowledged.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IrqStatus {
    /// `FRM_TIMING_VSYNC`.
    pub vsync: bool,
    /// `CFG_RDY_CLR`: the composer consumed `ctl2_nml_cfg_rdy`.
    pub cfg_rdy_clr: bool,
}

/// Number of mask registers, `dpu_int_reg_0` through `dpu_int_reg_9`.
///
/// The file is three banks of ten: masks at `dpu_int_reg_0..9`, statuses at
/// `dpu_int_reg_10..19`, raws at `dpu_int_reg_20..29`. Within a bank the order
/// is online0 (two registers), online1, online2, offline0, offline1
/// (`dpu_intp.h`).
const DPU_INT_BANK_LEN: usize = 10;
/// First status register, `dpu_int_reg_10`.
const DPU_INT_STATUS_FIRST: usize = DPU_INT_BASE + 10 * 4;

/// Masks **every** DPU interrupt and clears every latched status bit.
///
/// This must run *before* the PLIC source is enabled. `map_fdt_pin_to` enables
/// the pin as part of mapping it, so a source that is already asserting would
/// storm against a line that has no handler yet — a silent hang rather than
/// an error. Nothing tells us what the bootloader left enabled in this block,
/// so the only safe opening move is to make the whole device quiet.
///
/// All ten mask registers, not just online2: the vendor device tree names pin
/// 90 `ONLINE_IRQ` for `spacemit,dpu-online2` while online0 and online1 live in
/// the same register file, so silencing one path is not silencing the line.
/// Offline (writeback) is masked too; nothing in this kernel uses it.
pub(crate) fn quiesce_all(dpu: &IoMem) -> Result<(), ostd::Error> {
    disable_all(dpu)?;
    for index in 0..DPU_INT_BANK_LEN {
        dpu.write_once(DPU_INT_STATUS_FIRST + index * 4, &u32::MAX)?;
    }
    Ok(())
}

/// Masks every DPU interrupt, leaving latched status alone.
///
/// Used by the storm guard, which runs in the interrupt handler and has to be
/// able to make the line go away without knowing which block is shouting.
pub(crate) fn disable_all(dpu: &IoMem) -> Result<(), ostd::Error> {
    for index in 0..DPU_INT_BANK_LEN {
        dpu.write_once(DPU_INT_BASE + index * 4, &0u32)?;
    }
    Ok(())
}

/// Enables exactly the online2 vsync and cfg-ready-clear interrupts.
///
/// The fields are named `_msk` but the vendor driver writes 1 to *enable*
/// (`saturn_enable_vsync` in `dpu_saturn.c` writes
/// `onl2_nml_frm_timing_vsync_int_msk = enable ? 1 : 0`).
///
/// This *writes* the mask instead of OR-ing into it. Read-modify-write would
/// inherit whatever the bootloader had enabled, and this driver only knows how
/// to acknowledge two bits — an inherited third source would assert a line
/// nobody clears.
pub(crate) fn enable_online2(dpu: &IoMem) -> Result<(), ostd::Error> {
    dpu.write_once(DPU_INT_ONL2_MASK, &(INT_VSYNC | INT_CFG_RDY_CLR))
}

/// Reads the online2 raw register, acknowledges **everything** asserted, and
/// reports the two bits this driver acts on.
///
/// Every asserted bit is written back, not just the two we want: the status
/// register is write-1-to-clear, the line follows it, and a bit left set by a
/// source we do not recognise would re-assert immediately. Clearing bits we do
/// not use costs nothing here because this driver owns the block.
pub(crate) fn ack_online2(dpu: &IoMem) -> Result<IrqStatus, ostd::Error> {
    let raw = dpu.read_once::<u32>(DPU_INT_ONL2_RAW)?;
    if raw != 0 {
        dpu.write_once(DPU_INT_ONL2_STATUS, &raw)?;
    }
    ostd::mm::dma::dma_io_fence();
    Ok(IrqStatus {
        vsync: raw & INT_VSYNC != 0,
        cfg_rdy_clr: raw & INT_CFG_RDY_CLR != 0,
    })
}

/// Writes `ctl2_nml_cfg_rdy` so the address programmed into layer 0 latches
/// at the next frame boundary.
///
/// Orders CPU buffer and layer-register writes before publishing the latch.
/// The barrier does not replace cache maintenance for cached buffers.
pub(crate) fn request_latch(dpu: &IoMem) -> Result<(), ostd::Error> {
    ostd::mm::dma::dma_io_fence();
    dpu.write_once(DPU_CTL2_CFG_RDY, &1u32)?;
    // Publish the request before releasing the driver's register-state lock.
    ostd::mm::dma::dma_io_fence();
    Ok(())
}

/// Whether the composer still owns an unconsumed shadow-register request.
pub(crate) fn latch_pending(dpu: &IoMem) -> Result<bool, ostd::Error> {
    ostd::mm::dma::dma_io_fence();
    Ok(dpu.read_once::<u32>(DPU_CTL2_CFG_RDY)? & 1 != 0)
}

/// Returns the DPU online interrupt, but only if the device tree names it.
///
/// # Why there is no hard-coded fallback
///
/// `IrqChip::map_fdt_pin_to` enables the PLIC source as part of mapping it,
/// and the pin is level triggered, so if anything is already asserting it the
/// trap re-enters forever — before there is a handler to acknowledge it.
/// [`quiesce_all`] silences everything in the DPU's own interrupt block
/// first, which covers the case where the DPU was the source. It cannot
/// cover a pin that something else drives.
///
/// So the device tree is the switch. To turn vblank-timed page flips on, the
/// board DTS must name the node (this port keeps `dpu@c0340000` okay):
///
/// ```text
/// dpu@c0340000 {
///     compatible = "spacemit,dpu-online2";
///     interrupt-parent = <&plic>;
///     interrupts = <90>;
///     status = "okay";
/// };
/// ```
///
/// Until then [`AtomicDriver::flip_timing`](k1_musebook_drm::kms::AtomicDriver)
/// stays `Immediate`.
///
/// `status = "disabled"` on the node is honoured and equivalent to deleting
/// it.
pub(crate) fn lookup_interrupt() -> Option<InterruptSourceInFdt> {
    let dt = DEVICE_TREE.get()?;

    for compatible in ["spacemit,dpu-online2", "spacemit,dpu"] {
        if let Some(node) = dt.find_compatible(&[compatible])
            // Standard DT semantics: missing `status` means enabled, and only
            // "okay" / "ok" count as enabled when it is present.
            && node
                .property("status")
                .and_then(|status| status.as_str())
                .is_none_or(|status| status == "okay" || status == "ok")
            && let (Some(interrupt), Some(parent)) = (
                node.interrupts().and_then(|mut pins| pins.next()),
                node.property("interrupt-parent")
                    .and_then(|parent| parent.as_usize()),
            )
        {
            return Some(InterruptSourceInFdt {
                arguments: [interrupt as u32],
                interrupt_parent: parent as u32,
            });
        }
    }

    None
}

/// Maps an `ostd` I/O error onto the DRM I/O error used by the flip path.
pub(crate) fn io_error(_: ostd::Error) -> k1_musebook_drm::error::DrmError {
    k1_musebook_drm::error::DrmError::Io
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    /// The offsets `k1_edp` / `k1_kms` already write must be the Saturn
    /// ctl2 registers, or a flip programs the wrong controller.
    #[ktest]
    fn ctl2_offsets_match_the_saturn_map() {
        assert_eq!(DPU_CTL2_CFG_RDY, 0x56c);
        assert_eq!(DPU_CTL2_SW_START, 0x58c);
        assert_eq!(DPU_INT_ONL2_MASK, 0x910);
        assert_eq!(DPU_INT_ONL2_STATUS, 0x938);
        assert_eq!(DPU_INT_ONL2_RAW, 0x960);
    }

    #[ktest]
    fn online_irq_is_the_vendor_dts_cell() {
        assert_eq!(DPU_ONLINE_IRQ, 90);
    }
}
