// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Atomic state, validation and commit.
//!
//! This is the only path to the display hardware. A commit starts from the
//! currently committed state, applies a list of property assignments to a copy of
//! it, validates the result, hands it to the driver, and only then swaps it in.
//! Nothing partially applied is ever visible, which is the whole point of the
//! atomic interface — and the reason the legacy ioctls are translated into this
//! rather than given their own path.
//!
//! Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic.c>
//! and <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic_uapi.c>.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};

use ostd::timer::Jiffies;
use ostd_pod::IntoBytes;

use super::{
    BlobOwner, Crtc, FlipTiming, Framebuffer, InitialCrtc, ModeConfig, ObjectId, Plane,
    PropertyAssignment, PropertyBlob, StandardProperty,
};
use crate::{
    error::{DrmError, DrmResult},
    file::{DrmEvent, DrmFile},
    uapi::mode::ModeInfo,
};

/// How a commit is to be carried out, as `DRM_MODE_ATOMIC_*` describes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitFlags {
    /// Whether the commit may change the active mode or the routing.
    ///
    /// A commit that needs a modeset without this is refused, so that a
    /// compositor cannot accidentally black the screen out mid-frame.
    pub allow_modeset: bool,
    /// Whether to only validate.
    pub test_only: bool,
    /// Whether the caller declined to wait for the commit to land.
    ///
    /// Accepted and ignored: the ioctl never sleeps on vblank. A driver that
    /// reports [`FlipTiming::AtVblank`] still returns here; the completion
    /// event arrives later, from the interrupt. `NONBLOCK` has nothing extra
    /// to switch off.
    pub nonblock: bool,
    /// Whether the caller wants a completion event.
    pub event: bool,
    /// The cookie to echo back in the completion event.
    pub user_data: u64,
}

/// The requested state of one plane within a commit.
#[derive(Clone, Debug)]
pub struct PlaneState {
    /// The plane being programmed.
    pub plane: ObjectId,
    /// The CRTC to attach to, or `None` to disable the plane.
    pub crtc: Option<ObjectId>,
    /// The framebuffer to scan out, or `None` to disable the plane.
    pub framebuffer: Option<Arc<Framebuffer>>,
    /// Destination rectangle `(x, y, w, h)`, signed so the plane may hang off
    /// screen.
    pub crtc_rect: (i32, i32, u32, u32),
    /// Source rectangle `(x, y, w, h)` in 16.16 fixed point.
    pub src_rect: (u32, u32, u32, u32),
}

/// The requested state of one CRTC within a commit.
#[derive(Clone, Debug)]
pub struct CrtcState {
    /// The CRTC being programmed.
    pub crtc: ObjectId,
    /// Whether the CRTC should be scanning out.
    pub active: bool,
    /// The mode to program, or `None` when the CRTC is not configured.
    ///
    /// Linux calls this `enable`, and keeps it distinct from `active`: a
    /// configured but inactive CRTC is what DPMS off looks like, and it is what
    /// lets the compositor come back without a full modeset.
    pub mode: Option<ModeInfo>,
    /// The blob holding [`mode`](Self::mode), which is what `MODE_ID` reports.
    ///
    /// Always `Some` exactly when `mode` is, since the blob is where the mode came
    /// from.
    pub mode_blob: Option<ObjectId>,
    /// The connectors this CRTC should drive.
    pub connectors: Vec<ObjectId>,
}

/// A complete requested display state.
///
/// Atomic commits are all-or-nothing: [`AtomicDriver::atomic_check`] either
/// accepts the whole state or rejects it, and only an accepted state is applied.
/// Partial application is what the atomic interface exists to prevent.
///
/// Every CRTC and every plane of the device has an entry, whether the commit
/// touched it or not, so a driver never has to look anywhere else to know what
/// the hardware should be doing.
#[derive(Clone, Debug)]
pub struct AtomicState {
    /// Per-CRTC requested state, in CRTC index order.
    pub crtcs: Vec<CrtcState>,
    /// Per-plane requested state.
    pub planes: Vec<PlaneState>,
    /// How this commit was requested.
    ///
    /// On the state stored in [`ModeConfig`] these describe the commit that
    /// produced it, which is of no use to anyone but is harmless; a driver reads
    /// them from the state it is handed.
    pub flags: CommitFlags,
    // Candidate and committed states retain their mode blobs independently of
    // the creating file. This closes the prepare-versus-destroy race.
    mode_blobs: BTreeMap<ObjectId, Arc<PropertyBlob>>,
}

