// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: board-local DRM/KMS implementation and regression coverage.

//! Per-open file state.
//!

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
};

use aster_softirq::BottomHalfDisabled;
use ostd::sync::SpinLock;
use ostd_pod::IntoBytes;
use spin::Once;

use crate::{
    device::{DriverFeatures, DrmDevice, DrmMinor},
    error::{DrmError, DrmResult},
    fence::SyncObjTable,
    gem::HandleTable,
    uapi::{DrmEventHeader, DrmEventVblank, cap, event_type},
};

/// The identity of one open file.
///
/// KMS framebuffers and property blobs belong to the client that created them:
/// `MODE_GETRESOURCES` reports only the caller's own framebuffers, `RMFB` removes
/// only those, and closing a file drops them. That needs a name for "the client",
/// and a [`DrmFile`] reference is not one — handlers receive `&DrmFile`, which
/// cannot be compared for identity once the object has moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(u64);

/// The next file identity to hand out.
///
/// Never reused, for the same reason KMS object IDs are not: a stale ID that
/// suddenly names a different client would hand one client's framebuffers to
/// another.
static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

/// How many bytes of undelivered events one file may accumulate.
///
/// Linux gives each file a 4 KiB budget in `drm_file.event_space` and refuses a
/// flip whose completion event would not fit. The budget is what stops a client
/// that never reads its file descriptor from growing kernel memory without bound,
/// and refusing the flip up front is the only way to report it: an event dropped
/// after the fact leaves the client waiting for a frame that already happened.
///
/// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_file.c#L177>.
const EVENT_SPACE: usize = 4096;

/// The kernel's current `CAP_SYS_ADMIN` verdict, or its verdict at open time
/// when selecting the file's initial authentication state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    /// The caller holds `CAP_SYS_ADMIN` in the initial user namespace.
    Privileged,
    /// The caller lacks that capability.
    Unprivileged,
}

/// Opaque caller identity retained without owning a kernel thread.
///
/// A weak allocation identity cannot be confused with a later thread that reuses
/// a numeric ID. It also avoids a thread/file-table reference cycle. The kernel
/// creates this token from the actual thread owner on every open and ioctl.
#[derive(Clone, Debug)]
pub struct CallerIdentity {
    owner: Weak<dyn Any + Send + Sync>,
}

impl CallerIdentity {
    /// Creates an identity from a kernel-owned reference, without retaining it.
    pub fn from_owner<T: Any + Send + Sync>(owner: &Arc<T>) -> Self {
        let erased: Arc<dyn Any + Send + Sync> = owner.clone();
        Self {
            owner: Arc::downgrade(&erased),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.owner, &other.owner)
    }
}

/// The state attached to one open file descriptor on a DRM node.
///
/// GEM handles are per-file, not per-device: two clients may hold the same
/// handle number referring to different objects. Anything that resolves a handle
/// must therefore go through the [`DrmFile`] that produced it.
#[derive(Debug)]
pub struct DrmFile {
    device: Weak<DrmDevice>,
    minor: DrmMinor,
    opener: Option<CallerIdentity>,
    /// This file's identity, which is what owns its KMS objects.
    id: FileId,
    /// This file's GEM handle table.
    handles: HandleTable,
    /// This file's sync-object table.
    ///
    /// Per-file for the same reason the handle table is: the handle namespace
    /// belongs to the open file, and sharing an object between processes goes
    /// through `SYNCOBJ_HANDLE_TO_FD` rather than through the number.
    syncobjs: SyncObjTable,
    /// Everything about this file that can change after it is opened.
    ///
    /// One lock rather than one per field, so that a reader never observes a
    /// half-applied mastership change. [`DrmDevice`] takes its own mastership lock
    /// before this one; nothing takes them the other way round.
    state: SpinLock<FileState>,
    /// Events waiting to be read, shared with [`EventSink`] so a vblank
    /// interrupt can complete a flip after this borrow of the file has ended.
    events: Arc<FileEvents>,
    /// Whatever the driver wants to hang on this open file.
    ///
    /// Upstream's `drm_file::driver_priv`. Set once, on the first ioctl that
    /// needs it, and dropped with the file — which is what makes it safe in a
    /// way the alternative is not: a driver-side table keyed by the file's
    /// address depends on the destructor running before the memory is reused,
    /// and that is an implementation detail rather than a promise this
    /// interface makes.
    driver_priv: Once<Arc<dyn Any + Send + Sync>>,
}

