// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! The mode-setting ioctls.
//!
//! These are the ioctls `modetest` and `cosmic-comp` use to
//! enumerate a display pipeline and program it; the numbers and structures come
//! from `include/uapi/drm/drm_mode.h` and the permission flags are transcribed
//! from Linux's `drm_ioctls[]` table, where every mode-setting entry that changes
//! state carries `DRM_MASTER` and none carries `DRM_RENDER_ALLOW`.
//!
//! # The two-pass convention
//!
//! Every ioctl with an array follows the same protocol: the caller sets `count_*`
//! to the capacity it prepared, and the kernel fills up to that many entries and
//! writes the **total** back. Reporting the total rather than the number written
//! is what lets `libdrm` notice its buffer was too small and retry — see
//! [`copy_out_counted`].
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_mode_config.c>,
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_crtc.c>,
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c>,
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_connector.c>,
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_property.c>,
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_framebuffer.c> and
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic_uapi.c>.

use alloc::{vec, vec::Vec};

use ostd_pod::Pod;

use super::{
    AtomicRequest, BlobOwner, CommitFlags, Framebuffer, ModeConfig, ObjectId, ObjectKind,
    PropertyAssignment, PropertyKind,
};
use crate::{
    device::DrmDevice,
    error::{DrmError, DrmResult},
    file::DrmFile,
    gem::GemHandle,
    ioctl::{
        CallerContext, IoctlDesc, IoctlFlags, copy_in_array, copy_out_array, payload_read,
        payload_write,
    },
    uapi::{fourcc, ioctl_nr::nr, mode},
};

/// The mode-setting dispatch table.
///
/// Kept apart from [`CORE_IOCTLS`] to keep display-specific dispatch with KMS.
/// Both tables use binary search, so entries
/// must stay sorted by command number. The `kms_ioctls_are_sorted_and_disjoint`
/// test enforces both that and the absence of overlap with the core table.
///
/// [`CORE_IOCTLS`]: crate::ioctl::CORE_IOCTLS
pub const KMS_IOCTLS: &[IoctlDesc] = &[
    IoctlDesc {
        nr: nr::MODE_GETRESOURCES,
        name: "MODE_GETRESOURCES",
        flags: IoctlFlags::empty(),
        handler: get_resources,
    },
    IoctlDesc {
        nr: nr::MODE_GETCRTC,
        name: "MODE_GETCRTC",
        flags: IoctlFlags::empty(),
        handler: get_crtc,
    },
    IoctlDesc {
        nr: nr::MODE_SETCRTC,
        name: "MODE_SETCRTC",
        flags: IoctlFlags::MASTER,
        handler: set_crtc,
    },
    IoctlDesc {
        nr: nr::MODE_GETENCODER,
        name: "MODE_GETENCODER",
        flags: IoctlFlags::empty(),
        handler: get_encoder,
    },
    IoctlDesc {
        nr: nr::MODE_GETCONNECTOR,
        name: "MODE_GETCONNECTOR",
        flags: IoctlFlags::empty(),
        handler: get_connector,
    },
    IoctlDesc {
        nr: nr::MODE_GETPROPERTY,
        name: "MODE_GETPROPERTY",
        flags: IoctlFlags::empty(),
        handler: get_property,
    },
    IoctlDesc {
        nr: nr::MODE_GETPROPBLOB,
        name: "MODE_GETPROPBLOB",
        flags: IoctlFlags::empty(),
        handler: get_prop_blob,
    },
    IoctlDesc {
        nr: nr::MODE_ADDFB,
        name: "MODE_ADDFB",
        flags: IoctlFlags::empty(),
        handler: add_fb,
    },
    IoctlDesc {
        nr: nr::MODE_RMFB,
        name: "MODE_RMFB",
        flags: IoctlFlags::empty(),
        handler: rm_fb,
    },
    IoctlDesc {
        nr: nr::MODE_PAGE_FLIP,
        name: "MODE_PAGE_FLIP",
        flags: IoctlFlags::MASTER,
        handler: page_flip,
    },
    IoctlDesc {
        nr: nr::MODE_GETPLANERESOURCES,
        name: "MODE_GETPLANERESOURCES",
        flags: IoctlFlags::empty(),
        handler: get_plane_resources,
    },
    IoctlDesc {
        nr: nr::MODE_GETPLANE,
        name: "MODE_GETPLANE",
        flags: IoctlFlags::empty(),
        handler: get_plane,
    },
    IoctlDesc {
        nr: nr::MODE_SETPLANE,
        name: "MODE_SETPLANE",
        flags: IoctlFlags::MASTER,
        handler: set_plane,
    },
    IoctlDesc {
        nr: nr::MODE_ADDFB2,
        name: "MODE_ADDFB2",
        flags: IoctlFlags::empty(),
        handler: add_fb2,
    },
    IoctlDesc {
        nr: nr::MODE_OBJ_GETPROPERTIES,
        name: "MODE_OBJ_GETPROPERTIES",
        flags: IoctlFlags::empty(),
        handler: obj_get_properties,
    },
    IoctlDesc {
        nr: nr::MODE_OBJ_SETPROPERTY,
        name: "MODE_OBJ_SETPROPERTY",
        flags: IoctlFlags::MASTER,
        handler: obj_set_property,
    },
    IoctlDesc {
        nr: nr::MODE_ATOMIC,
        name: "MODE_ATOMIC",
        flags: IoctlFlags::MASTER,
        handler: atomic,
    },
    IoctlDesc {
        nr: nr::MODE_CREATEPROPBLOB,
        name: "MODE_CREATEPROPBLOB",
        flags: IoctlFlags::empty(),
        handler: create_prop_blob,
    },
    IoctlDesc {
        nr: nr::MODE_DESTROYPROPBLOB,
        name: "MODE_DESTROYPROPBLOB",
        flags: IoctlFlags::empty(),
        handler: destroy_prop_blob,
    },
];

