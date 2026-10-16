// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The ioctl dispatch table and the user-memory abstraction.
//!
//!
//! # Why handlers take a byte slice
//!
//! The character-device shell in `kernel/core/src/device/dri.rs` copies the caller's
//! argument structure in before dispatch and back out afterwards, so handlers
//! receive it as `&mut [u8]` and interpret it with [`Pod`]. That keeps the size
//! and direction handling in one place, and keeps this crate away from user
//! pointers entirely.
//!
//! [`Pod`]: ostd_pod::Pod

use alloc::vec::Vec;

use bitflags::bitflags;
use ostd_pod::{IntoBytes, Pod};

use crate::{
    device::{DrmDevice, DrmMinor},
    error::{DrmError, DrmResult},
    fence::{self, Blocker},
    file::{CallerIdentity, DrmFile, Privilege},
    gem,
    prime::{self, Descriptors},
    uapi::ioctl_nr,
};

pub mod handlers;
#[cfg(ktest)]
mod test;

bitflags! {
    /// Per-ioctl permission requirements, mirroring Linux's `DRM_*` ioctl flags.
    ///
    /// `BitOr` cannot be used in the `const` context where dispatch tables live;
    /// combine flags there with [`union`](Self::union) instead.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/drm/drm_ioctl.h#L110>.
    pub struct IoctlFlags: u32 {
        /// The caller must be authenticated.
        const AUTH = 1 << 0;
        /// The caller must be the device's master.
        const MASTER = 1 << 1;
        /// The caller must be root.
        const ROOT_ONLY = 1 << 2;
        /// The ioctl is permitted on a render node.
        ///
        /// Without this bit, an ioctl reaching a render node is rejected. This is
        /// the whole point of the render/primary split, so err on the side of
        /// leaving it off.
        const RENDER_ALLOW = 1 << 3;
    }
}

/// The signature every ioctl handler shares.
pub type IoctlHandler = fn(&DrmDevice, &DrmFile, &mut [u8], &CallerContext) -> DrmResult<()>;

/// One entry in a dispatch table.
#[derive(Clone, Copy)]
pub struct IoctlDesc {
    /// The command number, i.e. the low byte of the encoded request.
    pub nr: u32,
    /// The Linux name, used in diagnostics. Keep it identical to the `DRM_IOCTL_`
    /// symbol minus its prefix so that logs can be grepped against Linux sources.
    pub name: &'static str,
    /// Permission requirements.
    pub flags: IoctlFlags,
    /// The handler.
    pub handler: IoctlHandler,
}

impl core::fmt::Debug for IoctlDesc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IoctlDesc")
            .field("nr", &self.nr)
            .field("name", &self.name)
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

impl IoctlDesc {
    /// Checks whether this ioctl may be issued through the given node.
    pub fn check_minor(&self, minor: DrmMinor) -> DrmResult<()> {
        match minor {
            DrmMinor::Primary => Ok(()),
            DrmMinor::Render if self.flags.contains(IoctlFlags::RENDER_ALLOW) => Ok(()),
            DrmMinor::Render => Err(DrmError::PermissionDenied),
        }
    }

    /// Checks whether `file` is allowed to issue this ioctl.
    ///
    /// The four tests are applied in Linux's order and every failure is
    /// `EACCES`, because that is the value user space branches on.
    ///
    /// A render client satisfies [`IoctlFlags::AUTH`] unconditionally. Render
    /// nodes exist precisely so that an unprivileged client can submit work
    /// without obtaining a magic token from whoever owns the display, so
    /// demanding authentication there would defeat their purpose.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L530>
    /// (`drm_ioctl_permit`).
    pub fn check_permission(&self, file: &DrmFile, caller: &CallerContext) -> DrmResult<()> {
        let is_render_client = file.minor() == DrmMinor::Render;

        if self.flags.contains(IoctlFlags::ROOT_ONLY) && !caller.is_privileged() {
            return Err(DrmError::PermissionDenied);
        }

        if self.flags.contains(IoctlFlags::AUTH) && !is_render_client && !file.is_authenticated() {
            return Err(DrmError::PermissionDenied);
        }

        if self.flags.contains(IoctlFlags::MASTER) && !file.is_master() {
            return Err(DrmError::PermissionDenied);
        }

        self.check_minor(file.minor())
    }
}

