// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR firmware container and feature validation.

//! The firmware image container format.
//!
//! Ports `pvr_fw_info.h` (135 lines) and the validation logic from
//! `pvr_fw_validate()` in `pvr_fw.c`.
//!
//! The image is an ordinary ELF for the firmware processor with three blocks
//! appended, addressed backwards from the end of the file:
//!
//! ```text
//!  offset                          contents
//!  0                               original .ldr/.elf
//!  FILE_SIZE - 4K - device_info_size   device info (features/quirks/enhancements)
//!  FILE_SIZE - 4K                  FwInfoHeader
//!  ...                             layout table
//!  FILE_SIZE
//! ```
//!
//! Verified against `rogue_36.29.52.182_v1.fw`: 126,976 bytes total, header at
//! 122,880, six layout entries of 24 bytes, `device_info_size` 4096.

use alloc::vec::Vec;

use ostd_pod::Pod;

use crate::error::{PvrError, PvrResult};

/// Firmware sections are aligned to this in the image.
pub const FW_BLOCK_SIZE: usize = 4096;

/// The maximum number of layout table entries.
pub const MAX_LAYOUT_ENTRIES: usize = 8;

/// The only info-header version this driver accepts.
pub const INFO_VERSION: u32 = 3;

/// Set when the image is open-source firmware rather than DDK firmware.
///
/// The driver refuses images without this bit, which is the mechanism that keeps
/// the two PowerVR driver stacks from being mixed. Confirmed set in our image.
pub const FLAG_OPEN_SOURCE: u32 = 1 << 0;

/// The highest firmware major version this driver speaks to.
pub const MAX_SUPPORTED_MAJOR_VERSION: u16 = 1;

/// Which part of the firmware processor's address space a section belongs to.
///
/// The `META_*` and `RISCV_*` variants exist because the discriminants are ABI,
/// but they are unreachable on BXE-2-32, whose image uses only the `MIPS_*` ones.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionId {
    /// META code.
    MetaCode = 0,
    /// META private data.
    MetaPrivateData = 1,
    /// META core memory code.
    MetaCorememCode = 2,
    /// META core memory data.
    MetaCorememData = 3,
    /// MIPS code.
    MipsCode = 4,
    /// MIPS exception vectors.
    MipsExceptionsCode = 5,
    /// MIPS boot code.
    MipsBootCode = 6,
    /// MIPS private data.
    MipsPrivateData = 7,
    /// MIPS boot data.
    MipsBootData = 8,
    /// MIPS stack.
    MipsStack = 9,
    /// RISC-V uncached code.
    RiscvUncachedCode = 10,
    /// RISC-V cached code.
    RiscvCachedCode = 11,
    /// RISC-V private data.
    RiscvPrivateData = 12,
    /// RISC-V core memory code.
    RiscvCorememCode = 13,
    /// RISC-V core memory data.
    RiscvCorememData = 14,
}

impl SectionId {
    /// Decodes a raw layout-table `id`.
    ///
    /// `None` for a discriminant this driver does not know, which
    /// [`parse`] treats as a malformed image rather than skipping. Upstream's
    /// `layout_get_sizes()` switches on the value without a default arm, so an
    /// unknown id there is undefined behaviour; refusing the image is the safe
    /// reading of the same code.
    pub fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::MetaCode,
            1 => Self::MetaPrivateData,
            2 => Self::MetaCorememCode,
            3 => Self::MetaCorememData,
            4 => Self::MipsCode,
            5 => Self::MipsExceptionsCode,
            6 => Self::MipsBootCode,
            7 => Self::MipsPrivateData,
            8 => Self::MipsBootData,
            9 => Self::MipsStack,
            10 => Self::RiscvUncachedCode,
            11 => Self::RiscvCachedCode,
            12 => Self::RiscvPrivateData,
            13 => Self::RiscvCorememCode,
            14 => Self::RiscvCorememData,
            _ => return None,
        })
    }

    /// Which firmware processor this section belongs to.
    ///
    /// The layout table is the only place an image says which processor it is
    /// built for — there is no field for it — which is why
    /// `pvr/UPSTREAM` can record "BXE-2-32 runs MIPS" as an observation rather
    /// than a guess. See [`ParsedInfo::coprocessor`].
    pub fn coprocessor(self) -> Coprocessor {
        match self {
            Self::MetaCode
            | Self::MetaPrivateData
            | Self::MetaCorememCode
            | Self::MetaCorememData => Coprocessor::Meta,
            Self::MipsCode
            | Self::MipsExceptionsCode
            | Self::MipsBootCode
            | Self::MipsPrivateData
            | Self::MipsBootData
            | Self::MipsStack => Coprocessor::Mips,
            Self::RiscvUncachedCode
            | Self::RiscvCachedCode
            | Self::RiscvPrivateData
            | Self::RiscvCorememCode
            | Self::RiscvCorememData => Coprocessor::Riscv,
        }
    }

    /// Whether this is the section that holds the processor's private data.
    ///
    /// Mirrors upstream `pvr_fw_find_private_data()`, which accepts whichever of
    /// the three processors' private-data sections the image happens to carry.
    pub fn is_private_data(self) -> bool {
        matches!(
            self,
            Self::MetaPrivateData | Self::MipsPrivateData | Self::RiscvPrivateData
        )
    }
}