/// Returns the device's mode configuration.
///
/// A driver without one is not a mode-setting driver, which Linux reports as
/// `EOPNOTSUPP` from every one of these ioctls rather than by leaving them out of
/// the table. `aster-pvr`'s render node relies on that: it has no display.
fn mode_config(device: &DrmDevice) -> DrmResult<&ModeConfig> {
    device.driver().mode_config().ok_or(DrmError::NotSupported)
}

/// Copies up to `capacity` elements into a user array and returns the total.
///
/// Divergence from the description in `../../ABI.md`, and a deliberate one: the
/// count written back is the number of elements that **exist**, not the number
/// copied. That is what Linux does, and `libdrm`'s `drmModeGetResources` depends
/// on it — it compares the returned count against the capacity it allocated and
/// retries when the count is larger. Reporting the copied count would make it
/// silently accept a truncated list.
fn copy_out_counted<T: Pod>(
    caller: &CallerContext,
    ptr: u64,
    capacity: u32,
    items: &[T],
) -> DrmResult<u32> {
    copy_out_array(caller, ptr, capacity, items)?;
    Ok(items.len() as u32)
}

/// Copies a whole array or nothing at all, and returns the total.
///
/// The ioctls that describe one object's fixed-size attributes — a plane's format
/// list, a property's legal values — use this instead of the truncating variant,
/// because a partial list of formats would be read as the complete one.
fn copy_out_whole<T: Pod>(
    caller: &CallerContext,
    ptr: u64,
    capacity: u32,
    items: &[T],
) -> DrmResult<u32> {
    if capacity as usize >= items.len() {
        copy_out_array(caller, ptr, capacity, items)?;
    }
    Ok(items.len() as u32)
}

/// `DRM_IOCTL_MODE_GETRESOURCES`: enumerates CRTCs, encoders and connectors.
///
/// Framebuffers are the caller's own, not the device's: they belong to the file
/// that created them, exactly as Linux's per-file `fbs` list does.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_mode_config.c#L98>
/// (`drm_mode_getresources`).
fn get_resources(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::CardRes>(payload)?;
    let config = mode_config(device)?;

    let result = fill_resources(config, file, &mut args, caller);
    payload_write(payload, &args)?;
    result
}

fn fill_resources(
    config: &ModeConfig,
    file: &DrmFile,
    args: &mut mode::CardRes,
    caller: &CallerContext,
) -> DrmResult<()> {
    args.count_fbs = copy_out_counted(
        caller,
        args.fb_id_ptr,
        args.count_fbs,
        &config.framebuffer_ids_of(file.id()),
    )?;
    args.count_crtcs = copy_out_counted(
        caller,
        args.crtc_id_ptr,
        args.count_crtcs,
        &config.object_ids(ObjectKind::Crtc),
    )?;
    args.count_connectors = copy_out_counted(
        caller,
        args.connector_id_ptr,
        args.count_connectors,
        &config.object_ids(ObjectKind::Connector),
    )?;
    args.count_encoders = copy_out_counted(
        caller,
        args.encoder_id_ptr,
        args.count_encoders,
        &config.object_ids(ObjectKind::Encoder),
    )?;

    (args.min_width, args.min_height) = config.min_size();
    (args.max_width, args.max_height) = config.max_size();
    Ok(())
}

