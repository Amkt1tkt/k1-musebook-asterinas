// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Tests for buffer sharing.
//!
//! The two properties worth the most attention are the equality ones: one
//! object exports to one buffer, and one buffer imports to one handle per
//! file. Both break silently — the buffer still works, the *sharing* stops
//! working — and both are what Mesa uses to recognize a buffer it already
//! holds.

use alloc::vec;

use ostd::{
    mm::{HasPaddr, PAGE_SIZE},
    prelude::ktest,
};
use ostd_pod::Pod;

use super::*;
use crate::{
    UserMem,
    device::{DriverFeatures, DriverInfo, DrmDriver, DrmMinor, DumbBuffer, DumbCreateArgs},
    drivers::soft::SoftDriver,
    fence::test::TestDescriptors,
    file::{DrmFile, Privilege},
    gem::{GemHandle, ShmemObject},
    ioctl::CallerContext,
    uapi::{DrmGemClose, DrmPrimeHandle, ioctl_nr, mode, sync::prime_flags},
};

/// Copies a structure into the byte payload the dispatcher takes.
fn payload_of<T: Pod>(value: &T) -> Vec<u8> {
    value.as_bytes().to_vec()
}

/// A [`UserMem`] that only offers a descriptor table.
///
/// None of the PRIME ioctls touches user memory outside its argument
/// structure, which the shell has already copied in, so handing them one that
/// would fault is how this file states that.
struct PrimeUser {
    descriptors: TestDescriptors,
}

impl PrimeUser {
    fn new() -> Self {
        Self {
            descriptors: TestDescriptors::new(),
        }
    }
}

impl PrimeUser {
    /// The caller context a handler is given, with everything this fixture can
    /// supply. Unlike a real in-kernel caller it has both halves, because the
    /// point of the fixture is to exercise the paths that need them.
    fn context(&self) -> CallerContext<'_> {
        CallerContext::new(self).with_descriptors(&self.descriptors)
    }
}

impl UserMem for PrimeUser {
    fn read_bytes(&self, _addr: u64, _buf: &mut [u8]) -> DrmResult<()> {
        Err(DrmError::BadAddress)
    }

    fn write_bytes(&self, _addr: u64, _buf: &[u8]) -> DrmResult<()> {
        Err(DrmError::BadAddress)
    }
}

/// A second software driver, so that a cross-device import has somewhere to
/// go.
///
/// On the K1 the two devices are the DPU and the GPU; here they are two
/// instances of the same virtual driver, which is enough because what
/// [`import`] compares is device identity.
#[derive(Debug)]
struct OtherDriver;

impl DrmDriver for OtherDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "aster-other",
            date: "20260729",
            desc: "a second virtual DRM device",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::RENDER.union(DriverFeatures::GEM)
    }

    fn dumb_create(&self, _args: DumbCreateArgs) -> DrmResult<DumbBuffer> {
        Err(DrmError::NotSupported)
    }
}

fn device() -> Arc<DrmDevice> {
    DrmDevice::new_unregistered(SoftDriver::new(), Some(0), Some(128))
}

fn other_device() -> Arc<DrmDevice> {
    DrmDevice::new_unregistered(Arc::new(OtherDriver), None, Some(129))
}

fn open_file(device: &Arc<DrmDevice>) -> Arc<DrmFile> {
    device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap_or_else(|_| {
            device
                .open(DrmMinor::Render, Privilege::Unprivileged)
                .unwrap()
        })
}

fn object_of(pages: usize) -> Arc<dyn GemObject> {
    ShmemObject::new(pages * PAGE_SIZE, BufferUse::Scanout).unwrap()
}

fn ioctl<T: Pod>(
    device: &Arc<DrmDevice>,
    file: &DrmFile,
    user: &PrimeUser,
    nr: u32,
    args: T,
) -> DrmResult<T> {
    let mut payload = args.as_bytes().to_vec();
    device.ioctl(file, nr, &mut payload, &user.context())?;
    Ok(T::from_bytes(&payload))
}