impl AtomicState {
    /// Builds the state of a device that has not been programmed yet.
    ///
    /// [`ModeConfig::new`] overlays [`super::ModeConfigDesc::initial`] on top of
    /// this when the hardware was already scanning out at registration.
    pub(super) fn initial(crtcs: &[Arc<dyn Crtc>], planes: &[Arc<dyn Plane>]) -> Self {
        Self {
            crtcs: crtcs
                .iter()
                .map(|crtc| CrtcState {
                    crtc: crtc.id(),
                    active: false,
                    mode: None,
                    mode_blob: None,
                    connectors: Vec::new(),
                })
                .collect(),
            planes: planes
                .iter()
                .map(|plane| PlaneState {
                    plane: plane.id(),
                    crtc: None,
                    framebuffer: None,
                    crtc_rect: (0, 0, 0, 0),
                    src_rect: (0, 0, 0, 0),
                })
                .collect(),
            flags: CommitFlags::default(),
            mode_blobs: BTreeMap::new(),
        }
    }

    /// Returns the state of one CRTC.
    pub fn crtc(&self, id: ObjectId) -> DrmResult<&CrtcState> {
        self.crtcs
            .iter()
            .find(|state| state.crtc == id)
            .ok_or(DrmError::NotFound)
    }

    /// Returns the state of one plane.
    pub fn plane(&self, id: ObjectId) -> DrmResult<&PlaneState> {
        self.planes
            .iter()
            .find(|state| state.plane == id)
            .ok_or(DrmError::NotFound)
    }

    /// Returns which CRTC drives a connector, if any.
    pub fn crtc_of_connector(&self, connector: ObjectId) -> Option<ObjectId> {
        self.crtcs
            .iter()
            .find(|state| state.connectors.contains(&connector))
            .map(|state| state.crtc)
    }

    fn crtc_mut(&mut self, id: ObjectId) -> DrmResult<&mut CrtcState> {
        self.crtcs
            .iter_mut()
            .find(|state| state.crtc == id)
            .ok_or(DrmError::NotFound)
    }

    fn plane_mut(&mut self, id: ObjectId) -> DrmResult<&mut PlaneState> {
        self.planes
            .iter_mut()
            .find(|state| state.plane == id)
            .ok_or(DrmError::NotFound)
    }

    /// Whether this update removes a plane's current framebuffer reference.
    /// Geometry-only updates retain the same buffer in the new state and need
    /// no extra DMA owner; replacing or detaching it does.
    fn retires_scanout(&self, new: &Self, crtc: ObjectId) -> bool {
        self.planes.iter().any(|old| {
            if old.crtc != Some(crtc) {
                return false;
            }
            let Some(framebuffer) = old.framebuffer.as_ref() else {
                return false;
            };
            !new.planes.iter().any(|plane| {
                plane.plane == old.plane
                    && plane.crtc == old.crtc
                    && plane
                        .framebuffer
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(framebuffer, current))
            })
        })
    }

    /// Returns whether going from `self` to `new` requires reprogramming timings.
    ///
    /// Anything that changes what the display engine outputs — the mode, whether
    /// it outputs at all, or which connectors it feeds — is a modeset. Changing
    /// which framebuffer a plane scans out is not.
    fn needs_modeset(&self, new: &Self) -> bool {
        self.crtcs.iter().zip(&new.crtcs).any(|(old, new)| {
            old.active != new.active
                || old.mode_blob != new.mode_blob
                || !same_mode(old.mode.as_ref(), new.mode.as_ref())
                || old.connectors != new.connectors
        })
    }
}

/// Compares two modes by the fields that describe the signal.
///
/// The name is deliberately excluded: two blobs describing the same timings are
/// the same mode as far as the hardware is concerned, and user space routinely
/// re-sends a mode it read back.
fn same_mode(old: Option<&ModeInfo>, new: Option<&ModeInfo>) -> bool {
    match (old, new) {
        (None, None) => true,
        (Some(old), Some(new)) => {
            old.clock == new.clock
                && old.hdisplay == new.hdisplay
                && old.hsync_start == new.hsync_start
                && old.hsync_end == new.hsync_end
                && old.htotal == new.htotal
                && old.vdisplay == new.vdisplay
                && old.vsync_start == new.vsync_start
                && old.vsync_end == new.vsync_end
                && old.vtotal == new.vtotal
                && old.flags == new.flags
        }
        _ => false,
    }
}