/// `DRM_IOCTL_MODE_GETCRTC`: reports what a CRTC is currently doing.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_crtc.c#L502>
/// (`drm_mode_getcrtc`).
fn get_crtc(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::Crtc>(payload)?;
    let config = mode_config(device)?;

    let crtc = config.crtc(ObjectId(args.crtc_id))?;
    let primary = crtc.primary_plane().id();
    let state = config.state();
    let crtc_state = state.crtc(crtc.id())?;
    let plane_state = state.plane(primary)?;

    args.gamma_size = crtc.gamma_size();
    // The position is the primary plane's source rectangle, which is where the
    // legacy interface's `x`/`y` end up after translation.
    args.x = plane_state.src_rect.0 >> 16;
    args.y = plane_state.src_rect.1 >> 16;

    match (&crtc_state.mode, &plane_state.framebuffer) {
        (Some(mode), framebuffer) => {
            args.mode_valid = 1;
            args.mode = *mode;
            // Zero when the CRTC is configured but no client has ADDFB2'd a
            // buffer yet — the honest leftover of a boot-seeded pipeline.
            args.fb_id = framebuffer
                .as_ref()
                .map_or(0, |framebuffer| framebuffer.id.get());
        }
        (None, _) => {
            args.mode_valid = 0;
            args.mode = mode::ModeInfo::default();
            args.fb_id = 0;
        }
    }

    // `set_connectors_ptr` and `count_connectors` are inputs of `SETCRTC` only;
    // Linux leaves them untouched here and so do we.
    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_SETCRTC`: the legacy modeset, translated into an atomic commit.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_crtc.c#L568>
/// (`drm_mode_setcrtc`).
fn set_crtc(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<mode::Crtc>(payload)?;
    let config = mode_config(device)?;

    let connectors: Vec<u32> =
        copy_in_array(caller, args.set_connectors_ptr, args.count_connectors)?;
    config.legacy_set_crtc(&args, &connectors, file)
}

/// `DRM_IOCTL_MODE_GETENCODER`: reports an encoder's routing options.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_encoder.c#L215>
/// (`drm_mode_getencoder`).
fn get_encoder(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::GetEncoder>(payload)?;
    let config = mode_config(device)?;

    let encoder = config.encoder(ObjectId(args.encoder_id))?;
    args.encoder_type = encoder.encoder_type();
    args.possible_crtcs = encoder.possible_crtcs();
    args.possible_clones = encoder.possible_clones();

    // An encoder's CRTC is the one driving any connector it currently carries.
    let state = config.state();
    args.crtc_id = config
        .connectors()
        .iter()
        .filter(|connector| connector.encoders().contains(&encoder.id()))
        .find_map(|connector| state.crtc_of_connector(connector.id()))
        .map_or(0, |crtc| crtc.get());

    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_GETCONNECTOR`: reports a connector, its modes and properties.
///
/// This is the ioctl whose two-pass protocol user space is most sensitive to: a
/// mistake here shows up as `modetest` reporting a connector with no modes, which
/// looks like a hardware problem.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_connector.c#L2721>
/// (`drm_mode_getconnector`).
fn get_connector(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::GetConnector>(payload)?;
    let config = mode_config(device)?;

    let result = fill_connector(config, file, &mut args, caller);
    payload_write(payload, &args)?;
    result
}

fn fill_connector(
    config: &ModeConfig,
    file: &DrmFile,
    args: &mut mode::GetConnector,
    caller: &CallerContext,
) -> DrmResult<()> {
    let connector = config.connector(ObjectId(args.connector_id))?;
    let state = config.state();

    args.connector_type = connector.connector_type();
    args.connector_type_id = connector.connector_type_id();
    args.connection = connector.detect() as u32;
    (args.mm_width, args.mm_height) = connector.physical_size_mm();
    args.subpixel = connector.subpixel_order();
    args.pad = 0;
    args.encoder_id = state
        .crtc_of_connector(connector.id())
        .and_then(|crtc| {
            let crtc_bit = 1u32 << config.crtc_index(crtc).unwrap_or(0);
            connector.encoders().into_iter().find(|encoder| {
                config
                    .encoder(*encoder)
                    .is_ok_and(|encoder| encoder.possible_crtcs() & crtc_bit != 0)
            })
        })
        .map_or(0, |encoder| encoder.get());

    let encoders: Vec<u32> = connector
        .encoders()
        .iter()
        .map(|encoder| encoder.get())
        .collect();
    args.count_encoders =
        copy_out_counted(caller, args.encoders_ptr, args.count_encoders, &encoders)?;

    // A disconnected sink has no modes to report even if it remembers some, which
    // is how user space learns not to try.
    let modes = if args.connection == super::ConnectionStatus::Disconnected as u32 {
        Vec::new()
    } else {
        connector.modes()
    };
    args.count_modes = copy_out_counted(caller, args.modes_ptr, args.count_modes, &modes)?;

    let (ids, values) = object_properties(config, connector.id(), ObjectKind::Connector, file)?;
    args.count_props = copy_out_properties(
        caller,
        args.props_ptr,
        args.prop_values_ptr,
        args.count_props,
        &ids,
        &values,
    )?;
    Ok(())
}

/// Copies a property list and its values out, and returns the total.
///
/// Both arrays are bounded by the same capacity, which is why they are copied
/// together: overwriting `count_props` first and then reusing it as the capacity
/// of the value array would write past the end of the caller's buffer whenever it
/// asked for fewer properties than exist.
fn copy_out_properties(
    caller: &CallerContext,
    ids_ptr: u64,
    values_ptr: u64,
    capacity: u32,
    ids: &[u32],
    values: &[u64],
) -> DrmResult<u32> {
    copy_out_array(caller, ids_ptr, capacity, ids)?;
    copy_out_array(caller, values_ptr, capacity, values)?;
    Ok(ids.len() as u32)
}

/// Collects the properties of one object, and their current values.
fn object_properties(
    config: &ModeConfig,
    object: ObjectId,
    kind: ObjectKind,
    file: &DrmFile,
) -> DrmResult<(Vec<u32>, Vec<u64>)> {
    let state = config.state();
    let atomic_client = file.client_caps().atomic;

    let mut ids = Vec::new();
    let mut values = Vec::new();
    for property in config.properties().visible(kind, atomic_client) {
        ids.push(property.id.get());
        values.push(config.property_value(object, property.standard, &state)?);
    }
    Ok((ids, values))
}

/// `DRM_IOCTL_MODE_GETPROPERTY`: describes one property.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_property.c#L474>
/// (`drm_mode_getproperty_ioctl`).
fn get_property(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::GetProperty>(payload)?;
    let config = mode_config(device)?;

    let property = config
        .properties()
        .lookup(ObjectId(args.prop_id))
        .ok_or(DrmError::NotFound)?;
    let standard = property.standard;

    args.flags = standard.flags();
    args.name = [0; mode::PROP_NAME_LEN];
    let name = standard.name().as_bytes();
    args.name[..name.len()].copy_from_slice(name);

    let (values, enums): (Vec<u64>, Vec<mode::PropertyEnum>) = match standard.kind() {
        PropertyKind::Range { min, max } => (vec![min, max], Vec::new()),
        PropertyKind::SignedRange { min, max } => (vec![min as u64, max as u64], Vec::new()),
        PropertyKind::Object { object_type } => (vec![u64::from(object_type)], Vec::new()),
        PropertyKind::Blob => (Vec::new(), Vec::new()),
        PropertyKind::Enum(entries) => {
            let values = entries.iter().map(|(value, _)| *value).collect();
            let enums = entries
                .iter()
                .map(|(value, name)| {
                    let mut entry = mode::PropertyEnum {
                        value: *value,
                        name: [0; mode::PROP_NAME_LEN],
                    };
                    let name = name.as_bytes();
                    entry.name[..name.len()].copy_from_slice(name);
                    entry
                })
                .collect();
            (values, enums)
        }
    };

    args.count_values = copy_out_whole(caller, args.values_ptr, args.count_values, &values)?;
    args.count_enum_blobs =
        copy_out_whole(caller, args.enum_blob_ptr, args.count_enum_blobs, &enums)?;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_GETPROPBLOB`: reads a blob's contents.
///
/// The copy happens only when the caller's buffer is exactly the blob's length,
/// matching Linux; the length is reported either way, so the first call learns it.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_property.c#L613>
/// (`drm_mode_getblob_ioctl`).
fn get_prop_blob(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::GetBlob>(payload)?;
    let config = mode_config(device)?;

    let blob = config.blob(ObjectId(args.blob_id))?;
    let result = if args.length == blob.data.len() as u32 && args.length != 0 {
        if args.data == 0 {
            Err(DrmError::BadAddress)
        } else {
            caller.write_bytes(args.data, &blob.data)
        }
    } else {
        Ok(())
    };

    args.length = blob.data.len() as u32;
    payload_write(payload, &args)?;
    result
}

/// `DRM_IOCTL_MODE_CREATEPROPBLOB`: uploads a blob, typically a mode.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_property.c#L648>
/// (`drm_mode_createblob_ioctl`).
fn create_prop_blob(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::CreateBlob>(payload)?;
    let config = mode_config(device)?;

    // Divergence: Linux's upper bound is `INT_MAX`; this crate caps every user
    // array at 1 MiB, for the reason `../../ABI.md` gives.
    let data: Vec<u8> = copy_in_array(caller, args.data, args.length)?;
    if data.is_empty() {
        return Err(DrmError::InvalidArgument);
    }

    args.blob_id = config.create_blob(data, BlobOwner::File(file.id()))?.get();
    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_DESTROYPROPBLOB`: drops a blob the caller created.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_property.c#L697>
/// (`drm_mode_destroyblob_ioctl`).
fn destroy_prop_blob(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<mode::DestroyBlob>(payload)?;
    let config = mode_config(device)?;

    config.destroy_blob(ObjectId(args.blob_id), file.id())
}

/// `DRM_IOCTL_MODE_ADDFB`: the legacy framebuffer creation, in terms of `ADDFB2`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_framebuffer.c#L110>
/// (`drm_mode_addfb`).
fn add_fb(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::FbCmd>(payload)?;

    let pixel_format =
        fourcc::legacy_fb_format(args.bpp, args.depth).ok_or(DrmError::InvalidArgument)?;
    let command = mode::FbCmd2 {
        width: args.width,
        height: args.height,
        pixel_format,
        handles: [args.handle, 0, 0, 0],
        pitches: [args.pitch, 0, 0, 0],
        ..mode::FbCmd2::default()
    };

    args.fb_id = create_framebuffer(device, file, &command)?.get();
    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_ADDFB2`: turns buffer objects into something scannable.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_framebuffer.c#L364>
/// (`drm_mode_addfb2`).
fn add_fb2(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::FbCmd2>(payload)?;
    args.fb_id = create_framebuffer(device, file, &args)?.get();
    payload_write(payload, &args)
}

/// The shared body of `ADDFB` and `ADDFB2`.
fn create_framebuffer(
    device: &DrmDevice,
    file: &DrmFile,
    command: &mode::FbCmd2,
) -> DrmResult<ObjectId> {
    let config = mode_config(device)?;

    if command.flags & !mode::fb_flags::ALL != 0 {
        return Err(DrmError::InvalidArgument);
    }
    // Only single-plane formats are supported, so exactly one handle may be given.
    if command.handles[0] == 0 || command.handles[1..].iter().any(|handle| *handle != 0) {
        return Err(DrmError::InvalidArgument);
    }
    // Without the modifier flag, the whole array must be zero: a client that
    // filled it in without setting the flag believes something we would ignore.
    if command.flags & mode::fb_flags::MODIFIERS == 0
        && command.modifier.iter().any(|modifier| *modifier != 0)
    {
        return Err(DrmError::InvalidArgument);
    }
    if command.offsets[1..].iter().any(|offset| *offset != 0)
        || command.pitches[1..].iter().any(|pitch| *pitch != 0)
    {
        return Err(DrmError::InvalidArgument);
    }

    let object = file.handles().get(GemHandle(command.handles[0]))?;

    config.add_framebuffer(Framebuffer {
        id: ObjectId::NONE,
        width: command.width,
        height: command.height,
        pixel_format: command.pixel_format,
        modifier: command.modifier[0],
        objects: [Some(object), None, None, None],
        pitches: command.pitches,
        offsets: command.offsets,
        owner: file.id(),
    })
}

/// `DRM_IOCTL_MODE_RMFB`: drops a framebuffer the caller created.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_framebuffer.c#L520>
/// (`drm_mode_rmfb`).
fn rm_fb(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let fb_id = payload_read::<u32>(payload)?;
    let config = mode_config(device)?;

    config.remove_framebuffer(
        ObjectId::from_raw(fb_id).ok_or(DrmError::NotFound)?,
        file.id(),
    )
}

/// `DRM_IOCTL_MODE_PAGE_FLIP`: shows a different framebuffer on a live CRTC.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L1177>
/// (`drm_mode_page_flip_ioctl`).
fn page_flip(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<mode::CrtcPageFlip>(payload)?;
    mode_config(device)?.legacy_page_flip(&args, file)
}

/// `DRM_IOCTL_MODE_GETPLANERESOURCES`: enumerates planes.
///
/// A client that has not set `DRM_CLIENT_CAP_UNIVERSAL_PLANES` sees only overlay
/// planes, because it would have no way to program the primary and cursor planes
/// other than through the legacy ioctls that own them.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L742>
/// (`drm_mode_getplane_res`).
fn get_plane_resources(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::GetPlaneRes>(payload)?;
    let config = mode_config(device)?;

    let universal = file.client_caps().universal_planes;
    let planes: Vec<u32> = config
        .planes()
        .iter()
        .filter(|plane| universal || plane.plane_type() == super::PlaneType::Overlay)
        .map(|plane| plane.id().get())
        .collect();

    args.count_planes = copy_out_counted(caller, args.plane_id_ptr, args.count_planes, &planes)?;
    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_GETPLANE`: reports a plane's capabilities and current use.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L780>
/// (`drm_mode_getplane`).
fn get_plane(
    device: &DrmDevice,
    _file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::GetPlane>(payload)?;
    let config = mode_config(device)?;

    let plane = config.plane(ObjectId(args.plane_id))?;
    let state = config.state();
    let plane_state = state.plane(plane.id())?;

    args.possible_crtcs = plane.possible_crtcs();
    args.gamma_size = 0;
    args.crtc_id = plane_state.crtc.map_or(0, |crtc| crtc.get());
    args.fb_id = plane_state
        .framebuffer
        .as_ref()
        .map_or(0, |framebuffer| framebuffer.id.get());

    args.count_format_types = copy_out_whole(
        caller,
        args.format_type_ptr,
        args.count_format_types,
        plane.formats(),
    )?;
    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_SETPLANE`: the legacy plane update, translated into a commit.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L933>
/// (`drm_mode_setplane`).
fn set_plane(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<mode::SetPlane>(payload)?;
    mode_config(device)?.legacy_set_plane(&args, file)
}

/// `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`: lists any object's properties.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_mode_object.c#L403>
/// (`drm_mode_obj_get_properties_ioctl`).
fn obj_get_properties(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let mut args = payload_read::<mode::ObjGetProperties>(payload)?;
    let config = mode_config(device)?;

    let object = ObjectId::from_raw(args.obj_id).ok_or(DrmError::NotFound)?;
    let kind = config.object_kind(object).ok_or(DrmError::NotFound)?;
    // The object type the caller supplied has to agree with what the ID actually
    // names, or a client would be told about properties of the wrong object.
    if ObjectKind::from_uapi(args.obj_type) != Some(kind) {
        return Err(DrmError::NotFound);
    }

    let (ids, values) = object_properties(config, object, kind, file)?;
    args.count_props = copy_out_properties(
        caller,
        args.props_ptr,
        args.prop_values_ptr,
        args.count_props,
        &ids,
        &values,
    )?;

    payload_write(payload, &args)
}

/// `DRM_IOCTL_MODE_OBJ_SETPROPERTY`: sets one property, as an atomic commit.
///
/// Divergence: Linux has a separate legacy path here too, which for an atomic
/// driver ends up calling the same helpers. We build a one-assignment commit,
/// which means a legacy client setting `DPMS` goes through exactly the validation
/// an atomic client would get.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_mode_object.c#L515>
/// (`drm_mode_obj_set_property_ioctl`).
fn obj_set_property(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    _caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<mode::ObjSetProperty>(payload)?;
    let config = mode_config(device)?;

    let object = ObjectId::from_raw(args.obj_id).ok_or(DrmError::NotFound)?;
    let kind = config.object_kind(object).ok_or(DrmError::NotFound)?;
    if ObjectKind::from_uapi(args.obj_type) != Some(kind) {
        return Err(DrmError::NotFound);
    }
    // Only these three kinds have writable properties; Linux refuses the rest
    // before looking the property up.
    if !matches!(
        kind,
        ObjectKind::Crtc | ObjectKind::Plane | ObjectKind::Connector
    ) {
        return Err(DrmError::InvalidArgument);
    }

    let property = ObjectId::from_raw(args.prop_id).ok_or(DrmError::NotFound)?;
    let request = AtomicRequest {
        assignments: vec![PropertyAssignment {
            object,
            property,
            value: args.value,
        }],
        flags: CommitFlags {
            // A single property assignment is how legacy user space turns a
            // display off through `DPMS`, which is a modeset by any definition.
            allow_modeset: true,
            ..CommitFlags::default()
        },
    };
    config.commit(&request, file)
}

/// `DRM_IOCTL_MODE_ATOMIC`: the whole interface in one ioctl.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic_uapi.c#L1355>
/// (`drm_mode_atomic_ioctl`).
fn atomic(
    device: &DrmDevice,
    file: &DrmFile,
    payload: &mut [u8],
    caller: &CallerContext,
) -> DrmResult<()> {
    let args = payload_read::<mode::Atomic>(payload)?;
    let config = mode_config(device)?;

    if args.flags & !mode::atomic_flags::ALL != 0 || args.reserved != 0 {
        return Err(DrmError::InvalidArgument);
    }
    // The atomic interface is opt-in: a client that has not asked for it does not
    // know that properties, not ioctls, are how state changes.
    if !file.client_caps().atomic {
        return Err(DrmError::InvalidArgument);
    }
    // A test commit changes nothing, so an event would never arrive.
    if args.flags & mode::atomic_flags::TEST_ONLY != 0
        && args.flags & mode::atomic_flags::PAGE_FLIP_EVENT != 0
    {
        return Err(DrmError::InvalidArgument);
    }
    // `DRM_CAP_ASYNC_PAGE_FLIP` reports 0, so asking for a tearing flip is asking
    // for something we said we cannot do.
    if args.flags & mode::atomic_flags::PAGE_FLIP_ASYNC != 0 {
        return Err(DrmError::InvalidArgument);
    }

    let objects: Vec<u32> = copy_in_array(caller, args.objs_ptr, args.count_objs)?;
    let counts: Vec<u32> = copy_in_array(caller, args.count_props_ptr, args.count_objs)?;

    let total = counts
        .iter()
        .try_fold(0u32, |total, count| total.checked_add(*count))
        .ok_or(DrmError::InvalidArgument)?;
    let properties: Vec<u32> = copy_in_array(caller, args.props_ptr, total)?;
    let values: Vec<u64> = copy_in_array(caller, args.prop_values_ptr, total)?;
    if properties.len() != total as usize || values.len() != total as usize {
        return Err(DrmError::InvalidArgument);
    }

    let mut assignments = Vec::with_capacity(total as usize);
    let mut next = 0usize;
    for (object, count) in objects.iter().zip(&counts) {
        let object = ObjectId::from_raw(*object).ok_or(DrmError::NotFound)?;
        // Even a zero-property entry must name an object with KMS properties.
        if !matches!(
            config.object_kind(object),
            Some(ObjectKind::Crtc | ObjectKind::Connector | ObjectKind::Plane)
        ) {
            return Err(DrmError::NotFound);
        }
        for index in next..next + *count as usize {
            assignments.push(PropertyAssignment {
                object,
                property: ObjectId::from_raw(properties[index]).ok_or(DrmError::NotFound)?,
                value: values[index],
            });
        }
        next += *count as usize;
    }

    let request = AtomicRequest {
        assignments,
        flags: CommitFlags {
            allow_modeset: args.flags & mode::atomic_flags::ALLOW_MODESET != 0,
            test_only: args.flags & mode::atomic_flags::TEST_ONLY != 0,
            nonblock: args.flags & mode::atomic_flags::NONBLOCK != 0,
            event: args.flags & mode::atomic_flags::PAGE_FLIP_EVENT != 0,
            user_data: args.user_data,
        },
    };
    config.commit(&request, file)
}
