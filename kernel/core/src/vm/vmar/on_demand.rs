// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: demand-paged GEM/device mmap (Mesa/COSMIC).

//! Demand paging for RAM-backed device mappings.
//!
//! Each VMA retains a handler for one object. Drivers authorize the complete
//! mmap range before creating the binding; fork and split clone the handler
//! reference. Faults populate only the pages that user space actually touches.
//!
//! Lock order is VMAR read lock, page-table lock, then the handler's leaf locks.
//! `fault` and `page_mkwrite` run with preemption disabled. They must not sleep,
//! use a blocking allocator, or reenter VMAR. The page-table lock serializes
//! faults within one address space; handlers must also resolve the same offset
//! to the same frame across different address spaces.
//!
//! Shared mappings install the object's pages. Private mappings copy on write
//! and never notify the object of a private write. Missing backing and handler
//! allocation failure produce SIGBUS; ordinary VM failures keep their exception
//! signal. Device mappings cannot grow or discard object pages through madvise.
//!
//! Object pages retain the handler's cache policy. Before a private copy reads
//! the cached kernel alias, the handler synchronizes its retained DMA owner.
//! Private copies use writeback CPU memory and carry a software PTE bit, so fork
//! and mprotect cannot confuse them with original writeback object pages.
//! Noncoherent object mappings use write-combining plus driver I/O barriers.
//! A writeback handler owns cache synchronization;
//! write notifications identify potentially dirty pages and cannot be rearmed
//! after a page becomes writable, so they are not a per-submission dirty log.

use core::fmt::Debug;

use ostd::{
    mm::{
        CachePolicy, PageFlags, PageProperty, UFrame, VmSpace, tlb::TlbFlushOp,
        vm_space::VmQueriedItem,
    },
    task::disable_preempt,
};

use super::{MapHandle, vm_mapping::duplicate_frame};
use crate::{
    fs::file::MappedObject,
    prelude::*,
    vm::{
        perms::VmPerms,
        vmar::{PageFaultFailure, PageFaultSignal},
    },
};

/// Software PTE bit owned by demand-paged private copies. Unlike cache policy,
/// it distinguishes original WB object pages from already-copied WB RAM pages.
/// Fork retains it; mprotect preserves precisely this bit for demand mappings.
pub(super) const PRIVATE_COPY: PageFlags = PageFlags::AVAIL2;

/// Missing device backing. Both variants become SIGBUS with distinct kernel errnos.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(any(target_arch = "riscv64", ktest)), expect(dead_code))]
pub(crate) enum DeviceFaultError {
    /// The faulting page lies outside the object behind the device file.
    ///
    /// This is a user space bug: the mapping is larger than the object, or the
    /// object shrank. Carries [`Errno::EFAULT`]; reported as `SIGBUS`.
    OutOfBounds,
    /// Backing memory for the page could not be allocated.
    ///
    /// This is a resource problem, not a program error, and a program may
    /// legitimately catch it. Carries [`Errno::ENOMEM`]; reported as `SIGBUS`.
    OutOfMemory,
}

#[cfg(ktest)]
impl DeviceFaultError {
    /// Returns the errno that this failure is reported as.
    ///
    /// This is what in-kernel callers and the fault log see. It is *not* what
    /// decides the signal: the conversion to [`PageFaultFailure`] does, and it
    /// always picks [`PageFaultSignal::Bus`]. See the type-level
    /// documentation.
    pub(crate) const fn errno(self) -> Errno {
        match self {
            Self::OutOfBounds => Errno::EFAULT,
            Self::OutOfMemory => Errno::ENOMEM,
        }
    }
}

impl From<DeviceFaultError> for Error {
    fn from(err: DeviceFaultError) -> Self {
        match err {
            DeviceFaultError::OutOfBounds => Error::with_message(
                Errno::EFAULT,
                "the faulting page is outside the mapped device object",
            ),
            DeviceFaultError::OutOfMemory => Error::with_message(
                Errno::ENOMEM,
                "the device object could not allocate a frame for the faulting page",
            ),
        }
    }
}

impl From<DeviceFaultError> for PageFaultFailure {
    /// Marks the failure as `SIGBUS`, whichever variant it is.
    ///
    /// This is the single place that turns a handler's error into a signal, so
    /// there is no way for a handler to end up delivering `SIGSEGV` by
    /// accident.
    fn from(err: DeviceFaultError) -> Self {
        PageFaultFailure::new(Error::from(err), PageFaultSignal::Bus)
    }
}

/// A description of the page that needs to be resolved.
///
/// This is the equivalent of Linux's `struct vm_fault`, cut down to the fields
/// a device object can actually act on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeviceFaultRequest {
    offset: usize,
    address: Vaddr,
    access: VmPerms,
    is_shared: bool,
}

// Some accessors are used only by tests or configuration-dependent device mappings.
#[cfg_attr(not(ktest), expect(dead_code))]
impl DeviceFaultRequest {
    /// Creates a fault request.
    ///
    /// `offset` is the page-aligned byte offset within the mapped object and
    /// `address` is the page-aligned faulting virtual address. `access`
    /// contains only the `READ`/`WRITE`/`EXEC` bits actually required by the
    /// faulting memory access.
    ///
    /// This constructor is public so that a [`DeviceFaultHandler`] can be unit
    /// tested without a page table.
    pub(crate) fn new(offset: usize, address: Vaddr, access: VmPerms, is_shared: bool) -> Self {
        debug_assert!(offset.is_multiple_of(PAGE_SIZE));
        debug_assert!(address.is_multiple_of(PAGE_SIZE));

        Self {
            offset,
            address,
            access,
            is_shared,
        }
    }

    /// Returns the page-aligned byte offset within the retained object.
    /// It combines the binding's object offset with the page's distance from
    /// the VMA start. The DRM fake-offset namespace was resolved at mmap time.
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the index of the faulting page within the mapped object.
    ///
    /// This is [`Self::offset`] divided by the page size, i.e. Linux's
    /// `vmf->pgoff`.
    pub(crate) fn pgoff(&self) -> usize {
        self.offset / PAGE_SIZE
    }

    /// Returns the page-aligned virtual address that faulted.
    ///
    /// Only useful for diagnostics: an object must not key anything on it,
    /// because the same object can be mapped at different addresses in
    /// different processes.
    pub(crate) fn address(&self) -> Vaddr {
        self.address
    }

