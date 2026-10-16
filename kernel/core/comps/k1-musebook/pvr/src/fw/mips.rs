// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: MIPS firmware startup, addressing, and interrupt protocol.

//! The MIPS firmware processor.
//!
//! Ports `pvr_fw_mips.c` and `pvr_fw_mips.h` (248 lines), with the page table
//! side in [`crate::mm::vm_mips`].
//!
//! BXE-2-32's firmware runs on MIPS. This is not a guess: the layout table of
//! `rogue_36.29.52.182_v1.fw` contains `MIPS_CODE`, `MIPS_EXCEPTIONS_CODE`,
//! `MIPS_BOOT_CODE`, `MIPS_PRIVATE_DATA`, `MIPS_BOOT_DATA` and `MIPS_STACK` and
//! no META or RISC-V sections. [`super::info::ParsedInfo::coprocessor`] turns
//! that observation into a check.
//!
//! That is convenient, because MIPS is also the processor in AXE-1-16M — the GPU
//! upstream supports best and ships in production in TI's AM62 — so this is the
//! most exercised firmware path in the driver rather than the newest one.
//!
//! # Three ways the firmware reaches memory
//!
//! Only the third is a page table, which is why the remap registers matter as
//! much as [`crate::mm::vm_mips`] does:
//!
//! | firmware address | mechanism | programmed by |
//! |---|---|---|
//! | `0xbfc00000` boot code, `0xbfc01000` boot data | remap regions 1 and 2 | [`wrapper_init`] |
//! | `0x9fc02000` exception vectors | remap region 3 | [`wrapper_init`] |
//! | `0xc0000000`+ the firmware heap | the MIPS TLB, refilled from the page table | [`crate::mm::vm_mips`] |
//!
//! The boot code cannot be reached through the page table because the page table
//! is what the boot code sets up. Hence the remap registers, which are a
//! three-entry hardware translation the wrapper applies before the core's own
//! TLB sees the address at all.

use alloc::vec::Vec;

use ostd::mm::Daddr;

use super::{
    elf,
    heap::FwObject,
    info::{ParsedInfo, SectionId},
};
use crate::{
    device::PvrDevice,
    error::{PvrError, PvrResult},
    mm::vm_mips::{MipsPageTable, PT_PAGE_COUNT},
    rogue::{cr, mips as rmips},
};

/// Where the MIPS core sees the firmware heap.
///
/// Upstream `ROGUE_FW_HEAP_MIPS_BASE`, in `pvr_fw_mips.c`. Not one of the
/// generated `pvr_rogue_mips.h` constants: it is the driver's choice of where to
/// put the heap in the processor's `kseg2` region, and the firmware learns it
/// from the page table rather than having it compiled in.
pub const FW_HEAP_MIPS_BASE: u32 = 0xc000_0000;

/// Log2 of the MIPS firmware heap size. Upstream `ROGUE_FW_HEAP_MIPS_SHIFT`, 16 MiB.
pub const FW_HEAP_MIPS_LOG2_SIZE: u32 = 24;

/// Bytes the MIPS firmware reserves at the top of the heap.
///
/// Upstream `ROGUE_FW_HEAP_MIPS_RESERVED_SIZE`, `SZ_1M`.
pub const FW_HEAP_MIPS_RESERVED_SIZE: u32 = 1024 * 1024;

/// Whether the data allocation must land at the address the layout table names.
///
/// Upstream `pvr_fw_defs_mips.has_fixed_data_addr`, which is `true`: the MIPS
/// firmware's compiled-in pointers into its private data are absolute, so the
/// data allocation has to go where `MIPS_PRIVATE_DATA` says.
pub const HAS_FIXED_DATA_ADDR: bool = true;

