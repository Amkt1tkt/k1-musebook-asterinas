// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: PRIME dma-buf / syncobj file descriptors.

//! The process-side half of PRIME and sync objects.
//!
//! `k1-musebook-drm` decides *what* is shared and this file decides
//! *how*, the same split as [`UserMem`] for the address space and for the same
//! reason: a comp crate cannot see a descriptor table, a timer, or a pending
//! signal.
//!
//! Three things live here:
//!
//!  - [`DrmExportFile`], the file object a shared buffer or sync object hides
//!    behind. A dma-buf can be mapped and, matching Linux `dma_buf_llseek`,
//!    reports its size through `lseek(fd, 0, SEEK_END)`. Mesa's PowerVR
//!    winsys does exactly that before `PRIME_FD_TO_HANDLE`.
//!  - [`CurrentDescriptors`], which installs one into the calling process's
//!    descriptor table, resolves one back, and hands `SYNCOBJ_EVENTFD` a
//!    handle that can increment an eventfd. The increment itself lives here
//!    because `k1-musebook-drm` cannot name [`crate::events::EventFile`].
//!  - [`CurrentBlocker`], which turns `SYNCOBJ_WAIT`'s absolute deadline into
//!    a signal-aware sleep.

use core::{fmt::Display, time::Duration};

use k1_musebook_drm::{
    DrmError,
    fence::{Blocker, Deadline, Fence, FenceCallback},
    prime::{Descriptors, DrmExport, Eventfd, ExportFlags},
};
use keyable_arc::KeyableWeak;
use ostd::{
    sync::{LocalIrqDisabled, WaitQueue},
    task::Task,
};

use crate::{
    events::{EventFile, IoEvents},
    fs::{
        file::{
            AccessMode, FileCommon, FileLike, Mappable, MappableObject, MappedObject, SeekFrom,
            StatusFlags,
            file_table::{FdFlags, FileDesc},
        },
        pseudofs::AnonInodeFs,
    },
    prelude::*,
    process::signal::{PollHandle, Pollable, Pollee},
    time::clocks::MonotonicClock,
    vm::vmar::{
        FileMmapRequest, MapHandle,
        on_demand::{
            self, DeviceFaultError, DeviceFaultHandler, DeviceFaultRequest, DeviceFaultResponse,
        },
    },
};

/// A file descriptor that refers to a shared DRM object.
///
/// A dma-buf descriptor is mostly an opaque token — clients pass it to
/// another driver — but it is not inert. Linux's `dma_buf_llseek` answers
/// `lseek(fd, 0, SEEK_END)` with the buffer size and `lseek(fd, 0, SEEK_SET)`
/// with zero; Mesa's PowerVR winsys (`pvr_drm_winsys_buffer_create_from_fd`)
/// will not import a buffer whose size it cannot read that way. A sync-object
/// descriptor has no size and stays unseekable.
pub(super) struct DrmExportFile {
    export: DrmExport,
    /// Poll subscriptions are independent of cached readiness and do not own fences.
    wakeup: Arc<ExportWakeup>,
    common: FileCommon,
}

/// One callback per pending fence; callbacks keep only weak descriptor state.
struct ExportWakeup {
    pollee: Pollee,
    armed: SpinLock<BTreeSet<KeyableWeak<dyn Fence>>, LocalIrqDisabled>,
}

impl DrmExportFile {
    fn new(export: DrmExport, flags: ExportFlags) -> Self {
        // The name user space sees in `/proc/self/fd`. Linux calls a dma-buf
        // descriptor `dmabuf` and a sync object `sync_file`/`syncobj`;
        // debugging tools grep for these. `new_path` takes a plain function
        // pointer, so each name needs its own.
        let pseudo_path = match export {
            DrmExport::Buffer(_) => AnonInodeFs::new_path(|_| "anon_inode:dmabuf".to_string()),
            DrmExport::Sync(_) => AnonInodeFs::new_path(|_| "anon_inode:syncobj".to_string()),
            DrmExport::SyncFile(_) => AnonInodeFs::new_path(|_| "anon_inode:sync_file".to_string()),
        };

        let access = if flags.read_write {
            AccessMode::O_RDWR
        } else {
            AccessMode::O_RDONLY
        };

        Self {
            export,
            wakeup: Arc::new(ExportWakeup {
                pollee: Pollee::new(),
                armed: SpinLock::new(BTreeSet::new()),
            }),
            common: FileCommon::new(pseudo_path, access, StatusFlags::empty()),
        }
    }