    /// Returns the access that caused the fault.
    ///
    /// Contains only `READ`, `WRITE`, and/or `EXEC`.
    pub(crate) fn access(&self) -> VmPerms {
        self.access
    }

    /// Returns whether the faulting access is a write.
    ///
    /// Together with [`DeviceFaultHandler::page_mkwrite`], this is how a
    /// handler learns which pages the CPU may have dirtied. See the module
    /// documentation on cache coherency.
    pub(crate) fn is_write(&self) -> bool {
        self.access.contains(VmPerms::WRITE)
    }

    /// Returns whether the faulting mapping is `MAP_SHARED`.
    ///
    /// Writes through a private (`MAP_PRIVATE`) mapping are copied on write
    /// and never reach the object, so a handler must not treat a write fault
    /// on a private mapping as the object being dirtied.
    pub(crate) fn is_shared(&self) -> bool {
        self.is_shared
    }
}

/// The frame that backs a faulting page, and how to map it.
#[derive(Debug)]
pub(crate) struct DeviceFaultResponse {
    frame: UFrame,
    cache: CachePolicy,
}

// Some accessors are used only by tests or configuration-dependent device mappings.
#[cfg_attr(not(ktest), expect(dead_code))]
impl DeviceFaultResponse {
    /// Creates a response backing the faulting page with `frame`, mapped with
    /// `cache`.
    ///
    /// On K1 the right choice is almost always
    /// [`CachePolicy::WriteCombining`]: the GPU is not cache-coherent, and an
    /// uncached mapping removes the need for any cache maintenance on the CPU
    /// side. See the module documentation.
    pub(crate) fn new(frame: UFrame, cache: CachePolicy) -> Self {
        Self { frame, cache }
    }

    /// Returns the backing frame.
    pub(crate) fn frame(&self) -> &UFrame {
        &self.frame
    }

    /// Returns the cache policy to map the frame with.
    pub(crate) fn cache_policy(&self) -> CachePolicy {
        self.cache
    }

    fn into_parts(self) -> (UFrame, CachePolicy) {
        (self.frame, self.cache)
    }
}

/// Provides physical frames for a demand-paged device mapping.
///
/// A device file registers one of these by calling [`map`] from its
/// [`Mappable::map`](crate::fs::file::Mappable::map) implementation. The VM
/// layer then maps nothing at `mmap` time and calls back here per page.
///
/// Read the module documentation before implementing this trait: it specifies
/// the lock order, what a handler may not do while it is being called, what
/// concurrency the VM layer already takes care of, and who owns the memory
/// once the mapping goes away.
pub(crate) trait DeviceFaultHandler: Send + Sync + Debug {
    /// Resolves a page of the object.
    ///
    /// Returns the frame that backs `req` and the cache policy to map it with.
    /// The frame is reference counted; a handler that wants the page to
    /// survive `munmap`, or to be shared between processes, keeps its own
    /// clone and returns another.
    ///
    /// Called with preemption disabled and with the page table lock of the
    /// faulting page held. It must not sleep, block, or re-enter the VM layer.
    ///
    /// # Errors
    ///
    /// - [`DeviceFaultError::OutOfBounds`] if `req` is beyond the object.
    /// - [`DeviceFaultError::OutOfMemory`] if a frame could not be allocated.
    fn fault(
        &self,
        req: &DeviceFaultRequest,
    ) -> core::result::Result<DeviceFaultResponse, DeviceFaultError>;

    /// Prepares an original object page for a cached kernel copy. This has the
    /// same nonblocking constraints as `fault`; private copies never call it.
    fn prepare_cpu_read(
        &self,
        _req: &DeviceFaultRequest,
    ) -> core::result::Result<(), DeviceFaultError> {
        Ok(())
    }

    /// Returns whether the first CPU write to each page should be reported.
    ///
    /// When this returns `true`, a read fault installs a read-only page table
    /// entry, so that the first write to that page faults again and calls
    /// [`Self::page_mkwrite`]. That costs one extra fault per page that is
    /// read before it is written, and buys the handler a record of which pages
    /// the CPU has dirtied.
    ///
    /// Only useful for handlers that map their pages writeback and do their
    /// own cache maintenance. A handler that maps write-combining does not
    /// need this. The default is `false`.
    fn wants_write_notify(&self) -> bool {
        false
    }

    /// Reports that the CPU is about to write to a page for the first time.
    ///
    /// Called only when [`Self::wants_write_notify`] returns `true`, and only
    /// for shared mappings: a write to a private mapping is copied on write
    /// and never reaches the object. Called before the page table entry is
    /// made writable, so returning an error leaves the page read-only and
    /// fails the access.
    ///
    /// Same restrictions as [`Self::fault`]: no sleeping, no blocking, no
    /// re-entering the VM layer.
    ///
    /// May be called more than once for the same page: `fork` write-protects
    /// the parent's entries, so the next write in either process reports the
    /// page again. Treat this as "the page is dirty", not as a counter.
    ///
    /// The default implementation does nothing.
    fn page_mkwrite(&self, req: &DeviceFaultRequest) -> core::result::Result<(), DeviceFaultError> {
        let _ = req;
        Ok(())
    }
}

/// The demand-paged device object bound to a [`VmMapping`].
///
/// [`VmMapping`]: super::vm_mapping::VmMapping
#[derive(Debug)]
pub(in crate::vm) struct MappedDevice {
    handler: Arc<dyn DeviceFaultHandler>,
    /// The byte offset within the object that the start of the mapping
    /// corresponds to, i.e. `mmap`'s `offset` argument.
    offset: usize,
}

impl MappedDevice {
    /// Retains a handler whose device has already validated the mapping range.
    pub(in crate::vm) fn new(handler: Arc<dyn DeviceFaultHandler>, offset: usize) -> Self {
        debug_assert!(offset.is_multiple_of(PAGE_SIZE));
        Self { handler, offset }
    }

    /// Returns the offset within the object of the start of the mapping.
    pub(in crate::vm) fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the registered handler.
    pub(in crate::vm) fn handler(&self) -> &Arc<dyn DeviceFaultHandler> {
        &self.handler
    }

    /// Duplicates the binding, keeping the same offset.
    #[cfg(ktest)]
    pub(in crate::vm) fn dup(&self) -> Self {
        self.dup_at_offset(self.offset)
    }