#[ktest]
fn one_object_exports_to_one_buffer() {
    let device = device();
    let object = object_of(2);
    let other = object_of(2);

    let buffer = export(&device, &object).unwrap();

    // Mesa recognizes a buffer it already knows by comparing what came back. A
    // second `DmaBuf` for one object would make it treat a buffer it is
    // already using as a stranger.
    for _ in 0..3 {
        assert!(Arc::ptr_eq(&export(&device, &object).unwrap(), &buffer));
    }
    assert!(!Arc::ptr_eq(&export(&device, &other).unwrap(), &buffer));

    assert!(Arc::ptr_eq(buffer.object(), &object));
    assert_eq!(buffer.size(), 2 * PAGE_SIZE);
    assert!(buffer.is_from(&device));
    assert!(!buffer.is_from(&other_device()));
}

#[ktest]
fn an_object_that_refuses_export_is_refused() {
    /// Models a firmware-protected buffer that cannot be exported.
    #[derive(Debug)]
    struct Private;

    impl GemObject for Private {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
            self
        }

        fn size(&self) -> usize {
            PAGE_SIZE
        }

        fn buffer_use(&self) -> BufferUse {
            BufferUse::Render
        }

        fn segment(&self) -> Option<&Segment<()>> {
            None
        }

        fn fault(&self, _pgoff: usize) -> DrmResult<UFrame> {
            Err(DrmError::BadAddress)
        }

        fn is_exportable(&self) -> bool {
            false
        }
    }

    let device = device();
    let object: Arc<dyn GemObject> = Arc::new(Private);

    // A buffer the firmware owns must never become reachable from another
    // process, and a descriptor is exactly that.
    assert_eq!(
        export(&device, &object).unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn importing_back_into_the_exporting_device_yields_the_original_object() {
    let device = device();
    let object = object_of(1);

    let buffer = export(&device, &object).unwrap();
    let imported = import(&device, &buffer).unwrap();

    // Not a wrapper: a self-imported wrapper would be a second object over one
    // allocation, and the handle table would have no way to tell that a client
    // already holds it.
    assert!(Arc::ptr_eq(&imported, &object));
    assert!(imported.is_mappable());
}

#[ktest]
fn an_imported_object_cannot_be_mapped() {
    let exporter = device();
    let importer = other_device();
    let object = object_of(3);

    let buffer = export(&exporter, &object).unwrap();
    let imported = import(&importer, &buffer).unwrap();

    assert!(!Arc::ptr_eq(&imported, &object));
    assert_eq!(imported.size(), object.size());

    // The pages belong to the exporting driver. Upstream refuses the same
    // thing in `drm_gem_dumb_map_offset`, by checking `obj->import_attach`.
    assert!(!imported.is_mappable());
    assert!(imported.segment().is_none());
    assert_eq!(imported.fault(0).unwrap_err(), DrmError::BadAddress);

    // And the refusal is enforced where user space asks, not only in the
    // predicate.
    let file = open_file(&importer);
    let handle = file.handles().insert(imported).unwrap();
    let mut payload = payload_of(&mode::MapDumb {
        handle: handle.0,
        ..Default::default()
    });
    assert_eq!(
        crate::gem::ioctl::mode_map_dumb(
            &importer,
            &file,
            &mut payload,
            &PrimeUser::new().context()
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
}

/// A driver that claims imported buffers, the way a GPU with its own MMU must.
///
/// The object it answers with stands in for `aster-pvr`'s `PvrObject`: what
/// matters to [`import`] is only that the driver's answer is used instead of the
/// generic wrapper, not what the answer is.
#[derive(Debug)]
struct ClaimingDriver;

impl DrmDriver for ClaimingDriver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "claiming",
            date: "20260813",
            desc: "a driver that maps imported buffers into its own hardware",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::RENDER.union(DriverFeatures::GEM)
    }

    fn import_buffer(&self, buffer: &Arc<DmaBuf>) -> DrmResult<Option<Arc<dyn GemObject>>> {
        // A real driver builds an object over the exporter's frames; a refusal
        // to hand them over is the one case it cannot serve.
        if buffer.object().segment().is_none() {
            return Err(DrmError::InvalidArgument);
        }
        Ok(Some(object_of(buffer.size() / PAGE_SIZE)))
    }
}

/// The hook is consulted, and its answer replaces the generic wrapper.
///
/// Without this, a GPU whose driver *can* map an imported buffer still gets an
/// [`ImportedObject`] it cannot use, and the failure surfaces much later as
/// `ENOENT` from whichever ioctl tried to map it.
#[ktest]
fn a_driver_that_claims_an_import_gets_its_own_object() {
    let exporter = device();
    let importer = DrmDevice::new_unregistered(Arc::new(ClaimingDriver), None, Some(130));
    let object = object_of(2);

    let buffer = export(&exporter, &object).unwrap();
    let imported = import(&importer, &buffer).unwrap();

    // Not the generic wrapper, and not the exporter's object either.
    assert!(imported.as_any().downcast_ref::<ImportedObject>().is_none());
    assert!(!Arc::ptr_eq(&imported, &object));
    assert_eq!(imported.size(), object.size());
}

/// A driver that refuses an import makes the import fail, rather than silently
/// falling back to a wrapper it cannot use.
///
/// The distinction is diagnosability: a refusal here is reported by
/// `PRIME_FD_TO_HANDLE`, while a fallback would hand back a handle that fails
/// only when something tries to map it.
#[ktest]
fn a_driver_that_refuses_an_import_fails_the_import() {
    let importer = DrmDevice::new_unregistered(Arc::new(ClaimingDriver), None, Some(131));

    // An exporter with no host frames is what `ClaimingDriver` declines.
    let buffer = foreign_buffer_for_test(Arc::new(FramelessObject));

    assert_eq!(
        import(&importer, &buffer).unwrap_err(),
        DrmError::InvalidArgument
    );
}

/// An object whose memory is device-private, so it has no frames to share.
#[derive(Debug)]
struct FramelessObject;

impl GemObject for FramelessObject {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn size(&self) -> usize {
        PAGE_SIZE
    }

    fn buffer_use(&self) -> BufferUse {
        BufferUse::Scanout
    }

    fn segment(&self) -> Option<&Segment<()>> {
        None
    }

    fn fault(&self, _pgoff: usize) -> DrmResult<UFrame> {
        Err(DrmError::BadAddress)
    }
}

#[ktest]
fn re_exporting_an_imported_buffer_returns_the_original() {
    let exporter = device();
    let importer = other_device();
    let object = object_of(1);

    let buffer = export(&exporter, &object).unwrap();
    let imported = import(&importer, &buffer).unwrap();

    // A frame that travels GPU → compositor → display keeps one identity the
    // whole way, which is what `obj->import_attach->dmabuf` gives upstream.
    assert!(Arc::ptr_eq(&export(&importer, &imported).unwrap(), &buffer));
}

#[ktest]
fn an_exported_buffer_outlives_the_handle_it_came_from() {
    let device = device();
    let file = open_file(&device);
    let user = PrimeUser::new();

    let created = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::MODE_CREATE_DUMB,
        mode::CreateDumb {
            width: 64,
            height: 16,
            bpp: 32,
            ..Default::default()
        },
    )
    .unwrap();

    let object = file.handles().get(GemHandle(created.handle)).unwrap();
    let paddr = object.segment().unwrap().paddr();

    let exported = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::PRIME_HANDLE_TO_FD,
        DrmPrimeHandle {
            handle: created.handle,
            flags: prime_flags::CLOEXEC,
            fd: -1,
        },
    )
    .unwrap();
    assert!(exported.fd >= 0);
    assert!(
        user.descriptors.last_flags().unwrap().cloexec,
        "DRM_CLOEXEC was not passed through"
    );

    // Closing the handle right after exporting is the normal way to hand a
    // buffer to another process, so the descriptor has to keep it alive.
    ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::GEM_CLOSE,
        DrmGemClose {
            handle: created.handle,
            pad: 0,
        },
    )
    .unwrap();
    assert!(file.handles().is_empty());
    drop(object);

    let imported = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::PRIME_FD_TO_HANDLE,
        DrmPrimeHandle {
            handle: 0,
            flags: 0,
            fd: exported.fd,
        },
    )
    .unwrap();

    // The same memory, not a copy and not a fresh allocation.
    let recovered = file.handles().get(GemHandle(imported.handle)).unwrap();
    assert_eq!(recovered.segment().unwrap().paddr(), paddr);
}