    /// Returns the object behind the descriptor.
    fn export(&self) -> DrmExport {
        self.export.clone()
    }

    /// Reports readiness the way Linux `dma_buf_poll` does.
    ///
    /// `EPOLLIN` means a *reader* may proceed, so it waits only on the
    /// exclusive fence; `EPOLLOUT` means a *writer* may proceed, so it waits
    /// on the exclusive fence and every shared one as well. That is exactly
    /// the readers/writer split [`FenceSet`] already documents, so the two
    /// dependency queries map onto the two events one to one.
    ///
    /// A buffer with no fences at all is idle, and an idle buffer is ready for
    /// both reading and writing.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-buf.c#L237>
    ///
    /// [`FenceSet`]: k1_musebook_drm::fence::FenceSet
    fn check_io_events(&self) -> IoEvents {
        let buffer = match &self.export {
            DrmExport::Buffer(buffer) => buffer,
            DrmExport::Sync(_) => return IoEvents::IN | IoEvents::OUT,
            // Reference: Linux v6.6 drivers/dma-buf/sync_file.c, sync_file_poll.
            // Readability is reported for every terminal status,
            // including an error, and never reports writability.
            DrmExport::SyncFile(fence) => {
                return if fence.is_signaled() {
                    IoEvents::IN
                } else {
                    IoEvents::empty()
                };
            }
        };

        let fences = buffer.fences();
        let mut events = IoEvents::empty();
        if fences.read_dependencies().is_empty() {
            events |= IoEvents::IN;
        }
        if fences.write_dependencies().is_empty() {
            events |= IoEvents::OUT;
        }
        events
    }

    /// Subscribes without holding the subscription lock across fence callbacks.
    /// Returns false if a pending fence cannot provide completion notifications.
    fn arm_wakeup(&self) -> bool {
        let pending = match &self.export {
            DrmExport::Buffer(buffer) => buffer.fences().write_dependencies(),
            DrmExport::SyncFile(fence) if !fence.is_signaled() => vec![fence.clone()],
            DrmExport::Sync(_) | DrmExport::SyncFile(_) => return true,
        };
        let mut supported = true;
        for fence in pending {
            let key = KeyableWeak::from(Arc::downgrade(&fence));
            {
                let mut armed = self.wakeup.armed.lock();
                armed.retain(|entry| entry.strong_count() != 0);
                if !armed.insert(key.clone()) {
                    continue;
                }
            }
            let wakeup = Arc::downgrade(&self.wakeup);
            let completed_key = key.clone();
            let callback: FenceCallback = Arc::new(move |_status| {
                if let Some(wakeup) = wakeup.upgrade() {
                    wakeup.armed.lock().remove(&completed_key);
                    wakeup.pollee.notify(IoEvents::IN | IoEvents::OUT);
                }
            });
            // Already-completed fences invoke the callback inline. Weak state
            // lets descriptor close release subscriptions without owning a fence.
            if fence.add_callback(callback).is_err() {
                self.wakeup.armed.lock().remove(&key);
                supported = false;
            }
        }
        supported
    }
}

