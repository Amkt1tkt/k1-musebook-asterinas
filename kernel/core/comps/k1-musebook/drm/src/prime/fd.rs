// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Handing kernel objects to user space as file descriptors.
//!
//! # Why this is a trait
//!
//! Five ioctls — both PRIME directions, both sync-object handle/fd ones, and
//! `SYNCOBJ_EVENTFD` — reach a process file descriptor. A descriptor is a
//! process-wide, inheritable, `SCM_RIGHTS`-passable thing, and looking one up
//! means reaching the calling process's descriptor table, which a comp crate
//! cannot see. So this crate decides *what* is being shared and the
//! character-device shell decides *how*, the same split as [`UserMem`] for the
//! address space.
//!
//! The shell is expected to back each variant with a file object whose job
//! is to hold the [`DrmExport`] alive and hand it back on
//! [`resolve`](Descriptors::resolve). A dma-buf must also answer
//! `lseek(fd, 0, SEEK_END)` with its size: Mesa's PowerVR winsys uses that,
//! not an ioctl, to decide whether an import is large enough. The shell also
//! supplies dma-buf mappings and fence-based polling. Sync-object and
//! sync-file descriptors share identity through this interface; the
//! character-device shell implements their polling behavior.
//!
//! [`UserMem`]: crate::ioctl::UserMem

use alloc::sync::Arc;

use super::DmaBuf;
use crate::{
    error::DrmResult,
    fence::{Fence, SyncObj},
};

/// A kernel object reachable through a file descriptor.
#[derive(Clone, Debug)]
pub enum DrmExport {
    /// A shared buffer, from `PRIME_HANDLE_TO_FD`.
    Buffer(Arc<DmaBuf>),
    /// A sync object, from `SYNCOBJ_HANDLE_TO_FD`.
    ///
    /// The descriptor refers to the *object*, so a signal through one
    /// descriptor is visible through every handle importing it. That is what
    /// makes a sync object useful across processes at all.
    Sync(Arc<SyncObj>),
    /// A single fence, from `SYNCOBJ_HANDLE_TO_FD` with
    /// `DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE`.
    ///
    /// Linux's `sync_file`, a snapshot rather than a container: importing it
    /// into a sync object installs the fence it held at export time, and
    /// later signals of the exporting object do not follow.
    SyncFile(Arc<dyn Fence>),
}

/// How the exported descriptor behaves.
///
/// `PRIME_HANDLE_TO_FD` is the only caller that lets user space choose; the
/// sync-object ioctls always ask for a close-on-exec, read-write descriptor,
/// as upstream's `drm_syncobj_create_fd` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportFlags {
    /// Close the descriptor across `execve`.
    pub cloexec: bool,
    /// Open the descriptor for writing as well as reading.
    pub read_write: bool,
}

impl ExportFlags {
    /// The flags every sync-object export uses.
    pub const SYNC: Self = Self {
        cloexec: true,
        read_write: true,
    };
}

/// Increments an eventfd by one, the way Linux `eventfd_signal(ctx, 1)` does.
///
/// # Why a trait
///
/// `k1-musebook-drm` cannot name the kernel crate's `EventFile`: this crate must
/// compile without knowing that eventfd exists, the same reason
/// [`Descriptors`] cannot install a descriptor itself. The character-device
/// shell supplies the implementation through [`Descriptors::eventfd`]. Fence
/// callbacks may run in bottom-half context (see the fence contract), so
/// [`signal`](Self::signal) must not sleep.
pub trait Eventfd: Send + Sync {
    /// Increments the counter by one and wakes pollers.
    fn signal(&self);
}

/// The calling process's file descriptor table, as much of it as DRM needs.
///
/// Supplied by the character-device shell through
/// [`CallerContext::with_descriptors`](crate::ioctl::CallerContext::with_descriptors).
/// A caller that cannot supply one makes the descriptor ioctls report
/// [`DrmError::NotSupported`](crate::error::DrmError::NotSupported), which is
/// what an in-kernel client without a process context should see.
pub trait Descriptors {
    /// Installs `export` into a new descriptor and returns its number.
    fn install(&self, export: DrmExport, flags: ExportFlags) -> DrmResult<i32>;

    /// Returns the object behind a descriptor.
    ///
    /// # Errors
    ///
    /// [`DrmError::InvalidArgument`] (`EINVAL`) for a descriptor that is not
    /// open or does not refer to one of these objects, which is what
    /// `dma_buf_get` reports for a descriptor that is not a dma-buf.
    ///
    /// [`DrmError::InvalidArgument`]: crate::error::DrmError::InvalidArgument
    fn resolve(&self, fd: i32) -> DrmResult<DrmExport>;

    /// Returns a handle that will increment the eventfd at `fd` by one.
    ///
    /// Lives on this trait rather than as a third optional on
    /// [`CallerContext`](crate::ioctl::CallerContext) because looking up a
    /// process fd is exactly the job this trait already has. `SYNCOBJ_EVENTFD`
    /// is the fifth ioctl that needs the table, not a new kind of question.
    ///
    /// # Errors
    ///
    /// [`DrmError::BadFd`] (`EBADF`) if `fd` is not open, which is what
    /// `eventfd_ctx_fdget` reports for a closed number.
    ///
    /// [`DrmError::InvalidArgument`] (`EINVAL`) if `fd` is open but is not an
    /// eventfd, which is what `eventfd_ctx_fileget` reports for a non-eventfd
    /// `f_op`.
    ///
    /// [`DrmError::BadFd`]: crate::error::DrmError::BadFd
    /// [`DrmError::InvalidArgument`]: crate::error::DrmError::InvalidArgument
    fn eventfd(&self, fd: i32) -> DrmResult<Arc<dyn Eventfd>>;
}
