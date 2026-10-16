// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: /dev/dri character devices and ioctl shell.

//! The `/dev/dri/*` character devices.
//!
//! This is the half of the DRM subsystem that has to live in the kernel crate,
//! because it is the half that touches things comp crates cannot see: the ioctl
//! decoder, `current_userspace!()`, `PerOpenFileOps`, process credentials, and
//! `Errno`. Everything else — the device model, the dispatch table, the handlers —
//! is in `k1-musebook-drm`, keeping process services out of the device component.
//!
//! The shell's job for each ioctl is mechanical: decode the command, copy the
//! argument structure in, hand it to [`DrmDevice::ioctl`] as bytes, copy it back
//! out, and translate [`DrmError`] into an `Errno`. That last translation happens
//! in exactly one place, [`to_error`], so that the errno user space observes is a
//! deliberate decision rather than an accident of where an error was constructed.

use alloc::format;
use core::fmt::Debug;

use device_id::{DeviceId, MajorId, MinorId};
use k1_musebook_drm::{
    DRM_MAJOR, DrmDevice, DrmError, DrmFile, DrmMinor, EventNotifier, Privilege, UserMem,
    uapi::ioctl_nr,
};
use ostd::mm::VmIo;

use super::{
    Device, DeviceType,
    dri_export::{CurrentBlocker, CurrentDescriptors},
    registry::char,
};
use crate::{
    context::current_userspace,
    events::IoEvents,
    fs::{
        devtmpfs::DevtmpfsNodeMeta,
        file::{Mappable, MappableObject, MappedObject, PerOpenFileOps, StatusFlags},
        sysfs,
        vfs::{
            inode::FileOps,
            path::{Path, PathResolver},
        },
    },
    prelude::*,
    process::{
        credentials::capabilities::CapSet,
        posix_thread::AsPosixThread,
        signal::{PollHandle, Pollable, Pollee},
    },
    util::ioctl::RawIoctl,
    vm::vmar::{
        FileMmapRequest, MapHandle,
        on_demand::{
            self, DeviceFaultError, DeviceFaultHandler, DeviceFaultRequest, DeviceFaultResponse,
        },
    },
};

/// What this implementation expects a core command's argument to look like.
///
/// The size is deliberately *not* used to recognise the command; see
/// [`core_args::lookup`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CoreArg {
    /// The size of the structure the handler in `k1-musebook-drm` reads.
    ///
    /// Linux's `drv_size`, taken from the size the kernel's own `DRM_IOCTL_*`
    /// macro encodes.
    size: usize,
    /// The directions the command declares: [`IOC_WRITE`], [`IOC_READ`], both,
    /// or neither.
    ///
    /// Linux's `ioctl->cmd & (IOC_IN | IOC_OUT)`. Needed separately from the
    /// caller's, because a copy happens only in a direction *both* sides
    /// declare.
    dir: u32,
}

/// What each DRM core command's argument is, keyed by command number.
///
/// # Why this is a table and not the ioctl decoder
///
/// `dispatch_ioctl!` recognises a command by matching the *whole* encoded
/// number, argument size included. For most of Linux that is right, because most
/// ioctl arguments never change. DRM is the exception: `drm_ioctl` looks a
/// command up **by `nr` alone**, then reconciles the caller's size with its own
/// (see [`DriHandle::call`]). That is not laxity, it is the mechanism by which
/// DRM structures are allowed to grow — and using an exact match instead pins
/// this port to whichever kernel its headers were copied from.
///
/// For example, Linux 6.8 extended `SyncobjWait` from 32 to 40 bytes.
/// Both encodings must resolve to `SYNCOBJ_WAIT`; size reconciliation belongs
/// to the copy boundary, not command recognition.
///
/// # Reading an entry
///
/// The three constructors correspond one-to-one with the uapi macros —
/// [`inout`] is `DRM_IOWR`, [`input`] is `DRM_IOW`, [`none`] is `DRM_IO` — and
/// the type named is the structure `k1-musebook-drm`'s handler reads, which is where
/// the size comes from. The command numbers must equal the ones `k1-musebook-drm`'s
/// dispatch table uses; `every_dispatched_command_has_an_argument_entry` checks
/// that the two tables cover the same set.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1041-L1235>
/// (the `DRM_IOCTL_*` definitions this mirrors).
mod core_args {
    use k1_musebook_drm::uapi::{
        DrmGemClose, DrmGemFlink, DrmGemOpen, DrmGetCap, DrmPrimeHandle, DrmSetClientCap,
        DrmSetVersion, DrmUnique, DrmVersion,
        ioctl_nr::nr,
        mode,
        mode::{CreateDumb, DestroyDumb, MapDumb},
        sync::{
            SyncobjArray, SyncobjCreate, SyncobjDestroy, SyncobjEventfd, SyncobjHandle,
            SyncobjTimelineArray, SyncobjTimelineWait, SyncobjTransfer, SyncobjWait,
        },
    };

    use super::{CoreArg, IOC_READ, IOC_WRITE};

    /// `DRM_IOWR`: the argument is copied in and back out.
    const fn inout<T>() -> Option<CoreArg> {
        Some(CoreArg {
            size: size_of::<T>(),
            dir: IOC_WRITE | IOC_READ,
        })
    }

    /// `DRM_IOW`: the argument is copied in only.
    const fn input<T>() -> Option<CoreArg> {
        Some(CoreArg {
            size: size_of::<T>(),
            dir: IOC_WRITE,
        })
    }

    /// `DRM_IO`: there is no argument.
    const fn none() -> Option<CoreArg> {
        Some(CoreArg { size: 0, dir: 0 })
    }

    // There is deliberately no `output` constructor for `DRM_IOR`, because
    // nothing implemented here is one. `GET_MAGIC` (nr `0x02`) is the only
    // `DRM_IOR` in the whole of `drm.h`, and whoever implements it will have to
    // add one: writing it as `inout` gives `0xc0046402` where user space sends
    // `0x80046402`, changing the encoded direction.