impl Pollable for DrmExportFile {
    fn poll(&self, mask: IoEvents, poller: Option<&mut PollHandle>) -> IoEvents {
        // Deliberately not `Pollee::poll_with`: that caches until someone
        // calls `notify` or `invalidate`, and a fence signalling deep inside
        // `k1-musebook-drm` has no way to reach this descriptor to do either. The
        // fence set is cheap to read, so read it every time and let the pollee
        // carry only the wake-up.
        if let Some(poller) = poller {
            self.wakeup.pollee.register_poller(poller, mask);
        }

        let mut events = self.check_io_events();
        if !events.contains(IoEvents::IN | IoEvents::OUT) {
            let supported = self.arm_wakeup();
            // A fence may finish after the initial check but before callback
            // registration. In that case no future signal can wake this poller.
            events = self.check_io_events();
            if !supported {
                events |= IoEvents::ERR;
            }
        }

        events & (mask | IoEvents::ALWAYS_POLL)
    }
}

impl FileLike for DrmExportFile {
    /// Maps a shared buffer, starting at its first page.
    ///
    /// Unlike the DRM node, a dma-buf descriptor refers to exactly one buffer,
    /// so the file offset means what it says and there is no fake offset to
    /// decode. Everything else about the mapping matches the node's: pages are
    /// resolved on demand, and the buffer's own cache policy decides how they
    /// are installed. Preserve that cache policy to avoid conflicting CPU
    /// aliases to non-coherent device memory.
    ///
    /// A sync object has no memory, so mapping one is `ENODEV` by the trait's
    /// default.
    fn mappable(&self, _request: FileMmapRequest) -> Result<MappableObject<'_>> {
        if !matches!(self.export, DrmExport::Buffer(_)) {
            return_errno_with_message!(Errno::ENODEV, "a sync object cannot be mapped");
        }
        Ok(MappableObject::Device(self))
    }

    /// Reports a dma-buf's size the way Linux does.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/dma-buf/dma-buf.c#L170>
    /// (`dma_buf_llseek`). Only `offset == 0` with `SEEK_END` or `SEEK_SET` is
    /// accepted; `SEEK_CUR` and any non-zero offset are `EINVAL`. Mesa maps a
    /// failed `lseek` here to `VK_ERROR_INVALID_EXTERNAL_HANDLE`, which Zink
    /// then logs as `couldn't allocate memory`.
    fn seek(&self, seek_from: SeekFrom) -> Result<usize> {
        let DrmExport::Buffer(buffer) = &self.export else {
            return_errno_with_message!(Errno::ESPIPE, "a sync object does not support seeking");
        };

        match seek_from {
            SeekFrom::Start(0) => Ok(0),
            SeekFrom::End(0) => Ok(buffer.size()),
            SeekFrom::Start(_) | SeekFrom::End(_) | SeekFrom::Current(_) => {
                return_errno_with_message!(
                    Errno::EINVAL,
                    "dma-buf only accepts lseek(fd, 0, SEEK_SET) and lseek(fd, 0, SEEK_END)"
                )
            }
        }
    }

    fn dump_proc_fdinfo(self: Arc<Self>, fd_flags: FdFlags) -> Box<dyn Display> {
        struct FdInfo {
            inner: Arc<DrmExportFile>,
            fd_flags: FdFlags,
        }

        impl Display for FdInfo {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                let flags = self.inner.common().access_mode() as u32;
                writeln!(f, "pos:\t0")?;
                writeln!(f, "flags:\t0{:o}", flags)?;
                writeln!(f, "mnt_id:\t{}", AnonInodeFs::mount_node().id())?;
                writeln!(f, "ino:\t{}", AnonInodeFs::shared_inode().ino())?;
                writeln!(
                    f,
                    "fd_flags:\t0{:o}",
                    if self.fd_flags.contains(FdFlags::CLOEXEC) {
                        1
                    } else {
                        0
                    }
                )?;
                match &self.inner.export {
                    DrmExport::Buffer(buffer) => writeln!(f, "size:\t{}", buffer.size()),
                    DrmExport::Sync(_) | DrmExport::SyncFile(_) => Ok(()),
                }
            }
        }

        Box::new(FdInfo {
            inner: self,
            fd_flags,
        })
    }

    fn common(&self) -> &FileCommon {
        &self.common
    }
}