#[ktest]
fn importing_the_same_descriptor_twice_yields_one_handle() {
    let device = device();
    let file = open_file(&device);
    let user = PrimeUser::new();

    let object = object_of(1);
    let handle = file.handles().insert(object.clone()).unwrap();

    let exported = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::PRIME_HANDLE_TO_FD,
        DrmPrimeHandle {
            handle: handle.0,
            flags: 0,
            fd: -1,
        },
    )
    .unwrap();

    // Importing into the file that already holds the object returns the handle
    // it already has, rather than a second one. A client that got two would
    // close one and be surprised the buffer is still referenced — or treat
    // them as two buffers and render into the one being scanned out.
    let mut handles = Vec::new();
    for _ in 0..3 {
        let imported = ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::PRIME_FD_TO_HANDLE,
            DrmPrimeHandle {
                handle: 0,
                flags: 0,
                fd: exported.fd,
            },
        )
        .unwrap();
        handles.push(imported.handle);
    }

    assert_eq!(handles, vec![handle.0, handle.0, handle.0]);
    assert_eq!(file.handles().len(), 1);

    // A second file importing the same descriptor gets its own handle, which
    // is stable across repeats in exactly the same way.
    let other = open_file(&device);
    let first = ioctl(
        &device,
        &other,
        &user,
        ioctl_nr::nr::PRIME_FD_TO_HANDLE,
        DrmPrimeHandle {
            handle: 0,
            flags: 0,
            fd: exported.fd,
        },
    )
    .unwrap();
    let again = ioctl(
        &device,
        &other,
        &user,
        ioctl_nr::nr::PRIME_FD_TO_HANDLE,
        DrmPrimeHandle {
            handle: 0,
            flags: 0,
            fd: exported.fd,
        },
    )
    .unwrap();
    assert_eq!(first.handle, again.handle);
    assert_eq!(other.handles().len(), 1);
    assert!(Arc::ptr_eq(
        &other.handles().get(GemHandle(first.handle)).unwrap(),
        &object
    ));
}

