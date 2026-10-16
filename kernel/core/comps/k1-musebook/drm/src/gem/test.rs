// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Tests for handles, reference counting, the fake `mmap` offset space, and the
//! dumb buffer ioctls.
//!
//! The reference counting tests are the ones worth reading twice. A GEM object
//! is held by several unrelated parties and its pages by several more, and
//! getting either wrong produces a bug that only shows up as a use-after-free
//! or a leak under a compositor, long after the ioctl that caused it.

use alloc::{vec, vec::Vec};

use ostd::{mm::HasPaddr, prelude::ktest};
use ostd_pod::Pod;

use super::*;
use crate::{
    UserMem,
    device::{DriverFeatures, DrmDevice, DrmMinor},
    drivers::soft::SoftDriver,
    error::DrmError,
    file::{CallerIdentity, DrmFile, Privilege},
    ioctl::CallerContext,
    uapi::{DrmGemClose, DrmGemFlink, DrmGemOpen, ioctl_nr, mode},
};

/// A [`UserMem`] that fails every access.
///
/// None of the GEM ioctls touch user memory outside their argument structure,
/// which the shell has already copied in. Handing them one that would fault is
/// how this file states that.
struct NoUserMem;

impl UserMem for NoUserMem {
    fn read_bytes(&self, _addr: u64, _buf: &mut [u8]) -> DrmResult<()> {
        Err(DrmError::BadAddress)
    }

    fn write_bytes(&self, _addr: u64, _buf: &[u8]) -> DrmResult<()> {
        Err(DrmError::BadAddress)
    }
}

/// A device backed by the software driver, i.e. what `drm_smoke` runs against.
fn device() -> Arc<DrmDevice> {
    DrmDevice::new_unregistered(SoftDriver::new(), Some(0), Some(128))
}

fn file(device: &Arc<DrmDevice>) -> Arc<DrmFile> {
    device
        .open(DrmMinor::Primary, Privilege::Unprivileged)
        .unwrap()
}

/// Opens a second file that is allowed to issue the `DRM_AUTH` ioctls.
///
/// The first client on the primary node becomes master, and being master is what
/// makes it authenticated. A second client is neither, so `GEM_FLINK` and
/// `GEM_OPEN` -- both `DRM_AUTH` upstream, neither `DRM_RENDER_ALLOW` -- are
/// refused with `EACCES`, exactly as Linux refuses them.
///
/// Real clients get a magic token from the master. There is no `AUTH_MAGIC` here
/// yet, so hand mastership over instead. `drop_master` clears only the master
/// bit, so `master` keeps the authentication it already earned and both files can
/// then act.
fn authenticated_peer(device: &Arc<DrmDevice>, master: &Arc<DrmFile>) -> Arc<DrmFile> {
    let owner = Arc::new(());
    let caller = CallerContext::new(&NoUserMem)
        .with_credentials(CallerIdentity::from_owner(&owner), Privilege::Privileged);
    device.drop_master(master, &caller).unwrap();
    file(device)
}

fn object_of(pages: usize) -> Arc<dyn GemObject> {
    ShmemObject::new(pages * PAGE_SIZE, BufferUse::Scanout).unwrap()
}

/// Issues an ioctl the way the character-device shell does, returning the
/// argument structure it would copy back out.
fn ioctl<T: Pod>(device: &Arc<DrmDevice>, file: &DrmFile, nr: u32, args: T) -> DrmResult<T> {
    let mut payload = args.as_bytes().to_vec();
    device.ioctl(file, nr, &mut payload, &CallerContext::new(&NoUserMem))?;
    Ok(T::from_bytes(&payload))
}

/// Creates a dumb buffer through the ioctl path.
fn create_dumb(
    device: &Arc<DrmDevice>,
    file: &DrmFile,
    width: u32,
    height: u32,
) -> mode::CreateDumb {
    ioctl(
        device,
        file,
        ioctl_nr::nr::MODE_CREATE_DUMB,
        mode::CreateDumb {
            width,
            height,
            bpp: 32,
            ..Default::default()
        },
    )
    .unwrap()
}