/// The bootloader configuration block, at the start of the boot data page.
///
/// Transcribes upstream `struct rogue_mipsfw_boot_data` from
/// `pvr_rogue_mips.h`. It is not in [`crate::rogue::fwif`] because
/// `pvr_rogue_mips_check.h` has no assertions for it — upstream only asserts
/// over `rogue_mips_tlb_entry`, `rogue_mips_remap_entry` and
/// `rogue_mips_state` — so the layout assertions below are this port's own, and
/// `CONVENTIONS.md` layout contract requires them.
///
/// This is the very first thing the firmware reads. Every field wrong here looks
/// identical from the host: the core fetches its first instruction and never
/// reports anything again.
///
/// Reference: <https://elixir.bootlin.com/linux/latest/source/drivers/gpu/drm/imagination/pvr_rogue_mips.h>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod)]
pub struct MipsBootData {
    /// Bus address of the `MIPS_STACK` section.
    pub stack_phys_addr: u64,
    /// Bus address of the GPU's register window, so the firmware can reach its
    /// own register bank.
    pub reg_base: u64,
    /// Bus addresses of the page table's 4 KiB pages.
    pub pt_phys_addr: [u64; PT_PAGE_COUNT],
    /// Log2 of the page size the table describes.
    pub pt_log2_page_size: u32,
    /// Number of table pages.
    pub pt_num_pages: u32,
    /// Reserved; written zero.
    pub reserved1: u32,
    /// Reserved; written zero.
    pub reserved2: u32,
}

const _: () = assert!(size_of::<MipsBootData>() == 64);
const _: () = assert!(align_of::<MipsBootData>() == 8);
const _: () = assert!(core::mem::offset_of!(MipsBootData, stack_phys_addr) == 0);
const _: () = assert!(core::mem::offset_of!(MipsBootData, reg_base) == 8);
const _: () = assert!(core::mem::offset_of!(MipsBootData, pt_phys_addr) == 16);
const _: () = assert!(core::mem::offset_of!(MipsBootData, pt_log2_page_size) == 48);
const _: () = assert!(core::mem::offset_of!(MipsBootData, pt_num_pages) == 52);
const _: () = assert!(core::mem::offset_of!(MipsBootData, reserved1) == 56);
const _: () = assert!(core::mem::offset_of!(MipsBootData, reserved2) == 60);

/// MIPS-specific firmware state.
///
/// Ports upstream `struct pvr_fw_mips_data`.
pub struct MipsData {
    /// The firmware processor's page table.
    pub page_table: MipsPageTable,
    /// Bus address of `MIPS_BOOT_CODE`, for remap region 1.
    pub boot_code_daddr: Daddr,
    /// Bus address of `MIPS_BOOT_DATA`, for remap region 2.
    pub boot_data_daddr: Daddr,
    /// Bus address of `MIPS_EXCEPTIONS_CODE`, for remap region 3.
    pub exception_code_daddr: Daddr,
    /// Offset of `MIPS_BOOT_DATA` within the data allocation.
    ///
    /// The boot data page also carries the bootloader's progress counter and the
    /// NMI state block, which is where [`super::BootDiagnosis`] looks when the
    /// firmware does not come up.
    pub boot_data_offset: u32,
}

/// A processed image: the bytes to load and the state derived from them.
///
/// Upstream keeps the driver-side copies in `pvr_fw_mem::code` / `::data` and
/// re-copies them on a hard reset, because the firmware writes to its own data
/// section while it runs.
pub struct ProcessedImage {
    /// MIPS-specific state.
    pub mips: MipsData,
    /// Driver-side copy of the code allocation's contents.
    pub code: Vec<u8>,
    /// Driver-side copy of the data allocation's contents.
    pub data: Vec<u8>,
}