    /// Fork and split retain the same object at an adjusted offset.
    pub(in crate::vm) fn dup_at_offset(&self, offset: usize) -> Self {
        Self {
            handler: self.handler.clone(),
            offset,
        }
    }

    /// Returns whether both bindings refer to the same handler instance.
    #[cfg(ktest)]
    pub(in crate::vm) fn has_same_handler(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.handler, &other.handler)
    }
}

/// Creates the owned mapping returned by a demand-paged device file.
#[cfg_attr(not(any(target_arch = "riscv64", ktest)), expect(dead_code))]
pub(crate) fn map(handler: Arc<dyn DeviceFaultHandler>, offset: usize) -> Box<dyn MappedObject> {
    Box::new(MappedDevice::new(handler, offset))
}

impl MappedObject for MappedDevice {
    fn dup_at_offset(&self, offset: usize) -> Box<dyn MappedObject> {
        // MappedObject's offset is relative to this VMA, while the binding
        // stores an offset within the retained device object.
        let absolute = self
            .offset
            .checked_add(offset)
            .expect("device mapping offset overflow");
        Box::new(MappedDevice::dup_at_offset(self, absolute))
    }

    fn needs_write_faults(&self) -> bool {
        true
    }

    fn handle_page_fault(&self, _offset: usize, mut handle: MapHandle) -> Result<()> {
        handle.handle_device_fault(self)
    }
}

/// Resolves one page fault in a demand-paged device mapping.
///
/// Returns the change in the number of resident pages, so that the caller can
/// update the RSS counters: `1` when a page was newly installed and `0`
/// otherwise.
///
/// A [`DeviceFaultError`] from the handler becomes a [`PageFaultFailure`]
/// carrying [`PageFaultSignal::Bus`]; the failures this function raises on its
/// own do not, because they are not "the page has no backing store".
///
/// The parameters describe the mapping that contains the fault: `map_to_addr`
/// is its start address, `map_perms` the permissions of all of its pages, and
/// `is_shared` whether it is `MAP_SHARED`. `page_aligned_addr` is the faulting
/// page and `required_perms` the access that faulted; the caller has already
/// checked `required_perms` against `map_perms`.
///
/// This function is deliberately separate from the VMO and anonymous fault
/// paths rather than woven into them. The eager device path and the page cache
/// path keep working exactly as before, and this one can be tested against a
/// bare [`VmSpace`].
pub(in crate::vm) fn handle_device_fault(
    vm_space: &VmSpace,
    device: &MappedDevice,
    map_to_addr: Vaddr,
    map_perms: VmPerms,
    is_shared: bool,
    page_aligned_addr: Vaddr,
    required_perms: VmPerms,
) -> core::result::Result<isize, PageFaultFailure> {
    debug_assert!(page_aligned_addr.is_multiple_of(PAGE_SIZE));
    debug_assert!(page_aligned_addr >= map_to_addr);

    let req = DeviceFaultRequest::new(
        device
            .offset()
            .checked_add(page_aligned_addr - map_to_addr)
            .ok_or(DeviceFaultError::OutOfBounds)?,
        page_aligned_addr,
        required_perms,
        is_shared,
    );
    let is_write = req.is_write();
    let is_shared = req.is_shared();

    let preempt_guard = disable_preempt();
    let mut cursor = vm_space.cursor_mut(
        &preempt_guard,
        &(page_aligned_addr..page_aligned_addr + PAGE_SIZE),
    )?;

    let (va, item) = cursor.query().unwrap();
    match item {
        Some(VmQueriedItem::MappedRam { frame, mut prop }) => {
            if VmPerms::from(prop.flags).contains(required_perms) {
                // Another thread resolved this fault while we were waiting for
                // the page table lock. Only our TLB is stale.
                TlbFlushOp::for_range(va).perform_on_current();
                return Ok(0);
            }

            if !is_write {
                // The entry is only ever installed with fewer permissions than
                // the mapping for the sake of write notification, so a
                // non-write access cannot legitimately land here. Be lenient
                // rather than panicking: retry the access.
                TlbFlushOp::for_range(va).perform_on_current();
                return Ok(0);
            }

            let new_flags = PageFlags::W | PageFlags::ACCESSED | PageFlags::DIRTY;

            if is_shared {
                // The page was installed read-only to catch this write. Tell
                // the object before user space can dirty the page, so that a
                // handler doing its own cache maintenance never misses a page.
                device.handler().page_mkwrite(&req)?;

                cursor.protect_next(PAGE_SIZE, |flags, _cache| {
                    *flags |= new_flags;
                });
                cursor.flusher().issue_tlb_flush(TlbFlushOp::for_range(va));
                cursor.flusher().dispatch_tlb_flush();
            } else {
                // A private mapping. The write must not reach the object, so
                // copy the page. The handler is intentionally not notified.
                if !prop.flags.contains(PRIVATE_COPY) {
                    device.handler().prepare_cpu_read(&req)?;
                }
                let new_frame = duplicate_frame(&frame)?;
                prop.flags |= new_flags | PRIVATE_COPY;
                prop.cache = CachePolicy::Writeback;
                cursor.unmap(PAGE_SIZE);
                cursor.jump(va.start).unwrap();
                cursor.map(new_frame.into(), prop);
            }
            cursor.flusher().sync_tlb_flush();

            Ok(0)
        }
        // An eagerly populated MMIO page cannot appear inside a demand-paged
        // mapping. If it does, the two paths got crossed. This is a kernel
        // bug rather than a missing page, so it is not a `SIGBUS`.
        Some(VmQueriedItem::MappedIoMem { .. }) => Err(Error::with_message(
            Errno::EFAULT,
            "an I/O memory page was found in a demand-paged device mapping",
        )
        .into()),
        None => {
            let (frame, cache) = device.handler().fault(&req)?.into_parts();

            let private_copy = !is_shared && is_write;
            let (frame, vm_perms, cache) = if private_copy {
                // A private mapping being written. Copy the page up front, so
                // that the write cannot reach the object.
                device.handler().prepare_cpu_read(&req)?;
                (
                    duplicate_frame(&frame)?.into(),
                    map_perms,
                    CachePolicy::Writeback,
                )
            } else if !is_shared || (!is_write && device.handler().wants_write_notify()) {
                // Install read-only so that the first write faults again: for a
                // private mapping to copy the page, and for a shared mapping to
                // report the write to the object.
                (frame, map_perms - VmPerms::WRITE, cache)
            } else {
                (frame, map_perms, cache)
            };

            let mut page_flags = PageFlags::from(vm_perms) | PageFlags::ACCESSED;
            if private_copy {
                page_flags |= PRIVATE_COPY;
            }
            if is_write {
                page_flags |= PageFlags::DIRTY;
            }

            cursor.map(frame, PageProperty::new_user(page_flags, cache));

            Ok(1)
        }
    }
}