/// Access to the calling process's address space.
///
/// Implemented by the kernel crate, which is the only place that can reach
/// `current_userspace!()`. The variable-length ioctls need it because they write
/// directly into user-provided arrays rather than into a fixed argument struct.
pub trait UserMem {
    /// Reads `buf.len()` bytes from user address `addr`.
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> DrmResult<()>;

    /// Writes `buf` to user address `addr`.
    fn write_bytes(&self, addr: u64, buf: &[u8]) -> DrmResult<()>;
}

/// Kernel services and current credentials for one ioctl invocation.
///
/// The character-device shell supplies address-space access, caller identity,
/// and current capabilities. Descriptor and blocking services are optional for
/// in-kernel callers; operations requiring an absent service return
/// `EOPNOTSUPP`. A memory-only context has no privileged authority.
pub struct CallerContext<'a> {
    memory: &'a dyn UserMem,
    identity: Option<CallerIdentity>,
    privilege: Privilege,
    descriptors: Option<&'a dyn Descriptors>,
    blocker: Option<&'a dyn Blocker>,
}

impl<'a> CallerContext<'a> {
    /// A context that can copy memory and nothing else.
    ///
    /// What an in-kernel caller has. The five ioctls that touch a process
    /// descriptor, and the two that block, answer `EOPNOTSUPP` on one of these.
    pub fn new(memory: &'a dyn UserMem) -> Self {
        Self {
            memory,
            identity: None,
            privilege: Privilege::Unprivileged,
            descriptors: None,
            blocker: None,
        }
    }

    /// Adds the current caller's identity and capability verdict.
    /// The character-device shell supplies these anew for each ioctl; an opened
    /// descriptor does not preserve the opener's current privilege.
    pub fn with_credentials(mut self, identity: CallerIdentity, privilege: Privilege) -> Self {
        self.identity = Some(identity);
        self.privilege = privilege;
        self
    }

    /// Whether the current caller holds `CAP_SYS_ADMIN`.
    pub fn is_privileged(&self) -> bool {
        self.privilege == Privilege::Privileged
    }

    pub(crate) fn may_manage_master(&self, file: &DrmFile) -> bool {
        self.is_privileged()
            || (file.was_master()
                && self
                    .identity
                    .as_ref()
                    .is_some_and(|identity| file.opened_by(identity)))
    }

    /// Adds the calling process's descriptor table.
    pub fn with_descriptors(mut self, descriptors: &'a dyn Descriptors) -> Self {
        self.descriptors = Some(descriptors);
        self
    }

    /// Adds a way to sleep until a fence signals.
    pub fn with_blocker(mut self, blocker: &'a dyn Blocker) -> Self {
        self.blocker = Some(blocker);
        self
    }

    /// Reads `buf.len()` bytes from user address `addr`.
    pub fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> DrmResult<()> {
        self.memory.read_bytes(addr, buf)
    }

    /// Writes `buf` to user address `addr`.
    pub fn write_bytes(&self, addr: u64, buf: &[u8]) -> DrmResult<()> {
        self.memory.write_bytes(addr, buf)
    }

    /// Returns the calling process's descriptor table, if it has one.
    ///
    /// The five ioctls that touch a process descriptor — both PRIME
    /// directions, both sync-object handle/fd ones, and `SYNCOBJ_EVENTFD` —
    /// report `EOPNOTSUPP` without it, which is the right answer for a caller
    /// with no process behind it.
    pub fn descriptors(&self) -> Option<&dyn Descriptors> {
        self.descriptors
    }