/// Lays out the image's sections and builds the firmware's boot arguments.
///
/// Ports upstream `pvr_mips_fw_process()`. In order: load the ELF segments into
/// driver-side buffers, note the bus addresses the remap registers will need, and
/// fill in the bootloader configuration block.
///
/// `code` and `data` are the firmware-heap allocations the sections will be
/// copied into; this function does not write to them, it only reads their
/// addresses, so the caller is free to do the copy in one pass afterwards.
///
/// `regs_daddr` is the bus address of the GPU's register window, which the
/// firmware needs in order to reach its own register bank. Upstream takes it from
/// `pvr_dev->regs_resource->start`; here the platform layer's platform code supplies
/// it, because nothing below the platform layer knows the window's address.
///
/// The fixed addresses in the layout table (`0xbfc00000` boot, `0x9fc02000`
/// exceptions, `0xc0000000` code) are MIPS architectural, not negotiable.
pub fn process(
    parsed: &ParsedInfo,
    image: &[u8],
    code: &FwObject,
    data: &FwObject,
    page_table: MipsPageTable,
    regs_daddr: Daddr,
) -> PvrResult<ProcessedImage> {
    let sizes = parsed.section_sizes();
    if sizes.core_code != 0 || sizes.core_data != 0 {
        // BXE-2-32's image has no core-memory sections. Rather than carry an
        // untestable allocation path, say so.
        ostd::error!(
            "firmware image declares {} bytes of core code and {} of core data; \
             the MIPS port has no core memory allocation (see pvr/UPSTREAM)",
            sizes.core_code,
            sizes.core_data
        );
        return Err(PvrError::Unsupported);
    }

    let mut code_bytes = alloc::vec![0u8; sizes.code as usize];
    let mut data_bytes = alloc::vec![0u8; sizes.data as usize];
    elf::process_command_stream(
        image,
        &parsed.layout,
        &mut elf::Destination {
            code: &mut code_bytes,
            data: &mut data_bytes,
            core_code: None,
            core_data: None,
        },
    )?;

    let boot_code = required_entry(parsed, SectionId::MipsBootCode)?;
    let boot_data = required_entry(parsed, SectionId::MipsBootData)?;
    let exception_code = required_entry(parsed, SectionId::MipsExceptionsCode)?;
    let stack = required_entry(parsed, SectionId::MipsStack)?;

    let boot_code_daddr = code.daddr_at(boot_code as usize)?;
    let boot_data_daddr = data.daddr_at(boot_data as usize)?;
    let exception_code_daddr = code.daddr_at(exception_code as usize)?;
    let mips = MipsData {
        page_table,
        boot_code_daddr,
        boot_data_daddr,
        exception_code_daddr,
        boot_data_offset: boot_data,
    };

    let mut boot = MipsBootData {
        stack_phys_addr: data.daddr_at(stack as usize)? as u64,
        reg_base: regs_daddr as u64,
        pt_phys_addr: [0; PT_PAGE_COUNT],
        pt_log2_page_size: rmips::ROGUE_MIPSFW_LOG2_PAGE_SIZE_4K,
        pt_num_pages: rmips::ROGUE_MIPSFW_MAX_NUM_PAGETABLE_PAGES,
        reserved1: 0,
        reserved2: 0,
    };
    // Upstream translates between firmware 4 KiB pages and host pages here. The
    // `const` assertion in `super::heap` pins the two equal on this platform, so
    // page `n` of the table is host page `n` at offset zero.
    for (page_nr, slot) in boot.pt_phys_addr.iter_mut().enumerate() {
        *slot = mips.page_table.page_daddr(page_nr)? as u64;
    }

    let boot_offset = (boot_data + rmips::ROGUE_MIPSFW_BOOTLDR_CONF_OFFSET) as usize;
    let boot_end = boot_offset
        .checked_add(size_of::<MipsBootData>())
        .ok_or(PvrError::BadFirmware)?;
    data_bytes
        .get_mut(boot_offset..boot_end)
        .ok_or(PvrError::BadFirmware)?
        .copy_from_slice(ostd_pod::IntoBytes::as_bytes(&boot));

    ostd::debug!(
        "firmware boot data: stack {:#x}, regs {:#x}, page table {:#x}, {} pages of 2^{}",
        boot.stack_phys_addr,
        boot.reg_base,
        boot.pt_phys_addr[0],
        boot.pt_num_pages,
        boot.pt_log2_page_size
    );

    Ok(ProcessedImage {
        mips,
        code: code_bytes,
        data: data_bytes,
    })
}

/// The `alloc_offset` of a section the MIPS path cannot do without.
///
/// Upstream returns `-EINVAL` when any of the four is missing.
fn required_entry(parsed: &ParsedInfo, id: SectionId) -> PvrResult<u32> {
    match parsed.find(id) {
        Some(entry) => Ok(entry.alloc_offset),
        None => {
            ostd::error!("firmware image has no {:?} section", id);
            Err(PvrError::BadFirmware)
        }
    }
}