/// The hardware side of mode setting.
pub trait AtomicDriver: Send + Sync + core::fmt::Debug {
    /// Validates a requested state without touching hardware.
    ///
    /// Must be free of side effects: the core calls it for `TEST_ONLY` commits,
    /// and user space uses those to probe configurations.
    ///
    /// The core has already checked everything that follows from the objects
    /// themselves — that the state is internally consistent, that planes are on
    /// CRTCs they can reach, that formats and rectangles are supported. What is
    /// left for the driver is what only it knows, such as bandwidth limits or a
    /// panel that accepts exactly one mode.
    fn atomic_check(&self, state: &AtomicState) -> DrmResult<()>;

    /// Applies a validated state.
    ///
    /// Hardware drivers call [`super::Vblank::begin_update`] immediately before
    /// shadow publication and retain that generation with interrupt status.
    /// Software drivers require no generation when replacing state is immediate.
    ///
    /// Returning an error after having touched the hardware leaves the software
    /// state describing what was there before, so a driver that can fail halfway
    /// must undo its own partial work.
    fn atomic_commit(&self, state: &AtomicState) -> DrmResult<()>;

    /// Returns when a successful commit's completion event should be queued.
    ///
    /// The default is [`FlipTiming::Immediate`]: the event means the register
    /// write went out. A driver with a vblank interrupt overrides this to
    /// [`FlipTiming::AtVblank`] so the event means the panel has started the
    /// new frame.
    fn flip_timing(&self) -> FlipTiming {
        FlipTiming::Immediate
    }
}

/// A commit request: what user space asked for, before anything is resolved.
#[derive(Clone, Debug, Default)]
pub struct AtomicRequest {
    /// The property assignments to apply, in the order given.
    ///
    /// Order matters only in that a later assignment to the same property wins,
    /// exactly as it does in Linux.
    pub assignments: Vec<PropertyAssignment>,
    /// How to carry the commit out.
    pub flags: CommitFlags,
}

impl ModeConfig {
    /// Installs the CRTCs a driver declared were already running at registration.
    ///
    /// This is not a commit: the hardware is already doing what the state
    /// describes, so [`AtomicDriver::atomic_commit`] is not called. The same
    /// object-model checks a commit would run still apply, so a driver cannot
    /// seed a state the core would later refuse. The driver's
    /// [`AtomicDriver::atomic_check`] is asked as well, because only it knows
    /// whether the mode is one it can keep.
    ///
    /// Planes are left detached. Seeding a framebuffer would need an object no
    /// client created via `ADDFB2`; see [`InitialCrtc`].
    pub(super) fn install_initial(&self, specs: Vec<InitialCrtc>) -> DrmResult<()> {
        if specs.is_empty() {
            return Ok(());
        }

        let mut seen_crtcs = BTreeSet::new();
        let mut seen_connectors = BTreeSet::new();
        for spec in &specs {
            if spec.crtc.is_none() || !seen_crtcs.insert(spec.crtc) {
                return Err(DrmError::InvalidArgument);
            }
            for connector in &spec.connectors {
                if connector.is_none() || !seen_connectors.insert(*connector) {
                    return Err(DrmError::InvalidArgument);
                }
            }
        }

        let base = self.state();
        let mut new = (*base).clone();
        // The jump from the empty default to this state is a modeset. The flag
        // is only here so [`Self::check`] accepts it; it is cleared before the
        // state is stored, because no client produced this commit.
        new.flags.allow_modeset = true;

        for spec in specs {
            check_mode(&spec.mode).map_err(|_| DrmError::InvalidArgument)?;
            let blob = self.insert_blob(spec.mode.as_bytes().to_vec(), BlobOwner::Mode)?;
            let blob_id = blob.id;
            new.mode_blobs.insert(spec.crtc, blob);
            let crtc = new
                .crtc_mut(spec.crtc)
                .map_err(|_| DrmError::InvalidArgument)?;
            crtc.active = true;
            crtc.mode = Some(spec.mode);
            crtc.mode_blob = Some(blob_id);
            crtc.connectors = spec.connectors;
        }

        let touched = CommitScope { crtcs: seen_crtcs };
        self.check(&base, &new, &touched)
            .map_err(|error| match error {
                DrmError::NotFound => DrmError::InvalidArgument,
                other => other,
            })?;
        self.atomic_driver().atomic_check(&new)?;

        new.flags = CommitFlags::default();
        *self.state.lock() = Arc::new(new);
        Ok(())
    }