#[ktest]
fn handles_are_dense_and_never_zero() {
    let table = HandleTable::new();
    let objects: Vec<_> = (0..4).map(|_| object_of(1)).collect();

    let handles: Vec<_> = objects
        .iter()
        .map(|object| table.insert(object.clone()).unwrap())
        .collect();

    // Handle 0 means "no buffer" in `drm_mode_fb_cmd2` and friends, so it must
    // never be handed out.
    assert_eq!(
        handles,
        vec![GemHandle(1), GemHandle(2), GemHandle(3), GemHandle(4)]
    );
    assert_eq!(table.len(), 4);
    assert!(table.get(GemHandle(0)).is_err());
}

#[ktest]
fn a_closed_handle_number_is_reused() {
    let table = HandleTable::new();
    let object = object_of(1);

    for _ in 0..3 {
        table.insert(object.clone()).unwrap();
    }
    table.remove(GemHandle(2)).unwrap();

    // Lowest free first, as Linux's `idr` does, because user space has been
    // observed to assume handles stay small.
    assert_eq!(table.insert(object.clone()).unwrap(), GemHandle(2));
    assert_eq!(table.insert(object).unwrap(), GemHandle(4));
}

#[ktest]
fn closing_a_handle_invalidates_it() {
    let table = HandleTable::new();
    let object = object_of(1);
    let handle = table.insert(object.clone()).unwrap();

    assert!(table.get(handle).is_ok());
    table.remove(handle).unwrap();

    assert_eq!(table.get(handle).unwrap_err(), DrmError::NotFound);
    // `drm_gem_handle_delete` reports a handle that is not there as `EINVAL`,
    // not `ENOENT`, and both `GEM_CLOSE` and `MODE_DESTROY_DUMB` pass that
    // through untouched.
    assert_eq!(table.remove(handle).unwrap_err(), DrmError::InvalidArgument);
    assert!(table.is_empty());
}

#[ktest]
fn one_object_can_be_held_by_several_handles() {
    let table = HandleTable::new();
    let object = object_of(1);

    let first = table.insert(object.clone()).unwrap();
    let second = table.insert(object.clone()).unwrap();
    assert_ne!(first, second);

    // Both handles are the same buffer, and closing one leaves the other
    // usable. `find` answers with whichever comes first, which is all PRIME
    // import needs of it.
    assert!(Arc::ptr_eq(&table.get(first).unwrap(), &object));
    assert!(Arc::ptr_eq(&table.get(second).unwrap(), &object));
    assert_eq!(table.find(&object), Some(first));

    table.remove(first).unwrap();
    assert!(Arc::ptr_eq(&table.get(second).unwrap(), &object));
    assert_eq!(table.find(&object), Some(second));
}

#[ktest]
fn the_last_reference_releases_the_object() {
    let table = HandleTable::new();
    let object = object_of(2);
    let paddr = object.segment().unwrap().paddr();

    let first = table.insert(object.clone()).unwrap();
    let second = table.insert(object.clone()).unwrap();
    // One reference here, one per handle.
    assert_eq!(Arc::strong_count(&object), 3);

    table.remove(first).unwrap();
    assert_eq!(Arc::strong_count(&object), 2);
    table.remove(second).unwrap();
    assert_eq!(Arc::strong_count(&object), 1);

    // The object is still alive because this test holds it, which is exactly
    // the situation of a buffer whose handles have all been closed but whose
    // dma-buf is still open.
    assert_eq!(object.segment().unwrap().paddr(), paddr);
}

/// `GEM_CLOSE` of the last handle must drop the object when nothing else
/// holds it; closing the handle must release its strong reference.
#[ktest]
fn gem_close_of_the_last_handle_drops_the_object() {
    crate::live::reset_for_test();
    let device = device();
    let file = file(&device);
    let before = crate::live::gem_objects();

    let created = create_dumb(&device, &file, 16, 16);
    assert_eq!(crate::live::gem_objects(), before + 1);
    assert_eq!(crate::live::gem_handles(), 1);

    ioctl(
        &device,
        &file,
        ioctl_nr::nr::GEM_CLOSE,
        DrmGemClose {
            handle: created.handle,
            pad: 0,
        },
    )
    .unwrap();

    assert!(file.handles().is_empty());
    assert_eq!(
        crate::live::gem_objects(),
        before,
        "the last handle was the last reference"
    );
    assert_eq!(crate::live::gem_handles(), 0);
    assert_eq!(crate::live::gem_closes(), 1);
}