/// Programs the firmware wrapper and its three remap regions.
///
/// Ports upstream `pvr_mips_wrapper_init()`, register for register and in order.
/// Called from [`super::startstop::start`] between taking the GPU out of reset
/// and releasing the firmware core, which is the only window in which the
/// wrapper is writable and the core is not yet running.
///
/// Each remap region maps one fixed firmware address to the bus address of the
/// matching section, with the region size given as a log2 in the low bits of the
/// `CONFIG2` register.
pub fn wrapper_init(device: &PvrDevice, mips: &MipsData) -> PvrResult<()> {
    if !device.info.features.phys_bus_width {
        ostd::error!("firmware image does not report a physical bus width");
        return Err(PvrError::Unsupported);
    }
    let phys_bus_width = device.info.values.phys_bus_width;
    if phys_bus_width <= 32 {
        // Upstream's `WARN_ON(phys_bus_width <= 32)` then `-EINVAL`.
        ostd::error!(
            "MIPS firmware is only supported on a physical bus wider than 32 bits; got {}",
            phys_bus_width
        );
        return Err(PvrError::Unsupported);
    }

    // Upstream `const u64 remap_settings = ROGUE_MIPSFW_BOOT_REMAP_LOG2_SEGMENT_SIZE;`
    // All three regions are one 4 KiB segment.
    let remap_settings = rmips::ROGUE_MIPSFW_BOOT_REMAP_LOG2_SEGMENT_SIZE as u64;

    // The register bank's firmware address, in units of its alignment, plus the
    // ISA the core boots in. BXE-2-32's firmware is microMIPS.
    device.write32(
        cr::mips_wrapper_config::OFFSET,
        (rmips::ROGUE_MIPSFW_REGISTERS_VIRTUAL_BASE
            >> rmips::ROGUE_MIPSFW_WRAPPER_CONFIG_REGBANK_ADDR_ALIGN)
            | cr::mips_wrapper_config::BOOT_ISA_MODE_MICROMIPS as u32,
    )?;

    // Region 1: boot code at 0xbfc00000, whose remap input is 0x1fc00000.
    device.write64(
        cr::mips_addr_remap1_config1::OFFSET,
        rmips::ROGUE_MIPSFW_BOOT_REMAP_PHYS_ADDR_IN as u64
            | cr::mips_addr_remap1_config1::MODE_ENABLE_EN,
    )?;
    device.write64(
        cr::mips_addr_remap1_config2::OFFSET,
        (mips.boot_code_daddr as u64 & cr::mips_addr_remap1_config2::ADDR_OUT.mask)
            | remap_settings,
    )?;

    if device.info.quirks.brn63553 {
        // Upstream: always required on 36-bit cores, to stop the core making
        // continuous unmapped accesses to address 0. Region 5's input base is
        // left at zero, so address 0 lands on the boot code instead of nowhere.
        //
        // Divergence: upstream also `WARN_ON(phys_bus_width != 36)` here. This
        // port keeps the workaround but not the warning, because
        // `DeviceInfo` surfaces the width and a 40-bit part with this quirk set
        // is the firmware's statement about the hardware, not something the
        // driver should second-guess. Recorded in `ABI.md` compatibility limits.
        device.write64(
            cr::mips_addr_remap5_config1::OFFSET,
            cr::mips_addr_remap5_config1::MODE_ENABLE_EN,
        )?;
        device.write64(
            cr::mips_addr_remap5_config2::OFFSET,
            (mips.boot_code_daddr as u64 & cr::mips_addr_remap5_config2::ADDR_OUT.mask)
                | remap_settings,
        )?;
    }

    // Region 2: boot data at 0xbfc01000, remap input 0x1fc01000.
    device.write64(
        cr::mips_addr_remap2_config1::OFFSET,
        rmips::ROGUE_MIPSFW_DATA_REMAP_PHYS_ADDR_IN as u64
            | cr::mips_addr_remap2_config1::MODE_ENABLE_EN,
    )?;
    device.write64(
        cr::mips_addr_remap2_config2::OFFSET,
        (mips.boot_data_daddr as u64 & cr::mips_addr_remap2_config2::ADDR_OUT.mask)
            | remap_settings,
    )?;

    // Region 3: exception vectors at 0x9fc02000, remap input 0x1fc02000.
    device.write64(
        cr::mips_addr_remap3_config1::OFFSET,
        rmips::ROGUE_MIPSFW_CODE_REMAP_PHYS_ADDR_IN as u64
            | cr::mips_addr_remap3_config1::MODE_ENABLE_EN,
    )?;
    device.write64(
        cr::mips_addr_remap3_config2::OFFSET,
        (mips.exception_code_daddr as u64 & cr::mips_addr_remap3_config2::ADDR_OUT.mask)
            | remap_settings,
    )?;

    // Zero selects the firmware core's idle control. The upstream symbol retains
    // the META name even when the firmware processor is MIPS.
    device.write64(
        cr::mts_garten_wrapper_config::OFFSET,
        cr::mts_garten_wrapper_config::IDLE_CTRL_META,
    )?;

    // Turn on the EJTAG probe. Upstream writes zero, i.e. clears
    // DISABLE_PROBE_DEBUG.
    device.write32(cr::mips_debug_config::OFFSET, 0)?;

    ostd::debug!(
        "MIPS wrapper: boot code {:#x}, boot data {:#x}, exceptions {:#x}, bus {} bits{}",
        mips.boot_code_daddr,
        mips.boot_data_daddr,
        mips.exception_code_daddr,
        phys_bus_width,
        if device.info.quirks.brn63553 {
            ", BRN63553 workaround on"
        } else {
            ""
        }
    );
    Ok(())
}

