// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Sync-object user-space structures.
//!
//! Based on Linux 6.6 `include/uapi/drm/drm.h`, with later deadline fields
//! called out below. Field names and order follow the UAPI; see
//! `../../CONVENTIONS.md` for why that is not negotiable.
//!
//! None of these structures carries implicit padding in C — every one of them
//! either has all its 64-bit members eight-byte aligned already or spells its
//! tail padding out as a `pad` field — so none needs `#[padding_struct]`. The
//! `layouts_match_linux` test in [`crate::fence::test`] pins the sizes down.

/// `struct drm_syncobj_create`, the argument of `SYNCOBJ_CREATE`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L863>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjCreate {
    /// The new sync object's handle, an output.
    pub handle: u32,
    /// `DRM_SYNCOBJ_CREATE_*`; see [`create_flags`].
    pub flags: u32,
}

/// `struct drm_syncobj_destroy`, the argument of `SYNCOBJ_DESTROY`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L869>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjDestroy {
    /// The handle to release.
    pub handle: u32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_syncobj_handle`, shared by both sync-object file-descriptor
/// directions.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L876>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjHandle {
    /// The sync object handle; an output for `FD_TO_HANDLE`, an input otherwise.
    pub handle: u32,
    /// `DRM_SYNCOBJ_*_FLAGS_*_SYNC_FILE`; see [`handle_flags`].
    pub flags: u32,
    /// The file descriptor; an input for `FD_TO_HANDLE`, an output otherwise.
    pub fd: i32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_syncobj_transfer`, the argument of `SYNCOBJ_TRANSFER`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L884>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjTransfer {
    /// The sync object to copy a fence out of.
    pub src_handle: u32,
    /// The sync object to copy it into.
    pub dst_handle: u32,
    /// The timeline point to read, or zero for the binary fence.
    pub src_point: u64,
    /// The timeline point to write, or zero to replace the binary fence.
    pub dst_point: u64,
    /// `DRM_SYNCOBJ_WAIT_FLAGS_*`; only `WAIT_FOR_SUBMIT` is meaningful.
    pub flags: u32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_syncobj_wait`, the argument of `SYNCOBJ_WAIT`.
///
/// Linux 6.8 appended [`deadline_nsec`] to the original 32-byte structure.
/// Preserve the current 40-byte layout required by the userspace headers;
/// the core size assertions below guard against silently reverting it.
///
/// [`deadline_nsec`]: Self::deadline_nsec
///
/// Reference: <https://elixir.bootlin.com/linux/v6.8/source/include/uapi/drm/drm.h#L927>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjWait {
    /// User pointer to a `u32` array of sync-object handles.
    pub handles: u64,
    /// The deadline, as an *absolute* `CLOCK_MONOTONIC` time in nanoseconds.
    pub timeout_nsec: i64,
    /// Number of entries in `handles`.
    pub count_handles: u32,
    /// `DRM_SYNCOBJ_WAIT_FLAGS_*`; see [`wait_flags`].
    pub flags: u32,
    /// Index of the first handle that was signalled; only meaningful when
    /// [`wait_flags::WAIT_ALL`] is clear.
    pub first_signaled: u32,
    /// Padding; must be zero.
    pub pad: u32,
    /// A hint, in absolute `CLOCK_MONOTONIC` nanoseconds, of when the caller
    /// needs the fences by; read only when [`wait_flags::WAIT_DEADLINE`] is
    /// set.
    ///
    /// Advisory even upstream, where it lets a driver raise clocks to meet a
    /// frame deadline. Accepted and ignored here; see [`wait_flags::KNOWN`].
    pub deadline_nsec: u64,
}

/// `struct drm_syncobj_timeline_wait`, the argument of `SYNCOBJ_TIMELINE_WAIT`.
///
/// 48 bytes, for the reason [`SyncobjWait`] is 40: the same `deadline_nsec`
/// field was appended to both in Linux 6.8.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.8/source/include/uapi/drm/drm.h#L945>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjTimelineWait {
    /// User pointer to a `u32` array of sync-object handles.
    pub handles: u64,
    /// User pointer to a `u64` array of timeline points, one per handle.
    pub points: u64,
    /// The deadline, as an *absolute* `CLOCK_MONOTONIC` time in nanoseconds.
    pub timeout_nsec: i64,
    /// Number of entries in `handles` and in `points`.
    pub count_handles: u32,
    /// `DRM_SYNCOBJ_WAIT_FLAGS_*`; see [`wait_flags`].
    pub flags: u32,
    /// Index of the first handle that was signalled; only meaningful when
    /// [`wait_flags::WAIT_ALL`] is clear.
    pub first_signaled: u32,
    /// Padding; must be zero.
    pub pad: u32,
    /// A hint of when the caller needs the fences by; see
    /// [`SyncobjWait::deadline_nsec`].
    pub deadline_nsec: u64,
}

/// `struct drm_syncobj_eventfd`, the argument of `SYNCOBJ_EVENTFD`.
///
/// 24 bytes: `handle` + `flags` + `point` + `fd` + `pad`. There is no implicit
/// C padding — `point` is already eight-byte aligned — so a size other than 24
/// is a transcription error, not a compiler choice. Asserted in
/// [`crate::uapi`].
///
/// `flags` accepts only [`wait_flags::WAIT_AVAILABLE`] (`1 << 2`, not
/// `WAIT_FOR_SUBMIT`). Zero means "signal the eventfd when the fence
/// signals"; that bit means "signal it as soon as a fence exists for the
/// point". Every other bit is `EINVAL`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L919>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjEventfd {
    /// The sync object handle.
    pub handle: u32,
    /// Zero, or [`wait_flags::WAIT_AVAILABLE`].
    pub flags: u32,
    /// Timeline point, or zero for the binary fence.
    pub point: u64,
    /// An existing eventfd the kernel will increment by one.
    pub fd: i32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_syncobj_array`, the argument of `SYNCOBJ_RESET` and
/// `SYNCOBJ_SIGNAL`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L940>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjArray {
    /// User pointer to a `u32` array of sync-object handles.
    pub handles: u64,
    /// Number of entries in `handles`.
    pub count_handles: u32,
    /// Padding; must be zero.
    pub pad: u32,
}