/// The firmware processor an image is built for.
///
/// Derived from the section ids in the layout table. Only [`Self::Mips`] is
/// ported; `pvr/UPSTREAM` records META and RISC-V as deliberate exclusions,
/// so seeing either here means the exclusion was wrong and must be revisited
/// rather than worked around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coprocessor {
    /// An Imagination META core.
    Meta,
    /// A MIPS core. What BXE-2-32 has.
    Mips,
    /// A RISC-V core. Unrelated to the host also being RISC-V.
    Riscv,
}

impl core::fmt::Display for Coprocessor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Meta => "META",
            Self::Mips => "MIPS",
            Self::Riscv => "RISC-V",
        })
    }
}

/// How a section must be allocated and mapped.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionType {
    /// Unused entry.
    None = 0,
    /// Executable.
    FwCode = 1,
    /// Writable data.
    FwData = 2,
    /// Executable, in core memory.
    FwCorememCode = 3,
    /// Writable, in core memory.
    FwCorememData = 4,
}

impl SectionType {
    /// Decodes a raw layout-table `type`.
    ///
    /// `None` for an unknown discriminant; see [`SectionId::from_raw`].
    pub fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::None,
            1 => Self::FwCode,
            2 => Self::FwData,
            3 => Self::FwCorememCode,
            4 => Self::FwCorememData,
            _ => return None,
        })
    }
}

/// Total allocation size per section type, summed over the layout table.
///
/// Ports upstream `layout_get_sizes()`. Each section's `alloc_offset` is an
/// offset *within its type's* allocation, so these four totals are also the
/// bounds every `alloc_offset + alloc_size` must respect — see
/// [`ParsedInfo::section_sizes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionSizes {
    /// Bytes of [`SectionType::FwCode`].
    pub code: u32,
    /// Bytes of [`SectionType::FwData`].
    pub data: u32,
    /// Bytes of [`SectionType::FwCorememCode`].
    pub core_code: u32,
    /// Bytes of [`SectionType::FwCorememData`].
    pub core_data: u32,
}

impl SectionSizes {
    /// The total for one section type. [`SectionType::None`] contributes nothing.
    pub fn of(&self, ty: SectionType) -> u32 {
        match ty {
            SectionType::None => 0,
            SectionType::FwCode => self.code,
            SectionType::FwData => self.data,
            SectionType::FwCorememCode => self.core_code,
            SectionType::FwCorememData => self.core_data,
        }
    }
}

/// The header appended 4 KiB before the end of the image.
///
/// Corresponds to `struct pvr_fw_info_header`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod)]
pub struct FwInfoHeader {
    /// Format version. Must be [`INFO_VERSION`].
    pub info_version: u32,
    /// Length of this header.
    pub header_len: u32,
    /// Number of layout table entries.
    pub layout_entry_num: u32,
    /// Size of one layout table entry.
    pub layout_entry_size: u32,
    /// Packed BVNC the image was built for.
    pub bvnc: u64,
    /// Page size of the firmware processor.
    pub fw_page_size: u32,
    /// Compatibility flags. See [`FLAG_OPEN_SOURCE`].
    pub flags: u32,
    /// Firmware major version.
    pub fw_version_major: u16,
    /// Firmware minor version.
    pub fw_version_minor: u16,
    /// Firmware build number.
    pub fw_version_build: u32,
    /// Size of the device-info block preceding this header.
    pub device_info_size: u32,
    /// Padding; ignored.
    pub _padding_44: u32,
}