    /// Returns what this implementation expects for `nr`, or `None` for a
    /// command it does not implement.
    pub(super) const fn lookup(nr: u32) -> Option<CoreArg> {
        match nr {
            nr::VERSION => inout::<DrmVersion>(),
            nr::GET_UNIQUE => inout::<DrmUnique>(),
            nr::SET_VERSION => inout::<DrmSetVersion>(),
            nr::GEM_CLOSE => input::<DrmGemClose>(),
            nr::GEM_FLINK => inout::<DrmGemFlink>(),
            nr::GEM_OPEN => inout::<DrmGemOpen>(),
            nr::GET_CAP => inout::<DrmGetCap>(),
            nr::SET_CLIENT_CAP => input::<DrmSetClientCap>(),
            nr::SET_MASTER | nr::DROP_MASTER => none(),

            nr::PRIME_HANDLE_TO_FD | nr::PRIME_FD_TO_HANDLE => inout::<DrmPrimeHandle>(),

            // Mode setting. Every one of these is `DRM_IOWR` upstream, even the
            // ones that only read their argument, and `MODE_RMFB` really does
            // take a bare `unsigned int`.
            nr::MODE_GETRESOURCES => inout::<mode::CardRes>(),
            nr::MODE_GETCRTC | nr::MODE_SETCRTC => inout::<mode::Crtc>(),
            nr::MODE_GETENCODER => inout::<mode::GetEncoder>(),
            nr::MODE_GETCONNECTOR => inout::<mode::GetConnector>(),
            nr::MODE_GETPROPERTY => inout::<mode::GetProperty>(),
            nr::MODE_GETPROPBLOB => inout::<mode::GetBlob>(),
            nr::MODE_ADDFB => inout::<mode::FbCmd>(),
            nr::MODE_RMFB => inout::<u32>(),
            nr::MODE_PAGE_FLIP => inout::<mode::CrtcPageFlip>(),
            nr::MODE_GETPLANERESOURCES => inout::<mode::GetPlaneRes>(),
            nr::MODE_GETPLANE => inout::<mode::GetPlane>(),
            nr::MODE_SETPLANE => inout::<mode::SetPlane>(),
            nr::MODE_ADDFB2 => inout::<mode::FbCmd2>(),
            nr::MODE_OBJ_GETPROPERTIES => inout::<mode::ObjGetProperties>(),
            nr::MODE_OBJ_SETPROPERTY => inout::<mode::ObjSetProperty>(),
            nr::MODE_ATOMIC => inout::<mode::Atomic>(),
            nr::MODE_CREATEPROPBLOB => inout::<mode::CreateBlob>(),
            nr::MODE_DESTROYPROPBLOB => inout::<mode::DestroyBlob>(),

            nr::MODE_CREATE_DUMB => inout::<CreateDumb>(),
            nr::MODE_MAP_DUMB => inout::<MapDumb>(),
            nr::MODE_DESTROY_DUMB => inout::<DestroyDumb>(),

            nr::SYNCOBJ_CREATE => inout::<SyncobjCreate>(),
            nr::SYNCOBJ_DESTROY => inout::<SyncobjDestroy>(),
            nr::SYNCOBJ_HANDLE_TO_FD | nr::SYNCOBJ_FD_TO_HANDLE => inout::<SyncobjHandle>(),
            nr::SYNCOBJ_WAIT => inout::<SyncobjWait>(),
            nr::SYNCOBJ_RESET | nr::SYNCOBJ_SIGNAL => inout::<SyncobjArray>(),
            nr::SYNCOBJ_TIMELINE_WAIT => inout::<SyncobjTimelineWait>(),
            nr::SYNCOBJ_QUERY | nr::SYNCOBJ_TIMELINE_SIGNAL => inout::<SyncobjTimelineArray>(),
            nr::SYNCOBJ_TRANSFER => inout::<SyncobjTransfer>(),
            nr::SYNCOBJ_EVENTFD => inout::<SyncobjEventfd>(),

            _ => None,
        }
    }
}

#[derive(Debug)]
struct DriNode {
    device: Arc<DrmDevice>,
    minor: DrmMinor,
    /// The devtmpfs name, such as `card0` or `renderD128`.
    name: String,
}

impl Device for DriNode {
    fn type_(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id(&self) -> DeviceId {
        // Same values as Linux: major 226, minor 0 for `card0` and 128 for
        // `renderD128`. `libdrm` looks devices up by this pair through
        // `/sys/dev/char/<major>:<minor>`, so it is ABI.
        let minor = self
            .device
            .minor_number(self.minor)
            .expect("a DRI node exists only for a minor the device owns");
        DeviceId::new(MajorId::new(DRM_MAJOR as u16), MinorId::new(minor))
    }

    fn devtmpfs_meta(&self) -> Option<DevtmpfsNodeMeta> {
        // Linux puts DRM nodes in a `dri` subdirectory, and both `libdrm` and Mesa
        // hard-code that path.
        DevtmpfsNodeMeta::new(format!("dri/{}", self.name)).ok()
    }

    fn open(&self) -> Result<Box<dyn PerOpenFileOps>> {
        let file = self
            .device
            .open_with_identity(
                self.minor,
                current_privilege(),
                k1_musebook_drm::file::CallerIdentity::from_owner(&current_thread!()),
            )
            .map_err(to_error)?;

        // The event queue lives in `k1-musebook-drm`, but waking a reader needs a
        // `Pollee`, which comp crates cannot see. The driver signals through this
        // hook; both this handle and the file hold it, and it points at neither, so
        // there is no cycle to break.
        let events = Arc::new(DriEvents {
            pollee: Pollee::new(),
        });
        file.set_event_notifier(events.clone());
        Ok(Box::new(DriHandle {
            device: self.device.clone(),
            file,
            events,
        }))
    }
}

/// How a queued DRM event reaches whoever is in `read` or `poll`.
struct DriEvents {
    pollee: Pollee,
}

impl Debug for DriEvents {
    /// `Pollee` is not `Debug`, and what it holds — a list of waiting threads —
    /// would not be worth printing anyway.
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("DriEvents").finish_non_exhaustive()
    }
}

impl EventNotifier for DriEvents {
    fn notify_readable(&self) {
        self.pollee.notify(IoEvents::IN);
    }
}