/// The event queue of one file, held behind an `Arc` so [`EventSink`] can
/// outlive the ioctl that parked a flip.
#[derive(Debug)]
struct FileEvents {
    queue: SpinLock<EventQueue, BottomHalfDisabled>,
    notifier: Once<Arc<dyn EventNotifier>>,
}

impl FileEvents {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: SpinLock::new(EventQueue::default()),
            notifier: Once::new(),
        })
    }

    fn queue_event(&self, event: DrmEvent) {
        let bytes = event.to_uapi().as_bytes().to_vec();

        {
            let mut queue = self.queue.lock();
            queue.reserved = queue.reserved.saturating_sub(bytes.len());

            if queue.used + bytes.len() > EVENT_SPACE {
                ostd::warn!("dropping a DRM event: the client is not reading its file descriptor");
                return;
            }
            queue.used += bytes.len();
            queue.events.push_back(bytes);
        }

        if let Some(notifier) = self.notifier.get() {
            notifier.notify_readable();
        }
    }
}

/// A handle that can queue a DRM event after the ioctl that started the flip
/// has returned.
///
/// [`ModeConfig::commit`](crate::kms::ModeConfig::commit) receives `&DrmFile`.
/// A driver that completes flips at vblank needs to deliver the event later,
/// from a bottom half, so the file itself cannot be borrowed. This is a `Weak`
/// to the same queue: if the client closed the file, the event is dropped
/// rather than keeping the file alive.
#[derive(Clone, Debug)]
pub struct EventSink {
    events: Weak<FileEvents>,
    file_id: FileId,
}

impl EventSink {
    /// Queues `event` if the file is still open.
    ///
    /// Process or bottom-half context only; never call from a hard IRQ.
    pub fn queue_event(&self, event: DrmEvent) {
        if let Some(events) = self.events.upgrade() {
            events.queue_event(event);
        }
    }

    /// Returns the file this sink belongs to.
    pub fn file_id(&self) -> FileId {
        self.file_id
    }

    /// Returns whether the file has been closed.
    pub fn is_dead(&self) -> bool {
        self.events.upgrade().is_none()
    }
}

/// The events one file has queued but not yet delivered.
#[derive(Debug, Default)]
struct EventQueue {
    /// Complete events, in the order they were produced, already serialized.
    ///
    /// Serializing on the way in rather than on the way out keeps `read` from
    /// having to know about event kinds, and makes the queue's size in bytes —
    /// which is what [`EVENT_SPACE`] bounds — exact rather than estimated.
    events: VecDeque<Vec<u8>>,
    /// Bytes occupied by [`events`](Self::events).
    used: usize,
    /// Bytes promised to commits that have not completed yet.
    reserved: usize,
}

/// How a driver's event reaches whatever is waiting in `read` or `poll`.
///
/// Implemented by the kernel crate, which owns the file's `Pollee`; this crate
/// cannot see it. A file with no notifier still queues events correctly, which is
/// what makes the queue testable without a process.
pub trait EventNotifier: Send + Sync + core::fmt::Debug {
    /// Called after an event has been queued, outside every lock this crate holds.
    fn notify_readable(&self);
}

#[derive(Debug)]
struct FileState {
    /// Client capabilities enabled through `DRM_IOCTL_SET_CLIENT_CAP`.
    client_caps: ClientCaps,
    /// Whether this file has been authenticated.
    authenticated: bool,
    /// Whether this file has asked for the bus ID to be set.
    ///
    /// `GET_UNIQUE` answers with an empty string until it has. That is
    /// upstream's behaviour and it is load-bearing rather than incidental:
    /// `drm_master::unique` starts `NULL` and is only filled in by
    /// `drm_set_busid`, which runs from `DRM_IOCTL_SET_VERSION` when the client
    /// asks for interface version 1.1 or later. `libdrm`'s `drmOpenByName` --
    /// the path behind `modetest -M <driver>` -- opens a node, reads `VERSION`,
    /// then reads `GET_UNIQUE`, and **skips any device whose bus ID is not
    /// empty**, on the reasoning that a bus ID means some other client already
    /// claimed it. Answering unconditionally makes the driver invisible to
    /// every tool that opens by name.
    busid_set: bool,
    /// Whether this file currently holds mastership of the device.
    is_master: bool,
    /// Whether this file has ever held mastership, which is what lets a
    /// compositor drop and reclaim it around a VT switch without being root.
    was_master: bool,
    /// Whether [`DrmDriver::open`] accepted this file, and therefore whether
    /// `postclose` is owed to the driver.
    ///
    /// [`DrmDriver::open`]: crate::device::DrmDriver::open
    driver_opened: bool,
}