    /// Applies a commit request.
    ///
    /// This is the single entry point to the display hardware: `MODE_ATOMIC`
    /// arrives here directly, and `SETCRTC`, `PAGE_FLIP` and `SETPLANE` arrive
    /// here after [`super::legacy`] has turned them into assignments.
    pub fn commit(&self, request: &AtomicRequest, file: &DrmFile) -> DrmResult<()> {
        // No properties means no display state to validate or program. In
        // particular, preserve firmware scanout until a client configures it.
        if request.assignments.is_empty() {
            // An event needs a CRTC in the transaction to signal completion.
            return if request.flags.event {
                Err(DrmError::InvalidArgument)
            } else {
                Ok(())
            };
        }

        // One commit at a time. Linux takes a per-object lock and asks callers to
        // back off with `EDEADLK`; a single lock cannot deadlock, and the cost is
        // that two clients programming two independent CRTCs serialize — which
        // they already do in practice, since only the master may commit at all.
        let _serialize = self.commit_lock.lock();

        let base = self.state();
        let mut new = (*base).clone();
        new.flags = request.flags;

        let mut touched = CommitScope::default();
        for assignment in &request.assignments {
            self.apply(&mut new, assignment, &mut touched)?;
        }

        self.check(&base, &new, &touched)?;
        self.atomic_driver().atomic_check(&new)?;

        if request.flags.test_only {
            return Ok(());
        }

        // Reserve the room the completion events will need before anything is
        // programmed, as Linux's `drm_event_reserve_init` does: a flip whose
        // event cannot be delivered would leave the client waiting forever, and
        // failing before the hardware changes is the only way to report that.
        let events = if request.flags.event {
            let crtcs: Vec<ObjectId> = touched.crtcs.iter().copied().collect();
            if !file.reserve_events(crtcs.len()) {
                return Err(DrmError::OutOfMemory);
            }
            crtcs
        } else {
            Vec::new()
        };

        let timing = self.atomic_driver().flip_timing();
        self.vblank.retire_completed();
        let mut updates = Vec::with_capacity(self.crtcs().len());
        for crtc in self.crtcs() {
            let retires_scanout = base.retires_scanout(&new, crtc.id());
            if !self.vblank.can_update(crtc.id(), retires_scanout) {
                file.release_events(events.len());
                return Err(DrmError::Busy);
            }
            updates.push((
                crtc.id(),
                self.vblank.generation(crtc.id()),
                retires_scanout,
            ));
        }

        if let Err(error) = self.atomic_driver().atomic_commit(&new) {
            // The events promised above will never be queued, so the room they
            // reserved has to go back; otherwise a driver that fails commits would
            // slowly starve the file of event space.
            file.release_events(events.len());
            return Err(error);
        }

        let new = Arc::new(new);
        *self.state.lock() = new.clone();
        self.collect_mode_blobs();
        self.vblank.retire_completed();
        for (crtc, previous_generation, retires_scanout) in updates {
            let generation = self.vblank.generation(crtc);
            if retires_scanout && generation != previous_generation {
                self.vblank.retain_scanout(crtc, generation, base.clone());
            }
        }

        match timing {
            FlipTiming::Immediate => {
                let (tv_sec, tv_usec) = monotonic_timestamp();
                for crtc in events {
                    file.queue_event(DrmEvent::FlipComplete {
                        user_data: request.flags.user_data,
                        tv_sec,
                        tv_usec,
                        sequence: self.next_sequence(crtc),
                        crtc_id: crtc.get(),
                    });
                }
            }
            FlipTiming::AtVblank => {
                let sink = file.event_sink();
                for crtc in events {
                    if let Err(error) =
                        self.vblank
                            .park(crtc, sink.clone(), request.flags.user_data)
                    {
                        file.release_events(1);
                        return Err(error);
                    }
                }
            }
        }

        Ok(())
    }