/// `struct drm_syncobj_timeline_array`, the argument of `SYNCOBJ_QUERY` and
/// `SYNCOBJ_TIMELINE_SIGNAL`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L947>.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub struct SyncobjTimelineArray {
    /// User pointer to a `u32` array of sync-object handles.
    pub handles: u64,
    /// User pointer to a `u64` array of timeline points, one per handle. An
    /// output for `SYNCOBJ_QUERY` and an input for `SYNCOBJ_TIMELINE_SIGNAL`.
    pub points: u64,
    /// Number of entries in `handles` and in `points`.
    pub count_handles: u32,
    /// `DRM_SYNCOBJ_QUERY_FLAGS_*`; see [`query_flags`].
    pub flags: u32,
}

/// `DRM_SYNCOBJ_CREATE_*`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L865>.
pub mod create_flags {
    /// Create the object already signalled.
    ///
    /// User space uses this to represent "there is no work to wait for", so a
    /// client that never signals the object still makes progress.
    pub const SIGNALED: u32 = 1 << 0;

    /// Every bit this implementation understands.
    pub const KNOWN: u32 = SIGNALED;
}

/// `DRM_SYNCOBJ_{FD_TO_HANDLE,HANDLE_TO_FD}_FLAGS_*`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L874>.
pub mod handle_flags {
    /// Import a sync file into an existing sync object, or export the object's
    /// current fence as a sync file, rather than moving the object itself.
    ///
    /// The two directions share the value, as they do upstream.
    pub const SYNC_FILE: u32 = 1 << 0;

    /// Every bit this implementation understands.
    pub const KNOWN: u32 = SYNC_FILE;
}

/// `DRM_SYNCOBJ_WAIT_FLAGS_*`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L893>.
pub mod wait_flags {
    /// Return once every handle has signalled, rather than once any one has.
    pub const WAIT_ALL: u32 = 1 << 0;
    /// Wait for a fence to be *submitted* for a handle that has none yet,
    /// instead of failing.
    ///
    /// Mesa sets this whenever it waits on a sync object another thread is
    /// still filling in.
    pub const WAIT_FOR_SUBMIT: u32 = 1 << 1;
    /// Wait only until a fence becomes available for the point, not until it
    /// signals.
    pub const WAIT_AVAILABLE: u32 = 1 << 2;
    /// Read `deadline_nsec` and apply it to the fences being waited on.
    ///
    /// Upstream uses the hint to raise clocks so a frame lands on time. There
    /// is nothing here to raise them with, so the flag is accepted and the
    /// hint ignored — which is a legal answer, because the field is advisory
    /// even upstream. Rejecting it would not be: it arrives from Mesa's
    /// present paths, and this is a wait, so refusing it would fail work that
    /// is merely un-hinted.
    pub const WAIT_DEADLINE: u32 = 1 << 3;

    /// Every bit this implementation understands.
    pub const KNOWN: u32 = WAIT_ALL | WAIT_FOR_SUBMIT | WAIT_AVAILABLE | WAIT_DEADLINE;
}

/// `DRM_SYNCOBJ_QUERY_FLAGS_*`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L946>.
pub mod query_flags {
    /// Report the last point *submitted* on the timeline rather than the last
    /// one signalled.
    pub const LAST_SUBMITTED: u32 = 1 << 0;

    /// Every bit this implementation understands.
    pub const KNOWN: u32 = LAST_SUBMITTED;
}

/// `DRM_CLOEXEC` and `DRM_RDWR`, the flags `PRIME_HANDLE_TO_FD` accepts.
///
/// They are spelled as `O_CLOEXEC` and `O_RDWR` upstream, and the values are
/// the generic Linux ones that Asterinas shares.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L851>.
pub mod prime_flags {
    /// `DRM_RDWR`: the exported descriptor may be written through as well as
    /// read.
    pub const RDWR: u32 = 0o2;
    /// `DRM_CLOEXEC`: close the exported descriptor across `execve`.
    pub const CLOEXEC: u32 = 0o2000000;

    /// Every bit this implementation understands.
    pub const KNOWN: u32 = RDWR | CLOEXEC;
}