#[ktest]
fn a_faulted_page_is_a_second_reference_to_the_objects_own_frame() {
    let object = object_of(4);
    let segment = object.segment().unwrap();

    let frame = object.fault(2).unwrap();

    // The very page the object owns, not a copy of it: two mappings of one
    // buffer have to see each other's writes.
    assert_eq!(frame.paddr(), segment.paddr() + 2 * PAGE_SIZE);
    // One reference from the segment, one from the `DmaStream` a Scanout
    // object keeps for cache maintenance (it clones the segment), one just
    // handed out. In a real fault the last one goes into the page table.
    assert_eq!(frame.reference_count(), 3);

    // Faulting the same page again is idempotent, which is what makes the
    // handler safe to call concurrently from two address spaces.
    let again = object.fault(2).unwrap();
    assert_eq!(again.paddr(), frame.paddr());
    assert_eq!(frame.reference_count(), 4);

    // Dropping what a `munmap` would drop leaves the page with the object.
    drop(again);
    drop(frame);
    assert_eq!(object.fault(2).unwrap().reference_count(), 3);
}

#[ktest]
fn faulting_past_the_end_of_an_object_is_rejected() {
    let object = object_of(3);

    assert!(object.fault(2).is_ok());
    assert_eq!(object.fault(3).unwrap_err(), DrmError::BadAddress);
    assert_eq!(object.fault(usize::MAX).unwrap_err(), DrmError::BadAddress);
}

#[ktest]
fn a_scanout_buffer_is_mapped_write_combining() {
    // The K1 display engine does not snoop the D-cache and there is no ioctl
    // between a client's stores and the next scanout at which the kernel could
    // clean it, so a cached mapping would show a stale frame.
    assert_eq!(
        BufferUse::Scanout.cache_policy(),
        CachePolicy::WriteCombining
    );
    assert_eq!(
        BufferUse::Render.cache_policy(),
        CachePolicy::WriteCombining
    );
    // Nothing but the CPU touches a CPU-only buffer, so there is nothing to be
    // coherent with.
    assert_eq!(BufferUse::CpuOnly.cache_policy(), CachePolicy::Writeback);
}

#[ktest]
fn fake_offsets_round_trip_through_the_offset_space() {
    let offsets = MmapOffsets::new();
    let first = object_of(3);
    let second = object_of(5);

    let first_offset = offsets.offset_of(&first).unwrap();
    let second_offset = offsets.offset_of(&second).unwrap();

    // Nothing lives below the base, so a client that maps the node at a
    // plausible small offset gets a failure rather than somebody's buffer.
    assert!(first_offset >= MMAP_OFFSET_BASE);
    assert_eq!(second_offset, first_offset + 3 * PAGE_SIZE as u64);

    // Every page of every object decodes to itself: the offset says both which
    // object and which page of it, which is all the fault path is given.
    for (object, base, pages) in [(&first, first_offset, 3), (&second, second_offset, 5)] {
        for page in 0..pages {
            let offset = base + (page * PAGE_SIZE) as u64;
            let (found, found_base, found_page) = offsets.lookup(offset).unwrap();
            assert!(Arc::ptr_eq(&found, object));
            assert_eq!(found_base, base);
            assert_eq!(found_page, page);
        }
    }

    // One past the end of the last object belongs to nobody, and neither does
    // anything below the base.
    assert!(
        offsets
            .lookup(second_offset + 5 * PAGE_SIZE as u64)
            .is_none()
    );
    assert!(offsets.lookup(0).is_none());
    assert!(
        offsets
            .lookup(MMAP_OFFSET_BASE - PAGE_SIZE as u64)
            .is_none()
    );
}