    /// Applies one property assignment to a state being built.
    fn apply(
        &self,
        state: &mut AtomicState,
        assignment: &PropertyAssignment,
        touched: &mut CommitScope,
    ) -> DrmResult<()> {
        let property = self
            .properties()
            .lookup(assignment.property)
            .ok_or(DrmError::NotFound)?;
        let standard = property.standard;
        let value = assignment.value;

        // An immutable property is refused rather than ignored: a client that
        // thinks it changed a plane's type would go on to build on that belief.
        if standard.is_immutable() {
            return Err(DrmError::InvalidArgument);
        }
        check_property_range(standard, value)?;

        let object = assignment.object;
        if self.object_kind(object) != Some(standard.object_kind()) {
            return Err(DrmError::NotFound);
        }

        match standard {
            StandardProperty::CrtcActive => {
                state.crtc_mut(object)?.active = value != 0;
                touched.crtcs.insert(object);
            }
            StandardProperty::CrtcModeId => {
                let blob = self.mode_from_blob(value)?;
                let crtc = state.crtc_mut(object)?;
                crtc.mode = blob
                    .as_ref()
                    .map(|blob| <ModeInfo as ostd_pod::Pod>::from_bytes(&blob.data));
                crtc.mode_blob = blob.as_ref().map(|blob| blob.id);
                if let Some(blob) = blob {
                    state.mode_blobs.insert(object, blob);
                } else {
                    state.mode_blobs.remove(&object);
                }
                touched.crtcs.insert(object);
            }
            StandardProperty::PlaneFbId => {
                let framebuffer = match ObjectId::from_raw(value as u32) {
                    Some(id) => Some(self.framebuffer(id)?),
                    None => None,
                };
                state.plane_mut(object)?.framebuffer = framebuffer;
            }
            StandardProperty::PlaneCrtcId => {
                let crtc = match ObjectId::from_raw(value as u32) {
                    Some(id) => Some(self.crtc(id)?.id()),
                    None => None,
                };
                let plane = state.plane_mut(object)?;
                // Both the CRTC the plane leaves and the one it joins take part
                // in the commit, which is what makes them eligible for a
                // completion event.
                touched.crtcs.extend(plane.crtc);
                touched.crtcs.extend(crtc);
                plane.crtc = crtc;
            }
            StandardProperty::PlaneCrtcX => state.plane_mut(object)?.crtc_rect.0 = value as i32,
            StandardProperty::PlaneCrtcY => state.plane_mut(object)?.crtc_rect.1 = value as i32,
            StandardProperty::PlaneCrtcW => state.plane_mut(object)?.crtc_rect.2 = value as u32,
            StandardProperty::PlaneCrtcH => state.plane_mut(object)?.crtc_rect.3 = value as u32,
            StandardProperty::PlaneSrcX => state.plane_mut(object)?.src_rect.0 = value as u32,
            StandardProperty::PlaneSrcY => state.plane_mut(object)?.src_rect.1 = value as u32,
            StandardProperty::PlaneSrcW => state.plane_mut(object)?.src_rect.2 = value as u32,
            StandardProperty::PlaneSrcH => state.plane_mut(object)?.src_rect.3 = value as u32,
            StandardProperty::ConnectorCrtcId => {
                let crtc = match ObjectId::from_raw(value as u32) {
                    Some(id) => Some(self.crtc(id)?.id()),
                    None => None,
                };
                if let Some(previous) = state.crtc_of_connector(object) {
                    let previous_state = state.crtc_mut(previous)?;
                    previous_state.connectors.retain(|id| *id != object);
                    touched.crtcs.insert(previous);
                }
                if let Some(crtc) = crtc {
                    state.crtc_mut(crtc)?.connectors.push(object);
                    touched.crtcs.insert(crtc);
                }
            }
            StandardProperty::ConnectorDpms => {
                // Divergence from Linux only in shape, not in effect: it routes
                // `DPMS` through `drm_atomic_helper_connector_dpms`, which does
                // exactly this — turn the connector's CRTC on for `On` and off
                // for anything else. Standby and suspend are not distinguished
                // by any driver here.
                let Some(crtc) = state.crtc_of_connector(object) else {
                    // A connector with no CRTC has nothing to power; Linux
                    // records the value and returns success.
                    return Ok(());
                };
                state.crtc_mut(crtc)?.active = value == crate::uapi::mode::dpms::ON;
                // The CRTC joins the commit even though the client named a
                // connector, which is what makes the `allow_modeset` check above
                // see the change it caused.
                touched.crtcs.insert(crtc);
            }
            StandardProperty::PlaneType
            | StandardProperty::PlaneInFormats
            | StandardProperty::ConnectorEdid => unreachable!("immutable properties are refused"),
        }

        Ok(())
    }