/// Resolves the pages of one dma-buf and retains it until the final VMA is gone,
/// independently of GEM handle and descriptor closure.
#[derive(Debug)]
struct DmaBufFaultHandler {
    buffer: Arc<k1_musebook_drm::prime::DmaBuf>,
}

impl DeviceFaultHandler for DmaBufFaultHandler {
    fn prepare_cpu_read(
        &self,
        req: &DeviceFaultRequest,
    ) -> core::result::Result<(), DeviceFaultError> {
        self.buffer
            .object()
            .prepare_cpu_read(req.pgoff())
            .map_err(|error| match error {
                DrmError::OutOfMemory => DeviceFaultError::OutOfMemory,
                _ => DeviceFaultError::OutOfBounds,
            })
    }

    fn fault(
        &self,
        req: &DeviceFaultRequest,
    ) -> core::result::Result<DeviceFaultResponse, DeviceFaultError> {
        let object = self.buffer.object();

        let frame = object
            .fault(req.offset() / PAGE_SIZE)
            .map_err(|error| match error {
                DrmError::OutOfMemory => DeviceFaultError::OutOfMemory,
                _ => DeviceFaultError::OutOfBounds,
            })?;

        Ok(DeviceFaultResponse::new(
            frame,
            object.buffer_use().cache_policy(),
        ))
    }
}

/// The calling process's descriptor table, as much of it as DRM needs.
pub(super) struct CurrentDescriptors;

impl Descriptors for CurrentDescriptors {
    fn install(&self, export: DrmExport, flags: ExportFlags) -> k1_musebook_drm::DrmResult<i32> {
        let file = DrmExportFile::new(export, flags);
        let fd_flags = if flags.cloexec {
            FdFlags::CLOEXEC
        } else {
            FdFlags::empty()
        };

        let current_task = Task::current().ok_or(DrmError::NoDevice)?;
        let thread_local = current_task.as_thread_local().ok_or(DrmError::NoDevice)?;

        let file_table = thread_local.borrow_file_table();
        let mut file_table_locked = file_table.unwrap().write();

        Ok(file_table_locked.insert(Arc::new(file), fd_flags).into())
    }

    fn resolve(&self, fd: i32) -> k1_musebook_drm::DrmResult<DrmExport> {
        let current_task = Task::current().ok_or(DrmError::NoDevice)?;
        let thread_local = current_task.as_thread_local().ok_or(DrmError::NoDevice)?;

        let file_table = thread_local.borrow_file_table();
        let file_table_locked = file_table.unwrap().read();

        // A negative or out-of-range number is not a descriptor at all, which
        // is the same `EINVAL` a closed one gets.
        let fd = FileDesc::try_from(fd).map_err(|_| DrmError::InvalidArgument)?;
        let file = file_table_locked
            .get_file(fd)
            .map_err(|_| DrmError::InvalidArgument)?;

        // Not a DRM export at all. `dma_buf_get` reports `EINVAL` for a
        // descriptor that is open but is not a dma-buf, not `ENOTTY`.
        file.downcast_ref::<DrmExportFile>()
            .map(DrmExportFile::export)
            .ok_or(DrmError::InvalidArgument)
    }