#[ktest]
fn an_object_keeps_one_offset_for_its_whole_life() {
    let offsets = MmapOffsets::new();
    let object = object_of(2);

    let offset = offsets.offset_of(&object).unwrap();
    for _ in 0..4 {
        assert_eq!(offsets.offset_of(&object).unwrap(), offset);
    }
    assert_eq!(offsets.len(), 1);

    // A second object gets a run of its own, and the first one's offset is
    // unaffected.
    let other = object_of(1);
    assert_ne!(offsets.offset_of(&other).unwrap(), offset);
    assert_eq!(offsets.offset_of(&object).unwrap(), offset);
}

#[ktest]
fn a_released_object_does_not_keep_its_offset_alive() {
    let offsets = MmapOffsets::new();
    let object = object_of(2);
    let offset = offsets.offset_of(&object).unwrap();

    // The table holds a weak reference, so it is not what keeps a buffer
    // alive; Linux likewise frees the offset when the object is released.
    drop(object);
    assert!(offsets.lookup(offset).is_none());

    // The page numbers are not recycled either: a client still holding the
    // stale offset must not land on the next buffer allocated.
    let next = object_of(1);
    assert_ne!(offsets.offset_of(&next).unwrap(), offset);
}

#[ktest]
fn flink_names_are_stable_and_shared_device_wide() {
    let names = NameTable::new();
    let object = object_of(1);
    let other = object_of(1);

    let name = names.publish(&object).unwrap();
    assert_ne!(name, 0);
    // User space compares names to decide whether two handles are one buffer,
    // so an object must never have two.
    assert_eq!(names.publish(&object).unwrap(), name);
    assert_ne!(names.publish(&other).unwrap(), name);

    assert!(Arc::ptr_eq(&names.lookup(name).unwrap(), &object));
    assert_eq!(names.name_of(&object), Some(name));
    assert!(names.lookup(0).is_none());
    assert!(names.lookup(u32::MAX).is_none());

    // A name does not hold the object, matching Linux, where it disappears
    // with the last handle.
    drop(object);
    assert!(names.lookup(name).is_none());
}