    /// Parses a `MODE_ID` blob into a mode.
    fn mode_from_blob(&self, blob_id: u64) -> DrmResult<Option<Arc<PropertyBlob>>> {
        let Some(id) = ObjectId::from_raw(blob_id as u32) else {
            return Ok(None);
        };

        let blob = self.blob(id)?;
        if blob.data.len() != size_of::<ModeInfo>() {
            return Err(DrmError::InvalidArgument);
        }

        let mode = <ModeInfo as ostd_pod::Pod>::from_bytes(&blob.data);
        check_mode(&mode)?;
        Ok(Some(blob))
    }

    /// Validates everything about a state that follows from the objects it names.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic.c#L378>
    /// (`drm_atomic_crtc_check`) and
    /// <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_atomic.c#L594>
    /// (`drm_atomic_plane_check`).
    fn check(&self, base: &AtomicState, new: &AtomicState, touched: &CommitScope) -> DrmResult<()> {
        if !new.flags.allow_modeset && base.needs_modeset(new) {
            return Err(DrmError::InvalidArgument);
        }

        for crtc in &new.crtcs {
            // An active CRTC with no mode has nothing to output. Linux calls this
            // "active without enabled" and refuses it.
            if crtc.active && crtc.mode.is_none() {
                return Err(DrmError::InvalidArgument);
            }
            if crtc.mode.is_some() != crtc.mode_blob.is_some() {
                return Err(DrmError::InvalidArgument);
            }

            // An event on a CRTC that is off before and after would never be
            // delivered, so the request is refused instead of accepted and
            // forgotten.
            let was_active = base.crtc(crtc.crtc).is_ok_and(|old| old.active);
            if new.flags.event && touched.crtcs.contains(&crtc.crtc) && !crtc.active && !was_active
            {
                return Err(DrmError::InvalidArgument);
            }

            for connector in &crtc.connectors {
                self.check_routing(*connector, crtc.crtc)?;
            }
            // A configured CRTC drives connectors and an unconfigured one drives
            // none; anything else is a state nobody could have meant. Linux calls
            // this the "enabled/connectors mismatch" and refuses it in
            // `drm_atomic_helper_check_modeset`.
            if crtc.mode.is_some() == crtc.connectors.is_empty() {
                return Err(DrmError::InvalidArgument);
            }
        }

        for plane in &new.planes {
            self.check_plane(plane)?;
        }

        Ok(())
    }

    /// Checks that some encoder can carry a connector's signal from a CRTC.
    fn check_routing(&self, connector: ObjectId, crtc: ObjectId) -> DrmResult<()> {
        let connector = self.connector(connector)?;
        let crtc_bit = 1u32 << self.crtc_index(crtc)?;

        let reachable = connector.encoders().iter().any(|encoder| {
            self.encoder(*encoder)
                .is_ok_and(|encoder| encoder.possible_crtcs() & crtc_bit != 0)
        });
        if reachable {
            Ok(())
        } else {
            Err(DrmError::InvalidArgument)
        }
    }

    /// Checks one plane's requested state.
    fn check_plane(&self, state: &PlaneState) -> DrmResult<()> {
        let plane = self.plane(state.plane)?;

        // A plane with a CRTC but no framebuffer, or the other way round, is
        // neither on nor off. Linux refuses both.
        let (crtc, framebuffer) = match (state.crtc, state.framebuffer.as_ref()) {
            (None, None) => return Ok(()),
            (Some(crtc), Some(framebuffer)) => (crtc, framebuffer),
            _ => return Err(DrmError::InvalidArgument),
        };

        let crtc_bit = 1u32 << self.crtc_index(crtc)?;
        if plane.possible_crtcs() & crtc_bit == 0 {
            return Err(DrmError::InvalidArgument);
        }

        if !plane.formats().contains(&framebuffer.pixel_format) {
            return Err(DrmError::InvalidArgument);
        }
        if !plane.modifiers().contains(&framebuffer.modifier) {
            return Err(DrmError::InvalidArgument);
        }

        // The destination rectangle may hang off screen, but it may not wrap: a
        // driver computing `crtc_x + crtc_w` must not overflow.
        let (x, y, width, height) = state.crtc_rect;
        if width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err(DrmError::InvalidArgument);
        }
        if i64::from(x) + i64::from(width) > i64::from(i32::MAX)
            || i64::from(y) + i64::from(height) > i64::from(i32::MAX)
        {
            return Err(DrmError::InvalidArgument);
        }