#[cfg(ktest)]
mod test {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use ostd::{
        mm::{FrameAllocOptions, HasPaddr, Paddr, VmIo},
        prelude::ktest,
    };

    use super::*;
    use crate::thread::kernel_thread::ThreadOptions;

    /// Where the test mappings live. Any userspace-looking range works, as the
    /// tests drive a bare `VmSpace` rather than a `Vmar`.
    const MAP_BASE: Vaddr = 0x1_0000_0000;
    /// A deliberately oversized mapping: the point of demand paging is that
    /// only the touched pages of it ever get backed.
    const MAP_PAGES: usize = 64;
    /// The object behind the mapping is smaller than the mapping, so that
    /// faults past its end can be tested.
    const OBJECT_PAGES: usize = 16;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailWith {
        Nothing,
        OutOfBounds,
        OutOfMemory,
    }

    /// A minimal buffer object: allocates a frame per page on first fault and
    /// keeps a reference to it, like a GEM object would.
    #[derive(Debug)]
    struct TestObject {
        pages: SpinLock<BTreeMap<usize, UFrame>>,
        num_fault_calls: AtomicUsize,
        num_prepare_calls: AtomicUsize,
        prepared_byte: Option<u8>,
        fail_prepare: bool,
        num_mkwrite_calls: AtomicUsize,
        mkwrite_pgoffs: SpinLock<BTreeSet<usize>>,
        fail_with: FailWith,
        write_notify: bool,
        fail_mkwrite: bool,
        cache: CachePolicy,
        /// Whether to keep a reference to the handed-out frames. A handler
        /// that keeps none lets `munmap` free the memory.
        retain: bool,
    }

    /// How a [`TestObject`] should behave. The fields not spelled out by a
    /// test take the values in [`TestObjectOptions::default`].
    struct TestObjectOptions {
        prepared_byte: Option<u8>,
        fail_prepare: bool,
        fail_with: FailWith,
        write_notify: bool,
        fail_mkwrite: bool,
        cache: CachePolicy,
        retain: bool,
    }

    impl Default for TestObjectOptions {
        fn default() -> Self {
            Self {
                prepared_byte: None,
                fail_prepare: false,
                fail_with: FailWith::Nothing,
                write_notify: false,
                fail_mkwrite: false,
                cache: CachePolicy::Writeback,
                retain: true,
            }
        }
    }

    impl TestObject {
        fn new() -> Arc<Self> {
            Self::with_options(TestObjectOptions::default())
        }

        fn with_options(options: TestObjectOptions) -> Arc<Self> {
            Arc::new(Self {
                pages: SpinLock::new(BTreeMap::new()),
                num_fault_calls: AtomicUsize::new(0),
                num_prepare_calls: AtomicUsize::new(0),
                prepared_byte: options.prepared_byte,
                fail_prepare: options.fail_prepare,
                num_mkwrite_calls: AtomicUsize::new(0),
                mkwrite_pgoffs: SpinLock::new(BTreeSet::new()),
                fail_with: options.fail_with,
                write_notify: options.write_notify,
                fail_mkwrite: options.fail_mkwrite,
                cache: options.cache,
                retain: options.retain,
            })
        }

        fn num_fault_calls(&self) -> usize {
            self.num_fault_calls.load(Ordering::Relaxed)
        }

        fn num_mkwrite_calls(&self) -> usize {
            self.num_mkwrite_calls.load(Ordering::Relaxed)
        }

        fn paddr_of(&self, pgoff: usize) -> Option<Paddr> {
            self.pages.lock().get(&pgoff).map(|frame| frame.paddr())
        }

        fn refcount_of(&self, pgoff: usize) -> Option<u64> {
            self.pages
                .lock()
                .get(&pgoff)
                .map(|frame| frame.reference_count())
        }
    }

    impl DeviceFaultHandler for TestObject {
        fn prepare_cpu_read(
            &self,
            req: &DeviceFaultRequest,
        ) -> core::result::Result<(), DeviceFaultError> {
            self.num_prepare_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_prepare {
                return Err(DeviceFaultError::OutOfMemory);
            }
            if let Some(byte) = self.prepared_byte {
                self.pages
                    .lock()
                    .get(&req.pgoff())
                    .unwrap()
                    .write_val(0, &byte)
                    .unwrap();
            }
            Ok(())
        }

        fn fault(
            &self,
            req: &DeviceFaultRequest,
        ) -> core::result::Result<DeviceFaultResponse, DeviceFaultError> {
            self.num_fault_calls.fetch_add(1, Ordering::Relaxed);

            match self.fail_with {
                FailWith::Nothing => (),
                FailWith::OutOfBounds => return Err(DeviceFaultError::OutOfBounds),
                FailWith::OutOfMemory => return Err(DeviceFaultError::OutOfMemory),
            }

            if req.pgoff() >= OBJECT_PAGES {
                return Err(DeviceFaultError::OutOfBounds);
            }

            let mut pages = self.pages.lock();
            if let Some(frame) = pages.get(&req.pgoff()) {
                return Ok(DeviceFaultResponse::new(frame.clone(), self.cache));
            }

            let frame: UFrame = FrameAllocOptions::new()
                .alloc_frame()
                .map_err(|_| DeviceFaultError::OutOfMemory)?
                .into();
            if self.retain {
                pages.insert(req.pgoff(), frame.clone());
            }

            Ok(DeviceFaultResponse::new(frame, self.cache))
        }

        fn wants_write_notify(&self) -> bool {
            self.write_notify
        }

        fn page_mkwrite(
            &self,
            req: &DeviceFaultRequest,
        ) -> core::result::Result<(), DeviceFaultError> {
            self.num_mkwrite_calls.fetch_add(1, Ordering::Relaxed);
            self.mkwrite_pgoffs.lock().insert(req.pgoff());

            if self.fail_mkwrite {
                return Err(DeviceFaultError::OutOfMemory);
            }
            Ok(())
        }
    }

