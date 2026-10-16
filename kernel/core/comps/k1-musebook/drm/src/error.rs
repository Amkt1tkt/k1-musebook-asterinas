// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Errors reported across the DRM ioctl boundary.
//!
//! This crate deliberately does not use the kernel crate's `Errno`: comp crates
//! cannot see it, and keeping the dependency out means this crate stays
//! unit-testable in isolation. The kernel crate performs the single conversion,
//! in `kernel/core/src/device/dri.rs`, using [`DrmError::to_errno`].
//!
//! The variants are named after the errno that user space will observe, because
//! the DRM interface is defined in terms of those values and libdrm callers
//! branch on them. Choosing a variant is therefore an ABI decision, not a
//! stylistic one: consult Linux's behaviour for the ioctl in question rather
//! than picking whatever seems descriptive.

/// A DRM operation result.
pub type DrmResult<T> = Result<T, DrmError>;

/// An error from a DRM operation, carrying the errno user space must observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrmError {
    /// `EINVAL`: malformed request, or an unsupported ioctl/capability.
    ///
    /// This is also the correct answer for the interfaces we deliberately do not
    /// implement (DRI1, leases), because that is what a modern Linux driver
    /// returns for them.
    InvalidArgument,
    /// `ENOENT`: the referenced object ID or handle does not exist.
    NotFound,
    /// `ENOMEM`: allocation failed.
    OutOfMemory,
    /// `EACCES`: the caller lacks the required privilege.
    ///
    /// Used for master-only operations attempted on a render node, and for
    /// `DRM_AUTH` ioctls from an unauthenticated client.
    PermissionDenied,
    /// `EBUSY`: the object is in use, or a page flip is already pending.
    Busy,
    /// `ENOSPC`: an ID space or a fixed-size table is exhausted.
    NoSpace,
    /// `E2BIG`: the caller's buffer is too small to hold the answer.
    ///
    /// Distinct from [`Self::NoSpace`]: that is "the kernel's table is full",
    /// this is "your buffer was not big enough". Drivers that fill a
    /// caller-sized array return this so a two-pass protocol — query with a
    /// null pointer, allocate, query again — can resize and retry.
    TooBig,
    /// `EFAULT`: a user-space pointer could not be accessed.
    BadAddress,
    /// `EBADF`: a file descriptor is closed, or is not the kind this ioctl
    /// asked for.
    ///
    /// `SYNCOBJ_EVENTFD` reports this when `eventfd_ctx_fdget` would: the
    /// number is not an open descriptor. An open descriptor that is not an
    /// eventfd is [`InvalidArgument`] instead, which is what
    /// `eventfd_ctx_fileget` returns for a non-eventfd `f_op`.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/fs/eventfd.c>
    /// (`eventfd_ctx_fdget` / `eventfd_ctx_fileget`).
    BadFd,
    /// `ENODEV`: the device is gone or was never brought up.
    NoDevice,
    /// `EIO`: the hardware failed.
    ///
    /// Drivers use this for a wedged device: once a GPU has been declared lost,
    /// every subsequent ioctl on it fails this way until it is reset.
    Io,
    /// `EOPNOTSUPP`: the driver does not implement this operation.
    ///
    /// Distinct from [`Self::InvalidArgument`]: this means the request was
    /// well-formed but this particular driver cannot serve it.
    NotSupported,
    /// `ETIMEDOUT`: a wait on the hardware expired.
    ///
    /// For a *fence* wait reaching its deadline see [`Self::TimeExpired`]; the
    /// two are different values and user space branches on the difference.
    TimedOut,
    /// `ETIME`: a fence wait reached its deadline.
    ///
    /// Not [`Self::TimedOut`], however much it reads like it.
    /// `drm_syncobj_array_wait_timeout` reports `-ETIME` (62) and Vulkan's
    /// `vkWaitForFences` turns exactly that value into `VK_TIMEOUT`; anything
    /// else becomes an error, so a client asking "is it done yet?" with a zero
    /// timeout would see a failure instead of "not yet".
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_syncobj.c#L1143>.
    TimeExpired,
    /// `ERESTARTSYS`: the operation was interrupted and should be restarted.
    Interrupted,
    /// `EDEADLK`: atomic modeset locking detected a contention that requires the
    /// caller to back off and retry.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_modeset_lock.c>.
    WouldDeadlock,
}

impl DrmError {
    /// Returns the errno value that user space must observe for this error.
    ///
    /// Values are the riscv64/generic Linux ABI numbers, which Asterinas shares.
    pub fn to_errno(self) -> i32 {
        match self {
            Self::InvalidArgument => 22,  // EINVAL
            Self::NotFound => 2,          // ENOENT
            Self::OutOfMemory => 12,      // ENOMEM
            Self::PermissionDenied => 13, // EACCES
            Self::Busy => 16,             // EBUSY
            Self::NoSpace => 28,          // ENOSPC
            Self::TooBig => 7,            // E2BIG
            Self::BadAddress => 14,       // EFAULT
            Self::BadFd => 9,             // EBADF
            Self::NoDevice => 19,         // ENODEV
            Self::Io => 5,                // EIO
            Self::NotSupported => 95,     // EOPNOTSUPP
            Self::TimedOut => 110,        // ETIMEDOUT
            Self::TimeExpired => 62,      // ETIME
            Self::Interrupted => 512,     // ERESTARTSYS
            Self::WouldDeadlock => 35,    // EDEADLK
        }
    }
}