    fn eventfd(&self, fd: i32) -> k1_musebook_drm::DrmResult<Arc<dyn Eventfd>> {
        let current_task = Task::current().ok_or(DrmError::NoDevice)?;
        let thread_local = current_task.as_thread_local().ok_or(DrmError::NoDevice)?;

        let file_table = thread_local.borrow_file_table();
        let file_table_locked = file_table.unwrap().read();

        // Closed / out-of-range is `EBADF`, matching `eventfd_ctx_fdget`.
        // An open file that is not an eventfd is `EINVAL`, matching
        // `eventfd_ctx_fileget`. The two must stay distinct: smithay's
        // `supports_syncobj_eventfd` probe never reaches this lookup
        // (handle 0 fails first), but a compositor that hands the DRM
        // node to `SYNCOBJ_EVENTFD` by mistake must see `EINVAL`, not
        // "the ioctl does not exist".
        let fd = FileDesc::try_from(fd).map_err(|_| DrmError::BadFd)?;
        let file = file_table_locked
            .get_file(fd)
            .map_err(|_| DrmError::BadFd)?
            .clone();

        if file.downcast_ref::<EventFile>().is_none() {
            return Err(DrmError::InvalidArgument);
        }

        // Hold the `FileLike` rather than downcasting the `Arc`. There is
        // no safe `Arc<dyn FileLike> → Arc<EventFile>` conversion, and the
        // increment only needs `&EventFile` at signal time.
        Ok(Arc::new(KernelEventfd(file)))
    }
}

/// Increments a process eventfd on behalf of `k1-musebook-drm`.
///
/// Lives in this crate so [`k1_musebook_drm::prime::Eventfd`] can stay ignorant of
/// [`EventFile`]. The `Arc<dyn FileLike>` keeps the eventfd alive after the
/// caller closes its number, matching `eventfd_ctx_fdget`'s ref on the ctx.
struct KernelEventfd(Arc<dyn FileLike>);

impl Eventfd for KernelEventfd {
    fn signal(&self) {
        if let Some(file) = self.0.downcast_ref::<EventFile>() {
            file.signal();
        }
    }
}

/// Puts the calling thread to sleep on behalf of a fence wait.
pub(super) struct CurrentBlocker;

impl Blocker for CurrentBlocker {
    fn now_ns(&self) -> u64 {
        u64::try_from(MonotonicClock::get().read_time().as_nanos()).unwrap_or(u64::MAX)
    }

    /// Sleeps until the fences are ready, the deadline passes, or a signal
    /// arrives.
    ///
    /// The wait is always interruptible in practice, whatever the caller asks
    /// for: `pause_until_or_timeout` is the only primitive that both times out
    /// and notices signals, and an uninterruptible wait on a wedged GPU makes
    /// the calling process unkillable.
    fn block(
        &self,
        queue: &WaitQueue,
        ready: &mut dyn FnMut() -> bool,
        deadline: Deadline,
        _interruptible: bool,
    ) -> k1_musebook_drm::DrmResult<()> {
        let condition = || ready().then_some(());

        // The ioctls carry an *absolute* deadline and the wait primitive takes
        // a relative one, so the conversion happens here, against the same
        // clock `now_ns` reports. A deadline already in the past becomes a
        // zero-length wait rather than an unbounded one.
        let timeout = match deadline {
            Deadline::Never => None,
            Deadline::Immediate => Some(Duration::ZERO),
            Deadline::At(deadline) => {
                Some(Duration::from_nanos(deadline.saturating_sub(self.now_ns())))
            }
        };

        let result = match timeout {
            Some(timeout) => queue.pause_until_or_timeout(condition, &timeout),
            None => queue.pause_until(condition),
        };

        result.map_err(|error| match error.error() {
            Errno::EINTR | Errno::ERESTARTSYS => DrmError::Interrupted,
            // `ETIME`, not `ETIMEDOUT`: `drm_syncobj_array_wait_timeout`
            // reports the former and Vulkan turns exactly that into
            // `VK_TIMEOUT`.
            _ => DrmError::TimeExpired,
        })
    }
}

#[cfg(ktest)]
mod test {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use k1_musebook_drm::{
        fence::{FenceContext, FenceStatus, FenceWaiter, SoftwareFence},
        gem::{BufferUse, ShmemObject},
        prime::{DmaBuf, foreign_buffer_for_test},
    };
    use ostd::prelude::ktest;

    use super::*;

    struct CompletionObserver(Arc<AtomicUsize>);

