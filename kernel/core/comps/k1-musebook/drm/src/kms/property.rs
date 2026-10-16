// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! KMS properties: the only way user space changes display state.
//!
//! Every mode object exposes a set of named properties, and an atomic commit is
//! nothing but a list of `(object, property, value)` triples. That makes this
//! module the single interpreter of what a value means — including for the legacy
//! ioctls, which are translated into triples before anything is applied.
//!
//! # Why the property set is fixed
//!
//! Linux lets a driver create arbitrary properties. Nothing in this tree needs
//! that: the K1 display pipeline and the software driver both expose exactly the
//! standard set below, and user space matches properties by name against a
//! hard-coded list anyway. A closed set means an unknown property ID is an error
//! rather than a silent no-op, and it means the value of every property has one
//! documented meaning.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_mode_config.c#L336>
//! (`drm_mode_create_standard_properties`).

use alloc::{vec, vec::Vec};

use ostd_pod::IntoBytes;

use super::{AtomicState, ModeConfig, ObjectId, ObjectIds, ObjectKind};
use crate::{
    error::{DrmError, DrmResult},
    uapi::mode,
};

/// One assignment in an atomic commit.
///
/// This is the unit of change in the whole mode-setting interface. `MODE_ATOMIC`
/// carries a list of these directly; the legacy ioctls are translated into lists
/// of them by [`super::legacy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyAssignment {
    /// The object being modified.
    pub object: ObjectId,
    /// The property being set.
    pub property: ObjectId,
    /// The new value.
    pub value: u64,
}

/// The standard properties, which are the only ones this crate exposes.
///
/// The names are what user space matches on and are therefore ABI: `modetest`
/// and `cosmic-comp` look up "FB_ID" and "MODE_ID" by string, so the
/// capitalisation is Linux's, inconsistencies included ("type" is lower case,
/// everything else is not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardProperty {
    /// CRTC `ACTIVE`: whether the CRTC is scanning out.
    CrtcActive,
    /// CRTC `MODE_ID`: a blob holding a `drm_mode_modeinfo`, or zero.
    CrtcModeId,
    /// Plane `type`: primary, cursor or overlay. Immutable.
    PlaneType,
    /// Plane `FB_ID`: the framebuffer to scan out, or zero to disable.
    PlaneFbId,
    /// Plane `CRTC_ID`: the CRTC to attach to, or zero to detach.
    PlaneCrtcId,
    /// Plane `CRTC_X`: destination X, signed so a plane may hang off screen.
    PlaneCrtcX,
    /// Plane `CRTC_Y`: destination Y.
    PlaneCrtcY,
    /// Plane `CRTC_W`: destination width.
    PlaneCrtcW,
    /// Plane `CRTC_H`: destination height.
    PlaneCrtcH,
    /// Plane `SRC_X`: source X in 16.16 fixed point.
    PlaneSrcX,
    /// Plane `SRC_Y`: source Y in 16.16 fixed point.
    PlaneSrcY,
    /// Plane `SRC_W`: source width in 16.16 fixed point.
    PlaneSrcW,
    /// Plane `SRC_H`: source height in 16.16 fixed point.
    PlaneSrcH,
    /// Plane `IN_FORMATS`: a blob listing format/modifier pairs. Immutable.
    PlaneInFormats,
    /// Connector `CRTC_ID`: the CRTC driving this connector, or zero.
    ConnectorCrtcId,
    /// Connector `DPMS`: the power state, derived from the CRTC's `ACTIVE`.
    ConnectorDpms,
    /// Connector `EDID`: a blob holding the sink's EDID. Immutable.
    ConnectorEdid,
}

/// The `type` property's values, as `drm_plane_type` names them.
const PLANE_TYPE_ENUM: &[(u64, &str)] = &[(0, "Overlay"), (1, "Primary"), (2, "Cursor")];

/// The `DPMS` property's values.
const DPMS_ENUM: &[(u64, &str)] = &[
    (mode::dpms::ON, "On"),
    (mode::dpms::STANDBY, "Standby"),
    (mode::dpms::SUSPEND, "Suspend"),
    (mode::dpms::OFF, "Off"),
];

