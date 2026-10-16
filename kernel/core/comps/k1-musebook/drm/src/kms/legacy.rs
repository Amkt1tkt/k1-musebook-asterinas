// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Translating the legacy mode-setting ioctls into atomic commits.
//!
//! `SETCRTC`, `PAGE_FLIP` and `SETPLANE` predate the atomic interface and each
//! describes a change in its own shape. Linux keeps a second implementation for
//! them; we translate instead, into exactly the property assignments an atomic
//! client would have sent, and then run [`ModeConfig::commit`].
//!
//! Divergence from Linux, and a deliberate one: there is no legacy path. The
//! reason is not tidiness. Two implementations of "put this framebuffer on that
//! CRTC" drift, and the drift shows up as a bug that reproduces through one ioctl
//! and not the other — which is the hardest kind of mode-setting bug to
//! attribute. The cost of translating is that a legacy request is subject to the
//! atomic validation rules, which are stricter; where that changes an error code,
//! it is called out below.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic_helper.c#L3068>
//! (`__drm_atomic_helper_set_config`, the same translation Linux performs for
//! atomic drivers before its own legacy path takes over), and
//! <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_crtc.c#L568>
//! (`drm_mode_setcrtc`).

use alloc::{vec, vec::Vec};

use ostd_pod::IntoBytes;

use super::{
    AtomicRequest, BlobOwner, CommitFlags, ModeConfig, ObjectId, PropertyAssignment,
    StandardProperty, atomic::check_mode,
};
use crate::{
    error::{DrmError, DrmResult},
    file::DrmFile,
    uapi::mode,
};

/// The `fb_id` value that means "keep whatever the CRTC is already scanning out".
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_crtc.c#L640>.
const FB_ID_KEEP_CURRENT: u32 = u32::MAX;

impl ModeConfig {
    /// Translates a legacy `SETCRTC` request into an atomic commit.
    ///
    /// `connectors` is the already-copied-in `set_connectors_ptr` array.
    pub fn legacy_set_crtc(
        &self,
        request: &mode::Crtc,
        connectors: &[u32],
        file: &DrmFile,
    ) -> DrmResult<()> {
        let crtc = self.crtc(ObjectId(request.crtc_id))?.id();
        let primary = self.crtc(crtc)?.primary_plane().id();
        let state = self.state();

        // Linux's own consistency rules for the legacy request, checked before
        // anything is translated so that the error codes stay what user space
        // expects.
        if request.mode_valid != 0 {
            if connectors.is_empty() {
                return Err(DrmError::InvalidArgument);
            }
        } else if !connectors.is_empty() {
            return Err(DrmError::InvalidArgument);
        }

        let mut assignments = Vec::new();

        // Whatever is on this CRTC now goes away first, exactly as
        // `drm_atomic_helper_set_config` does: the request lists the connectors it
        // wants, not the ones it wants added.
        for connector in &state.crtc(crtc)?.connectors {
            assignments.push(self.assign(
                *connector,
                StandardProperty::ConnectorCrtcId,
                ObjectId::NONE.get() as u64,
            ));
        }

        if request.mode_valid == 0 {
            // No mode means shut the pipeline down. A framebuffer or a connector
            // alongside would be contradictory, and Linux warns about it.
            assignments.extend([
                self.assign(crtc, StandardProperty::CrtcActive, 0),
                self.assign(crtc, StandardProperty::CrtcModeId, 0),
                self.assign(primary, StandardProperty::PlaneCrtcId, 0),
                self.assign(primary, StandardProperty::PlaneFbId, 0),
            ]);
            return self.commit(
                &AtomicRequest {
                    assignments,
                    flags: CommitFlags {
                        allow_modeset: true,
                        ..CommitFlags::default()
                    },
                },
                file,
            );
        }

        let mode = request.mode;
        check_mode(&mode)?;

        // `fb_id == -1` asks for a modeset with the framebuffer already in use,
        // which is how user space changes the mode without reallocating.
        let framebuffer = if request.fb_id == FB_ID_KEEP_CURRENT {
            state
                .plane(primary)?
                .framebuffer
                .as_ref()
                .ok_or(DrmError::InvalidArgument)?
                .clone()
        } else {
            self.framebuffer(ObjectId::from_raw(request.fb_id).ok_or(DrmError::InvalidArgument)?)?
        };

        // The viewport check belongs here rather than in the atomic core because
        // Linux reports it as `ENOSPC` for this ioctl specifically, and `modetest`
        // prints the errno.
        let (hdisplay, vdisplay) = (u32::from(mode.hdisplay), u32::from(mode.vdisplay));
        if u64::from(request.x) + u64::from(hdisplay) > u64::from(framebuffer.width)
            || u64::from(request.y) + u64::from(vdisplay) > u64::from(framebuffer.height)
        {
            return Err(DrmError::NoSpace);
        }

        // Validate every connector before creating a core-owned mode blob:
        // an invalid ID must not return early and leak that allocation.
        for connector in connectors {
            self.connector(ObjectId(*connector))?;
        }
        let mode_blob = self.insert_blob(mode.as_bytes().to_vec(), BlobOwner::Mode)?;

        assignments.extend([
            self.assign(
                crtc,
                StandardProperty::CrtcModeId,
                mode_blob.id.get() as u64,
            ),
            self.assign(crtc, StandardProperty::CrtcActive, 1),
        ]);
        for connector in connectors {
            let connector = self.connector(ObjectId(*connector))?.id();
            assignments.push(self.assign(
                connector,
                StandardProperty::ConnectorCrtcId,
                crtc.get() as u64,
            ));
        }
        assignments.extend(self.assign_plane(
            primary,
            crtc,
            framebuffer.id,
            (0, 0, hdisplay, vdisplay),
            (
                request.x << 16,
                request.y << 16,
                hdisplay << 16,
                vdisplay << 16,
            ),
        ));

        let result = self.commit(
            &AtomicRequest {
                assignments,
                flags: CommitFlags {
                    allow_modeset: true,
                    ..CommitFlags::default()
                },
            },
            file,
        );

        if result.is_err() {
            // The blob above belongs to no client, so nothing else would ever free
            // it: a rejected modeset must not cost a blob. A successful commit
            // collects the one it superseded by itself.
            self.collect_mode_blobs();
        }
        result
    }