/// The client capabilities a file has opted into.
///
/// These change what the kernel *reports*, not just what it accepts: with
/// `universal_planes` set, `MODE_GETPLANERESOURCES` includes primary and cursor
/// planes, and with `atomic` set, mode objects expose their atomic properties.
/// Handlers must consult this rather than assuming a fixed view.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientCaps {
    /// `DRM_CLIENT_CAP_STEREO_3D`.
    pub stereo_3d: bool,
    /// `DRM_CLIENT_CAP_UNIVERSAL_PLANES`.
    pub universal_planes: bool,
    /// `DRM_CLIENT_CAP_ATOMIC`.
    ///
    /// Setting this implies `universal_planes` and `aspect_ratio` in Linux; keep
    /// that behaviour.
    pub atomic: bool,
    /// `DRM_CLIENT_CAP_ASPECT_RATIO`.
    pub aspect_ratio: bool,
    /// `DRM_CLIENT_CAP_WRITEBACK_CONNECTORS`.
    pub writeback_connectors: bool,
    /// `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`.
    ///
    /// linux-6.6 has no such capability, so `SET_CLIENT_CAP` rejects it; the field
    /// is here for whoever tracks a later kernel.
    pub cursor_plane_hotspot: bool,
}

impl DrmFile {
    /// Creates the per-open state for a file on `device`.
    ///
    /// A privileged client starts out authenticated, matching Linux's
    /// `file->authenticated = capable(CAP_SYS_ADMIN)`.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_file.c#L167>
    /// (`drm_file_alloc`).
    pub(crate) fn new(
        device: Weak<DrmDevice>,
        minor: DrmMinor,
        privilege: Privilege,
        opener: Option<CallerIdentity>,
    ) -> Arc<Self> {
        crate::live::note_file_alloc();
        Arc::new(Self {
            device,
            minor,
            opener,
            id: FileId(NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed)),
            handles: HandleTable::new(),
            syncobjs: SyncObjTable::new(),
            state: SpinLock::new(FileState {
                client_caps: ClientCaps::default(),
                authenticated: privilege == Privilege::Privileged,
                busid_set: false,
                is_master: false,
                was_master: false,
                driver_opened: false,
            }),
            events: FileEvents::new(),
            driver_priv: Once::new(),
        })
    }

    /// Whether `GET_UNIQUE` should answer with the device's bus ID.
    ///
    /// False until `SET_VERSION` asks for it; see [`FileState::busid_set`].
    pub fn busid_set(&self) -> bool {
        self.state.lock().busid_set
    }

    /// Records that the client asked for the bus ID to be set.
    ///
    /// Upstream's `drm_set_busid`. Idempotent, as that is too.
    pub fn set_busid(&self) {
        self.state.lock().busid_set = true;
    }

    /// Returns this file's identity.
    pub fn id(&self) -> FileId {
        self.id
    }

    /// Returns the device this file was opened on, if it is still alive.
    pub fn device(&self) -> Option<Arc<DrmDevice>> {
        self.device.upgrade()
    }

    /// Returns which node this file was opened through.
    pub fn minor(&self) -> DrmMinor {
        self.minor
    }

    /// Returns this file's GEM handle table.
    pub fn handles(&self) -> &HandleTable {
        &self.handles
    }

    /// Returns this file's sync-object table.
    pub fn syncobjs(&self) -> &SyncObjTable {
        &self.syncobjs
    }

    /// Returns the client capabilities this file has enabled.
    pub fn client_caps(&self) -> ClientCaps {
        self.state.lock().client_caps
    }

    /// Whether a current caller is the thread that opened this file.
    pub(crate) fn opened_by(&self, caller: &CallerIdentity) -> bool {
        self.opener
            .as_ref()
            .is_some_and(|opener| opener.matches(caller))
    }

    /// Returns whether this file is authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.state.lock().authenticated
    }

    /// Returns whether this file holds mastership.
    pub fn is_master(&self) -> bool {
        self.state.lock().is_master
    }

    /// Returns whether this file has ever held mastership.
    pub fn was_master(&self) -> bool {
        self.state.lock().was_master
    }

    /// Records that this file has become the device's master.
    ///
    /// Only [`DrmDevice`] may call this, and only while holding its mastership
    /// lock, so that the device-wide bit and this one cannot disagree.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_auth.c#L165>
    /// (`drm_new_set_master`).
    pub(crate) fn become_master(&self) {
        let mut state = self.state.lock();
        state.is_master = true;
        state.was_master = true;
        // Becoming master implies authentication: the client that programs the
        // display is by definition trusted with it.
        state.authenticated = true;
    }

    /// Records that this file is no longer the device's master.
    pub(crate) fn resign_master(&self) {
        self.state.lock().is_master = false;
    }

    /// Records that the driver accepted this file, so its destructor owes the
    /// driver a `postclose`.
    pub(crate) fn mark_driver_opened(&self) {
        self.state.lock().driver_opened = true;
    }

    /// Applies a `DRM_IOCTL_SET_CLIENT_CAP` request.
    ///
    /// Rejecting `UNIVERSAL_PLANES` or `ATOMIC` makes `cosmic-comp` abort its
    /// backend setup, so both must succeed.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_ioctl.c#L306>
    /// (`drm_setclientcap`).
    pub fn set_client_cap(&self, capability: u64, value: u64) -> DrmResult<()> {
        let device = self.device().ok_or(DrmError::NoDevice)?;
        let features = device.driver().features();

        // Every client capability changes how mode-setting objects are reported,
        // so a render-only driver has none of them to offer.
        if !features.contains(DriverFeatures::MODESET) {
            return Err(DrmError::NotSupported);
        }

        let mut state = self.state.lock();
        let caps = &mut state.client_caps;

        match capability {
            cap::client::STEREO_3D => caps.stereo_3d = as_flag(value)?,
            cap::client::UNIVERSAL_PLANES => caps.universal_planes = as_flag(value)?,
            cap::client::ATOMIC => {
                if !features.contains(DriverFeatures::ATOMIC) {
                    return Err(DrmError::NotSupported);
                }
                // Divergence: Linux accepts 2 here but stores the value into a
                // one-bit field, so 2 silently turns the capability *off*. Any
                // non-zero value enables it here instead.
                if value > 2 {
                    return Err(DrmError::InvalidArgument);
                }
                let enabled = value != 0;
                caps.atomic = enabled;
                // An atomic client is assumed to cope with universal planes and
                // the aspect-ratio mode flags, because none of the atomic user
                // space in existence does not.
                caps.universal_planes = enabled;
                caps.aspect_ratio = enabled;
            }
            cap::client::ASPECT_RATIO => caps.aspect_ratio = as_flag(value)?,
            cap::client::WRITEBACK_CONNECTORS => {
                if !caps.atomic {
                    return Err(DrmError::InvalidArgument);
                }
                caps.writeback_connectors = as_flag(value)?;
            }
            _ => return Err(DrmError::InvalidArgument),
        }

        Ok(())
    }

    /// Installs the hook that wakes readers when an event is queued.
    ///
    /// Called once, by the character-device shell, right after the file is
    /// opened. A second call is ignored rather than refused: there is only one
    /// caller, and a file whose notifier changed halfway through its life would be
    /// a bug nobody could observe.
    pub fn set_event_notifier(&self, notifier: Arc<dyn EventNotifier>) {
        self.events.notifier.call_once(|| notifier);
    }

    /// Returns a handle that can queue events after this borrow ends.
    ///
    /// Used by the vblank path: the completion event is delivered from an
    /// bottom half, long after `ModeConfig::commit` has returned.
    pub fn event_sink(&self) -> EventSink {
        EventSink {
            events: Arc::downgrade(&self.events),
            file_id: self.id,
        }
    }

    /// Installs this file's driver-private state.
    ///
    /// Called from [`DrmDriver::open`], which is where upstream allocates
    /// `drm_file::driver_priv`. A second call is ignored, matching
    /// [`Self::set_event_notifier`]: `open` runs once per file.
    ///
    /// [`DrmDriver::open`]: crate::device::DrmDriver::open
    pub fn set_driver_priv(&self, state: Arc<dyn Any + Send + Sync>) {
        self.driver_priv.call_once(|| state);
    }

    /// Returns this file's driver-private state.
    ///
    /// `None` before [`Self::set_driver_priv`] runs, and `None` if `T` is not
    /// what was installed. The second case means two drivers are sharing one
    /// file, which cannot happen — a file belongs to one device — but it is
    /// reported rather than panicked because every caller is a handler with an
    /// errno to return.
    pub fn driver_priv<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.driver_priv.get()?.clone().downcast::<T>().ok()
    }

    /// Queues a DRM event for delivery through `read()`.
    ///
    /// Process or bottom-half context only; never call from a hard IRQ.
    ///
    /// Used for page-flip and vblank completions. `cosmic-comp` drives its frame
    /// loop off these, so ordering matters: events are delivered in the order they
    /// were queued, and never merged — a client that asked for two flips is owed
    /// two events, because it counts them.
    ///
    /// The room for the event must have been promised by
    /// [`reserve_events`](Self::reserve_events) before the flip was started. An
    /// event that arrives without a reservation and does not fit is dropped, with a
    /// warning: there is nothing better to do at completion time, which is exactly
    /// why the reservation exists.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1063>
    /// and <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_file.c#L761>
    /// (`drm_send_event_helper`).
    pub fn queue_event(&self, event: DrmEvent) {
        self.events.queue_event(event);
    }

    /// Promises room for `count` completion events.
    ///
    /// Returns whether the promise could be made. A commit that cannot promise its
    /// events must fail before it programs anything.
    pub fn reserve_events(&self, count: usize) -> bool {
        let Some(needed) = count
            .checked_mul(size_of::<DrmEventVblank>())
            .filter(|needed| *needed <= EVENT_SPACE)
        else {
            return false;
        };
        let mut queue = self.events.queue.lock();

        if queue.used + queue.reserved + needed > EVENT_SPACE {
            return false;
        }
        queue.reserved += needed;
        true
    }

    /// Gives back a promise that will not be used, because the commit failed.
    pub fn release_events(&self, count: usize) {
        let released = count.saturating_mul(size_of::<DrmEventVblank>());
        let mut queue = self.events.queue.lock();
        queue.reserved = queue.reserved.saturating_sub(released);
    }

    /// Returns whether any event is waiting to be read.
    ///
    /// This is what the character-device shell's `poll` reports as `POLLIN`.
    pub fn has_events(&self) -> bool {
        !self.events.queue.lock().events.is_empty()
    }

    /// Delivers at most one event, if a whole one fits in `capacity` bytes.
    ///
    /// `copy` is handed the serialized event and moves it into the caller's
    /// buffer. The three outcomes are Linux's, and user space depends on all
    /// three:
    ///
    /// - no event, or the first one is larger than `capacity`: `Ok(0)`, and the
    ///   event stays queued. A partial event would desynchronize the reader
    ///   forever, since events are self-delimiting only if read whole.
    /// - `copy` succeeds: `Ok(length)`, and the event is gone.
    /// - `copy` fails: the error, and the event stays queued. Losing an event
    ///   because the caller passed a bad pointer would cost it a frame.
    ///
    /// The queue lock is not held while `copy` runs, because copying to user space
    /// can fault.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_file.c#L537>
    /// (`drm_read`).
    pub fn read_event<E>(
        &self,
        capacity: usize,
        copy: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<usize, E> {
        let Some(bytes) = self.take_event(capacity) else {
            return Ok(0);
        };

        match copy(&bytes) {
            Ok(()) => {
                self.events.queue.lock().used -= bytes.len();
                Ok(bytes.len())
            }
            Err(error) => {
                self.return_event(bytes);
                Err(error)
            }
        }
    }

    /// Removes the first event, if it fits.
    fn take_event(&self, capacity: usize) -> Option<Vec<u8>> {
        let mut queue = self.events.queue.lock();

        if queue.events.front()?.len() > capacity {
            return None;
        }
        let bytes = queue.events.pop_front()?;
        // Retain its budget while user copying may fault. A concurrent flip
        // must not consume the space needed to return this event on EFAULT.
        Some(bytes)
    }

    /// Puts an event back at the head of the queue.
    fn return_event(&self, bytes: Vec<u8>) {
        let mut queue = self.events.queue.lock();
        queue.events.push_front(bytes);
    }
}