    /// Returns a way to put the calling thread to sleep, if there is one.
    ///
    /// Timers and signal delivery live in the kernel crate, so a blocking fence
    /// wait has to be delegated; see [`Blocker`]. Without one, `SYNCOBJ_WAIT`
    /// and its timeline sibling degrade to a poll and report `ETIME` rather
    /// than spinning.
    pub fn blocker(&self) -> Option<&dyn Blocker> {
        self.blocker
    }
}

/// Reads a POD value from user address `addr`.
pub fn read_val<T: Pod>(caller: &CallerContext, addr: u64) -> DrmResult<T> {
    let mut bytes = alloc::vec![0u8; size_of::<T>()];
    caller.read_bytes(addr, &mut bytes)?;
    Ok(T::from_bytes(&bytes))
}

/// Writes a POD value to user address `addr`.
pub fn write_val<T: Pod>(caller: &CallerContext, addr: u64, value: &T) -> DrmResult<()> {
    caller.write_bytes(addr, value.as_bytes())
}

/// The largest user array a single ioctl may ask us to read in one go.
///
/// Divergence: Linux bounds each array against the object count it is about to
/// describe (`dev->mode_config.num_crtc` and its siblings). Not all of those
/// counts are visible at this layer, so a blanket cap keeps a bogus `count_*`
/// from becoming an unbounded kernel allocation. Real requests are orders of
/// magnitude below it.
const MAX_ARRAY_BYTES: usize = 1 << 20;

/// Copies a slice into a user array, honouring the two-pass convention.
///
/// `capacity` is what the caller said it could accept. Returns how many elements
/// were actually written, which the caller must store back into the `count_*`
/// field. When `capacity` is zero nothing is written, which is how user space asks
/// "how many are there?" before allocating.
///
/// Getting this wrong is the classic cause of `modetest` reporting zero
/// connectors, so it lives here rather than being open-coded per ioctl.
pub fn copy_out_array<T: Pod>(
    caller: &CallerContext,
    ptr: u64,
    capacity: u32,
    items: &[T],
) -> DrmResult<u32> {
    let count = (capacity as usize).min(items.len());
    if count == 0 {
        return Ok(0);
    }
    if ptr == 0 {
        return Err(DrmError::BadAddress);
    }

    caller.write_bytes(ptr, items[..count].as_bytes())?;
    Ok(count as u32)
}

/// Reads a user array of at most `count` elements.
pub fn copy_in_array<T: Pod>(caller: &CallerContext, ptr: u64, count: u32) -> DrmResult<Vec<T>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr == 0 {
        return Err(DrmError::BadAddress);
    }

    let bytes = (count as usize)
        .checked_mul(size_of::<T>())
        .filter(|bytes| *bytes <= MAX_ARRAY_BYTES)
        .ok_or(DrmError::InvalidArgument)?;

    let mut buffer = alloc::vec![0u8; bytes];
    caller.read_bytes(ptr, &mut buffer)?;

    Ok(buffer
        .chunks_exact(size_of::<T>())
        .map(T::from_bytes)
        .collect())
}

/// Copies a string into a user buffer, truncating if it does not fit, and returns
/// the length that must be written back into the matching `*_len` field.
///
/// This is the protocol behind `DRM_IOCTL_VERSION`. The reported length is the
/// *full* length of the value even when the copy was truncated or skipped
/// entirely, which is what lets `libdrm` issue the ioctl once with zero-length
/// buffers to learn the sizes, allocate, and issue it again. A zero `capacity`
/// therefore writes nothing at all, and pairing it with a null pointer is legal
/// and must not fail.
///
/// The copy is not NUL-terminated, matching Linux; `libdrm` appends the
/// terminator itself using the returned length.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L472>
/// (`drm_copy_field`).
pub fn copy_out_string(
    caller: &CallerContext,
    ptr: u64,
    capacity: usize,
    value: &str,
) -> DrmResult<usize> {
    let bytes = value.as_bytes();
    let len = bytes.len().min(capacity);

    if len > 0 && ptr != 0 {
        caller.write_bytes(ptr, &bytes[..len])?;
    }

    Ok(bytes.len())
}

