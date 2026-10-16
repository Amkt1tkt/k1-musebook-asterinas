// SPDX-License-Identifier: MIT OR GPL-2.0-only
// K1 MUSE Book: PowerVR errors mapped once to the DRM ABI.

//! Error type for the PowerVR driver.
//!
//! Unlike the DDK, the open driver has no `PVRSRV_ERROR` enumeration to
//! translate: upstream returns plain negative errnos. So this is a small
//! enumeration whose only job is to name the failure modes precisely enough to
//! debug them, while mapping onto the errno that upstream returns at the same
//! point.

use k1_musebook_drm::DrmError;

/// A driver-internal error.
///
/// Variants document Linux-compatible errno mappings. Deliberate differences
/// are recorded in ABI.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PvrError {
    /// A caller passed an out-of-range value or an unknown flag. `-EINVAL`.
    InvalidArgument,
    /// A user-space pointer could not be read or written. `-EFAULT`.
    ///
    /// Distinct from [`Self::InvalidArgument`] because user space branches on it:
    /// upstream's `pvr_get_uobj` / `pvr_set_uobj` return `-EINVAL` for a stride
    /// below the mandatory minimum and `-EFAULT` for a pointer it could not
    /// follow, and those are two different bugs in the caller.
    BadAddress,
    /// A caller's buffer is too small to hold the answer. `-E2BIG`.
    ///
    /// Returned by the `DEV_QUERY` sub-queries that fill a user array when the
    /// caller's `count` is short. The two-pass protocol — query with a null
    /// pointer, allocate, query again — is how user space avoids it.
    TooBig,
    /// A handle did not resolve to a live object. `-EINVAL`.
    ///
    /// This is the errno upstream's `xa_load()` / `xa_erase()` misses produce,
    /// which covers every handle the driver allocates itself: VM contexts,
    /// rendering contexts, free lists and HWRT datasets. Buffer objects are the
    /// exception — they live in the DRM core's table and a miss there is
    /// [`Self::NotFound`].
    NoSuchHandle,
    /// The thing named does not exist. `-ENOENT`.
    ///
    /// Two call sites, both matching upstream: `pvr_gem_object_from_handle()`
    /// failing to resolve a buffer object, and `pvr_vm_unmap()` finding no
    /// mapping at the address and length it was given. User space branches on
    /// the difference from [`Self::NoSuchHandle`] — an unknown VM context and an
    /// unknown buffer object are different mistakes.
    NotFound,
    /// Memory, GPU address space, firmware slots or a non-reusable ID namespace is exhausted. `-ENOMEM`.
    NoMemory,
    /// The firmware image is missing, truncated, or fails validation.
    ///
    /// Includes the `info_version != 3` and missing-`PVR_FW_FLAGS_OPEN_SOURCE`
    /// rejections. `-EINVAL`.
    BadFirmware,
    /// The GPU stopped responding and the device has been marked lost.
    ///
    /// Once set this is sticky: every subsequent ioctl fails with `-EIO` until reboot. Mirrors upstream's `pvr_device::lost`.
    DeviceLost,
    /// A firmware round trip did not complete in time. `-ETIMEDOUT`.
    Timeout,
    /// The hardware or the loaded firmware lacks a feature this path needs.
    ///
    /// This is the expected outcome for the parts of the driver that BXE-2-32
    /// does not implement, and must be reported rather than worked around.
    /// `-ENODEV`.
    Unsupported,
    /// A resource is busy and the caller asked not to block. `-EBUSY`.
    Busy,
    /// The operation was interrupted by a signal. `-EINTR`.
    Interrupted,
}

/// A driver-internal result.
pub type PvrResult<T> = Result<T, PvrError>;

impl From<PvrError> for DrmError {
    fn from(err: PvrError) -> Self {
        match err {
            PvrError::InvalidArgument | PvrError::NoSuchHandle | PvrError::BadFirmware => {
                DrmError::InvalidArgument
            }
            PvrError::NotFound => DrmError::NotFound,
            PvrError::BadAddress => DrmError::BadAddress,
            // Upstream `pvr_dev_query_quirks_get` /
            // `pvr_dev_query_enhancements_get` return `-E2BIG` when the caller's
            // `count` is short of the array they would write.
            PvrError::TooBig => DrmError::TooBig,
            PvrError::NoMemory => DrmError::OutOfMemory,
            PvrError::DeviceLost => DrmError::Io,
            PvrError::Timeout => DrmError::TimedOut,
            PvrError::Unsupported => DrmError::NoDevice,
            PvrError::Busy => DrmError::Busy,
            PvrError::Interrupted => DrmError::Interrupted,
        }
    }
}