#[ktest]
fn a_dumb_buffer_can_be_created_mapped_and_destroyed() {
    let device = device();
    let file = file(&device);

    let created = create_dumb(&device, &file, 64, 32);

    // The pitch is rounded up by the driver, which is why user space must use
    // the returned value rather than computing one.
    assert_eq!(created.pitch, 64 * 4);
    assert_eq!(created.size, (created.pitch as u64) * 32);
    assert!(created.size.is_multiple_of(PAGE_SIZE as u64));
    assert_ne!(created.handle, 0);
    assert_eq!(file.handles().len(), 1);

    let mapped = ioctl(
        &device,
        &file,
        ioctl_nr::nr::MODE_MAP_DUMB,
        mode::MapDumb {
            handle: created.handle,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(mapped.offset >= MMAP_OFFSET_BASE);

    // Asking twice returns the same offset, or a client's second mapping would
    // be of a different buffer.
    let again = ioctl(
        &device,
        &file,
        ioctl_nr::nr::MODE_MAP_DUMB,
        mode::MapDumb {
            handle: created.handle,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(again.offset, mapped.offset);

    // Every page of the buffer resolves through the offset.
    let object = file.handles().get(GemHandle(created.handle)).unwrap();
    for page in 0..(created.size as usize / PAGE_SIZE) {
        let offset = mapped.offset + (page * PAGE_SIZE) as u64;
        let (resolved, base_offset, pgoff) = device.gem().offsets().lookup(offset).unwrap();
        assert!(Arc::ptr_eq(&resolved, &object));
        assert_eq!(base_offset, mapped.offset);
        assert_eq!(pgoff, page);
        assert_eq!(
            resolved.fault(pgoff).unwrap().paddr(),
            object.segment().unwrap().paddr() + page * PAGE_SIZE
        );
    }

    // One page past the end is outside the object's run, and the offset space
    // has nothing there.
    assert!(
        device
            .gem()
            .offsets()
            .lookup(mapped.offset + created.size)
            .is_none()
    );

    ioctl(
        &device,
        &file,
        ioctl_nr::nr::MODE_DESTROY_DUMB,
        mode::DestroyDumb {
            handle: created.handle,
        },
    )
    .unwrap();
    assert!(file.handles().is_empty());

    // With the last handle gone so is the object, and with it the fake offset:
    // a client holding a stale offset gets a fault, not another buffer.
    drop(object);
    assert!(device.gem().offsets().lookup(mapped.offset).is_none());
}

#[ktest]
fn destroying_a_dumb_buffer_twice_is_rejected() {
    let device = device();
    let file = file(&device);
    let created = create_dumb(&device, &file, 16, 16);

    let destroy = mode::DestroyDumb {
        handle: created.handle,
    };
    ioctl(&device, &file, ioctl_nr::nr::MODE_DESTROY_DUMB, destroy).unwrap();
    assert_eq!(
        ioctl(&device, &file, ioctl_nr::nr::MODE_DESTROY_DUMB, destroy).unwrap_err(),
        DrmError::InvalidArgument
    );

    // And so is a handle that never existed, through either spelling.
    assert_eq!(
        ioctl(
            &device,
            &file,
            ioctl_nr::nr::GEM_CLOSE,
            DrmGemClose {
                handle: 0xdead,
                pad: 0,
            },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );
    assert_eq!(
        ioctl(
            &device,
            &file,
            ioctl_nr::nr::MODE_MAP_DUMB,
            mode::MapDumb {
                handle: 0xdead,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );
}

#[ktest]
fn dumb_creation_rejects_impossible_geometry() {
    let device = device();
    let file = file(&device);

    let cases = [
        // A zero in any dimension.
        (0, 32, 32, 0),
        (32, 0, 32, 0),
        (32, 32, 0, 0),
        // Products that do not fit in the 32-bit `pitch` and `size` fields.
        (u32::MAX, 32, 32, 0),
        (0x10000, 0x10000, 32, 0),
        (32, 32, u32::MAX, 0),
        // Flags we do not understand.
        (32, 32, 32, 1),
    ];

    for (width, height, bpp, flags) in cases {
        let args = mode::CreateDumb {
            width,
            height,
            bpp,
            flags,
            ..Default::default()
        };
        assert_eq!(
            ioctl(&device, &file, ioctl_nr::nr::MODE_CREATE_DUMB, args).unwrap_err(),
            DrmError::InvalidArgument,
            "{width}x{height}x{bpp} flags {flags} was accepted"
        );
    }

    // Nothing was allocated and no handle leaked along the way.
    assert!(file.handles().is_empty());
}

#[ktest]
fn a_handle_is_private_to_the_file_that_owns_it() {
    let device = device();
    let owner = file(&device);
    let other = file(&device);

    let created = create_dumb(&device, &owner, 32, 32);

    // Handles are per-file: the same number in another file is not this
    // buffer, and here it is nothing at all.
    assert_eq!(
        other.handles().get(GemHandle(created.handle)).unwrap_err(),
        DrmError::NotFound
    );
    assert_eq!(
        ioctl(
            &device,
            &other,
            ioctl_nr::nr::MODE_MAP_DUMB,
            mode::MapDumb {
                handle: created.handle,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );
}

#[ktest]
fn flink_and_open_move_a_buffer_between_files() {
    let device = device();
    let owner = file(&device);
    let other = authenticated_peer(&device, &owner);

    let created = create_dumb(&device, &owner, 32, 32);
    let object = owner.handles().get(GemHandle(created.handle)).unwrap();

    let flink = ioctl(
        &device,
        &owner,
        ioctl_nr::nr::GEM_FLINK,
        DrmGemFlink {
            handle: created.handle,
            name: 0,
        },
    )
    .unwrap();
    assert_ne!(flink.name, 0);

    let opened = ioctl(
        &device,
        &other,
        ioctl_nr::nr::GEM_OPEN,
        DrmGemOpen {
            name: flink.name,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(opened.size, created.size);
    assert!(Arc::ptr_eq(
        &other.handles().get(GemHandle(opened.handle)).unwrap(),
        &object
    ));

    // The importing file's handle is a reference of its own, so the buffer
    // survives the exporter closing its handle. Mesa relies on this.
    ioctl(
        &device,
        &owner,
        ioctl_nr::nr::GEM_CLOSE,
        DrmGemClose {
            handle: created.handle,
            pad: 0,
        },
    )
    .unwrap();
    assert!(owner.handles().is_empty());
    assert!(Arc::ptr_eq(
        &other.handles().get(GemHandle(opened.handle)).unwrap(),
        &object
    ));

    // An unknown name is `ENOENT`.
    assert_eq!(
        ioctl(
            &device,
            &other,
            ioctl_nr::nr::GEM_OPEN,
            DrmGemOpen {
                name: flink.name + 1000,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotFound
    );
}

#[ktest]
fn a_driver_without_gem_refuses_the_gem_ioctls() {
    #[derive(Debug)]
    struct NoGemDriver;

    impl crate::device::DrmDriver for NoGemDriver {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }

        fn info(&self) -> crate::device::DriverInfo {
            crate::device::DriverInfo {
                name: "nogem",
                date: "20260729",
                desc: "a driver without buffer objects",
                major: 1,
                minor: 0,
                patchlevel: 0,
            }
        }

        fn features(&self) -> DriverFeatures {
            DriverFeatures::MODESET
        }
    }

    let device = DrmDevice::new_unregistered(Arc::new(NoGemDriver), Some(0), None);
    let file = file(&device);

    for nr in [
        ioctl_nr::nr::GEM_CLOSE,
        ioctl_nr::nr::GEM_FLINK,
        ioctl_nr::nr::GEM_OPEN,
    ] {
        let mut payload = [0u8; 16];
        assert_eq!(
            device
                .ioctl(&file, nr, &mut payload, &CallerContext::new(&NoUserMem))
                .unwrap_err(),
            DrmError::NotSupported
        );
    }

    // Divergence: Linux answers `ENOSYS` here, which `DrmError` cannot spell.
    assert_eq!(
        ioctl(
            &device,
            &file,
            ioctl_nr::nr::MODE_CREATE_DUMB,
            mode::CreateDumb {
                width: 16,
                height: 16,
                bpp: 32,
                ..Default::default()
            },
        )
        .unwrap_err(),
        DrmError::NotSupported
    );
}

#[ktest]
fn the_gem_ioctls_carry_the_node_permissions_linux_gives_them() {
    let device = device();
    let render = device
        .open(DrmMinor::Render, Privilege::Unprivileged)
        .unwrap();

    // `GEM_CLOSE` is `DRM_RENDER_ALLOW`: a render client allocates buffers and
    // has to be able to free them.
    assert_eq!(
        ioctl(
            &device,
            &render,
            ioctl_nr::nr::GEM_CLOSE,
            DrmGemClose { handle: 1, pad: 0 },
        )
        .unwrap_err(),
        DrmError::InvalidArgument
    );

    // The rest are not. Dumb buffers are for scanout, and the flink namespace
    // is device-global with no notion of ownership — which is precisely what
    // render nodes were introduced to keep out of, so a render client cannot
    // reach a buffer by name at all. It is also why nothing else may stand in
    // for a handle when deciding who can map an object.
    for nr in [
        ioctl_nr::nr::GEM_FLINK,
        ioctl_nr::nr::GEM_OPEN,
        ioctl_nr::nr::MODE_CREATE_DUMB,
        ioctl_nr::nr::MODE_MAP_DUMB,
        ioctl_nr::nr::MODE_DESTROY_DUMB,
    ] {
        let mut payload = [0u8; 32];
        assert_eq!(
            device
                .ioctl(&render, nr, &mut payload, &CallerContext::new(&NoUserMem))
                .unwrap_err(),
            DrmError::PermissionDenied,
            "ioctl {nr:#04x} was allowed on a render node"
        );
    }
}

#[ktest]
fn preparing_a_cpu_read_uses_a_retained_dma_owner_and_checks_the_page() {
    for usage in [BufferUse::CpuOnly, BufferUse::Scanout] {
        let object = ShmemObject::new(2 * PAGE_SIZE, usage).unwrap();
        let guard = ostd::task::disable_preempt();
        assert!(object.prepare_cpu_read(0).is_ok());
        assert!(object.prepare_cpu_read(1).is_ok());
        assert_eq!(object.prepare_cpu_read(2), Err(DrmError::BadAddress));
        assert_eq!(
            object.prepare_cpu_read(usize::MAX),
            Err(DrmError::BadAddress)
        );
        drop(guard);
    }
}