    impl crate::events::Observer<IoEvents> for CompletionObserver {
        fn on_events(&self, _events: &IoEvents) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn completion_observer() -> (
        Arc<AtomicUsize>,
        crate::process::signal::PollAdaptor<CompletionObserver>,
    ) {
        let count = Arc::new(AtomicUsize::new(0));
        let observer =
            crate::process::signal::PollAdaptor::with_observer(CompletionObserver(count.clone()));
        (count, observer)
    }

    fn exported_file(export: DrmExport) -> DrmExportFile {
        crate::time::clocks::init_for_ktest();
        DrmExportFile::new(export, ExportFlags::SYNC)
    }

    fn exported_buffer() -> (Arc<DmaBuf>, DrmExportFile) {
        let object = ShmemObject::new(PAGE_SIZE, BufferUse::Render).unwrap();
        let buffer = foreign_buffer_for_test(object);
        let file = exported_file(DrmExport::Buffer(buffer.clone()));
        (buffer, file)
    }

    fn pending_fence() -> Arc<SoftwareFence> {
        SoftwareFence::new(FenceContext::new(), 1)
    }

    /// An idle dma-buf must be both readable and writable. Smithay's
    /// implicit-sync path polls for `POLLIN` before applying a surface commit;
    /// reporting only `OUT` would leave the client waiting for its callback.
    #[ktest]
    fn an_idle_buffer_is_readable_and_writable() {
        let (_buffer, file) = exported_buffer();

        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(events.contains(IoEvents::IN), "an idle dma-buf must read");
        assert!(events.contains(IoEvents::OUT), "an idle dma-buf must write");
    }

    /// `EPOLLIN` means a reader may proceed, so it waits on the write fence
    /// alone; `EPOLLOUT` means a writer may proceed, so it also waits on the
    /// reads still in flight. Matches `dma_buf_poll`.
    #[ktest]
    fn a_read_in_flight_blocks_only_writers() {
        let (buffer, file) = exported_buffer();
        let read = pending_fence();
        buffer.fences().add_read(read.clone() as Arc<dyn Fence>);

        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(
            events.contains(IoEvents::IN),
            "a reader waits on writes only"
        );
        assert!(
            !events.contains(IoEvents::OUT),
            "a writer waits on the read"
        );

        read.signal(None);
        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(events.contains(IoEvents::OUT));
    }

    #[ktest]
    fn a_write_in_flight_blocks_both() {
        let (buffer, file) = exported_buffer();
        let write = pending_fence();
        buffer.fences().add_write(write.clone() as Arc<dyn Fence>);

        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(events.is_empty(), "an unsignalled write blocks both");

        write.signal(None);
        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(events.contains(IoEvents::IN | IoEvents::OUT));
    }

    /// A sync object has no buffer fences, so it must not look like a dma-buf
    /// that is permanently busy.
    #[ktest]
    fn a_sync_object_descriptor_is_always_ready() {
        let file = exported_file(DrmExport::Sync(k1_musebook_drm::fence::SyncObj::new(false)));

        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(events.contains(IoEvents::IN | IoEvents::OUT));
    }