impl Drop for DrmFile {
    /// Releases what the rest of the kernel is holding on this file's behalf.
    ///
    /// Mastership must be released here rather than by the character-device shell:
    /// a file that closed while holding it would otherwise leave the device
    /// permanently unclaimable.
    ///
    /// Reference: <https://elixir.bootlin.com/linux/v6.6/source/drivers/gpu/drm/drm_file.c#L245>
    /// (`drm_file_free`).
    fn drop(&mut self) {
        crate::live::note_file_free();
        let Some(device) = self.device.upgrade() else {
            return;
        };

        device.release_master(self);

        // Framebuffers and property blobs die with the client that created them.
        // Otherwise a compositor that crashed would keep its scanout buffers alive
        // for the rest of the boot, and the next one would inherit a mode
        // configuration full of objects it cannot account for.
        if let Some(config) = device.driver().mode_config() {
            config.release_file(self.id);
        }

        if self.state.lock().driver_opened {
            device.driver().postclose(self);
        }
    }
}

/// Interprets a `SET_CLIENT_CAP` value that Linux restricts to zero or one.
fn as_flag(value: u64) -> DrmResult<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(DrmError::InvalidArgument),
    }
}

/// An event delivered to user space by reading a DRM node.
#[derive(Clone, Copy, Debug)]
pub enum DrmEvent {
    /// `DRM_EVENT_VBLANK`.
    Vblank {
        /// The cookie user space supplied with the request.
        user_data: u64,
        /// Timestamp seconds.
        tv_sec: u32,
        /// Timestamp microseconds.
        tv_usec: u32,
        /// Vblank sequence number.
        sequence: u32,
        /// Index of the CRTC that produced the event.
        crtc_id: u32,
    },
    /// `DRM_EVENT_FLIP_COMPLETE`.
    FlipComplete {
        /// The cookie user space supplied with the flip request.
        user_data: u64,
        /// Timestamp seconds.
        tv_sec: u32,
        /// Timestamp microseconds.
        tv_usec: u32,
        /// Vblank sequence number at which the flip landed.
        sequence: u32,
        /// Object ID of the CRTC that completed the flip.
        crtc_id: u32,
    },
}