        // The source rectangle is in 16.16 fixed point and must lie inside the
        // framebuffer. This is the check that stops a plane from scanning out
        // whatever memory follows the buffer.
        let (src_x, src_y, src_width, src_height) = state.src_rect;
        let right = u64::from(src_x) + u64::from(src_width);
        let bottom = u64::from(src_y) + u64::from(src_height);
        if right > u64::from(framebuffer.width) << 16
            || bottom > u64::from(framebuffer.height) << 16
        {
            return Err(DrmError::InvalidArgument);
        }

        // Without a scaler the two rectangles must describe the same size, and
        // the source must land on whole pixels.
        if !plane.can_scale()
            && (src_width != width << 16
                || src_height != height << 16
                || src_x & 0xffff != 0
                || src_y & 0xffff != 0)
        {
            return Err(DrmError::NotSupported);
        }

        Ok(())
    }
}

/// Which objects a commit touched, and therefore which CRTCs may receive events.
///
/// Linux derives this from which object states were pulled into the commit, and
/// the consequence is subtle enough to be worth stating: an atomic commit that
/// sets only a plane's `FB_ID`, without naming a CRTC, pulls no CRTC in and
/// therefore produces no completion event even with `PAGE_FLIP_EVENT` set. Real
/// clients always set `CRTC_ID` alongside, and so do the legacy translations in
/// [`super::legacy`].
#[derive(Debug, Default)]
struct CommitScope {
    crtcs: BTreeSet<ObjectId>,
}

/// Checks a value against the property's declared range.
///
/// Linux does this in `drm_property_change_valid_get`; without it a client could
/// put an out-of-range value into a state that the driver then trusts.
fn check_property_range(standard: StandardProperty, value: u64) -> DrmResult<()> {
    use super::PropertyKind;

    let valid = match standard.kind() {
        PropertyKind::Range { min, max } => (min..=max).contains(&value),
        PropertyKind::SignedRange { min, max } => (min..=max).contains(&(value as i64)),
        PropertyKind::Enum(values) => values.iter().any(|(candidate, _)| *candidate == value),
        // The referenced object is looked up by the caller, which is a stronger
        // check than a range would be.
        PropertyKind::Blob | PropertyKind::Object { .. } => value <= u64::from(u32::MAX),
    };

    if valid {
        Ok(())
    } else {
        Err(DrmError::InvalidArgument)
    }
}

/// Validates a mode that came from user space.
///
/// A mode is a set of counters the display engine is programmed with, so an
/// inconsistent one does not produce a wrong picture, it produces no picture and
/// possibly a hung engine. Linux applies the same ordering constraints in
/// `drm_mode_convert_umode`.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_modes.c#L2093>.
pub(super) fn check_mode(mode: &ModeInfo) -> DrmResult<()> {
    if mode.clock == 0 || mode.hdisplay == 0 || mode.vdisplay == 0 {
        return Err(DrmError::InvalidArgument);
    }
    if mode.hsync_start < mode.hdisplay
        || mode.hsync_end < mode.hsync_start
        || mode.htotal < mode.hsync_end
    {
        return Err(DrmError::InvalidArgument);
    }
    if mode.vsync_start < mode.vdisplay
        || mode.vsync_end < mode.vsync_start
        || mode.vtotal < mode.vsync_end
    {
        return Err(DrmError::InvalidArgument);
    }
    if mode.flags & !crate::uapi::mode::mode_flag::ALL != 0 {
        return Err(DrmError::InvalidArgument);
    }
    // The name is copied into fixed-size structures and reported back, so it has
    // to be a NUL-terminated string rather than arbitrary bytes.
    if !mode.name.contains(&0) {
        return Err(DrmError::InvalidArgument);
    }
    Ok(())
}

/// Returns the current monotonic time as the seconds and microseconds an event
/// carries.
///
/// `DRM_CAP_TIMESTAMP_MONOTONIC` promises this clock does not jump, which is why
/// it is the jiffy counter rather than wall time.
fn monotonic_timestamp() -> (u32, u32) {
    let now = Jiffies::elapsed().as_duration();
    (now.as_secs() as u32, now.subsec_micros())
}
