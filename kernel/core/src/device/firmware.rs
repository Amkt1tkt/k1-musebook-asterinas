// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: request firmware blobs from /lib/firmware.

//! Reading firmware images out of the root filesystem.
//!
//! Linux's equivalent is `request_firmware()`. A device driver that needs a blob
//! from `/lib/firmware` cannot read it itself: drivers live in component crates,
//! which have no file system, and the file system does not exist yet when
//! components initialize. So the driver names what it wants and the kernel
//! fetches it, once there is a root to fetch it from.
//!
//! This is deliberately smaller than Linux's: no asynchronous variant, no
//! caching, no built-in blobs, no fallback to a user-space helper. There is one
//! caller and one moment at which it runs, and a firmware framework with no
//! second consumer is unnecessary here.

use crate::{
    fs::{
        file::InodeType,
        vfs::path::{FsPath, PathResolver},
    },
    prelude::*,
};

/// Where firmware lives, matching Linux's default search path.
///
/// Distributions install into this directory and drivers name blobs relative to
/// it, so the two agree without either being configurable.
const FIRMWARE_DIR: &str = "/lib/firmware";

/// The largest image this will read.
///
/// A guard against a corrupt or hostile initramfs handing back something that
/// exhausts memory during boot. The packaged Rogue image is 124 KiB; this
/// bound admits larger firmware while limiting a single boot-time allocation.
const MAX_IMAGE_LEN: usize = 64 * 1024 * 1024;

/// Reads `<name>` from `/lib/firmware` into memory.
///
/// `name` is relative to [`FIRMWARE_DIR`] and may contain directories, as
/// PowerVR's `powervr/rogue_36.29.52.182_v1.fw` does.
///
/// # Errors
///
/// `ENOENT` if the file is absent, which is the ordinary case on a system whose
/// rootfs simply does not ship that blob, and which callers are expected to
/// treat as "this device stays unavailable" rather than as a boot failure.
pub(super) fn request(path_resolver: &PathResolver, name: &str) -> Result<Vec<u8>> {
    let path = path_resolver.lookup(&FsPath::try_from(
        alloc::format!("{}/{}", FIRMWARE_DIR, name).as_str(),
    )?)?;
    let inode = path.inode();

    if inode.type_() != InodeType::File {
        return_errno_with_message!(Errno::EINVAL, "the firmware path is not a regular file");
    }

    let len = inode.size();
    if len > MAX_IMAGE_LEN {
        return_errno_with_message!(Errno::EFBIG, "the firmware image is implausibly large");
    }

    let mut image = alloc::vec![0u8; len];
    let read = inode.read_bytes_at(0, &mut image)?;
    if read != len {
        // Reject truncation at the filesystem boundary, before firmware parsing.
        return_errno_with_message!(Errno::EIO, "the firmware image was read short");
    }

    Ok(image)
}