/// Copies a string into a user buffer only if it fits whole, and returns its full
/// length.
///
/// `DRM_IOCTL_GET_UNIQUE` uses this all-or-nothing variant rather than the
/// truncating [`copy_out_string`]: callers compare bus IDs for equality, so a
/// silently shortened one would be worse than none at all.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L116>
/// (`drm_getunique`).
pub fn copy_out_string_whole(
    caller: &CallerContext,
    ptr: u64,
    capacity: usize,
    value: &str,
) -> DrmResult<usize> {
    let bytes = value.as_bytes();

    if capacity >= bytes.len() && !bytes.is_empty() {
        if ptr == 0 {
            return Err(DrmError::BadAddress);
        }
        caller.write_bytes(ptr, bytes)?;
    }

    Ok(bytes.len())
}

/// Interprets an already-copied-in ioctl payload as `T`.
///
/// A payload shorter than `T` means the character-device shell and this table
/// disagree about the ioctl's argument type, which is a kernel bug rather than a
/// user error. It is still reported as `EINVAL` rather than panicking.
pub fn payload_read<T: Pod>(payload: &[u8]) -> DrmResult<T> {
    if payload.len() < size_of::<T>() {
        return Err(DrmError::InvalidArgument);
    }
    Ok(T::from_first_bytes(payload))
}