impl StandardProperty {
    /// Every standard property, in the order user space is shown them.
    pub const ALL: &'static [Self] = &[
        Self::CrtcActive,
        Self::CrtcModeId,
        Self::PlaneType,
        Self::PlaneFbId,
        Self::PlaneCrtcId,
        Self::PlaneCrtcX,
        Self::PlaneCrtcY,
        Self::PlaneCrtcW,
        Self::PlaneCrtcH,
        Self::PlaneSrcX,
        Self::PlaneSrcY,
        Self::PlaneSrcW,
        Self::PlaneSrcH,
        Self::PlaneInFormats,
        Self::ConnectorEdid,
        Self::ConnectorDpms,
        Self::ConnectorCrtcId,
    ];

    /// Returns the name user space knows this property by.
    pub fn name(self) -> &'static str {
        match self {
            Self::CrtcActive => "ACTIVE",
            Self::CrtcModeId => "MODE_ID",
            Self::PlaneType => "type",
            Self::PlaneFbId => "FB_ID",
            Self::PlaneCrtcId | Self::ConnectorCrtcId => "CRTC_ID",
            Self::PlaneCrtcX => "CRTC_X",
            Self::PlaneCrtcY => "CRTC_Y",
            Self::PlaneCrtcW => "CRTC_W",
            Self::PlaneCrtcH => "CRTC_H",
            Self::PlaneSrcX => "SRC_X",
            Self::PlaneSrcY => "SRC_Y",
            Self::PlaneSrcW => "SRC_W",
            Self::PlaneSrcH => "SRC_H",
            Self::PlaneInFormats => "IN_FORMATS",
            Self::ConnectorDpms => "DPMS",
            Self::ConnectorEdid => "EDID",
        }
    }

    /// Returns which kind of object carries this property.
    pub fn object_kind(self) -> ObjectKind {
        match self {
            Self::CrtcActive | Self::CrtcModeId => ObjectKind::Crtc,
            Self::PlaneType
            | Self::PlaneFbId
            | Self::PlaneCrtcId
            | Self::PlaneCrtcX
            | Self::PlaneCrtcY
            | Self::PlaneCrtcW
            | Self::PlaneCrtcH
            | Self::PlaneSrcX
            | Self::PlaneSrcY
            | Self::PlaneSrcW
            | Self::PlaneSrcH
            | Self::PlaneInFormats => ObjectKind::Plane,
            Self::ConnectorCrtcId | Self::ConnectorDpms | Self::ConnectorEdid => {
                ObjectKind::Connector
            }
        }
    }

    /// Returns what kind of value this property carries.
    pub fn kind(self) -> PropertyKind {
        match self {
            Self::CrtcActive => PropertyKind::Range { min: 0, max: 1 },
            Self::CrtcModeId | Self::PlaneInFormats | Self::ConnectorEdid => PropertyKind::Blob,
            Self::PlaneType => PropertyKind::Enum(PLANE_TYPE_ENUM),
            Self::ConnectorDpms => PropertyKind::Enum(DPMS_ENUM),
            Self::PlaneFbId => PropertyKind::Object {
                object_type: mode::object_type::FB,
            },
            Self::PlaneCrtcId | Self::ConnectorCrtcId => PropertyKind::Object {
                object_type: mode::object_type::CRTC,
            },
            Self::PlaneCrtcX | Self::PlaneCrtcY => PropertyKind::SignedRange {
                min: i32::MIN as i64,
                max: i32::MAX as i64,
            },
            Self::PlaneCrtcW | Self::PlaneCrtcH => PropertyKind::Range {
                min: 0,
                max: i32::MAX as u64,
            },
            // The source rectangle is 16.16 fixed point, so its range is the
            // whole of `u32` rather than the `INT_MAX` the destination uses.
            Self::PlaneSrcX | Self::PlaneSrcY | Self::PlaneSrcW | Self::PlaneSrcH => {
                PropertyKind::Range {
                    min: 0,
                    max: u32::MAX as u64,
                }
            }
        }
    }

    /// Returns whether user space may only read this property.
    pub fn is_immutable(self) -> bool {
        matches!(
            self,
            Self::PlaneType | Self::PlaneInFormats | Self::ConnectorEdid
        )
    }

    /// Returns whether this property is part of the atomic interface.
    ///
    /// A property with this bit is hidden from clients that have not set
    /// `DRM_CLIENT_CAP_ATOMIC`, because a legacy client that saw `FB_ID` would
    /// have no way to set it.
    pub fn is_atomic(self) -> bool {
        !matches!(
            self,
            Self::PlaneType | Self::PlaneInFormats | Self::ConnectorEdid | Self::ConnectorDpms
        )
    }

    /// Returns the `DRM_MODE_PROP_*` flags `MODE_GETPROPERTY` reports.
    pub fn flags(self) -> u32 {
        let mut flags = match self.kind() {
            PropertyKind::Range { .. } => mode::prop_flags::RANGE,
            PropertyKind::SignedRange { .. } => mode::prop_flags::SIGNED_RANGE,
            PropertyKind::Enum(_) => mode::prop_flags::ENUM,
            PropertyKind::Blob => mode::prop_flags::BLOB,
            PropertyKind::Object { .. } => mode::prop_flags::OBJECT,
        };
        if self.is_immutable() {
            flags |= mode::prop_flags::IMMUTABLE;
        }
        if self.is_atomic() {
            flags |= mode::prop_flags::ATOMIC;
        }
        flags
    }
}