/// Evaluates the current caller's SYS_ADMIN capability in the initial user namespace.
fn current_privilege() -> Privilege {
    use crate::{process::UserNamespace, security::lsm::hooks};
    let thread = current_thread!();
    let allowed = hooks::on_capable(hooks::CapableContext::new(
        UserNamespace::get_init_singleton().as_ref(),
        thread.as_posix_thread().unwrap(),
        CapSet::SYS_ADMIN,
    ))
    .is_ok();
    if allowed {
        Privilege::Privileged
    } else {
        Privilege::Unprivileged
    }
}

/// One open file descriptor on a `/dev/dri` node.
#[derive(Debug)]
struct DriHandle {
    device: Arc<DrmDevice>,
    file: Arc<DrmFile>,
    events: Arc<DriEvents>,
}

/// A VMA retains exactly one GEM object, independently of its handle and file.
#[derive(Debug)]
struct GemFaultHandler {
    object: Arc<dyn k1_musebook_drm::gem::GemObject>,
}

impl GemFaultHandler {
    /// Resolves and authorizes the complete range before the VMA is installed.
    ///
    /// Linux `drm_gem_mmap` requires the object's exact fake offset and a range
    /// contained in its allocation. The resulting VMA owns a reference that
    /// remains valid after `GEM_CLOSE`; a fresh mmap still requires a handle.
    fn for_mapping(
        offsets: &k1_musebook_drm::gem::MmapOffsets,
        handles: &k1_musebook_drm::gem::HandleTable,
        offset: usize,
        size: usize,
    ) -> Result<Self> {
        let (object, base, _) = offsets.lookup(offset as u64).ok_or_else(|| {
            Error::with_message(Errno::EINVAL, "the mmap offset names no GEM object")
        })?;
        if base != offset as u64 || size == 0 || size > object.size() || !object.is_mappable() {
            return_errno_with_message!(Errno::EINVAL, "the mmap range is outside the GEM object");
        }
        if handles.find(&object).is_none() {
            return_errno_with_message!(Errno::EACCES, "the file has no handle for this GEM object");
        }
        Ok(Self { object })
    }
}

impl DeviceFaultHandler for GemFaultHandler {
    fn prepare_cpu_read(
        &self,
        req: &DeviceFaultRequest,
    ) -> core::result::Result<(), DeviceFaultError> {
        self.object
            .prepare_cpu_read(req.pgoff())
            .map_err(to_fault_error)
    }

    fn fault(
        &self,
        req: &DeviceFaultRequest,
    ) -> core::result::Result<DeviceFaultResponse, DeviceFaultError> {
        let frame = self.object.fault(req.pgoff()).map_err(to_fault_error)?;
        Ok(DeviceFaultResponse::new(
            frame,
            self.object.buffer_use().cache_policy(),
        ))
    }
}

/// Both unavailable backing pages and allocation failures are reported as SIGBUS.
fn to_fault_error(error: DrmError) -> DeviceFaultError {
    match error {
        DrmError::OutOfMemory => DeviceFaultError::OutOfMemory,
        _ => DeviceFaultError::OutOfBounds,
    }
}

impl DriHandle {
    /// Copies the argument in, runs the handler, and copies it back out.
    ///
    /// # Reconciling two sizes
    ///
    /// The caller's argument size and this implementation's need not agree, and
    /// upstream goes to some trouble not to require it:
    ///
    /// ```text
    /// ksize = max(in_size, out_size, drv_size)
    /// ```
    ///
    /// so the buffer is large enough for whichever side wants more, the tail
    /// beyond what the caller supplied is zeroed, and only as much as the caller
    /// supplied is copied back. An older client that sends a shorter structure
    /// therefore gets zeros where it does not know there are fields, and a newer
    /// client that sends a longer one has its unknown tail preserved rather than
    /// truncated. This lets clients with older or newer structure layouts
    /// reach the same handler.
    ///
    /// A direction is honoured only when *both* sides declare it, matching
    /// `cmd & ioctl->cmd & IOC_IN`: a command this implementation declares
    /// write-only is not copied back even if the caller asked for it.
    ///
    /// `arg.size` of zero means "no size of our own", which is the case for
    /// driver-private commands whose structures belong to the driver. Then
    /// `ksize` is the caller's size alone — again what upstream does for driver
    /// commands — and a short argument becomes the driver's own `EINVAL` from
    /// `payload_read` rather than a silently truncated structure.
    ///
    /// # Copying back after a failure
    ///
    /// The argument goes back out even when the handler fails, matching
    /// `drm_ioctl`. That is not cosmetic: the two-pass length protocol behind
    /// `DRM_IOCTL_VERSION`, and PowerVR's equivalent for `DEV_QUERY`, report how
    /// much room a buffer needs by writing into the very structure that an
    /// `E2BIG` would otherwise discard.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L850-L906>.
    fn call(&self, raw_ioctl: RawIoctl, arg: CoreArg) -> Result<i32> {
        let cmd = raw_ioctl.cmd();
        let nr = ioctl_nr::nr_of(cmd);

        // The encoding gives the size 14 bits, so the caller's size is bounded
        // by 16,383 bytes without any cap of our own.
        let user_size = ioctl_arg_size(cmd) as usize;
        let direction = ioctl_dir(cmd) & arg.dir;

        let in_size = if direction & IOC_WRITE != 0 {
            user_size
        } else {
            0
        };
        let out_size = if direction & IOC_READ != 0 {
            user_size
        } else {
            0
        };
        let ksize = in_size.max(out_size).max(arg.size);

        let mut payload = alloc::vec![0u8; ksize];
        if in_size != 0 {
            current_userspace!().read_bytes(raw_ioctl.arg(), &mut payload[..in_size])?;
        }

        let result = self
            .device
            .ioctl(&self.file, nr, &mut payload, &caller_context());

        let copy_back: Result<()> = if out_size != 0 {
            current_userspace!()
                .write_bytes(raw_ioctl.arg(), &payload[..out_size])
                .map_err(Error::from)
        } else {
            Ok(())
        };

        let ioctl_err = result.err().map(to_error);
        copy_back?;
        if let Some(err) = ioctl_err {
            return Err(err);
        }

        Ok(0)
    }
}

