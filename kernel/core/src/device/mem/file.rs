// SPDX-License-Identifier: MPL-2.0

use crate::{
    events::IoEvents,
    fs::{
        file::{PerOpenFileOps, StatusFlags},
        vfs::inode::FileOps,
    },
    prelude::*,
    process::signal::{PollHandle, Pollable},
    util::random,
};

pub(crate) fn geturandom(writer: &mut VmWriter) -> Result<usize> {
    const IO_CAPABILITY: usize = 4096;

    if !writer.has_avail() {
        return Ok(0);
    }

    let mut buffer = vec![0; writer.avail().min(IO_CAPABILITY)];
    let mut written_bytes = 0;

    while writer.has_avail() {
        random::getrandom(&mut buffer[..writer.avail().min(IO_CAPABILITY)]);
        match writer.write_fallible(&mut VmReader::from(buffer.as_slice())) {
            Ok(len) => written_bytes += len,
            Err((err, 0)) if written_bytes == 0 => return Err(err.into()),
            Err((_, len)) => return Ok(written_bytes + len),
        }
    }

    Ok(written_bytes)
}

// TODO: Support true randomness by collecting environment noise.
pub(crate) use geturandom as getrandom;

#[expect(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(in crate::device) enum MemFile {
    Mem,
    Kmem,
    Null,
    Port,
    Zero,
    Core,
    Full,
    Random,
    Urandom,
    Aio,
    Kmsg,
    Oldmem,
}

impl MemFile {
    pub(super) fn minor(&self) -> u32 {
        match self {
            MemFile::Mem => 1,
            MemFile::Kmem => 2,
            MemFile::Null => 3,
            MemFile::Port => 4,
            MemFile::Zero => 5,
            MemFile::Core => 6,
            MemFile::Full => 7,
            MemFile::Random => 8,
            MemFile::Urandom => 9,
            MemFile::Aio => 10,
            MemFile::Kmsg => 11,
            MemFile::Oldmem => 12,
        }
    }

    pub(super) fn name(&self) -> &'static str {
        match self {
            MemFile::Mem => "mem",
            MemFile::Kmem => "kmem",
            MemFile::Null => "null",
            MemFile::Port => "port",
            MemFile::Zero => "zero",
            MemFile::Core => "core",
            MemFile::Full => "full",
            MemFile::Random => "random",
            MemFile::Urandom => "urandom",
            MemFile::Aio => "aio",
            MemFile::Kmsg => "kmsg",
            MemFile::Oldmem => "oldmem",
        }
    }
}

impl Pollable for MemFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        // K1 MUSE Book --- (do not advertise IN on /dev/kmsg; journald would spin)
        // let events = IoEvents::IN | IoEvents::OUT;
        let events = match self {
            // Nothing is ever readable from our /dev/kmsg (see `read_at`);
            // advertising IN would make journald spin on it.
            MemFile::Kmsg => IoEvents::OUT,
            _ => IoEvents::IN | IoEvents::OUT,
        };
        // K1 MUSE Book ---
        events & mask
    }
}

// K1 MUSE Book --- (systemd console records without a printk ring)
/// Limits synchronous console output per record.
const KMSG_LINE_MAX: usize = 1024;

/// Linux devkmsg_write rejects oversized records rather than reporting dropped bytes.
fn kmsg_write(reader: &mut VmReader) -> Result<usize> {
    let len = reader.remain();
    if len > KMSG_LINE_MAX {
        return_errno_with_message!(Errno::EINVAL, "the kmsg record is too large");
    }
    if len == 0 {
        return Ok(0);
    }
    let mut buf = vec![0u8; len];
    reader.read_fallible(&mut VmWriter::from(&mut buf[..]))?;
    let text = String::from_utf8_lossy(&buf);
    ostd::early_println!(
        "[kmsg] {}",
        kmsg_payload(&text).trim_end_matches(['\n', '\r'])
    );
    Ok(len)
}

/// Removes only a valid decimal syslog priority prefix; malformed text is retained.
fn kmsg_payload(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix('<')
        && let Some((priority, payload)) = rest.split_once('>')
        && !priority.is_empty()
        && priority.bytes().all(|byte| byte.is_ascii_digit())
        && priority.parse::<u32>().is_ok()
    {
        payload
    } else {
        text
    }
}

#[cfg(ktest)]
mod kmsg_tests {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn oversized_kmsg_records_are_rejected_without_consuming_input() {
        let bytes = vec![b'x'; KMSG_LINE_MAX + 1];
        let mut reader = VmReader::from(bytes.as_slice()).to_fallible();
        assert_eq!(kmsg_write(&mut reader).unwrap_err().error(), Errno::EINVAL);
        assert_eq!(reader.remain(), bytes.len());
        assert_eq!(
            kmsg_write(&mut VmReader::from(&[][..]).to_fallible()).unwrap(),
            0
        );
    }

    #[ktest]
    fn only_decimal_syslog_priorities_are_removed() {
        assert_eq!(kmsg_payload("<14>record"), "record");
        assert_eq!(kmsg_payload("<4294967295>record"), "record");
        for text in [
            "record",
            "<bad>record",
            "<>record",
            "<14,tag>record",
            "<4294967296>record",
            "<14record",
            "<+14>record",
        ] {
            assert_eq!(kmsg_payload(text), text);
        }
    }
}
// K1 MUSE Book ---

impl FileOps for MemFile {
    fn read_at(
        &self,
        _offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        match self {
            MemFile::Full | MemFile::Zero => {
                let len = writer.avail();
                writer.fill_zeros(len)?;
                Ok(len)
            }
            MemFile::Null => Ok(0),
            MemFile::Random => getrandom(writer),
            MemFile::Urandom => geturandom(writer),
            // K1 MUSE Book --- (no printk ring; EAGAIN so journald/dmesg -w do not spin)
            MemFile::Kmsg => return_errno_with_message!(
                Errno::EAGAIN,
                "/dev/kmsg has no records to read (no printk buffer)"
            ),
            // K1 MUSE Book ---
            _ => return_errno_with_message!(Errno::EINVAL, "read is not supported yet"),
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        match self {
            MemFile::Null | MemFile::Random | MemFile::Urandom | MemFile::Zero => {
                let len = reader.remain();
                reader.skip(len);
                Ok(len)
            }
            MemFile::Full => {
                return_errno_with_message!(Errno::ENOSPC, "no space left on /dev/full")
            }
            // K1 MUSE Book --- (journald / StandardOutput=kmsg)
            MemFile::Kmsg => kmsg_write(reader),
            // K1 MUSE Book ---
            _ => return_errno_with_message!(Errno::EINVAL, "write is not supported yet"),
        }
    }
}

impl PerOpenFileOps for MemFile {
    fn check_seekable(&self) -> Result<()> {
        Ok(())
    }

    fn is_offset_aware(&self) -> bool {
        false
    }
}