#[ktest]
fn the_prime_ioctls_reject_what_they_do_not_understand() {
    let device = device();
    let file = open_file(&device);
    let user = PrimeUser::new();

    let handle = file.handles().insert(object_of(1)).unwrap();

    // Flag bits with no meaning, so that a field which gains one later cannot
    // have been silently ignored in the meantime.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::PRIME_HANDLE_TO_FD,
            DrmPrimeHandle {
                handle: handle.0,
                flags: 0x8000_0000,
                fd: -1,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // `flags` is documented as unused for the import direction, so a client
    // passing `DRM_CLOEXEC` by mistake must find out rather than believe it
    // took effect.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::PRIME_FD_TO_HANDLE,
            DrmPrimeHandle {
                handle: 0,
                flags: prime_flags::CLOEXEC,
                fd: 0,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // A handle nobody holds, and a descriptor nobody opened.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::PRIME_HANDLE_TO_FD,
            DrmPrimeHandle {
                handle: 0xdead,
                flags: 0,
                fd: -1,
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::PRIME_FD_TO_HANDLE,
            DrmPrimeHandle {
                handle: 0,
                flags: 0,
                fd: 7,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn a_closed_descriptor_no_longer_names_a_buffer() {
    let device = device();
    let file = open_file(&device);
    let user = PrimeUser::new();

    let handle = file.handles().insert(object_of(1)).unwrap();
    let exported = ioctl(
        &device,
        &file,
        &user,
        ioctl_nr::nr::PRIME_HANDLE_TO_FD,
        DrmPrimeHandle {
            handle: handle.0,
            flags: 0,
            fd: -1,
        },
    )
    .unwrap();

    user.descriptors.close(exported.fd);

    // The buffer itself is still alive — this file holds a handle to the
    // object — but the descriptor number is not a way to reach it any more,
    // and a stale number must fail rather than land on whatever was opened
    // next.
    assert_eq!(
        ioctl(
            &device,
            &file,
            &user,
            ioctl_nr::nr::PRIME_FD_TO_HANDLE,
            DrmPrimeHandle {
                handle: 0,
                flags: 0,
                fd: exported.fd,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
}

#[ktest]
fn prime_is_allowed_on_a_render_node() {
    let device = device();
    let render = device
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();
    let user = PrimeUser::new();

    let handle = render.handles().insert(object_of(1)).unwrap();

    // The whole point of the interface: a render client that cannot touch the
    // display still has to be able to hand it a finished frame. Both
    // directions carry `DRM_RENDER_ALLOW` upstream.
    let exported = ioctl(
        &device,
        &render,
        &user,
        ioctl_nr::nr::PRIME_HANDLE_TO_FD,
        DrmPrimeHandle {
            handle: handle.0,
            flags: 0,
            fd: -1,
        },
    )
    .unwrap();
    assert!(exported.fd >= 0);
}

#[ktest]
fn the_capability_reports_import_and_export() {
    use crate::uapi::cap;

    let device = device();

    // PRIME capability bits must advertise both implemented directions.
    assert_eq!(
        device.capability(cap::device::PRIME).unwrap(),
        cap::device::PRIME_IMPORT | cap::device::PRIME_EXPORT
    );
}

#[ktest]
fn a_shared_buffer_carries_the_fences_an_importer_must_observe() {
    use crate::fence::{FenceContext, SoftwareFence};

    let device = device();
    let object = object_of(1);
    let buffer = export(&device, &object).unwrap();

    assert!(buffer.fences().is_idle());

    let write = SoftwareFence::new(FenceContext::new(), 1);
    buffer.fences().add_write(write.clone());

    // This is what makes implicit synchronization work across the boundary:
    // the importer learns what it must wait for without the two drivers having
    // to agree on anything else.
    assert_eq!(buffer.fences().read_dependencies().len(), 1);
    assert!(!buffer.fences().is_idle());

    write.signal(None);
    assert!(buffer.fences().is_idle());
}

/// Cross-device ioctl imports must preserve identity even when the driver
/// constructs a fresh view each time, and after a closed number is reused.
#[ktest]
fn foreign_import_identity_survives_repetition_and_handle_reuse() {
    let exporter = device();
    for importer in [
        other_device(),
        DrmDevice::new_unregistered(Arc::new(ClaimingDriver), None, Some(130)),
    ] {
        let file = open_file(&importer);
        let user = PrimeUser::new();
        let buffer = export(&exporter, &object_of(1)).unwrap();
        let fd = user
            .descriptors
            .install(
                DrmExport::Buffer(buffer.clone()),
                ExportFlags {
                    cloexec: true,
                    read_write: true,
                },
            )
            .unwrap();
        let import_one = |file: &DrmFile| {
            ioctl(
                &importer,
                file,
                &user,
                ioctl_nr::nr::PRIME_FD_TO_HANDLE,
                DrmPrimeHandle {
                    fd,
                    ..Default::default()
                },
            )
            .unwrap()
            .handle
        };
        let first = import_one(&file);
        let original_view = file.handles().get(GemHandle(first)).unwrap();
        for _ in 0..3 {
            assert_eq!(import_one(&file), first);
            assert_eq!(file.handles().len(), 1);
        }
        assert!(Arc::ptr_eq(
            &export(&importer, &original_view).unwrap(),
            &buffer
        ));
        file.handles().remove(GemHandle(first)).unwrap();
        let unrelated = object_of(1);
        let reused = file.handles().insert(unrelated.clone()).unwrap();
        assert_eq!(reused.0, first);
        let second = import_one(&file);
        assert_ne!(second, reused.0);
        assert_eq!(file.handles().len(), 2);
        assert!(Arc::ptr_eq(
            &file.handles().get(reused).unwrap(),
            &unrelated
        ));
        assert_eq!(import_one(&file), second);
        let other_file = open_file(&importer);
        import_one(&other_file);
        assert_eq!(other_file.handles().len(), 1);
        // Closing one file's imported handle does not remove another's.
        file.handles().remove(GemHandle(second)).unwrap();
        assert_eq!(other_file.handles().len(), 1);
    }
}