/// Extracts the argument size from an ioctl command, Linux's `_IOC_SIZE`.
///
/// `ioctl_nr` encodes commands but does not decode these two fields, and this is
/// the only caller that needs them: every other command's size and direction are
/// fixed by its argument type.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/asm-generic/ioctl.h#L83>.
const fn ioctl_arg_size(cmd: u32) -> u32 {
    (cmd >> 16) & 0x3fff
}

/// Extracts the direction bits from an ioctl command, Linux's `_IOC_DIR`.
const fn ioctl_dir(cmd: u32) -> u32 {
    cmd >> 30
}

/// `_IOC_WRITE`: user space writes the argument, so the kernel copies it in.
const IOC_WRITE: u32 = 1;
/// `_IOC_READ`: the kernel writes the argument, so it copies it back out.
const IOC_READ: u32 = 2;

/// [`UserMem`] backed by the calling process's address space.
///
/// The variable-length ioctls write into arrays that user space allocated, rather
/// than into their fixed argument structure, so they need a way to reach the
/// caller's address space from inside `k1-musebook-drm`. This is that way, and it is the
/// only one.
struct CurrentUserMem;

impl UserMem for CurrentUserMem {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> k1_musebook_drm::DrmResult<()> {
        let addr = usize::try_from(addr).map_err(|_| DrmError::BadAddress)?;
        current_userspace!()
            .read_bytes(addr, buf)
            .map_err(|_| DrmError::BadAddress)
    }

    fn write_bytes(&self, addr: u64, buf: &[u8]) -> k1_musebook_drm::DrmResult<()> {
        let addr = usize::try_from(addr).map_err(|_| DrmError::BadAddress)?;
        current_userspace!()
            .write_bytes(addr, buf)
            .map_err(|_| DrmError::BadAddress)
    }
}

/// The caller context handed to every ioctl issued through this character
/// device.
///
/// All three halves come from the calling thread, which is why only the shell
/// can build one: the address space is `current_userspace!()`, the descriptor
/// table is the current process's, and the blocker reaches the timer subsystem
/// and this thread's pending signals.
fn caller_context() -> k1_musebook_drm::ioctl::CallerContext<'static> {
    k1_musebook_drm::ioctl::CallerContext::new(&CurrentUserMem)
        .with_credentials(
            k1_musebook_drm::file::CallerIdentity::from_owner(&current_thread!()),
            current_privilege(),
        )
        .with_descriptors(&CurrentDescriptors)
        .with_blocker(&CurrentBlocker)
}

/// Translates a [`DrmError`] into the error user space observes.
///
/// Match every variant explicitly so additions to [`DrmError`] require an
/// errno decision here. `errnos_agree_with_k1_musebook_drm` checks this mapping
/// against [`DrmError::to_errno`].
fn to_error(error: DrmError) -> Error {
    let errno = match error {
        DrmError::InvalidArgument => Errno::EINVAL,
        DrmError::NotFound => Errno::ENOENT,
        DrmError::OutOfMemory => Errno::ENOMEM,
        DrmError::PermissionDenied => Errno::EACCES,
        DrmError::Busy => Errno::EBUSY,
        DrmError::NoSpace => Errno::ENOSPC,
        DrmError::TooBig => Errno::E2BIG,
        DrmError::BadAddress => Errno::EFAULT,
        DrmError::NoDevice => Errno::ENODEV,
        DrmError::Io => Errno::EIO,
        DrmError::NotSupported => Errno::EOPNOTSUPP,
        DrmError::TimedOut => Errno::ETIMEDOUT,
        DrmError::TimeExpired => Errno::ETIME,
        DrmError::Interrupted => Errno::ERESTARTSYS,
        DrmError::WouldDeadlock => Errno::EDEADLK,
        DrmError::BadFd => Errno::EBADF,
    };
    Error::new(errno)
}

impl DriHandle {
    /// Moves as many whole queued events into `writer` as fit.
    ///
    /// Events are delimited only by their own length field, so a partial one would
    /// desynchronize the reader for good. Hence the three outcomes, all of which
    /// are Linux's:
    ///
    /// - nothing queued: `EAGAIN`, which is what a blocking caller waits on;
    /// - something queued but the buffer is too small for the first event: zero
    ///   bytes, with the event left where it was;
    /// - otherwise: as many events as fit, whole.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_file.c#L537>
    /// (`drm_read`).
    fn read_events(&self, writer: &mut VmWriter) -> Result<usize> {
        let mut total = 0;

        loop {
            let capacity = writer.avail();
            let read = self.file.read_event(capacity, |bytes| -> Result<()> {
                let mut reader = VmReader::from(bytes);
                writer.write_fallible(&mut reader)?;
                Ok(())
            })?;

            if read == 0 {
                break;
            }
            total += read;
        }

        // Consumption invalidates cached readiness, including the empty-queue
        // path. It does not notify: draining old events creates no new event.
        self.events.pollee.invalidate();

        if total == 0 && !self.file.has_events() {
            return_errno_with_message!(Errno::EAGAIN, "the DRM device has no pending events");
        }
        Ok(total)
    }

    fn check_io_events(&self) -> IoEvents {
        // DRM nodes have no write handler; Linux's `drm_poll` reports read events only.
        if self.file.has_events() {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }
}

impl Pollable for DriHandle {
    fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        self.events
            .pollee
            .poll_with(mask, poller, || self.check_io_events())
    }
}

impl FileOps for DriHandle {
    /// Reading a DRM node dequeues pending events.
    ///
    /// This is how a compositor learns that a page flip has landed, and it is the
    /// only thing `read` on a DRM node does.
    fn read_at(
        &self,
        _offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        if status_flags.contains(StatusFlags::O_NONBLOCK) {
            self.read_events(writer)
        } else {
            self.wait_events(IoEvents::IN, None, || self.read_events(writer))
        }
    }

    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        // DRM nodes have no `write` handler in Linux either.
        return_errno_with_message!(Errno::EINVAL, "DRM device nodes cannot be written to");
    }
}

impl PerOpenFileOps for DriHandle {
    fn check_seekable(&self) -> Result<()> {
        Ok(())
    }

    fn is_offset_aware(&self) -> bool {
        false
    }