    fn page_addr(index: usize) -> Vaddr {
        MAP_BASE + index * PAGE_SIZE
    }

    fn touch(
        vm_space: &VmSpace,
        device: &MappedDevice,
        index: usize,
        perms: VmPerms,
        is_shared: bool,
    ) -> core::result::Result<isize, PageFaultFailure> {
        handle_device_fault(
            vm_space,
            device,
            MAP_BASE,
            VmPerms::READ | VmPerms::WRITE,
            is_shared,
            page_addr(index),
            perms,
        )
    }

    fn query_at(vm_space: &VmSpace, index: usize) -> Option<(Paddr, PageFlags, CachePolicy)> {
        let preempt_guard = disable_preempt();
        let range = page_addr(index)..page_addr(index) + PAGE_SIZE;
        let mut cursor = vm_space.cursor(&preempt_guard, &range).unwrap();
        match cursor.query().unwrap() {
            (_, Some(VmQueriedItem::MappedRam { frame, prop })) => {
                Some((frame.paddr(), prop.flags, prop.cache))
            }
            _ => None,
        }
    }

    fn num_mapped_pages(vm_space: &VmSpace) -> usize {
        (0..MAP_PAGES)
            .filter(|index| query_at(vm_space, *index).is_some())
            .count()
    }

    #[ktest]
    fn the_request_describes_the_faulting_page() {
        let req = DeviceFaultRequest::new(
            9 * PAGE_SIZE,
            MAP_BASE,
            VmPerms::READ | VmPerms::WRITE,
            true,
        );

        assert_eq!(req.offset(), 9 * PAGE_SIZE);
        assert_eq!(req.pgoff(), 9);
        assert_eq!(req.address(), MAP_BASE);
        assert_eq!(req.access(), VmPerms::READ | VmPerms::WRITE);
        assert!(req.is_write());
        assert!(req.is_shared());

        let read_only = DeviceFaultRequest::new(0, MAP_BASE, VmPerms::READ, false);
        assert!(!read_only.is_write());
        assert!(!read_only.is_shared());
    }

    #[ktest]
    fn the_response_carries_the_frame_and_its_cache_policy() {
        let frame: UFrame = FrameAllocOptions::new().alloc_frame().unwrap().into();
        let paddr = frame.paddr();

        let response = DeviceFaultResponse::new(frame, CachePolicy::WriteCombining);
        assert_eq!(response.frame().paddr(), paddr);
        assert_eq!(response.cache_policy(), CachePolicy::WriteCombining);
    }

    #[ktest]
    fn a_handler_can_be_handed_to_the_mapping_layer_as_a_mapped_object() {
        let object = TestObject::new();
        let mappable = map(object.clone(), 2 * PAGE_SIZE);
        assert!(mappable.needs_write_faults());
        assert!(mappable.dup_at_offset(PAGE_SIZE).needs_write_faults());
        assert_eq!(object.num_fault_calls(), 0);

        let device = MappedDevice::new(object.clone(), 2 * PAGE_SIZE);
        assert_eq!(device.offset(), 2 * PAGE_SIZE);
        assert!(device.has_same_handler(&device.dup()));

        let vm_space = VmSpace::new();
        touch(&vm_space, &device, 0, VmPerms::READ, true).unwrap();
        assert_eq!(object.num_fault_calls(), 1);
        assert!(object.paddr_of(2).is_some());
    }

    #[derive(Debug)]
    struct MappingOwner {
        last_offset: AtomicUsize,
    }

    impl DeviceFaultHandler for MappingOwner {
        fn fault(
            &self,
            req: &DeviceFaultRequest,
        ) -> core::result::Result<DeviceFaultResponse, DeviceFaultError> {
            self.last_offset.store(req.offset(), Ordering::Relaxed);
            Err(DeviceFaultError::OutOfBounds)
        }
    }

    #[ktest]
    fn fork_and_split_retain_the_object_and_adjust_fault_offsets() {
        let owner = Arc::new(MappingOwner {
            last_offset: AtomicUsize::new(0),
        });
        let weak = Arc::downgrade(&owner);
        let original = MappedDevice::new(owner.clone(), 7 * PAGE_SIZE);
        let fork = original.dup();
        let split = original.dup_at_offset(9 * PAGE_SIZE);
        let nested = split.dup_at_offset(10 * PAGE_SIZE);
        drop(owner);
        drop(original);
        drop(split);
        let space = VmSpace::new();
        assert!(touch(&space, &nested, 0, VmPerms::READ, true).is_err());
        assert_eq!(
            weak.upgrade().unwrap().last_offset.load(Ordering::Relaxed),
            10 * PAGE_SIZE
        );
        drop(nested);
        assert!(weak.upgrade().is_some());
        drop(fork);
        assert!(weak.upgrade().is_none());
    }

    #[ktest]
    fn mapping_without_access_allocates_nothing() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let _device = MappedDevice::new(object.clone(), 0);