/// Whether the firmware wrapper has an interrupt pending.
///
/// Ports upstream `pvr_mips_irq_pending()`.
pub fn irq_pending(device: &PvrDevice) -> PvrResult<bool> {
    let status = device.read32(cr::mips_wrapper_irq_status::OFFSET)?;
    Ok(u64::from(status) & cr::mips_wrapper_irq_status::EVENT_EN != 0)
}

/// Acknowledges the firmware wrapper's interrupt.
///
/// Ports upstream `pvr_mips_irq_clear()`.
pub fn irq_clear(device: &PvrDevice) -> PvrResult<()> {
    device.write32(
        cr::mips_wrapper_irq_clear::OFFSET,
        cr::mips_wrapper_irq_clear::EVENT_EN as u32,
    )
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::*;

    /// The remap registers take a bus address masked to `ADDR_OUT` — bits 12
    /// upwards — or-ed with a log2 segment size in the low bits. Dropping the
    /// mask would put the segment size bits into the address.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn remap_config2_keeps_the_address_and_the_segment_size_apart() {
        let daddr: u64 = 0x9_8765_4000;
        let settings = rmips::ROGUE_MIPSFW_BOOT_REMAP_LOG2_SEGMENT_SIZE as u64;
        let value = (daddr & cr::mips_addr_remap1_config2::ADDR_OUT.mask) | settings;

        assert_eq!(
            cr::mips_addr_remap1_config2::ADDR_OUT.get(value) << 12,
            daddr & 0x0000_00ff_ffff_f000
        );
        assert_eq!(
            cr::mips_addr_remap1_config2::REGION_SIZE_POW2.get(value),
            settings
        );
        // 4 KiB segments.
        assert_eq!(settings, 12);
    }

    /// The wrapper config packs the register bank's firmware address divided by
    /// its alignment, and BXE-2-32 boots microMIPS rather than MIPS32.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn wrapper_config_encodes_the_register_bank_and_isa() {
        let value = (rmips::ROGUE_MIPSFW_REGISTERS_VIRTUAL_BASE
            >> rmips::ROGUE_MIPSFW_WRAPPER_CONFIG_REGBANK_ADDR_ALIGN)
            | cr::mips_wrapper_config::BOOT_ISA_MODE_MICROMIPS as u32;

        assert_eq!(
            cr::mips_wrapper_config::REGBANK_BASE_ADDR.get(value as u64),
            (0xcf80_0000u64 >> 16)
        );
        assert_eq!(
            cr::mips_wrapper_config::BOOT_ISA_MODE.get(value as u64),
            cr::mips_wrapper_config::BOOT_ISA_MODE_MICROMIPS
                >> cr::mips_wrapper_config::BOOT_ISA_MODE.shift
        );
    }
}