    /// Maps a buffer object of this device into the calling process.
    ///
    /// Nothing is decided here, not even which object is being mapped: the
    /// offset user space passed to `mmap` is not available at this point, and
    /// for a DRM node it identifies a buffer. `Mappable::map` resolves the
    /// object and validates the complete range before installing the VMA.
    fn mappable(&self, _request: FileMmapRequest) -> Result<MappableObject<'_>> {
        Ok(MappableObject::Device(self))
    }

    /// Routes one ioctl, the way `drm_ioctl` does: by command number, with the
    /// argument size reconciled rather than required to match.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L837>
    /// (`drm_ioctl`).
    fn ioctl(&self, _path: &Path, raw_ioctl: RawIoctl) -> Result<i32> {
        let cmd = raw_ioctl.cmd();

        // Linux distinguishes the two failures: a command that is not a DRM
        // command at all is `ENOTTY`, while an unimplemented DRM command is
        // `EINVAL`. `libdrm` relies on the difference to probe for optional
        // interfaces.
        //
        // Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L834>.
        if ioctl_nr::type_of(cmd) != ioctl_nr::DRM_IOCTL_BASE {
            return_errno_with_message!(Errno::ENOTTY, "not a DRM ioctl command");
        }

        let nr = ioctl_nr::nr_of(cmd);

        // A driver-private command's argument type belongs to the driver, which
        // this crate must not know: the fourteen PowerVR structures are
        // `k1-musebook-pvr`'s. So there is no size of our own to reconcile, and the
        // encoded one is used alone — which is exactly what upstream does for
        // driver commands. A short argument is then the driver's own `EINVAL`
        // from `payload_read` rather than a silently truncated structure.

        if ioctl_nr::is_driver_private(nr) {
            return self.call(
                raw_ioctl,
                CoreArg {
                    size: 0,
                    dir: IOC_WRITE | IOC_READ,
                },
            );
        }

        let Some(arg) = core_args::lookup(nr) else {
            debug!("the DRM ioctl command {:#x} is unimplemented", cmd);
            return_errno_with_message!(Errno::EINVAL, "the DRM ioctl command is unimplemented");
        };

        self.call(raw_ioctl, arg)
    }
}

pub(super) fn init_in_first_kthread() {
    // Every driver has had its chance to register by now, because component
    // initialization has finished. That is why the software driver is enrolled
    // here rather than from `k1-musebook-drm`'s own init: components initialize in
    // dependency order, so `aster-framebuffer` had not run yet at that point and
    // the fallback would always have won.
    if k1_musebook_drm::devices().is_empty()
        && let Err(err) = k1_musebook_drm::drivers::soft::register()
    {
        warn!("failed to register the software DRM driver: {:?}", err);
        return;
    }

    for device in k1_musebook_drm::devices() {
        publish_device(&device);
    }
}

/// Loads firmware for any device that could not finish initializing without it.
///
/// Runs after the root filesystem is unpacked, which is the earliest a blob can
/// be read at all — and therefore after [`init_in_first_kthread`], so the
/// devices published here are ones no client has seen yet.
///
/// A GPU whose firmware is missing stays absent rather than taking the boot
/// down with it. That is upstream's behaviour too, and the right one: the blob
/// is redistributable but not everyone installs it, and a machine without a
/// working GPU still needs to reach a shell.
pub(super) fn load_firmware_in_first_kthread(path_resolver: &PathResolver) {
    #[cfg(feature = "pvr")]
    {
        let Some(name) = k1_musebook_pvr::platform::firmware_name() else {
            return;
        };

        // Firmware is required in initramfs before the final root is mounted.
        let image = match super::firmware::request(path_resolver, name) {
            Ok(image) => {
                info!("firmware {} read, {} bytes", name, image.len());
                image
            }
            Err(err) => {
                warn!(
                    "firmware {} could not be read: {:?} — the GPU stays absent. \
                     Note this runs before the real root is mounted, so the image has to be \
                     in the initramfs, not only in the rootfs.",
                    name, err
                );
                return;
            }
        };

        let before = k1_musebook_drm::devices().len();
        if let Err(err) = k1_musebook_pvr::platform::attach(image) {
            warn!("the GPU rejected its firmware image: {:?}", err);
            return;
        }
        info!(
            "firmware accepted; publishing {} new DRM device(s)",
            k1_musebook_drm::devices().len().saturating_sub(before)
        );

        // Publish only what `attach` added. Everything already registered got
        // its nodes from `init_in_first_kthread`, and registering a char device
        // twice under one name fails.
        for device in k1_musebook_drm::devices().into_iter().skip(before) {
            publish_device(&device);
        }

        // The driver's own sleepable context, which it cannot create for itself:
        // a bare `ostd` task is never enqueued by the scheduler (it wants an
        // `Arc<Thread>` in the task's data), and `Thread` lives here rather than
        // in a component crate. Without it the GPU has no watchdog of its own, no
        // job-timeout sweep, nowhere to release the firmware structures of a
        // client that crashed, and no way to act on a reset its interrupt handler
        // asked for. See `kernel/core/comps/k1-musebook/pvr/src/service.rs`.
        //
        // Spawned after the nodes are published, so that the first thing it can
        // possibly find to do belongs to a client that could actually open the
        // device.
        crate::thread::kernel_thread::ThreadOptions::new(k1_musebook_pvr::platform::run_service)
            .spawn();
    }

    #[cfg(not(feature = "pvr"))]
    let _ = path_resolver;
}

/// Registers one DRM device's character nodes and publishes it in sysfs.
fn publish_device(device: &Arc<DrmDevice>) {
    let mut nodes = Vec::new();

    for minor in [DrmMinor::Primary, DrmMinor::Render] {
        let Some(name) = device.node_name(minor) else {
            continue;
        };

        let node = Arc::new(DriNode {
            device: device.clone(),
            minor,
            name: name.clone(),
        });
        let id = node.id();
        if let Err(err) = char::register(node) {
            warn!("failed to register a DRI char device: {:?}", err);
            continue;
        }
        nodes.push((name, id));
    }

    publish_in_sysfs(device, &nodes);
}