        // Creating the mapping is all that has happened. No page of it may be
        // resident, and the object must not have been asked for anything: this
        // is what makes the mapping demand-paged rather than eager.
        assert_eq!(num_mapped_pages(&vm_space), 0);
        assert_eq!(object.num_fault_calls(), 0);
        assert!(object.pages.lock().is_empty());
    }

    #[ktest]
    fn first_access_allocates_one_page() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        assert_eq!(
            touch(&vm_space, &device, 3, VmPerms::READ, true).unwrap(),
            1
        );

        assert_eq!(object.num_fault_calls(), 1);
        assert_eq!(num_mapped_pages(&vm_space), 1);

        let (paddr, flags, _) = query_at(&vm_space, 3).unwrap();
        assert_eq!(paddr, object.paddr_of(3).unwrap());
        assert!(flags.contains(PageFlags::R));

        // The neighbours stay untouched: no fault-around for device mappings.
        assert!(query_at(&vm_space, 2).is_none());
        assert!(query_at(&vm_space, 4).is_none());
    }

    #[ktest]
    fn sparse_access_backs_only_touched_pages() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        let touched = [0usize, 5, 7, 11, 15];
        for index in touched {
            touch(&vm_space, &device, index, VmPerms::WRITE, true).unwrap();
        }

        assert_eq!(object.num_fault_calls(), touched.len());
        assert_eq!(num_mapped_pages(&vm_space), touched.len());
        for index in 0..MAP_PAGES {
            assert_eq!(
                query_at(&vm_space, index).is_some(),
                touched.contains(&index),
                "page {} has the wrong residency",
                index
            );
        }
    }

    #[ktest]
    fn repeated_access_does_not_reallocate() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        assert_eq!(
            touch(&vm_space, &device, 2, VmPerms::WRITE, true).unwrap(),
            1
        );
        let paddr = query_at(&vm_space, 2).unwrap().0;

        for _ in 0..8 {
            // Every later fault on the page finds it already resident and
            // returns without asking the object again.
            assert_eq!(
                touch(&vm_space, &device, 2, VmPerms::WRITE, true).unwrap(),
                0
            );
            assert_eq!(
                touch(&vm_space, &device, 2, VmPerms::READ, true).unwrap(),
                0
            );
        }

        assert_eq!(object.num_fault_calls(), 1);
        assert_eq!(query_at(&vm_space, 2).unwrap().0, paddr);
    }

    #[ktest]
    fn concurrent_access_to_one_page_allocates_once() {
        const NUM_THREADS: usize = 4;

        let vm_space = Arc::new(VmSpace::new());
        let object = TestObject::new();
        let device = Arc::new(MappedDevice::new(object.clone(), 0));

        let threads: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let vm_space = vm_space.clone();
                let device = device.clone();
                ThreadOptions::new(move || {
                    touch(&vm_space, &device, 9, VmPerms::WRITE, true).unwrap();
                })
                .spawn()
            })
            .collect();
        for thread in threads {
            thread.join();
        }

        // The page table lock of the faulting page serializes the racers, so
        // exactly one of them reaches the object and exactly one entry exists.
        assert_eq!(object.num_fault_calls(), 1);
        assert_eq!(num_mapped_pages(&vm_space), 1);
        assert_eq!(
            query_at(&vm_space, 9).unwrap().0,
            object.paddr_of(9).unwrap()
        );
    }

    #[ktest]
    fn unmap_releases_the_mappings_reference_only() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        for index in 0..4 {
            touch(&vm_space, &device, index, VmPerms::WRITE, true).unwrap();
        }

        // One reference from the object's page array, one from the page table.
        let mapped_refcount = object.refcount_of(0).unwrap();
        assert!(mapped_refcount >= 2);

        {
            let preempt_guard = disable_preempt();
            let range = page_addr(0)..page_addr(MAP_PAGES);
            let mut cursor = vm_space.cursor_mut(&preempt_guard, &range).unwrap();
            assert_eq!(cursor.unmap(range.len()), 4);
            cursor.flusher().sync_tlb_flush();
        }

        assert_eq!(num_mapped_pages(&vm_space), 0);

        // The object still holds the memory: a GEM handle or a PRIME fd in
        // another process must not lose its pages because one mapper went away.
        //
        // The page table hands its own reference to `RcuDrop`, so the reference
        // is gone from the page table already (asserted above) but the frame's
        // count only falls once a grace period elapses. Demanding an exact
        // count here would race that grace period, so the reuse check below is
        // what proves the pages were never freed.
        for index in 0..4 {
            assert!(object.paddr_of(index).is_some());
            assert!(object.refcount_of(index).unwrap() <= mapped_refcount);
        }

        // Touching again reuses the very same physical pages.
        let paddr_before = object.paddr_of(1).unwrap();
        touch(&vm_space, &device, 1, VmPerms::WRITE, true).unwrap();
        assert_eq!(query_at(&vm_space, 1).unwrap().0, paddr_before);
        assert_eq!(object.num_fault_calls(), 5);
    }

    #[ktest]
    fn unmap_frees_pages_the_object_does_not_keep() {
        let vm_space = VmSpace::new();
        let object = TestObject::with_options(TestObjectOptions {
            retain: false,
            ..Default::default()
        });
        let device = MappedDevice::new(object.clone(), 0);

        touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap();

        {
            let preempt_guard = disable_preempt();
            let range = page_addr(0)..page_addr(1);
            let mut cursor = vm_space.cursor_mut(&preempt_guard, &range).unwrap();
            assert_eq!(cursor.unmap(range.len()), 1);
            cursor.flusher().sync_tlb_flush();
        }

        // Nothing holds the frame any more, so the allocator may hand it back.
        // The object kept no record either, so the next touch has to allocate
        // from scratch.
        assert!(object.pages.lock().is_empty());
        touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap();
        assert_eq!(object.num_fault_calls(), 2);
    }

    #[ktest]
    fn out_of_bounds_access_reports_efault_and_sigbus() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        // Inside the mapping but past the end of the object.
        let err = touch(&vm_space, &device, OBJECT_PAGES, VmPerms::READ, true).unwrap_err();
        assert_eq!(err.error().error(), Errno::EFAULT);
        assert_eq!(DeviceFaultError::OutOfBounds.errno(), Errno::EFAULT);
        // Not `SIGSEGV`: the address is mapped, the page just has no backing
        // store. This is what `drm_gem_shmem_fault` reports too.
        assert_eq!(err.signal(), PageFaultSignal::Bus);

        // A failed fault must not leave a half-installed entry behind.
        assert_eq!(num_mapped_pages(&vm_space), 0);

        // An object that rejects a page it would otherwise own, because it
        // shrank, is reported the same way.
        let shrunk = TestObject::with_options(TestObjectOptions {
            fail_with: FailWith::OutOfBounds,
            ..Default::default()
        });
        let device = MappedDevice::new(shrunk, 0);
        let err = touch(&vm_space, &device, 0, VmPerms::READ, true).unwrap_err();
        assert_eq!(err.error().error(), Errno::EFAULT);
        assert_eq!(err.signal(), PageFaultSignal::Bus);
        assert_eq!(num_mapped_pages(&vm_space), 0);
    }

    #[ktest]
    fn allocation_failure_reports_enomem_and_sigbus() {
        let vm_space = VmSpace::new();
        let object = TestObject::with_options(TestObjectOptions {
            fail_with: FailWith::OutOfMemory,
            ..Default::default()
        });
        let device = MappedDevice::new(object.clone(), 0);

        let err = touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap_err();
        assert_eq!(err.error().error(), Errno::ENOMEM);
        assert_eq!(DeviceFaultError::OutOfMemory.errno(), Errno::ENOMEM);
        assert_eq!(err.signal(), PageFaultSignal::Bus);
        assert_eq!(num_mapped_pages(&vm_space), 0);
    }

    #[ktest]
    fn a_write_notification_failure_is_also_a_sigbus() {
        let vm_space = VmSpace::new();
        let object = TestObject::with_options(TestObjectOptions {
            write_notify: true,
            fail_mkwrite: true,
            ..Default::default()
        });
        let device = MappedDevice::new(object.clone(), 0);

        // The read fault succeeds and installs a read-only entry.
        touch(&vm_space, &device, 0, VmPerms::READ, true).unwrap();

        // The object then refuses to let the page be dirtied. The entry must
        // stay read-only and the write must be reported as `SIGBUS`, just like
        // a refusal from `fault`.
        let err = touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap_err();
        assert_eq!(err.signal(), PageFaultSignal::Bus);
        let (_, flags, _) = query_at(&vm_space, 0).unwrap();
        assert!(!flags.contains(PageFlags::W));
    }

    /// The failures that the VM layer raises on its own are kernel bugs rather
    /// than missing pages, so they keep the signal the CPU exception implies.
    #[ktest]
    fn a_plain_error_does_not_become_a_sigbus() {
        let failure = PageFaultFailure::from(Error::new(Errno::EFAULT));
        assert_eq!(failure.signal(), PageFaultSignal::FromException);
    }

    #[ktest]
    fn mapping_offset_is_added_to_the_page_offset() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        // The mapping starts four pages into the object, as it would after an
        // `mmap` with a non-zero offset.
        let device = MappedDevice::new(object.clone(), 4 * PAGE_SIZE);

        touch(&vm_space, &device, 1, VmPerms::READ, true).unwrap();

        assert!(object.paddr_of(5).is_some());
        assert!(object.paddr_of(1).is_none());

        // Page 12 of the mapping is page 16 of the object, which is past its
        // end even though it is well inside the mapping.
        let err = touch(&vm_space, &device, 12, VmPerms::READ, true).unwrap_err();
        assert_eq!(err.error().error(), Errno::EFAULT);
    }

    #[ktest]
    fn write_notification_reports_the_first_write() {
        let vm_space = VmSpace::new();
        let object = TestObject::with_options(TestObjectOptions {
            write_notify: true,
            ..Default::default()
        });
        let device = MappedDevice::new(object.clone(), 0);

        // A read fault installs a read-only entry so that the write is caught.
        touch(&vm_space, &device, 0, VmPerms::READ, true).unwrap();
        let (paddr, flags, _) = query_at(&vm_space, 0).unwrap();
        assert!(flags.contains(PageFlags::R));
        assert!(!flags.contains(PageFlags::W));
        assert_eq!(object.num_mkwrite_calls(), 0);

        // The first write upgrades it, after telling the object.
        assert_eq!(
            touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap(),
            0
        );
        let (paddr_after, flags, _) = query_at(&vm_space, 0).unwrap();
        assert_eq!(paddr_after, paddr, "a shared page must not be copied");
        assert!(flags.contains(PageFlags::W));
        assert_eq!(object.num_mkwrite_calls(), 1);
        assert!(object.mkwrite_pgoffs.lock().contains(&0));

        // Later writes are not reported again.
        touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap();
        assert_eq!(object.num_mkwrite_calls(), 1);
        assert_eq!(object.num_fault_calls(), 1);
    }

    #[ktest]
    fn write_faults_map_writable_without_notification() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap();
        let (_, flags, _) = query_at(&vm_space, 0).unwrap();
        assert!(flags.contains(PageFlags::W));
        assert!(flags.contains(PageFlags::DIRTY));

        // Reads on a shared mapping that does not want write notification also
        // get a writable entry, so no second fault is taken.
        touch(&vm_space, &device, 1, VmPerms::READ, true).unwrap();
        let (_, flags, _) = query_at(&vm_space, 1).unwrap();
        assert!(flags.contains(PageFlags::W));
        assert_eq!(object.num_mkwrite_calls(), 0);
    }

    #[ktest]
    fn private_mapping_copies_on_write() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        touch(&vm_space, &device, 0, VmPerms::READ, false).unwrap();
        let (paddr, flags, _) = query_at(&vm_space, 0).unwrap();
        assert_eq!(paddr, object.paddr_of(0).unwrap());
        assert!(!flags.contains(PageFlags::W));

        touch(&vm_space, &device, 0, VmPerms::WRITE, false).unwrap();
        let (paddr_after, flags, _) = query_at(&vm_space, 0).unwrap();
        assert!(flags.contains(PageFlags::W));
        assert_ne!(
            paddr_after,
            object.paddr_of(0).unwrap(),
            "a private write must not reach the object"
        );

        // The object is not told about a write it never received.
        assert_eq!(object.num_mkwrite_calls(), 0);
    }

    #[ktest]
    fn a_private_write_fault_copies_before_mapping() {
        let vm_space = VmSpace::new();
        let object = TestObject::new();
        let device = MappedDevice::new(object.clone(), 0);

        // Writing a page of a private mapping that was never read must not get
        // the object's own frame mapped writable, or the write would land in
        // the object.
        touch(&vm_space, &device, 0, VmPerms::WRITE, false).unwrap();

        let (paddr, flags, _) = query_at(&vm_space, 0).unwrap();
        assert!(flags.contains(PageFlags::W));
        assert_ne!(paddr, object.paddr_of(0).unwrap());
        assert_eq!(object.num_mkwrite_calls(), 0);
    }

    #[ktest]
    fn the_handlers_cache_policy_is_honoured() {
        let vm_space = VmSpace::new();
        let object = TestObject::with_options(TestObjectOptions {
            cache: CachePolicy::WriteCombining,
            ..Default::default()
        });
        let device = MappedDevice::new(object.clone(), 0);

        touch(&vm_space, &device, 0, VmPerms::WRITE, true).unwrap();

        // K1 has no hardware DMA coherency, so an object that wants the GPU to
        // see CPU stores without cache maintenance asks for an uncached
        // mapping and must get one.
        let (_, _, cache) = query_at(&vm_space, 0).unwrap();
        assert_eq!(cache, CachePolicy::WriteCombining);
    }
    fn mapped_byte(vm_space: &VmSpace, index: usize) -> u8 {
        let preempt_guard = disable_preempt();
        let range = page_addr(index)..page_addr(index) + PAGE_SIZE;
        let mut cursor = vm_space.cursor(&preempt_guard, &range).unwrap();
        let (_, Some(VmQueriedItem::MappedRam { frame, .. })) = cursor.query().unwrap() else {
            panic!("RAM mapping expected");
        };
        frame.read_val(0).unwrap()
    }

    #[ktest]
    fn original_pages_are_prepared_once_and_private_copies_are_writeback() {
        for cache in [CachePolicy::Writeback, CachePolicy::WriteCombining] {
            for read_first in [false, true] {
                let vm_space = VmSpace::new();
                let object = TestObject::with_options(TestObjectOptions {
                    cache,
                    prepared_byte: Some(0x5a),
                    ..Default::default()
                });
                let device = MappedDevice::new(object.clone(), 0);
                if read_first {
                    touch(&vm_space, &device, 0, VmPerms::READ, false).unwrap();
                    let (_, flags, mapped_cache) = query_at(&vm_space, 0).unwrap();
                    assert_eq!(mapped_cache, cache);
                    assert!(!flags.contains(PRIVATE_COPY));
                    assert_eq!(object.num_prepare_calls.load(Ordering::Relaxed), 0);
                }
                touch(&vm_space, &device, 0, VmPerms::WRITE, false).unwrap();
                let (_, flags, copied_cache) = query_at(&vm_space, 0).unwrap();
                assert_eq!(copied_cache, CachePolicy::Writeback);
                assert!(flags.contains(PRIVATE_COPY));
                assert_eq!(object.num_prepare_calls.load(Ordering::Relaxed), 1);
                assert_eq!(
                    mapped_byte(&vm_space, 0),
                    0x5a,
                    "preparation happens before the source is copied"
                );
                let shared = VmSpace::new();
                touch(&shared, &device, 0, VmPerms::WRITE, true).unwrap();
                let (_, flags, mapped_cache) = query_at(&shared, 0).unwrap();
                assert_eq!(mapped_cache, cache);
                assert!(!flags.contains(PRIVATE_COPY));
                assert_eq!(
                    object.num_prepare_calls.load(Ordering::Relaxed),
                    1,
                    "shared faults add no private-copy preparation"
                );
            }
        }
    }

    #[ktest]
    fn mprotect_and_fork_keep_private_copy_identity_without_resyncing_the_object() {
        use core::num::NonZeroUsize;

        use crate::vm::vmar::{
            MmapMode,
            vm_mapping::{MappedMemory, VmMapping},
            vmar_impls::cow_copy_pt_for_test,
        };
        for cache in [CachePolicy::Writeback, CachePolicy::WriteCombining] {
            let vm_space = VmSpace::new();
            let object = TestObject::with_options(TestObjectOptions {
                cache,
                prepared_byte: Some(0x5a),
                ..Default::default()
            });
            let device = MappedDevice::new(object.clone(), 0);
            touch(&vm_space, &device, 0, VmPerms::WRITE, false).unwrap();
            let original_copy = query_at(&vm_space, 0).unwrap().0;
            let mapping = VmMapping::new(
                NonZeroUsize::new(PAGE_SIZE).unwrap(),
                MAP_BASE,
                MappedMemory::Device(map(object.clone(), 0)),
                None,
                MmapMode::Private,
                false,
                VmPerms::READ | VmPerms::WRITE,
            );
            let mapping = mapping.protect(&vm_space, VmPerms::READ);
            assert!(query_at(&vm_space, 0).unwrap().1.contains(PRIVATE_COPY));
            let mapping = mapping.protect(&vm_space, VmPerms::READ | VmPerms::WRITE);
            assert!(query_at(&vm_space, 0).unwrap().1.contains(PRIVATE_COPY));
            {
                let guard = disable_preempt();
                let range = page_addr(0)..page_addr(1);
                let mut cursor = vm_space.cursor(&guard, &range).unwrap();
                let (_, Some(VmQueriedItem::MappedRam { frame, .. })) = cursor.query().unwrap()
                else {
                    panic!("private RAM copy expected");
                };
                frame.write_val(0, &0x6bu8).unwrap();
            }
            let child = VmSpace::new();
            {
                let guard = disable_preempt();
                let range = page_addr(0)..page_addr(1);
                let mut parent_cursor = vm_space.cursor_mut(&guard, &range).unwrap();
                let mut child_cursor = child.cursor_mut(&guard, &range).unwrap();
                assert_eq!(
                    cow_copy_pt_for_test(&mut parent_cursor, &mut child_cursor, PAGE_SIZE),
                    1
                );
            }
            assert!(query_at(&child, 0).unwrap().1.contains(PRIVATE_COPY));
            touch(&child, &device, 0, VmPerms::WRITE, false).unwrap();
            let (new_copy, flags, child_cache) = query_at(&child, 0).unwrap();
            assert_ne!(new_copy, original_copy);
            assert_eq!(child_cache, CachePolicy::Writeback);
            assert!(flags.contains(PRIVATE_COPY));
            assert_eq!(mapped_byte(&child, 0), 0x6b);
            assert_eq!(object.num_prepare_calls.load(Ordering::Relaxed), 1);
            mapping.unmap(&vm_space);
        }
    }

    #[ktest]
    fn failed_copy_preparation_keeps_the_original_page_unmodified() {
        for read_first in [false, true] {
            let vm_space = VmSpace::new();
            let object = TestObject::with_options(TestObjectOptions {
                fail_prepare: true,
                ..Default::default()
            });
            let device = MappedDevice::new(object.clone(), 0);
            if read_first {
                touch(&vm_space, &device, 0, VmPerms::READ, false).unwrap();
            }
            let error = touch(&vm_space, &device, 0, VmPerms::WRITE, false).unwrap_err();
            assert_eq!(error.signal(), PageFaultSignal::Bus);
            assert_eq!(error.error().error(), Errno::ENOMEM);
            if read_first {
                let (paddr, flags, _) = query_at(&vm_space, 0).unwrap();
                assert_eq!(paddr, object.paddr_of(0).unwrap());
                assert!(!flags.intersects(PageFlags::W | PRIVATE_COPY));
            } else {
                assert!(query_at(&vm_space, 0).is_none());
            }
        }
    }
}