    /// Translates a legacy `PAGE_FLIP` request into an atomic commit.
    ///
    /// The flip keeps the plane's geometry and changes only the framebuffer, which
    /// is why it can be — and is — refused if it would need a modeset.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L1177>
    /// (`drm_mode_page_flip_ioctl`).
    pub fn legacy_page_flip(&self, request: &mode::CrtcPageFlip, file: &DrmFile) -> DrmResult<()> {
        if request.flags & !mode::page_flip_flags::ALL != 0 || request.reserved != 0 {
            return Err(DrmError::InvalidArgument);
        }
        // `DRM_CAP_ASYNC_PAGE_FLIP` and `DRM_CAP_PAGE_FLIP_TARGET` both report 0,
        // so a client asking for either is asking for something we promised not to
        // do.
        if request.flags
            & (mode::page_flip_flags::ASYNC
                | mode::page_flip_flags::TARGET_ABSOLUTE
                | mode::page_flip_flags::TARGET_RELATIVE)
            != 0
        {
            return Err(DrmError::InvalidArgument);
        }

        let crtc = self.crtc(ObjectId(request.crtc_id))?.id();
        let primary = self.crtc(crtc)?.primary_plane().id();
        let framebuffer =
            self.framebuffer(ObjectId::from_raw(request.fb_id).ok_or(DrmError::NotFound)?)?;

        let state = self.state();
        let plane = state.plane(primary)?;

        // Flipping before the first modeset has nothing to flip from. Linux
        // reports this as `EBUSY`, which user space reads as "not ready yet"
        // rather than as "your request is malformed".
        if plane.framebuffer.is_none() || plane.crtc != Some(crtc) {
            return Err(DrmError::Busy);
        }

        // The plane keeps its rectangles, so the new framebuffer has to be big
        // enough for the source rectangle already in force.
        let (src_x, src_y, src_w, src_h) = plane.src_rect;
        if u64::from(src_x) + u64::from(src_w) > u64::from(framebuffer.width) << 16
            || u64::from(src_y) + u64::from(src_h) > u64::from(framebuffer.height) << 16
        {
            return Err(DrmError::NoSpace);
        }

        // `CRTC_ID` is assigned even though it does not change, because that is
        // what brings the CRTC into the commit and therefore what makes the
        // completion event possible. `drm_atomic_helper_page_flip` does the same.
        let assignments = vec![
            self.assign(primary, StandardProperty::PlaneCrtcId, crtc.get() as u64),
            self.assign(
                primary,
                StandardProperty::PlaneFbId,
                framebuffer.id.get() as u64,
            ),
        ];

        self.commit(
            &AtomicRequest {
                assignments,
                flags: CommitFlags {
                    allow_modeset: false,
                    event: request.flags & mode::page_flip_flags::EVENT != 0,
                    user_data: request.user_data,
                    ..CommitFlags::default()
                },
            },
            file,
        )
    }