impl DrmEvent {
    /// Renders this event into the structure user space reads.
    ///
    /// Both kinds share `drm_event_vblank`, differing only in the header's type
    /// code, which is why the reservation arithmetic in [`DrmFile`] can assume one
    /// size.
    pub fn to_uapi(self) -> DrmEventVblank {
        let (type_, user_data, tv_sec, tv_usec, sequence, crtc_id) = match self {
            Self::Vblank {
                user_data,
                tv_sec,
                tv_usec,
                sequence,
                crtc_id,
            } => (
                event_type::VBLANK,
                user_data,
                tv_sec,
                tv_usec,
                sequence,
                crtc_id,
            ),
            Self::FlipComplete {
                user_data,
                tv_sec,
                tv_usec,
                sequence,
                crtc_id,
            } => (
                event_type::FLIP_COMPLETE,
                user_data,
                tv_sec,
                tv_usec,
                sequence,
                crtc_id,
            ),
        };

        DrmEventVblank {
            base: DrmEventHeader {
                type_,
                length: size_of::<DrmEventVblank>() as u32,
            },
            user_data,
            tv_sec,
            tv_usec,
            sequence,
            crtc_id,
        }
    }
}

#[cfg(ktest)]
mod regression {
    use ostd::prelude::ktest;

    use super::*;
    use crate::drivers::soft::SoftDriver;

