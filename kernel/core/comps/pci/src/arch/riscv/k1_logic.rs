// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: DesignWare iATU / CFG-window arithmetic for the K1 RC.
//! Pure iATU / config-window arithmetic for the K1 DWC Root Complex.
//!
//! Kept free of MMIO so the programming formulas can be ktested without a
//! board. The hardware path in [`super::k1`] writes the values produced here.

/// iATU region enable (CTRL2 bit 31).
pub(super) const ATU_ENABLE: u32 = 1 << 31;
/// iATU TLP type: memory.
pub(super) const ATU_TYPE_MEM: u32 = 0x0;
/// iATU TLP type: Type 0 configuration.
pub(super) const ATU_TYPE_CFG0: u32 = 0x4;

/// Downstream bus that CFG0 maps (matches the RC type-1 secondary bus).
pub(super) const DOWNSTREAM_BUS: u8 = 1;

/// Values written into one outbound iATU region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AtuProg {
    pub lower_base: u32,
    pub upper_base: u32,
    pub limit: u32,
    pub lower_target: u32,
    pub upper_target: u32,
    pub ctrl1: u32,
    pub ctrl2: u32,
}

/// Computes the seven outbound iATU registers for one region.
///
/// `cpu_addr` is the CPU-side window, `pci_addr` the PCI-side target, `size`
/// the window length in bytes (must be ≥ 1). The limit register is the last
/// included CPU address, matching the DesignWare unrolled iATU layout.
pub(super) fn atu_prog(type_: u32, cpu_addr: u64, pci_addr: u64, size: usize) -> AtuProg {
    let last = cpu_addr.saturating_add(size.saturating_sub(1) as u64);
    AtuProg {
        lower_base: cpu_addr as u32,
        upper_base: (cpu_addr >> 32) as u32,
        limit: last as u32,
        lower_target: pci_addr as u32,
        upper_target: (pci_addr >> 32) as u32,
        ctrl1: type_,
        ctrl2: ATU_ENABLE,
    }
}

/// Encodes a `(bus, device, function)` triple into the `bdf` form used by
/// [`cfg_win_off_from_bdf`].
#[inline]
pub(super) fn bdf_of(bus: u8, dev: u8, fun: u8) -> u32 {
    ((bus as u32) << 16) | ((dev as u32) << 11) | ((fun as u32) << 8)
}

/// Offset into the CFG0 CPU window for a config access, or `None` if the
/// request is not the single downstream device (bus 1, device 0).
///
/// Returning `None` is the caller's cue to complete the access as all-ones
/// (no device) without touching MMIO.
pub(super) fn cfg_win_off_from_bdf(bdf: u32, offset: u32) -> Option<usize> {
    let bus = ((bdf >> 16) & 0xFF) as u8;
    let dev = ((bdf >> 11) & 0x1F) as u8;
    let fun = ((bdf >> 8) & 0x7) as u8;
    cfg_win_off(bus, dev, fun, offset)
}

/// Same as [`cfg_win_off_from_bdf`] with the BDF already split.
pub(super) fn cfg_win_off(bus: u8, dev: u8, fun: u8, offset: u32) -> Option<usize> {
    if bus != DOWNSTREAM_BUS || dev != 0 {
        return None;
    }
    Some(((dev as usize) << 15) | ((fun as usize) << 12) | (offset as usize & 0xFFF))
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::{
        ATU_ENABLE, ATU_TYPE_CFG0, ATU_TYPE_MEM, DOWNSTREAM_BUS, atu_prog, bdf_of, cfg_win_off,
        cfg_win_off_from_bdf,
    };

    #[ktest]
    fn cfg0_maps_the_downstream_bus_into_the_target() {
        let p = atu_prog(
            ATU_TYPE_CFG0,
            0x9F00_0000,
            (DOWNSTREAM_BUS as u64) << 24,
            0x2000,
        );
        assert_eq!(p.ctrl1, ATU_TYPE_CFG0);
        assert_eq!(p.ctrl2, ATU_ENABLE);
        assert_eq!(p.lower_base, 0x9F00_0000);
        assert_eq!(p.upper_base, 0);
        assert_eq!(p.limit, 0x9F00_1FFF);
        assert_eq!(p.lower_target, 0x0100_0000);
        assert_eq!(p.upper_target, 0);
    }

    #[ktest]
    fn mem_region_is_identity_and_limit_is_inclusive() {
        let p = atu_prog(ATU_TYPE_MEM, 0x9000_0000, 0x9000_0000, 0x0F00_0000);
        assert_eq!(p.ctrl1, ATU_TYPE_MEM);
        assert_eq!(p.lower_base, 0x9000_0000);
        assert_eq!(p.lower_target, 0x9000_0000);
        assert_eq!(p.limit, 0x9EFF_FFFF);
    }

    #[ktest]
    fn portc_cfg_window_keeps_the_proven_one_mib_limit() {
        let p = atu_prog(
            ATU_TYPE_CFG0,
            0xA000_0000,
            (DOWNSTREAM_BUS as u64) << 24,
            0x10_0000,
        );
        assert_eq!(p.lower_base, 0xA000_0000);
        assert_eq!(p.limit, 0xA00F_FFFF);
        assert_eq!(p.lower_target, 0x0100_0000);
    }

    #[ktest]
    fn cfg_offset_is_only_the_downstream_function() {
        assert_eq!(cfg_win_off(1, 0, 0, 0x00), Some(0x00));
        assert_eq!(cfg_win_off(1, 0, 0, 0x10), Some(0x10));
        assert_eq!(cfg_win_off(1, 0, 1, 0x04), Some(0x1004));
        assert_eq!(cfg_win_off(0, 0, 0, 0x00), None);
        assert_eq!(cfg_win_off(1, 1, 0, 0x00), None);
        assert_eq!(cfg_win_off(2, 0, 0, 0x00), None);
        assert_eq!(cfg_win_off_from_bdf(bdf_of(1, 0, 0), 0x3C), Some(0x3C));
        assert_eq!(cfg_win_off_from_bdf(bdf_of(1, 1, 0), 0x00), None);
    }

    #[ktest]
    fn atu_enable_is_bit_31() {
        assert_eq!(ATU_ENABLE, 1 << 31);
        assert_eq!(
            atu_prog(ATU_TYPE_MEM, 0, 0, 1).ctrl2 & ATU_ENABLE,
            ATU_ENABLE
        );
    }
}