/// One entry in the layout table.
///
/// Corresponds to `struct pvr_fw_layout_entry`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod)]
pub struct LayoutEntry {
    /// Which section this is. A [`SectionId`] discriminant.
    pub id: u32,
    /// How it must be allocated. A [`SectionType`] discriminant.
    pub r#type: u32,
    /// Base address in the firmware's address space.
    pub base_addr: u32,
    /// Maximum size in bytes.
    pub max_size: u32,
    /// Bytes actually allocated.
    pub alloc_size: u32,
    /// Offset of the allocation.
    pub alloc_offset: u32,
}

/// Parsed trailing info header and layout table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedInfo {
    /// The validated header.
    pub header: FwInfoHeader,
    /// Layout table entries, in image order.
    pub layout: Vec<LayoutEntry>,
}

impl ParsedInfo {
    /// Decodes the packed BVNC from the header into `(b, v, n, c)`.
    pub fn bvnc_parts(&self) -> (u16, u16, u16, u16) {
        unpack_bvnc(self.header.bvnc)
    }

    /// Finds the layout entry for a section, if the image has one.
    ///
    /// Ports upstream `pvr_fw_find_layout_entry()`.
    pub fn find(&self, id: SectionId) -> Option<&LayoutEntry> {
        self.layout.iter().find(|entry| entry.id == id as u32)
    }

    /// Finds the firmware processor's private-data section.
    ///
    /// Ports upstream `pvr_fw_find_private_data()`. Its `base_addr` is where the
    /// data allocation has to land when the processor
    /// [`has_fixed_data_addr`](crate::fw::mips::HAS_FIXED_DATA_ADDR), which MIPS
    /// does.
    pub fn find_private_data(&self) -> Option<&LayoutEntry> {
        self.layout
            .iter()
            .find(|entry| SectionId::from_raw(entry.id).is_some_and(SectionId::is_private_data))
    }

    /// Which firmware processor this image is built for.
    ///
    /// Every section must agree; a mixture is a malformed image. An empty layout
    /// table cannot say, and is also rejected.
    ///
    /// This is the check that turns `pvr/UPSTREAM`'s exclusion of the META and
    /// RISC-V paths into something enforced rather than assumed.
    pub fn coprocessor(&self) -> PvrResult<Coprocessor> {
        let mut found: Option<Coprocessor> = None;
        for entry in &self.layout {
            let id = SectionId::from_raw(entry.id).ok_or(PvrError::BadFirmware)?;
            let this = id.coprocessor();
            match found {
                None => found = Some(this),
                Some(previous) if previous == this => {}
                Some(_) => return Err(PvrError::BadFirmware),
            }
        }
        found.ok_or(PvrError::BadFirmware)
    }

    /// Total allocation size per section type.
    ///
    /// Ports upstream `layout_get_sizes()`.
    pub fn section_sizes(&self) -> SectionSizes {
        let mut sizes = SectionSizes::default();
        for entry in &self.layout {
            // `parse` has already rejected unknown types, so the sum cannot
            // silently drop a section.
            let Some(ty) = SectionType::from_raw(entry.r#type) else {
                continue;
            };
            let slot = match ty {
                SectionType::None => continue,
                SectionType::FwCode => &mut sizes.code,
                SectionType::FwData => &mut sizes.data,
                SectionType::FwCorememCode => &mut sizes.core_code,
                SectionType::FwCorememData => &mut sizes.core_data,
            };
            // `parse` bounded every total below `u32::MAX`.
            *slot = slot.saturating_add(entry.alloc_size);
        }
        sizes
    }

    /// The device-info block, which sits immediately before the info header.
    ///
    /// Ports the offset arithmetic of upstream `pvr_fw_get_device_info()`.
    /// [`parse`] has already checked that the block starts at or after offset 0,
    /// so the only way this fails is a caller passing a different image from the
    /// one that was parsed.
    pub fn device_info_block<'a>(&self, image: &'a [u8]) -> PvrResult<&'a [u8]> {
        let header_off = image
            .len()
            .checked_sub(FW_BLOCK_SIZE)
            .ok_or(PvrError::BadFirmware)?;
        let start = header_off
            .checked_sub(self.header.device_info_size as usize)
            .ok_or(PvrError::BadFirmware)?;
        image.get(start..header_off).ok_or(PvrError::BadFirmware)
    }
}