/// Stores `value` into an ioctl payload for the shell to copy back out.
pub fn payload_write<T: Pod>(payload: &mut [u8], value: &T) -> DrmResult<()> {
    let bytes = value.as_bytes();
    if payload.len() < bytes.len() {
        return Err(DrmError::InvalidArgument);
    }
    payload[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

/// The core dispatch table, covering the ioctls that are not driver-private.
///
/// Covers device, GEM, mode-setting, PRIME, and sync-object operations.
/// Each handler documents its Linux counterpart, per `../../CONVENTIONS.md`.
///
/// Entries must be unique and sorted by [`IoctlDesc::nr`] for [`lookup`]'s
/// binary search. The `core_ioctls_are_sorted` test enforces this.
///
/// An unlisted command number is reported as `EINVAL`, which is what Linux does
/// for the table slots it fills with `drm_invalid_op`.
pub const CORE_IOCTLS: &[IoctlDesc] = &[
    IoctlDesc {
        nr: ioctl_nr::nr::VERSION,
        name: "VERSION",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: handlers::version,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::GET_UNIQUE,
        name: "GET_UNIQUE",
        flags: IoctlFlags::empty(),
        handler: handlers::get_unique,
    },
    // `DRM_MASTER` upstream, which is not ceremony: its side effect is to fill
    // in the bus ID a subsequent `GET_UNIQUE` reports, and that is per-file
    // state a non-master has no business changing.
    IoctlDesc {
        nr: ioctl_nr::nr::SET_VERSION,
        name: "SET_VERSION",
        flags: IoctlFlags::MASTER,
        handler: handlers::set_version,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::GEM_CLOSE,
        name: "GEM_CLOSE",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: gem::ioctl::gem_close,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::GEM_FLINK,
        name: "GEM_FLINK",
        flags: IoctlFlags::AUTH,
        handler: gem::ioctl::gem_flink,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::GEM_OPEN,
        name: "GEM_OPEN",
        flags: IoctlFlags::AUTH,
        handler: gem::ioctl::gem_open,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::GET_CAP,
        name: "GET_CAP",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: handlers::get_cap,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SET_CLIENT_CAP,
        name: "SET_CLIENT_CAP",
        flags: IoctlFlags::empty(),
        handler: handlers::set_client_cap,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SET_MASTER,
        name: "SET_MASTER",
        flags: IoctlFlags::empty(),
        handler: handlers::set_master,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::DROP_MASTER,
        name: "DROP_MASTER",
        flags: IoctlFlags::empty(),
        handler: handlers::drop_master,
    },
    // Both PRIME ioctls are `DRM_RENDER_ALLOW` upstream so render clients
    // can share completed frames without permission to control the display.
    IoctlDesc {
        nr: ioctl_nr::nr::PRIME_HANDLE_TO_FD,
        name: "PRIME_HANDLE_TO_FD",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: prime::ioctl::prime_handle_to_fd,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::PRIME_FD_TO_HANDLE,
        name: "PRIME_FD_TO_HANDLE",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: prime::ioctl::prime_fd_to_handle,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::MODE_CREATE_DUMB,
        name: "MODE_CREATE_DUMB",
        flags: IoctlFlags::empty(),
        handler: gem::ioctl::mode_create_dumb,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::MODE_MAP_DUMB,
        name: "MODE_MAP_DUMB",
        flags: IoctlFlags::empty(),
        handler: gem::ioctl::mode_map_dumb,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::MODE_DESTROY_DUMB,
        name: "MODE_DESTROY_DUMB",
        flags: IoctlFlags::empty(),
        handler: gem::ioctl::mode_destroy_dumb,
    },
    // All twelve sync-object ioctls are `DRM_RENDER_ALLOW`. Each handler
    // additionally checks `DRIVER_SYNCOBJ`, because a
    // driver that does not claim the capability must not answer them.
    // `SYNCOBJ_EVENTFD` further requires `DRIVER_SYNCOBJ_TIMELINE`, matching
    // linux-6.6 `drm_syncobj_eventfd_ioctl`.
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_CREATE,
        name: "SYNCOBJ_CREATE",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_create,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_DESTROY,
        name: "SYNCOBJ_DESTROY",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_destroy,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_HANDLE_TO_FD,
        name: "SYNCOBJ_HANDLE_TO_FD",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_handle_to_fd,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_FD_TO_HANDLE,
        name: "SYNCOBJ_FD_TO_HANDLE",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_fd_to_handle,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_WAIT,
        name: "SYNCOBJ_WAIT",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_wait,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_RESET,
        name: "SYNCOBJ_RESET",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_reset,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_SIGNAL,
        name: "SYNCOBJ_SIGNAL",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_signal,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_TIMELINE_WAIT,
        name: "SYNCOBJ_TIMELINE_WAIT",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_timeline_wait,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_QUERY,
        name: "SYNCOBJ_QUERY",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_query,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_TRANSFER,
        name: "SYNCOBJ_TRANSFER",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_transfer,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_TIMELINE_SIGNAL,
        name: "SYNCOBJ_TIMELINE_SIGNAL",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_timeline_signal,
    },
    IoctlDesc {
        nr: ioctl_nr::nr::SYNCOBJ_EVENTFD,
        name: "SYNCOBJ_EVENTFD",
        flags: IoctlFlags::RENDER_ALLOW,
        handler: fence::ioctl::syncobj_eventfd,
    },
];

/// Looks up an ioctl by command number, searching driver-private entries first.
///
/// The mode-setting ioctls live in their own table ([`kms::KMS_IOCTLS`]) rather
/// than in [`CORE_IOCTLS`] to keep display-specific dispatch with KMS.
/// The two tables are disjoint and each sorted, which the
/// `kms_ioctls_are_sorted_and_disjoint` test enforces.
///
/// [`kms::KMS_IOCTLS`]: crate::kms::KMS_IOCTLS
pub fn lookup(device: &DrmDevice, nr: u32) -> Option<&IoctlDesc> {
    if ioctl_nr::is_driver_private(nr) {
        // A driver's table is short and its order is the driver's business, so
        // this side is a linear scan.
        return device
            .driver()
            .private_ioctls()
            .iter()
            .find(|desc| desc.nr == nr);
    }

    if let Ok(index) = CORE_IOCTLS.binary_search_by_key(&nr, |desc| desc.nr) {
        return Some(&CORE_IOCTLS[index]);
    }

    let table = crate::kms::KMS_IOCTLS;
    let index = table.binary_search_by_key(&nr, |desc| desc.nr).ok()?;
    Some(&table[index])
}