    #[ktest]
    fn oversized_event_reservations_do_not_consume_budget() {
        let device = DrmDevice::new_unregistered(SoftDriver::new(), Some(0), Some(128));
        let file = device
            .open(DrmMinor::Primary, Privilege::Unprivileged)
            .unwrap();
        assert!(!file.reserve_events(usize::MAX));
        assert!(file.reserve_events(EVENT_SPACE / size_of::<DrmEventVblank>()));
        assert!(!file.reserve_events(1));
        file.release_events(usize::MAX);
        assert!(file.reserve_events(1));
    }

    #[ktest]
    fn faulting_event_copy_keeps_its_budget_and_original_event() {
        let device = DrmDevice::new_unregistered(SoftDriver::new(), Some(0), Some(128));
        let file = device
            .open(DrmMinor::Primary, Privilege::Unprivileged)
            .unwrap();
        let event = DrmEvent::FlipComplete {
            user_data: 0xabc,
            tv_sec: 0,
            tv_usec: 0,
            sequence: 1,
            crtc_id: 1,
        };
        let size = size_of::<DrmEventVblank>();
        assert!(file.reserve_events(1));
        file.queue_event(event);
        assert_eq!(
            file.read_event(size, |_| {
                // All remaining budget is consumed while this event is in flight.
                assert!(file.reserve_events(EVENT_SPACE / size - 1));
                assert!(!file.reserve_events(1));
                Err(DrmError::BadAddress)
            }),
            Err(DrmError::BadAddress)
        );
        assert!(file.has_events());
        assert!(!file.reserve_events(1));
        file.read_event::<()>(size, |bytes| {
            assert_eq!(bytes, event.to_uapi().as_bytes());
            Ok(())
        })
        .unwrap();
        assert!(file.reserve_events(1));
        assert!(!file.reserve_events(1));
    }
}