/// Publishes a device and its nodes in sysfs.
///
/// `libdrm` enumerates DRM devices through
/// `/sys/dev/char/<major>:<minor>/device/`; a device node alone is insufficient.
/// Required paths and attributes are documented in
/// `kernel/core/src/fs/fs_impls/sysfs/device.rs`.
fn publish_in_sysfs(device: &Arc<DrmDevice>, nodes: &[(String, DeviceId)]) {
    let driver = device.driver().info().name;

    let Some(identity) = device.driver().platform_identity() else {
        // Not an error: a driver may have reasons not to be enumerable. It does
        // mean no `libdrm` client will find it, which is worth saying out loud.
        debug!("the {} device is not published in sysfs", driver);
        return;
    };

    let (parent_info, node_infos) = sysfs_entries(driver, identity, nodes);

    let parent = match sysfs::register_platform_device(parent_info) {
        Ok(parent) => parent,
        Err(err) => {
            warn!(
                "failed to publish the {} device in sysfs: {:?}",
                driver, err
            );
            return;
        }
    };

    for info in node_infos {
        let name = info.name.clone();
        if let Err(err) = parent.register_char_device(info) {
            warn!("failed to publish /dev/dri/{} in sysfs: {:?}", name, err);
        }
    }
}

/// Turns a device's identity and its nodes into the sysfs entries they need.
///
/// Separated from the registration so that the values — which are what `libdrm`
/// actually reads — can be checked by a test rather than only by running Mesa.
///
/// Both nodes of one device deliberately go under the same parent: that is what
/// makes `libdrm` fold them into a single `drmDevice` with two nodes, and it is the
/// same folding that requires two *different* devices to carry two different
/// `OF_FULLNAME`s.
fn sysfs_entries(
    driver: &str,
    identity: k1_musebook_drm::PlatformIdentity,
    nodes: &[(String, DeviceId)],
) -> (sysfs::PlatformDeviceInfo, Vec<sysfs::CharDeviceInfo>) {
    let parent = sysfs::PlatformDeviceInfo {
        name: identity.name.into(),
        of_fullname: identity.of_fullname,
        of_compatible: identity.of_compatible,
        driver: Some(driver.to_string()),
    };

    let nodes = nodes
        .iter()
        .map(|(name, id)| sysfs::CharDeviceInfo {
            // Names the `device/drm` directory `drmNodeIsDRM` probes for. A node
            // that fails that probe is dropped from the enumeration without a
            // word, so this string is load-bearing.
            class: "drm".into(),
            name: name.clone().into(),
            // `DEVNAME`, relative to `/dev`, which is how
            // `drmGetDeviceNameFromFd2` reconstructs the node's path.
            dev_name: format!("dri/{}", name),
            dev_type: Some("drm_minor".to_string()),
            id: *id,
        })
        .collect();

    (parent, nodes)
}

#[cfg(ktest)]
mod test {
    use k1_musebook_drm::uapi::ioctl_nr::nr;
    use ostd::prelude::ktest;

    use super::*;

    /// The errno numbers in `k1-musebook-drm` and the mapping here must agree, since the
    /// crate documents its variants in terms of what user space will observe.
    #[ktest]
    fn errnos_agree_with_k1_musebook_drm() {
        let errors = [
            DrmError::InvalidArgument,
            DrmError::NotFound,
            DrmError::OutOfMemory,
            DrmError::PermissionDenied,
            DrmError::Busy,
            DrmError::NoSpace,
            DrmError::TooBig,
            DrmError::BadAddress,
            DrmError::NoDevice,
            DrmError::Io,
            DrmError::NotSupported,
            DrmError::TimedOut,
            DrmError::TimeExpired,
            DrmError::Interrupted,
            DrmError::WouldDeadlock,
            DrmError::BadFd,
        ];

        for error in errors {
            assert_eq!(
                to_error(error).error() as i32,
                error.to_errno(),
                "{:?} maps to two different errnos",
                error
            );
        }
    }

    fn mapping_tables() -> (
        k1_musebook_drm::gem::MmapOffsets,
        k1_musebook_drm::gem::HandleTable,
    ) {
        (
            k1_musebook_drm::gem::MmapOffsets::new(),
            k1_musebook_drm::gem::HandleTable::new(),
        )
    }

    #[ktest]
    fn a_gem_mapping_cannot_cross_into_an_adjacent_object() {
        use k1_musebook_drm::gem::{BufferUse, GemObject, ShmemObject};
        let (offsets, handles) = mapping_tables();
        let first: Arc<dyn GemObject> = ShmemObject::new(PAGE_SIZE, BufferUse::CpuOnly).unwrap();
        let second: Arc<dyn GemObject> = ShmemObject::new(PAGE_SIZE, BufferUse::CpuOnly).unwrap();
        handles.insert(first.clone()).unwrap();
        handles.insert(second.clone()).unwrap();
        let base = offsets.offset_of(&first).unwrap() as usize;
        assert_eq!(
            offsets.offset_of(&second).unwrap() as usize,
            base + PAGE_SIZE
        );
        let error =
            GemFaultHandler::for_mapping(&offsets, &handles, base, 2 * PAGE_SIZE).unwrap_err();
        assert_eq!(error.error(), Errno::EINVAL);
        let bound = GemFaultHandler::for_mapping(&offsets, &handles, base, PAGE_SIZE).unwrap();
        let request = DeviceFaultRequest::new(
            PAGE_SIZE,
            0x1_0000_0000,
            crate::vm::perms::VmPerms::READ,
            true,
        );
        assert_eq!(
            bound.fault(&request).unwrap_err(),
            DeviceFaultError::OutOfBounds
        );
    }