/// What kind of value a property carries.
#[derive(Clone, Copy, Debug)]
pub enum PropertyKind {
    /// An unsigned value within an inclusive range.
    Range {
        /// Lowest legal value.
        min: u64,
        /// Highest legal value.
        max: u64,
    },
    /// A signed value within an inclusive range, carried as a sign-extended `u64`.
    SignedRange {
        /// Lowest legal value.
        min: i64,
        /// Highest legal value.
        max: i64,
    },
    /// One of a fixed set of named values.
    Enum(&'static [(u64, &'static str)]),
    /// The object ID of a property blob, or zero.
    Blob,
    /// The object ID of a mode object of one type, or zero.
    Object {
        /// The `DRM_MODE_OBJECT_*` type the value must name.
        object_type: u32,
    },
}

/// One property of a device, with the ID user space refers to it by.
#[derive(Clone, Copy, Debug)]
pub struct Property {
    /// The property's object ID.
    pub id: ObjectId,
    /// Which standard property this is.
    pub standard: StandardProperty,
}

/// A device's property table.
#[derive(Debug)]
pub struct Properties {
    /// One entry per [`StandardProperty::ALL`], in that order.
    entries: Vec<Property>,
}

impl Properties {
    /// Creates the standard properties, numbering them from `ids`.
    pub(super) fn new(ids: &ObjectIds) -> Self {
        let entries = StandardProperty::ALL
            .iter()
            .map(|standard| Property {
                id: ids.allocate(),
                standard: *standard,
            })
            .collect();
        Self { entries }
    }

    /// Returns the ID of a standard property.
    pub fn id(&self, standard: StandardProperty) -> ObjectId {
        self.entries
            .iter()
            .find(|entry| entry.standard == standard)
            .expect("every standard property is in the table")
            .id
    }

    /// Looks up a property by object ID.
    pub fn lookup(&self, id: ObjectId) -> Option<Property> {
        self.entries.iter().copied().find(|entry| entry.id == id)
    }

    /// Returns every property ID.
    pub fn ids(&self) -> Vec<u32> {
        self.entries.iter().map(|entry| entry.id.get()).collect()
    }

    /// Returns the properties of one kind of object that a client may see.
    ///
    /// `atomic_client` is [`ClientCaps::atomic`]: without it, the atomic-only
    /// properties are hidden, exactly as `drm_mode_object_get_properties` hides
    /// them.
    ///
    /// [`ClientCaps::atomic`]: crate::file::ClientCaps::atomic
    pub fn visible(&self, kind: ObjectKind, atomic_client: bool) -> Vec<Property> {
        self.entries
            .iter()
            .copied()
            .filter(|entry| entry.standard.object_kind() == kind)
            .filter(|entry| atomic_client || !entry.standard.is_atomic())
            .collect()
    }
}

