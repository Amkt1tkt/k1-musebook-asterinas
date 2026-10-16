// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: bounded MIPS firmware ELF loading.

//! Loading the firmware image's ELF segments.
//!
//! Ports `pvr_fw_util.c` (66 lines) and `pvr_fw_find_mmu_segment()` from
//! `pvr_fw.c`.
//!
//! The firmware image is an ordinary ELF32 for the firmware processor with the
//! container described in [`super::info`] appended. Loading it means walking the
//! program headers and copying each `PT_LOAD` segment into whichever driver-side
//! section buffer covers the segment's virtual address, which the layout table
//! answers.
//!
//! # Validation this adds
//!
//! Upstream reads `e_phoff`, `e_phnum` and each program header straight out of
//! the image with no bounds checking, then `memcpy`s `p_filesz` bytes from
//! `fw + p_offset`. That is safe only for a trusted image. The firmware is
//! trusted here too — it comes from the rootfs, not from user space — but
//! `#![deny(unsafe_code)]` means the alternative to checking is panicking on a
//! slice bound, and a panic during GPU bring-up is a worse outcome than
//! [`PvrError::BadFirmware`]. Recorded in `ABI.md` compatibility limits.

use ostd_pod::Pod;

use super::info::{LayoutEntry, SectionType};
use crate::error::{PvrError, PvrResult};

/// Segment type of a loadable program header. `PT_LOAD` from `<linux/elf.h>`.
pub const PT_LOAD: u32 = 1;

/// `EI_NIDENT`-byte identification prefix of an ELF file.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `ELFCLASS32`.
const ELFCLASS32: u8 = 1;
/// `ELFDATA2LSB`.
const ELFDATA2LSB: u8 = 1;

/// ELF32 file header.
///
/// Transcribes `struct elf32_hdr` from `<linux/elf.h>`. Only `e_phoff`,
/// `e_phnum` and `e_phentsize` are read; the rest is present so the layout
/// assertions below are meaningful.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod)]
pub struct Elf32Ehdr {
    /// Magic and class/data/version identification.
    pub e_ident: [u8; 16],
    /// Object file type.
    pub e_type: u16,
    /// Target architecture.
    pub e_machine: u16,
    /// Object file version.
    pub e_version: u32,
    /// Entry point.
    pub e_entry: u32,
    /// Program header table offset.
    pub e_phoff: u32,
    /// Section header table offset.
    pub e_shoff: u32,
    /// Processor-specific flags.
    pub e_flags: u32,
    /// Size of this header.
    pub e_ehsize: u16,
    /// Size of one program header.
    pub e_phentsize: u16,
    /// Number of program headers.
    pub e_phnum: u16,
    /// Size of one section header.
    pub e_shentsize: u16,
    /// Number of section headers.
    pub e_shnum: u16,
    /// Section name string table index.
    pub e_shstrndx: u16,
}

/// ELF32 program header.
///
/// Transcribes `struct elf32_phdr` from `<linux/elf.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod)]
pub struct Elf32Phdr {
    /// Segment type. See [`PT_LOAD`].
    pub p_type: u32,
    /// Offset of the segment's bytes within the file.
    pub p_offset: u32,
    /// Virtual address the segment loads at.
    pub p_vaddr: u32,
    /// Physical address. Unused here.
    pub p_paddr: u32,
    /// Bytes present in the file.
    pub p_filesz: u32,
    /// Bytes the segment occupies in memory; the tail is zero-filled.
    pub p_memsz: u32,
    /// Segment flags.
    pub p_flags: u32,
    /// Alignment.
    pub p_align: u32,
}

const _: () = assert!(size_of::<Elf32Ehdr>() == 52);
const _: () = assert!(size_of::<Elf32Phdr>() == 32);

/// The driver-side buffers the image's sections are copied into.
///
/// Ports the four `u8 *` arguments upstream threads through
/// `pvr_fw_process_elf_command_stream()` and `pvr_fw_find_mmu_segment()`. The
/// core-memory buffers are absent when the image has no such sections, which is
/// the case for BXE-2-32.
pub struct Destination<'a> {
    /// [`SectionType::FwCode`] buffer, sized by the layout table.
    pub code: &'a mut [u8],
    /// [`SectionType::FwData`] buffer.
    pub data: &'a mut [u8],
    /// [`SectionType::FwCorememCode`] buffer, if the image has one.
    pub core_code: Option<&'a mut [u8]>,
    /// [`SectionType::FwCorememData`] buffer, if the image has one.
    pub core_data: Option<&'a mut [u8]>,
}