    #[ktest]
    fn a_fresh_gem_mapping_requires_an_exact_offset_and_a_current_handle() {
        use k1_musebook_drm::gem::{BufferUse, GemObject, HandleTable, ShmemObject};
        let (offsets, handles) = mapping_tables();
        let object: Arc<dyn GemObject> =
            ShmemObject::new(2 * PAGE_SIZE, BufferUse::CpuOnly).unwrap();
        let handle = handles.insert(object.clone()).unwrap();
        let base = offsets.offset_of(&object).unwrap() as usize;
        assert_eq!(
            GemFaultHandler::for_mapping(&offsets, &handles, base, 0)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert_eq!(
            GemFaultHandler::for_mapping(&offsets, &handles, base + PAGE_SIZE, PAGE_SIZE)
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
        assert_eq!(
            GemFaultHandler::for_mapping(&offsets, &HandleTable::new(), base, PAGE_SIZE)
                .unwrap_err()
                .error(),
            Errno::EACCES
        );
        let bound = Arc::new(
            GemFaultHandler::for_mapping(&offsets, &handles, base, 2 * PAGE_SIZE).unwrap(),
        );
        let mapping = on_demand::map(bound.clone(), 0);
        let fork = mapping.dup();
        let split = mapping.dup_at_offset(PAGE_SIZE);
        let weak = Arc::downgrade(&object);
        handles.remove(handle).unwrap();
        assert_eq!(
            GemFaultHandler::for_mapping(&offsets, &handles, base, PAGE_SIZE)
                .unwrap_err()
                .error(),
            Errno::EACCES
        );
        let request = DeviceFaultRequest::new(
            PAGE_SIZE,
            0x1_0000_0000,
            crate::vm::perms::VmPerms::READ,
            true,
        );
        assert!(bound.fault(&request).is_ok());
        drop((object, bound, mapping));
        assert!(weak.upgrade().is_some());
        drop(fork);
        assert!(weak.upgrade().is_some());
        drop(split);
        assert!(weak.upgrade().is_none());
    }

    /// Every dispatched core command needs an argument entry in the kernel shell.
    #[ktest]
    fn every_dispatched_command_has_an_argument_entry() {
        for desc in k1_musebook_drm::ioctl::CORE_IOCTLS
            .iter()
            .chain(k1_musebook_drm::kms::KMS_IOCTLS)
        {
            assert!(
                core_args::lookup(desc.nr).is_some(),
                "{} ({:#04x}) has a handler but no argument entry, so the shell \
                 would report it unimplemented",
                desc.name,
                desc.nr
            );
        }
    }

    /// The sizes and directions the argument table gives must be the ones the
    /// uapi macros encode.
    ///
    /// The direction matters as much as the size and is easier to get wrong,
    /// because most of DRM is `DRM_IOWR` and the exceptions are unremarkable to
    /// look at. A command declared `DRM_IOWR` here but `DRM_IOW` upstream would
    /// write back to a buffer upstream leaves alone.
    #[ktest]
    fn argument_entries_match_the_uapi_declarations() {
        const IN: u32 = IOC_WRITE;
        const INOUT: u32 = IOC_WRITE | IOC_READ;

        // Transcribed from the `DRM_IOCTL_*` definitions, not from the table
        // under test: `DRM_IOWR` is both directions, `DRM_IOW` is in only, and
        // `DRM_IO` is neither.
        let cases: [(u32, usize, u32); 46] = [
            (nr::VERSION, 64, INOUT),
            (nr::GET_UNIQUE, 16, INOUT),
            (nr::SET_VERSION, 16, INOUT),
            (nr::GEM_CLOSE, 8, IN),
            (nr::GEM_FLINK, 8, INOUT),
            (nr::GEM_OPEN, 16, INOUT),
            (nr::GET_CAP, 16, INOUT),
            (nr::SET_CLIENT_CAP, 16, IN),
            (nr::SET_MASTER, 0, 0),
            (nr::DROP_MASTER, 0, 0),
            (nr::PRIME_HANDLE_TO_FD, 12, INOUT),
            (nr::PRIME_FD_TO_HANDLE, 12, INOUT),
            (nr::MODE_GETRESOURCES, 64, INOUT),
            (nr::MODE_GETCRTC, 104, INOUT),
            (nr::MODE_SETCRTC, 104, INOUT),
            (nr::MODE_GETENCODER, 20, INOUT),
            (nr::MODE_GETCONNECTOR, 80, INOUT),
            (nr::MODE_GETPROPERTY, 64, INOUT),
            (nr::MODE_GETPROPBLOB, 16, INOUT),
            (nr::MODE_ADDFB, 28, INOUT),
            // `unsigned int`, not a structure.
            (nr::MODE_RMFB, 4, INOUT),
            (nr::MODE_PAGE_FLIP, 24, INOUT),
            (nr::MODE_GETPLANERESOURCES, 16, INOUT),
            (nr::MODE_GETPLANE, 32, INOUT),
            (nr::MODE_SETPLANE, 48, INOUT),
            (nr::MODE_ADDFB2, 104, INOUT),
            (nr::MODE_OBJ_GETPROPERTIES, 32, INOUT),
            (nr::MODE_OBJ_SETPROPERTY, 24, INOUT),
            (nr::MODE_ATOMIC, 56, INOUT),
            (nr::MODE_CREATEPROPBLOB, 16, INOUT),
            (nr::MODE_DESTROYPROPBLOB, 4, INOUT),
            (nr::MODE_CREATE_DUMB, 32, INOUT),
            (nr::MODE_MAP_DUMB, 16, INOUT),
            (nr::MODE_DESTROY_DUMB, 4, INOUT),
            (nr::SYNCOBJ_CREATE, 8, INOUT),
            (nr::SYNCOBJ_DESTROY, 8, INOUT),
            (nr::SYNCOBJ_HANDLE_TO_FD, 16, INOUT),
            (nr::SYNCOBJ_FD_TO_HANDLE, 16, INOUT),
            // 40 since Linux 6.8 appended `deadline_nsec`.
            (nr::SYNCOBJ_WAIT, 40, INOUT),
            (nr::SYNCOBJ_RESET, 16, INOUT),
            (nr::SYNCOBJ_SIGNAL, 16, INOUT),
            (nr::SYNCOBJ_TIMELINE_WAIT, 48, INOUT),
            (nr::SYNCOBJ_QUERY, 24, INOUT),
            (nr::SYNCOBJ_TRANSFER, 32, INOUT),
            (nr::SYNCOBJ_TIMELINE_SIGNAL, 24, INOUT),
            (nr::SYNCOBJ_EVENTFD, 24, INOUT),
        ];

        for (nr, size, dir) in cases {
            assert_eq!(
                core_args::lookup(nr),
                Some(CoreArg { size, dir }),
                "the argument entry for {nr:#04x} disagrees with the uapi declaration"
            );
        }
    }

    /// A command whose argument size differs from this implementation's must
    /// still reach its handler, and one with the wrong direction must not.
    ///
    /// Extending the argument structure must not make a known command unrecognisable.
    #[ktest]
    fn a_command_is_recognised_by_number_rather_than_by_argument_size() {
        let entry = core_args::lookup(nr::SYNCOBJ_WAIT).unwrap();

        // Sizes on either side of ours, as an older and a newer libdrm would
        // send. Recognition must not depend on them.
        for size in [entry.size - 8, entry.size, entry.size + 8] {
            let cmd = ioctl_nr::iowr(nr::SYNCOBJ_WAIT, size);
            assert_eq!(ioctl_nr::nr_of(cmd), nr::SYNCOBJ_WAIT);
            assert!(core_args::lookup(ioctl_nr::nr_of(cmd)).is_some());
        }

        // A number outside both tables is the one case that must not resolve.
        assert!(core_args::lookup(nr::MODE_CURSOR).is_none());
        assert!(core_args::lookup(nr::WAIT_VBLANK).is_none());
    }

    /// The sysfs entries a device gets are the ones `libdrm` reads.
    ///
    /// Every value here is one `xf86drm.c` looks for by name, and every one of them
    /// fails silently when wrong: a missing `device/drm` directory or a mistyped
    /// `DEVNAME` makes `drmGetDevices2` report zero devices with no diagnostic.
    /// This checks the attribute values; sysfs tests separately cover link
    /// resolution in the resulting tree.
    #[ktest]
    fn sysfs_entries_carry_what_libdrm_reads() {
        use k1_musebook_drm::DrmDriver;

        let driver = k1_musebook_drm::drivers::soft::SoftDriver::new();
        let identity = driver
            .platform_identity()
            .expect("the software driver publishes itself");

        let nodes = [
            (
                "card0".to_string(),
                DeviceId::new(MajorId::new(226), MinorId::new(0)),
            ),
            (
                "renderD128".to_string(),
                DeviceId::new(MajorId::new(226), MinorId::new(128)),
            ),
        ];
        let (parent, node_infos) = sysfs_entries(driver.info().name, identity, &nodes);

        // `drmParseOFBusInfo` reads `OF_FULLNAME` as the device's identity, and
        // `drmFoldDuplicatedDevices` merges platform devices that share one. The
        // K1 DPU and GPU therefore must not collide with each other or with this.
        assert_eq!(parent.of_fullname, "/soft/aster-drm");
        assert_ne!(parent.of_fullname, "/soc/imggpu@cac00000");
        assert_ne!(parent.of_fullname, "/soc/dpu@c0340000");
        // Mesa picks a user-space driver by this name.
        assert_eq!(parent.driver.as_deref(), Some("aster"));
        assert!(!parent.of_compatible.is_empty());

        assert_eq!(node_infos.len(), 2);
        for (info, (name, id)) in node_infos.iter().zip(&nodes) {
            assert_eq!(&*info.class, "drm");
            assert_eq!(&*info.name, name.as_str());
            assert_eq!(info.dev_name, format!("dri/{}", name));
            assert_eq!(info.dev_type.as_deref(), Some("drm_minor"));
            // The major/minor pair is the only thing tying the sysfs entry to the
            // device node.
            assert_eq!(info.id, *id);
        }
    }

    /// Driver-private commands must reach [`DriHandle::call`] with no size of
    /// their own, so that the size and direction come back out of the command
    /// they were given.
    ///
    /// The shell has no argument type for driver-private commands, so it must
    /// decode the caller's size and direction and forward the request to the
    /// driver, which validates the payload.
    #[ktest]
    fn driver_private_commands_are_routed_and_decoded() {
        // `DRM_IOCTL_PVR_DEV_QUERY`, `DRM_IOWR(DRM_COMMAND_BASE + 0x00, struct
        // drm_pvr_ioctl_dev_query_args)`, whose argument is 16 bytes.
        const DEV_QUERY_NR: u32 = ioctl_nr::DRM_COMMAND_BASE;
        const DEV_QUERY_SIZE: usize = 16;
        let cmd = ioctl_nr::iowr(DEV_QUERY_NR, DEV_QUERY_SIZE);

        assert!(ioctl_nr::is_driver_private(ioctl_nr::nr_of(cmd)));
        assert_eq!(ioctl_nr::type_of(cmd), ioctl_nr::DRM_IOCTL_BASE);
        assert_eq!(ioctl_nr::nr_of(cmd), DEV_QUERY_NR);
        // `DRM_IOWR` means both directions, so the argument is copied in and out.
        assert_eq!(ioctl_arg_size(cmd) as usize, DEV_QUERY_SIZE);
        assert_eq!(ioctl_dir(cmd), IOC_READ | IOC_WRITE);

        // The core commands must not be mistaken for private ones, or the
        // decoder above would never see them.
        for nr in [
            nr::VERSION,
            nr::GET_CAP,
            nr::MODE_ATOMIC,
            nr::SYNCOBJ_CREATE,
        ] {
            assert!(!ioctl_nr::is_driver_private(nr), "{nr:#04x} is not private");
        }

        // A one-directional and a zero-argument private command decode too; the
        // copies are skipped for a size of zero, so that must not be confused
        // with a missing direction.
        assert_eq!(ioctl_dir(ioctl_nr::iow(DEV_QUERY_NR, 8)), IOC_WRITE);
        assert_eq!(ioctl_dir(ioctl_nr::ior(DEV_QUERY_NR, 8)), IOC_READ);
        assert_eq!(ioctl_arg_size(ioctl_nr::io(DEV_QUERY_NR)), 0);
    }
}

impl Mappable for DriHandle {
    fn supports_private_mapping(&self) -> bool {
        true
    }

    fn map(&self, offset: usize, handle: MapHandle) -> Result<Box<dyn MappedObject>> {
        let handler = GemFaultHandler::for_mapping(
            self.device.gem().offsets(),
            self.file.handles(),
            offset,
            handle.size(),
        )?;
        Ok(on_demand::map(Arc::new(handler), 0))
    }
}