/// Packs a BVNC the way `struct drm_pvr_dev_query_gpu_info::gpu_id` and the
/// firmware info header store it.
pub const fn pack_bvnc(b: u16, v: u16, n: u16, c: u16) -> u64 {
    ((b as u64) << 48) | ((v as u64) << 32) | ((n as u64) << 16) | (c as u64)
}

/// Inverse of [`pack_bvnc`].
pub const fn unpack_bvnc(packed: u64) -> (u16, u16, u16, u16) {
    (
        (packed >> 48) as u16,
        (packed >> 32) as u16,
        (packed >> 16) as u16,
        packed as u16,
    )
}

/// Parses the trailing firmware info header and layout table from an image.
///
/// Mirrors the checks in upstream `pvr_fw_validate()`, **except** the hardware
/// BVNC comparison (`pvr_gpu_id_to_packed_bvnc(&pvr_dev->gpu_id) != header->bvnc`).
/// That check needs a live device's `RGX_CR_CORE_ID` reading and belongs to
/// [`crate::fw::validate_and_init_device_info`] — callers of [`parse`] must not
/// treat a successful return as "firmware matches this silicon".
///
/// Rejects, among other things:
/// - images shorter than 4 KiB or not a multiple of [`FW_BLOCK_SIZE`];
/// - `info_version != 3`;
/// - header / layout entry size mismatches;
/// - more than [`MAX_LAYOUT_ENTRIES`] layout entries;
/// - missing [`FLAG_OPEN_SOURCE`];
/// - unsupported firmware major version;
/// - layout table past the end of the image;
/// - a section whose `base_addr + alloc_size` wraps or is empty;
/// - a `device_info_size` that would place the device-info block before offset 0;
/// - an unrecognised section `id` or `type`;
/// - a section whose `alloc_size` exceeds the `max_size` it declares;
/// - a section whose `alloc_offset + alloc_size` runs past the total allocation
///   for its type.
///
/// The last three are checks upstream does not make. Upstream's
/// `layout_get_sizes()` switches on `type` with no default arm, and its copy
/// loop indexes a `kzalloc()` of exactly the summed size by `alloc_offset`; both
/// are memory-safe only while the image is well formed. `#![deny(unsafe_code)]`
/// means this port cannot make the same assumption quietly — it either checks or
/// panics on a slice bound, and checking gives a diagnosable error instead.
/// Recorded in `ABI.md` compatibility limits.
pub fn parse(image: &[u8]) -> PvrResult<ParsedInfo> {
    if image.len() < FW_BLOCK_SIZE || !image.len().is_multiple_of(FW_BLOCK_SIZE) {
        return Err(PvrError::BadFirmware);
    }

    let header_off = image.len() - FW_BLOCK_SIZE;
    let header_bytes = image
        .get(header_off..header_off + size_of::<FwInfoHeader>())
        .ok_or(PvrError::BadFirmware)?;
    let header = FwInfoHeader::from_bytes(header_bytes);

    if header.info_version != INFO_VERSION {
        return Err(PvrError::BadFirmware);
    }
    if header.header_len as usize != size_of::<FwInfoHeader>()
        || header.layout_entry_size as usize != size_of::<LayoutEntry>()
        || header.layout_entry_num as usize > MAX_LAYOUT_ENTRIES
    {
        return Err(PvrError::BadFirmware);
    }
    if header.flags & FLAG_OPEN_SOURCE == 0
        || header.fw_version_major == 0
        || header.fw_version_major > MAX_SUPPORTED_MAJOR_VERSION
    {
        return Err(PvrError::BadFirmware);
    }

    let layout_off = header_off
        .checked_add(header.header_len as usize)
        .ok_or(PvrError::BadFirmware)?;
    let layout_table_size = (header.layout_entry_size as usize)
        .checked_mul(header.layout_entry_num as usize)
        .ok_or(PvrError::BadFirmware)?;
    let layout_end = layout_off
        .checked_add(layout_table_size)
        .ok_or(PvrError::BadFirmware)?;
    if layout_end > image.len() {
        return Err(PvrError::BadFirmware);
    }

    let mut layout = Vec::with_capacity(header.layout_entry_num as usize);
    for i in 0..header.layout_entry_num as usize {
        let off = layout_off + i * size_of::<LayoutEntry>();
        let entry = LayoutEntry::from_bytes(
            image
                .get(off..off + size_of::<LayoutEntry>())
                .ok_or(PvrError::BadFirmware)?,
        );
        let start = entry.base_addr;
        let end = start.wrapping_add(entry.alloc_size);
        // Reject empty or wrapping ranges, matching upstream's
        // `start_addr >= end_addr` check on unsigned wrap.
        if start >= end {
            return Err(PvrError::BadFirmware);
        }
        if SectionId::from_raw(entry.id).is_none() || SectionType::from_raw(entry.r#type).is_none()
        {
            return Err(PvrError::BadFirmware);
        }
        if entry.alloc_size > entry.max_size {
            return Err(PvrError::BadFirmware);
        }
        layout.push(entry);
    }

    if (header.device_info_size as usize) > header_off {
        return Err(PvrError::BadFirmware);
    }

    let parsed = ParsedInfo { header, layout };
    validate_section_bounds(&parsed)?;
    Ok(parsed)
}

/// Checks that every section fits inside the allocation its type will get.
///
/// A section is copied to `allocation_for(type) + alloc_offset` for
/// `alloc_size` bytes, and the allocation is exactly the sum of the
/// `alloc_size`s of that type, so `alloc_offset + alloc_size` must not exceed
/// the sum. Both the totals and each section's end are computed in `u64` so an
/// image cannot get past this by wrapping.
fn validate_section_bounds(parsed: &ParsedInfo) -> PvrResult<()> {
    let mut totals = [0u64; 5];
    for entry in &parsed.layout {
        let ty = SectionType::from_raw(entry.r#type).ok_or(PvrError::BadFirmware)?;
        totals[ty as usize] += entry.alloc_size as u64;
    }
    // Each total also caps the driver-side allocation, so it must fit a `u32`.
    if totals.iter().any(|total| *total > u32::MAX as u64) {
        return Err(PvrError::BadFirmware);
    }
    for entry in &parsed.layout {
        let ty = SectionType::from_raw(entry.r#type).ok_or(PvrError::BadFirmware)?;
        if ty == SectionType::None {
            continue;
        }
        let end = entry.alloc_offset as u64 + entry.alloc_size as u64;
        if end > totals[ty as usize] {
            return Err(PvrError::BadFirmware);
        }
    }
    Ok(())
}

const _: () = assert!(size_of::<FwInfoHeader>() == 48);
const _: () = assert!(size_of::<LayoutEntry>() == 24);
const _: () = assert!(align_of::<FwInfoHeader>() == 8);
const _: () = assert!(align_of::<LayoutEntry>() == 4);

/// Synthetic image fixtures shared by every `fw` module's tests.
///
/// Reproduces the fixed trailer fields recorded in `UPSTREAM` without loading
/// the licensed initramfs firmware. [`crate::fw::fixture`] combines this trailer
/// with a synthetic ELF body and device-info block for deterministic tests.
#[cfg(any(test, ktest))]
pub(crate) mod fixture {
    use ostd_pod::IntoBytes;

    use super::*;

    /// BVNC verified offline against `rogue_36.29.52.182_v1.fw`.
    pub(crate) const EXPECTED_BVNC: (u16, u16, u16, u16) = (36, 29, 52, 182);

    /// The six sections that image's layout table carries, in image order.
    pub(crate) const EXPECTED_LAYOUT_IDS: [SectionId; 6] = [
        SectionId::MipsCode,
        SectionId::MipsExceptionsCode,
        SectionId::MipsBootCode,
        SectionId::MipsPrivateData,
        SectionId::MipsBootData,
        SectionId::MipsStack,
    ];

    /// Size of every fixture section. One 4 KiB block each keeps the arithmetic
    /// checkable by hand.
    pub(crate) const SECTION_SIZE: u32 = FW_BLOCK_SIZE as u32;

    /// Firmware address each fixture section is loaded at.
    ///
    /// The three code addresses are MIPS-architectural and come from
    /// [`crate::rogue::mips`]; the data ones are where a 16 MiB firmware heap
    /// based at `0xc0000000` would put them. `MIPS_PRIVATE_DATA` is deliberately
    /// 4 MiB into the heap, because its `base_addr` is what fixes the whole data
    /// allocation's position and it must not collide with the code allocation at
    /// heap offset 0.
    pub(crate) const SECTION_BASE_ADDRS: [u32; 6] = [
        crate::rogue::mips::ROGUE_MIPSFW_BOOT_NMI_CODE_VIRTUAL_BASE + 0x0040_0000, // MIPS_CODE
        crate::rogue::mips::ROGUE_MIPSFW_EXCEPTIONS_VIRTUAL_BASE,                  // exceptions
        crate::rogue::mips::ROGUE_MIPSFW_BOOT_NMI_CODE_VIRTUAL_BASE,               // boot code
        0xc040_0000,                                                               // private data
        crate::rogue::mips::ROGUE_MIPSFW_BOOT_NMI_DATA_VIRTUAL_BASE,               // boot data
        crate::rogue::mips::ROGUE_MIPSFW_STACK_VIRTUAL_BASE,                       // stack
    ];

    /// The info header of a well-formed image.
    pub(crate) fn default_header() -> FwInfoHeader {
        FwInfoHeader {
            info_version: INFO_VERSION,
            header_len: size_of::<FwInfoHeader>() as u32,
            layout_entry_num: EXPECTED_LAYOUT_IDS.len() as u32,
            layout_entry_size: size_of::<LayoutEntry>() as u32,
            bvnc: pack_bvnc(
                EXPECTED_BVNC.0,
                EXPECTED_BVNC.1,
                EXPECTED_BVNC.2,
                EXPECTED_BVNC.3,
            ),
            fw_page_size: 4096,
            flags: FLAG_OPEN_SOURCE,
            fw_version_major: 1,
            fw_version_minor: 1,
            fw_version_build: 6976702,
            device_info_size: 4096,
            _padding_44: 0,
        }
    }

    /// Whether a section id is loaded into the data allocation.
    pub(crate) fn is_data(id: SectionId) -> bool {
        matches!(
            id,
            SectionId::MipsPrivateData | SectionId::MipsBootData | SectionId::MipsStack
        )
    }

    /// The layout table of a well-formed image.
    ///
    /// `alloc_offset` counts from the start of the section's *own type's*
    /// allocation, so the code and data offsets both start at zero. Getting this
    /// wrong is what [`validate_section_bounds`] rejects.
    pub(crate) fn default_layout() -> [LayoutEntry; 6] {
        let mut entries = [LayoutEntry {
            id: 0,
            r#type: SectionType::FwCode as u32,
            base_addr: 0,
            max_size: SECTION_SIZE,
            alloc_size: SECTION_SIZE,
            alloc_offset: 0,
        }; 6];
        let mut code_offset = 0;
        let mut data_offset = 0;
        for (i, id) in EXPECTED_LAYOUT_IDS.iter().enumerate() {
            entries[i].id = *id as u32;
            entries[i].base_addr = SECTION_BASE_ADDRS[i];
            if is_data(*id) {
                entries[i].r#type = SectionType::FwData as u32;
                entries[i].alloc_offset = data_offset;
                data_offset += SECTION_SIZE;
            } else {
                entries[i].r#type = SectionType::FwCode as u32;
                entries[i].alloc_offset = code_offset;
                code_offset += SECTION_SIZE;
            }
        }
        entries
    }

    /// Writes the trailing info header and layout table into `image`.
    ///
    /// `image` must already be a multiple of [`FW_BLOCK_SIZE`] long, with its
    /// last block reserved for the header region.
    pub(crate) fn write_trailer(image: &mut [u8], header: &FwInfoHeader, layout: &[LayoutEntry]) {
        assert_eq!(header.layout_entry_num as usize, layout.len());
        let header_off = image.len() - FW_BLOCK_SIZE;
        image[header_off..header_off + size_of::<FwInfoHeader>()]
            .copy_from_slice(header.as_bytes());
        let layout_off = header_off + size_of::<FwInfoHeader>();
        for (i, entry) in layout.iter().enumerate() {
            let off = layout_off + i * size_of::<LayoutEntry>();
            image[off..off + size_of::<LayoutEntry>()].copy_from_slice(entry.as_bytes());
        }
    }

    /// Builds the smallest image `parse` accepts: a zeroed body, the
    /// device-info block left blank, and the given trailer.
    pub(crate) fn build_fixture(header: &FwInfoHeader, layout: &[LayoutEntry]) -> Vec<u8> {
        let total = header.device_info_size as usize + FW_BLOCK_SIZE;
        assert!(total.is_multiple_of(FW_BLOCK_SIZE));
        let mut image = alloc::vec![0u8; total];
        write_trailer(&mut image, header, layout);
        image
    }
}

/// Synthetic-fixture tests for [`parse`].
///
/// Ordinary `#[test]`s for host `cargo test`, and `#[ktest]` for
/// `cargo osdk test`.
#[cfg(any(test, ktest))]
mod tests {
    #[cfg(ktest)]
    use ostd::prelude::*;

    use super::{fixture::*, *};

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn parse_accepts_synthetic_rogue_36_29_52_182() {
        let header = default_header();
        let layout = default_layout();
        let image = build_fixture(&header, &layout);

        let parsed = parse(&image).expect("fixture must parse");
        assert_eq!(parsed.header.info_version, 3);
        assert_eq!(parsed.bvnc_parts(), EXPECTED_BVNC);
        assert_eq!(parsed.header.flags, FLAG_OPEN_SOURCE);
        assert_eq!(parsed.header.fw_version_major, 1);
        assert_eq!(parsed.header.fw_version_minor, 1);
        assert_eq!(parsed.header.fw_version_build, 6976702);
        assert_eq!(parsed.header.device_info_size, 4096);
        assert_eq!(parsed.layout.len(), 6);
        for (entry, expected) in parsed.layout.iter().zip(EXPECTED_LAYOUT_IDS.iter()) {
            assert_eq!(entry.id, *expected as u32);
        }
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_wrong_info_version() {
        let mut header = default_header();
        header.info_version = 2;
        let image = build_fixture(&header, &default_layout());
        assert_eq!(parse(&image), Err(PvrError::BadFirmware));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_missing_open_source_flag() {
        let mut header = default_header();
        header.flags = 0;
        let image = build_fixture(&header, &default_layout());
        assert_eq!(parse(&image), Err(PvrError::BadFirmware));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_too_many_layout_entries() {
        let mut header = default_header();
        header.layout_entry_num = (MAX_LAYOUT_ENTRIES + 1) as u32;
        // Build a layout table that physically fits but exceeds the limit.
        let mut layout = Vec::new();
        for i in 0..MAX_LAYOUT_ENTRIES + 1 {
            layout.push(LayoutEntry {
                id: SectionId::MipsCode as u32,
                r#type: SectionType::FwCode as u32,
                base_addr: (i as u32) * 0x1000,
                max_size: 0x1000,
                alloc_size: 0x1000,
                alloc_offset: (i as u32) * 0x1000,
            });
        }
        // Header claims 9 entries; table of 9*24 = 216 fits in the 4K region.
        let image = build_fixture(&header, &layout);
        assert_eq!(parse(&image), Err(PvrError::BadFirmware));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_section_range_overflow() {
        let header = default_header();
        let mut layout = default_layout();
        // base_addr + alloc_size wraps around u32. `max_size` grows with it so
        // that this exercises the wrap check rather than the size check below.
        layout[0].base_addr = 0xffff_f000;
        layout[0].alloc_size = 0x2000;
        layout[0].max_size = 0x2000;
        // Keep the code allocation's totals consistent so the bounds check does
        // not fire first.
        layout[1].alloc_offset = 0x2000;
        layout[2].alloc_offset = 0x3000;
        let image = build_fixture(&header, &layout);
        assert_eq!(parse(&image), Err(PvrError::BadFirmware));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_empty_section() {
        let header = default_header();
        let mut layout = default_layout();
        layout[0].alloc_size = 0;
        let image = build_fixture(&header, &layout);
        assert_eq!(parse(&image), Err(PvrError::BadFirmware));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_an_unknown_section_id_or_type() {
        let header = default_header();

        let mut layout = default_layout();
        layout[0].id = 15; // one past RISCV_COREMEM_DATA
        assert_eq!(
            parse(&build_fixture(&header, &layout)),
            Err(PvrError::BadFirmware)
        );

        let mut layout = default_layout();
        layout[0].r#type = 5; // one past FW_COREMEM_DATA
        assert_eq!(
            parse(&build_fixture(&header, &layout)),
            Err(PvrError::BadFirmware)
        );
    }

    /// `alloc_offset + alloc_size` must stay inside the allocation the section's
    /// type will get. Upstream indexes a `kzalloc()` of exactly that size by
    /// `alloc_offset`, so an image that violates this would have upstream write
    /// past the end of its own buffer.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_a_section_outside_its_allocation() {
        let header = default_header();
        let mut layout = default_layout();
        // MIPS_BOOT_CODE is the last code section; push it one block past the
        // end of the summed code allocation.
        layout[2].alloc_offset += SECTION_SIZE;
        assert_eq!(
            parse(&build_fixture(&header, &layout)),
            Err(PvrError::BadFirmware)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn rejects_alloc_size_above_max_size() {
        let header = default_header();
        let mut layout = default_layout();
        layout[0].max_size = SECTION_SIZE - 1;
        assert_eq!(
            parse(&build_fixture(&header, &layout)),
            Err(PvrError::BadFirmware)
        );
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn layout_lookups_and_sizes() {
        let image = build_fixture(&default_header(), &default_layout());
        let parsed = parse(&image).expect("fixture must parse");

        assert_eq!(
            parsed.find(SectionId::MipsBootCode).map(|e| e.base_addr),
            Some(crate::rogue::mips::ROGUE_MIPSFW_BOOT_NMI_CODE_VIRTUAL_BASE)
        );
        assert!(parsed.find(SectionId::MetaCode).is_none());
        assert_eq!(
            parsed.find_private_data().map(|e| e.id),
            Some(SectionId::MipsPrivateData as u32)
        );
        // Three code sections and three data sections of one block each.
        assert_eq!(
            parsed.section_sizes(),
            SectionSizes {
                code: 3 * SECTION_SIZE,
                data: 3 * SECTION_SIZE,
                core_code: 0,
                core_data: 0,
            }
        );
    }

    /// The layout table is the only evidence of which firmware processor an
    /// image is for, and `pvr/UPSTREAM` bets the META and RISC-V exclusions on
    /// it. Six `MIPS_*` sections must read as MIPS, a META section must not, and
    /// a mixture must be refused rather than resolved.
    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn coprocessor_comes_from_the_section_ids() {
        let header = default_header();
        let parsed = parse(&build_fixture(&header, &default_layout())).expect("fixture");
        assert_eq!(parsed.coprocessor(), Ok(Coprocessor::Mips));

        let meta = [LayoutEntry {
            id: SectionId::MetaCode as u32,
            r#type: SectionType::FwCode as u32,
            base_addr: 0x8000_0000,
            max_size: SECTION_SIZE,
            alloc_size: SECTION_SIZE,
            alloc_offset: 0,
        }];
        let mut meta_header = header;
        meta_header.layout_entry_num = 1;
        let parsed = parse(&build_fixture(&meta_header, &meta)).expect("well-formed META image");
        assert_eq!(parsed.coprocessor(), Ok(Coprocessor::Meta));

        let mut riscv = meta;
        riscv[0].id = SectionId::RiscvCachedCode as u32;
        let parsed = parse(&build_fixture(&meta_header, &riscv)).expect("well-formed RISC-V image");
        assert_eq!(parsed.coprocessor(), Ok(Coprocessor::Riscv));

        // A mixture describes no processor at all.
        let mut mixed = default_layout();
        mixed[0].id = SectionId::MetaCode as u32;
        let parsed =
            parse(&build_fixture(&header, &mixed)).expect("container is still well formed");
        assert_eq!(parsed.coprocessor(), Err(PvrError::BadFirmware));
    }

    #[cfg_attr(ktest, ktest)]
    #[cfg_attr(all(test, not(ktest)), test)]
    fn device_info_block_is_the_4k_before_the_header() {
        let header = default_header();
        let image = build_fixture(&header, &default_layout());
        let parsed = parse(&image).expect("fixture");
        let block = parsed.device_info_block(&image).expect("block in range");
        assert_eq!(block.len(), header.device_info_size as usize);
        assert_eq!(block.as_ptr() as usize - image.as_ptr() as usize, 0);
    }
}