impl Destination<'_> {
    /// The buffer for a section type, or `None` when the image declared no
    /// sections of it.
    fn buffer_mut(&mut self, ty: SectionType) -> Option<&mut [u8]> {
        match ty {
            SectionType::None => None,
            SectionType::FwCode => Some(self.code),
            SectionType::FwData => Some(self.data),
            SectionType::FwCorememCode => self.core_code.as_deref_mut(),
            SectionType::FwCorememData => self.core_data.as_deref_mut(),
        }
    }
}

/// Where a firmware-address range lands in the driver-side buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Which buffer.
    pub section_type: SectionType,
    /// Offset within that buffer.
    pub offset: u32,
}

/// Finds the section covering `[addr, addr + size)` in firmware address space.
///
/// Ports upstream `pvr_fw_find_mmu_segment()`. The range must fall entirely
/// inside one layout entry; a segment that straddles two sections, or that no
/// section covers, is a malformed image rather than something to load partially.
pub fn find_mmu_segment(layout: &[LayoutEntry], addr: u32, size: u32) -> PvrResult<Placement> {
    let end_addr = addr.checked_add(size).ok_or(PvrError::BadFirmware)?;
    // Upstream's `end_addr <= addr` guard, which also rejects a zero-length
    // segment.
    if end_addr <= addr {
        return Err(PvrError::BadFirmware);
    }

    for entry in layout {
        let entry_start = entry.base_addr;
        let entry_end = entry_start.wrapping_add(entry.alloc_size);
        if !(addr >= entry_start
            && addr < entry_end
            && end_addr > entry_start
            && end_addr <= entry_end)
        {
            continue;
        }
        let section_type = SectionType::from_raw(entry.r#type).ok_or(PvrError::BadFirmware)?;
        if section_type == SectionType::None {
            return Err(PvrError::BadFirmware);
        }
        // Upstream: `addr -= base_addr; addr += alloc_offset;`
        let offset = (addr - entry_start)
            .checked_add(entry.alloc_offset)
            .ok_or(PvrError::BadFirmware)?;
        return Ok(Placement {
            section_type,
            offset,
        });
    }

    Err(PvrError::BadFirmware)
}

/// Copies every loadable segment of `image` into `dest`.
///
/// Ports upstream `pvr_fw_process_elf_command_stream()`. A segment whose section
/// type has no buffer is skipped, matching upstream's "write to FW allocation
/// only if available"; the bytes between `p_filesz` and `p_memsz` are zeroed, as
/// ELF requires.
pub fn process_command_stream(
    image: &[u8],
    layout: &[LayoutEntry],
    dest: &mut Destination<'_>,
) -> PvrResult<()> {
    let header = read_header(image)?;
    let phentsize = header.e_phentsize as usize;
    let table_start = header.e_phoff as usize;

    for index in 0..header.e_phnum as usize {
        let start = table_start
            .checked_add(index * phentsize)
            .ok_or(PvrError::BadFirmware)?;
        let bytes = image
            .get(start..start.checked_add(phentsize).ok_or(PvrError::BadFirmware)?)
            .ok_or(PvrError::BadFirmware)?;
        let phdr = Elf32Phdr::from_first_bytes(bytes);

        if phdr.p_type != PT_LOAD {
            continue;
        }
        if phdr.p_filesz > phdr.p_memsz {
            ostd::error!(
                "firmware ELF segment {} has filesz {} > memsz {}",
                index,
                phdr.p_filesz,
                phdr.p_memsz
            );
            return Err(PvrError::BadFirmware);
        }

        let placement = find_mmu_segment(layout, phdr.p_vaddr, phdr.p_memsz).inspect_err(|_| {
            ostd::error!(
                "firmware address {:#010x} (size {}) is in no layout section",
                phdr.p_vaddr,
                phdr.p_memsz
            );
        })?;

        let source_start = phdr.p_offset as usize;
        let source_end = source_start
            .checked_add(phdr.p_filesz as usize)
            .ok_or(PvrError::BadFirmware)?;
        let source = image
            .get(source_start..source_end)
            .ok_or(PvrError::BadFirmware)?;

        let Some(buffer) = dest.buffer_mut(placement.section_type) else {
            // The image declares a core-memory section but this device has no
            // core memory allocation for it. Upstream skips too.
            continue;
        };
        let target_start = placement.offset as usize;
        let target_end = target_start
            .checked_add(phdr.p_memsz as usize)
            .ok_or(PvrError::BadFirmware)?;
        let target = buffer
            .get_mut(target_start..target_end)
            .ok_or(PvrError::BadFirmware)?;

        let (filled, zeroed) = target.split_at_mut(source.len());
        filled.copy_from_slice(source);
        zeroed.fill(0);
    }

    Ok(())
}

/// Reads and sanity-checks the ELF file header.
fn read_header(image: &[u8]) -> PvrResult<Elf32Ehdr> {
    if image.len() < size_of::<Elf32Ehdr>() {
        return Err(PvrError::BadFirmware);
    }
    let header = Elf32Ehdr::from_first_bytes(image);

    if header.e_ident[..4] != ELF_MAGIC {
        ostd::error!("firmware image is not an ELF file");
        return Err(PvrError::BadFirmware);
    }
    if header.e_ident[4] != ELFCLASS32 || header.e_ident[5] != ELFDATA2LSB {
        ostd::error!(
            "firmware ELF is class {} endianness {}; expected 32-bit little-endian",
            header.e_ident[4],
            header.e_ident[5]
        );
        return Err(PvrError::BadFirmware);
    }
    if header.e_phnum != 0 && header.e_phentsize as usize != size_of::<Elf32Phdr>() {
        return Err(PvrError::BadFirmware);
    }

    let table_size = (header.e_phnum as usize)
        .checked_mul(header.e_phentsize as usize)
        .ok_or(PvrError::BadFirmware)?;
    let table_end = (header.e_phoff as usize)
        .checked_add(table_size)
        .ok_or(PvrError::BadFirmware)?;
    if table_end > image.len() {
        return Err(PvrError::BadFirmware);
    }

    Ok(header)
}

#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::{super::info::fixture::*, *};

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn find_mmu_segment_maps_addresses_to_section_offsets() {
        let layout = default_layout();

        // MIPS_BOOT_CODE is the third code section, so its bytes live two blocks
        // into the code buffer.
        let placement = find_mmu_segment(&layout, SECTION_BASE_ADDRS[2], SECTION_SIZE).unwrap();
        assert_eq!(
            placement,
            Placement {
                section_type: SectionType::FwCode,
                offset: 2 * SECTION_SIZE,
            }
        );

        // MIPS_BOOT_DATA is the second data section, and an offset within a
        // section carries through.
        let placement = find_mmu_segment(&layout, SECTION_BASE_ADDRS[4] + 0x40, 0x10).unwrap();
        assert_eq!(
            placement,
            Placement {
                section_type: SectionType::FwData,
                offset: SECTION_SIZE + 0x40,
            }
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn find_mmu_segment_rejects_addresses_no_section_covers() {
        let layout = default_layout();
        // Below every section.
        assert_eq!(
            find_mmu_segment(&layout, 0x1000, 0x10),
            Err(PvrError::BadFirmware)
        );
        // Starts inside a section but runs off its end.
        assert_eq!(
            find_mmu_segment(&layout, SECTION_BASE_ADDRS[2], SECTION_SIZE + 1),
            Err(PvrError::BadFirmware)
        );
        // Zero length.
        assert_eq!(
            find_mmu_segment(&layout, SECTION_BASE_ADDRS[2], 0),
            Err(PvrError::BadFirmware)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_a_body_that_is_not_an_elf() {
        let image = alloc::vec![0u8; 128];
        assert_eq!(read_header(&image).err(), Some(PvrError::BadFirmware));
        assert_eq!(read_header(&[]).err(), Some(PvrError::BadFirmware));
    }
}