    /// Translates a legacy `SETPLANE` request into an atomic commit.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_plane.c#L933>
    /// (`drm_mode_setplane`).
    pub fn legacy_set_plane(&self, request: &mode::SetPlane, file: &DrmFile) -> DrmResult<()> {
        if request.flags & !mode::present_flags::ALL != 0 {
            return Err(DrmError::InvalidArgument);
        }

        let plane = self.plane(ObjectId(request.plane_id))?.id();

        // No framebuffer means turn the plane off, and then the CRTC is ignored
        // rather than required to be valid.
        let assignments = match ObjectId::from_raw(request.fb_id) {
            None => vec![
                self.assign(plane, StandardProperty::PlaneCrtcId, 0),
                self.assign(plane, StandardProperty::PlaneFbId, 0),
            ],
            Some(fb_id) => {
                let crtc = self.crtc(ObjectId(request.crtc_id))?.id();
                let framebuffer = self.framebuffer(fb_id)?;
                self.assign_plane(
                    plane,
                    crtc,
                    framebuffer.id,
                    (
                        request.crtc_x,
                        request.crtc_y,
                        request.crtc_w,
                        request.crtc_h,
                    ),
                    (request.src_x, request.src_y, request.src_w, request.src_h),
                )
                .to_vec()
            }
        };

        self.commit(
            &AtomicRequest {
                assignments,
                flags: CommitFlags::default(),
            },
            file,
        )
    }

    /// Builds one assignment, resolving the property's ID.
    fn assign(
        &self,
        object: ObjectId,
        standard: StandardProperty,
        value: u64,
    ) -> PropertyAssignment {
        PropertyAssignment {
            object,
            property: self.properties().id(standard),
            value,
        }
    }

    /// Builds the assignments that place a framebuffer on a CRTC through a plane.
    ///
    /// Both legacy entry points that show a framebuffer go through this, so
    /// "`SETCRTC` and `SETPLANE` agree" is true by construction rather than by
    /// inspection.
    fn assign_plane(
        &self,
        plane: ObjectId,
        crtc: ObjectId,
        framebuffer: ObjectId,
        crtc_rect: (i32, i32, u32, u32),
        src_rect: (u32, u32, u32, u32),
    ) -> [PropertyAssignment; 10] {
        [
            self.assign(plane, StandardProperty::PlaneCrtcId, crtc.get() as u64),
            self.assign(plane, StandardProperty::PlaneFbId, framebuffer.get() as u64),
            self.assign(
                plane,
                StandardProperty::PlaneCrtcX,
                crtc_rect.0 as i64 as u64,
            ),
            self.assign(
                plane,
                StandardProperty::PlaneCrtcY,
                crtc_rect.1 as i64 as u64,
            ),
            self.assign(plane, StandardProperty::PlaneCrtcW, u64::from(crtc_rect.2)),
            self.assign(plane, StandardProperty::PlaneCrtcH, u64::from(crtc_rect.3)),
            self.assign(plane, StandardProperty::PlaneSrcX, u64::from(src_rect.0)),
            self.assign(plane, StandardProperty::PlaneSrcY, u64::from(src_rect.1)),
            self.assign(plane, StandardProperty::PlaneSrcW, u64::from(src_rect.2)),
            self.assign(plane, StandardProperty::PlaneSrcH, u64::from(src_rect.3)),
        ]
    }
}