impl ModeConfig {
    /// Returns the current value of a property of an object.
    ///
    /// The value is always derived from the committed state rather than stored
    /// alongside it, so there is no second copy that could disagree with what the
    /// hardware was last told.
    pub fn property_value(
        &self,
        object: ObjectId,
        standard: StandardProperty,
        state: &AtomicState,
    ) -> DrmResult<u64> {
        match standard {
            StandardProperty::CrtcActive => Ok(state.crtc(object)?.active as u64),
            StandardProperty::CrtcModeId => Ok(state
                .crtc(object)?
                .mode_blob
                .unwrap_or(ObjectId::NONE)
                .get() as u64),
            StandardProperty::PlaneType => Ok(self.plane(object)?.plane_type().to_uapi()),
            StandardProperty::PlaneFbId => Ok(state
                .plane(object)?
                .framebuffer
                .as_ref()
                .map_or(0, |framebuffer| framebuffer.id.get() as u64)),
            StandardProperty::PlaneCrtcId => {
                Ok(state.plane(object)?.crtc.unwrap_or(ObjectId::NONE).get() as u64)
            }
            StandardProperty::PlaneCrtcX => Ok(state.plane(object)?.crtc_rect.0 as i64 as u64),
            StandardProperty::PlaneCrtcY => Ok(state.plane(object)?.crtc_rect.1 as i64 as u64),
            StandardProperty::PlaneCrtcW => Ok(state.plane(object)?.crtc_rect.2 as u64),
            StandardProperty::PlaneCrtcH => Ok(state.plane(object)?.crtc_rect.3 as u64),
            StandardProperty::PlaneSrcX => Ok(state.plane(object)?.src_rect.0 as u64),
            StandardProperty::PlaneSrcY => Ok(state.plane(object)?.src_rect.1 as u64),
            StandardProperty::PlaneSrcW => Ok(state.plane(object)?.src_rect.2 as u64),
            StandardProperty::PlaneSrcH => Ok(state.plane(object)?.src_rect.3 as u64),
            StandardProperty::PlaneInFormats => {
                Ok(self.plane_in_formats(self.plane(object)?.id()).get() as u64)
            }
            StandardProperty::ConnectorCrtcId => Ok(state
                .crtc_of_connector(object)
                .unwrap_or(ObjectId::NONE)
                .get() as u64),
            // Divergence: on an atomic driver Linux derives `DPMS` from the
            // CRTC's `ACTIVE` too, but keeps a separate connector field that a
            // legacy `SETPROPERTY` writes. We have no second field, so a
            // connector with no CRTC always reads back as off rather than
            // remembering what it was last set to. A connector attached to an
            // active CRTC therefore reads On, including when that attachment was
            // seeded at registration rather than committed.
            StandardProperty::ConnectorDpms => {
                let active = state
                    .crtc_of_connector(object)
                    .and_then(|crtc| state.crtc(crtc).ok())
                    .is_some_and(|crtc| crtc.active);
                Ok(if active {
                    mode::dpms::ON
                } else {
                    mode::dpms::OFF
                })
            }
            StandardProperty::ConnectorEdid => {
                Ok(self.connector_edid(self.connector(object)?.id()).get() as u64)
            }
        }
    }
}

/// Builds the `IN_FORMATS` blob of a plane.
///
/// Every listed modifier is reported as applying to every listed format, which is
/// true for both drivers in this tree: neither has a modifier that only some
/// formats can be tiled with. The bitmask is 64 bits wide, so a plane with more
/// than 64 formats needs one entry per modifier and per 64-format group — hence
/// the chunking, which costs nothing here and stops the blob from silently
/// dropping formats if a driver ever grows a long list.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L154>
/// (`create_in_format_blob`).
pub(super) fn in_formats_blob(formats: &[u32], modifiers: &[u64]) -> DrmResult<Vec<u8>> {
    const GROUP: usize = u64::BITS as usize;

    if formats.is_empty() || modifiers.is_empty() {
        return Err(DrmError::InvalidArgument);
    }

    let header_size = size_of::<mode::FormatModifierBlob>();
    let formats_size = size_of_val(formats);
    // The modifier array must be eight-byte aligned, as Linux's `ALIGN(..., 8)`
    // makes it: user space casts the offset to a pointer to a `__u64`-containing
    // structure.
    let modifiers_offset = (header_size + formats_size).next_multiple_of(8);

    let groups = formats.len().div_ceil(GROUP);
    let entries: Vec<mode::FormatModifier> = modifiers
        .iter()
        .flat_map(|modifier| {
            (0..groups).map(move |group| {
                let remaining = formats.len() - group * GROUP;
                let bits = if remaining >= GROUP {
                    u64::MAX
                } else {
                    (1u64 << remaining) - 1
                };
                mode::FormatModifier {
                    formats: bits,
                    offset: (group * GROUP) as u32,
                    pad: 0,
                    modifier: *modifier,
                }
            })
        })
        .collect();

    let header = mode::FormatModifierBlob {
        version: mode::FORMAT_BLOB_CURRENT,
        flags: 0,
        count_formats: formats.len() as u32,
        formats_offset: header_size as u32,
        count_modifiers: entries.len() as u32,
        modifiers_offset: modifiers_offset as u32,
    };

    let mut blob = vec![0u8; modifiers_offset + entries.len() * size_of::<mode::FormatModifier>()];
    blob[..header_size].copy_from_slice(header.as_bytes());
    blob[header_size..header_size + formats_size].copy_from_slice(formats.as_bytes());
    blob[modifiers_offset..].copy_from_slice(entries.as_bytes());
    Ok(blob)
}