    /// Repeated polling must register only one callback per pending fence.
    #[ktest]
    fn repeated_polls_do_not_pile_callbacks_on_one_fence() {
        let (buffer, file) = exported_buffer();
        let write = pending_fence();
        buffer.fences().add_write(write.clone() as Arc<dyn Fence>);

        let (count, mut observer) = completion_observer();
        for _ in 0..16 {
            let _ = file.poll(IoEvents::IN, Some(observer.as_handle_mut()));
        }

        assert!(write.signal(None));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[ktest]
    fn a_sync_file_waits_for_its_snapshot_fence() {
        for error in [None, Some(DrmError::Io)] {
            let fence = pending_fence();
            let file = exported_file(DrmExport::SyncFile(fence.clone()));
            assert!(file.poll(IoEvents::IN | IoEvents::OUT, None).is_empty());
            fence.signal(error);
            assert_eq!(file.poll(IoEvents::IN | IoEvents::OUT, None), IoEvents::IN);
        }
    }

    #[ktest]
    fn a_new_write_can_arm_while_an_older_read_is_pending() {
        let (buffer, file) = exported_buffer();
        let read = pending_fence();
        buffer.fences().add_read(read.clone());
        let (count, mut observer) = completion_observer();
        let _ = file.poll(IoEvents::OUT, Some(observer.as_handle_mut()));
        let write = pending_fence();
        buffer.fences().add_write(write.clone());
        assert!(
            file.poll(IoEvents::IN, Some(observer.as_handle_mut()))
                .is_empty()
        );
        write.signal(None);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(file.poll(IoEvents::IN, None), IoEvents::IN);
        assert!(!read.is_signaled());
    }

    #[ktest]
    fn pending_callbacks_do_not_keep_a_closed_descriptor_alive() {
        let fence = pending_fence();
        let file = exported_file(DrmExport::SyncFile(fence.clone()));
        let weak = Arc::downgrade(&file.wakeup);
        let _ = file.poll(IoEvents::IN, None);
        drop(file);
        assert!(weak.upgrade().is_none());
        fence.signal(None);
    }

    /// Completes in the gap between poll's readiness check and wakeup arming.
    #[derive(Debug)]
    struct CompletesDuringArm {
        inner: Arc<SoftwareFence>,
        queries: AtomicUsize,
    }

    impl Fence for CompletesDuringArm {
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn context(&self) -> FenceContext {
            self.inner.context()
        }
        fn seqno(&self) -> u64 {
            self.inner.seqno()
        }
        fn status(&self) -> FenceStatus {
            // check_io_events queries the exclusive fence for IN and OUT.
            // The next query comes from arm_wakeup's pending snapshot.
            if self.queries.fetch_add(1, Ordering::Relaxed) == 2 {
                self.inner.signal(None);
            }
            self.inner.status()
        }
        fn attach(&self, waiter: &Arc<FenceWaiter>, index: usize) -> bool {
            self.inner.attach(waiter, index)
        }
        fn detach(&self, waiter: &Arc<FenceWaiter>) {
            self.inner.detach(waiter);
        }
        fn waiter_count(&self) -> usize {
            self.inner.waiter_count()
        }
        fn add_callback(&self, callback: FenceCallback) -> k1_musebook_drm::DrmResult<()> {
            self.inner.add_callback(callback)
        }
    }

    #[ktest]
    fn completion_before_callback_registration_is_ready() {
        let (buffer, file) = exported_buffer();
        let fence = Arc::new(CompletesDuringArm {
            inner: pending_fence(),
            queries: AtomicUsize::new(0),
        });
        buffer.fences().add_write(fence.clone());
        let events = file.poll(IoEvents::IN | IoEvents::OUT, None);
        assert!(
            fence.inner.is_signaled(),
            "the fence completed during arming"
        );
        assert!(fence.queries.load(Ordering::Relaxed) >= 3);
        assert!(
            events.contains(IoEvents::IN | IoEvents::OUT),
            "returning the initial empty snapshot would strand the poller"
        );
        assert!(file.wakeup.armed.lock().is_empty());
    }
}

impl Mappable for DrmExportFile {
    fn supports_private_mapping(&self) -> bool {
        true
    }

    fn map(&self, offset: usize, handle: MapHandle) -> Result<Box<dyn MappedObject>> {
        let DrmExport::Buffer(buffer) = &self.export else {
            return_errno_with_message!(Errno::ENODEV, "a sync object cannot be mapped");
        };
        if offset > buffer.size() || handle.size() > buffer.size() - offset {
            return_errno_with_message!(Errno::EINVAL, "the mmap range is outside the dma-buf");
        }
        Ok(on_demand::map(
            Arc::new(DmaBufFaultHandler {
                buffer: buffer.clone(),
            }),
            offset,
        ))
    }
}
