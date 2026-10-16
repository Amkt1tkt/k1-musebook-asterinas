// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: `/proc/sys/kernel/random/{boot_id,uuid}` for systemd (`sd_id128_get_boot`, `%b`).

//! `/proc/sys/kernel/random/{boot_id,uuid}`.
//!
//! `boot_id` is a random UUID fixed for the lifetime of the kernel; systemd
//! (`sd_id128_get_boot()`) reads it in journald, logind, and every unit's
//! `%b` specifier. `uuid` yields a fresh random UUID on every read.

use alloc::format;

use aster_util::printer::VmPrinter;
use spin::Once;

use crate::{
    fs::{
        file::{InodeType, mkmod},
        procfs::{
            StaticEntry,
            template::{
                ProcDir, ProcDirOps, ProcFile, ProcFileOps, ReaddirEntry,
                listed_entries_from_table, lookup_child_from_table, visit_listed_entries,
            },
        },
        vfs::inode::Inode,
    },
    prelude::*,
    util::random::getrandom,
};

/// Represents the inode at `/proc/sys/kernel/random`.
pub(super) struct RandomDirOps;

impl RandomDirOps {
    pub(super) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        // Reference: <https://elixir.bootlin.com/linux/v6.16.5/source/drivers/char/random.c#L1674>
        ProcDir::new(Self, parent, mkmod!(a+rx))
    }

    const STATIC_ENTRIES: &'static [StaticEntry] = &[
        ("boot_id", InodeType::File, BootIdFileOps::new_inode),
        ("uuid", InodeType::File, UuidFileOps::new_inode),
    ];
}

impl ProcDirOps for RandomDirOps {
    fn lookup_child(&self, this_dir: &ProcDir<Self>, name: &str) -> Result<Arc<dyn Inode>> {
        if let Some(child) = lookup_child_from_table(name, Self::STATIC_ENTRIES, |f| {
            (f)(this_dir.this_weak().clone())
        }) {
            return Ok(child);
        }

        return_errno_with_message!(Errno::ENOENT, "the file does not exist");
    }

    fn visit_entries_from_offset<'a, F>(&'a self, offset: usize, visit_fn: F) -> Result<()>
    where
        F: FnMut(ReaddirEntry<'a>) -> Result<()>,
    {
        visit_listed_entries(
            offset,
            listed_entries_from_table(Self::STATIC_ENTRIES),
            visit_fn,
        )
    }
}

/// Formats 16 random bytes as an RFC 4122 version-4 UUID string.
fn format_uuid(bytes: &mut [u8; 16]) -> String {
    // Version 4, variant 1 -- the same fix-ups Linux applies.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    )
}

/// The boot ID: generated once, on first read.
fn boot_id() -> &'static str {
    static BOOT_ID: Once<String> = Once::new();
    BOOT_ID.call_once(|| {
        let mut bytes = [0u8; 16];
        getrandom(&mut bytes);
        format_uuid(&mut bytes)
    })
}

/// Represents the inode at `/proc/sys/kernel/random/boot_id`.
struct BootIdFileOps;

impl BootIdFileOps {
    pub(crate) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        ProcFile::new(Self, parent, mkmod!(a+r))
    }
}

impl ProcFileOps for BootIdFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut printer = VmPrinter::new_skip(writer, offset);
        writeln!(printer, "{}", boot_id())?;
        Ok(printer.bytes_written())
    }
}

/// Represents the inode at `/proc/sys/kernel/random/uuid`.
struct UuidFileOps;

impl UuidFileOps {
    pub(crate) fn new_inode(parent: Weak<dyn Inode>) -> Arc<dyn Inode> {
        ProcFile::new(Self, parent, mkmod!(a+r))
    }
}

impl ProcFileOps for UuidFileOps {
    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        let mut bytes = [0u8; 16];
        getrandom(&mut bytes);
        let mut printer = VmPrinter::new_skip(writer, offset);
        writeln!(printer, "{}", format_uuid(&mut bytes))?;
        Ok(printer.bytes_written())
    }
}
