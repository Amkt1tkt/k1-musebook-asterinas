// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: DPU KMS wrapper (CRTC/plane/connector, page-flip, dumb, PRIME).
// Hardware cursor plane: RDMA2 + composer nl6, gated by `k1.cursorplane=1`.

//! KMS for the SpacemiT K1 display pipeline.
//!
//! This is a wrapper, not a driver: [`k1_edp`] brings the DPU, the MIPI-DSI
//! link, the LT8911EXB bridge and the panel backlight up. What is added here
//! is a `k1-musebook-drm` device in front of the already-programmed pipeline.
//!
//! # Why this lives in `aster-framebuffer`
//!
//! The DPU can have exactly one owner. `aster-framebuffer` already claimed it —
//! it allocates the scanout buffer, programs the timings, and draws the
//! console into it — so KMS has to be registered by that owner rather than by
//! a second driver that would race it for the same registers.
//!
//! # What a commit actually does
//!
//! Only the scanout address and its stride. The mode is fixed: the panel
//! accepts one timing, which the bridge was trained for during boot, so a
//! commit asking for anything else is refused rather than attempted.
//!
//! [`k1_edp`]: super::k1_edp

use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aster_softirq::Taskless;
use k1_musebook_drm::{
    DriverFeatures, DrmDriver, PlatformIdentity,
    device::{DriverInfo, DumbBuffer, DumbCreateArgs},
    error::{DrmError, DrmResult},
    gem::{BufferUse, GemObject, ShmemObject},
    kms::{
        AtomicDriver, AtomicState, ConnectionStatus, Connector, Crtc, Encoder, FlipTiming,
        Framebuffer, HardwareObservation, InitialCrtc, ModeConfig, ModeConfigDesc, ObjectId,
        ObjectIds, Plane, PlaneType, Vblank, mode_info,
    },
    prime::DmaBuf,
    uapi::{cap, fourcc, mode},
};
use ostd::{
    arch::irq::{IRQ_CHIP, MappedIrqLine},
    io::IoMem,
    irq::IrqLine,
    mm::{HasPaddr, Paddr, Segment, UFrame, VmIoOnce},
    sync::{LocalIrqDisabled, SpinLock},
    timer::Jiffies,
};
use spin::Once;

use super::{k1_cursor, k1_edp, k1_vblank};
use crate::framebuffer::FrameBuffer;

/// The device tree path of the DPU, reported as `OF_FULLNAME`.
///
/// `libdrm` merges platform devices that share an `OF_FULLNAME`, and Mesa
/// would then pick a user-space driver by the wrong name. This string keeps
/// the DPU distinct from the GPU (`/soc/imggpu@cac00000`).
const OF_FULLNAME: &str = "/soc/dpu@c0340000";

/// The directory name under `/sys/devices/platform`.
const DEVICE_NAME: &str = "dpu@c0340000";

/// The panel's physical size, from the vendor device tree
/// (`lcd_lt8911_edp_1920x1080.dtsi`: `width-mm = <309>`, `height-mm = <174>`).
const PANEL_SIZE_MM: (u32, u32) = (309, 174);

/// The formats the DPU layer is programmed for.
///
/// The boot sequence configures the layer as 32 bits per pixel with the blue
/// channel in the low byte (`PixelFormat::BgrReserved`, and `OUTFMT_RGB888` on
/// the output side), which is what `DRM_FORMAT_XRGB8888` names.
const FORMATS: &[u32] = &[fourcc::XRGB8888, fourcc::ARGB8888];

/// Linear only. An empty list tells Mesa modifiers are unsupported, which is
/// a different statement.
const MODIFIERS: &[u64] = &[fourcc::MOD_LINEAR];

/// The stride alignment the DPU layer requires.
const PITCH_ALIGN: u32 = 64;

// DPU registers, at the same offsets `k1_edp::dsi_dpu_init` programs them.
/// Layer 0 scanout address, low 32 bits.
const DPU_L0_ADDR_LO: usize = 0xda0;
/// Layer 0 scanout address, high 32 bits.
const DPU_L0_ADDR_HI: usize = 0xda4;
/// Layer 0 stride in bytes.
const DPU_L0_STRIDE: usize = 0xdb8;
/// `ctl2_nml_cfg_rdy` (`dpu_ctl_reg_27` at `0x500 + 27 * 4`).
const DPU_CTRL_LOAD: usize = 0x56c;
const _: () = assert!(DPU_CTRL_LOAD == k1_vblank::DPU_CTL2_CFG_RDY);

/// Registers the K1 display pipeline with `k1-musebook-drm`.
///
/// Called at the end of [`k1_edp::init`], once the pipeline is up and the
/// scanout buffer is registered as the global framebuffer. `dpu` is the
/// register window that function used, handed over because the flip path
/// needs it and two `IoMem` windows cannot overlap.
pub(super) fn register(dpu: IoMem, scanout: Arc<FrameBuffer>, pixel_clock_hz: u32) {
    let driver = match K1Driver::new(dpu, &scanout, pixel_clock_hz) {
        Ok(driver) => driver,
        Err(error) => {
            ostd::warn!("not registering a DRM device: {:?}", error);
            return;
        }
    };

    match k1_musebook_drm::register_device(driver) {
        Ok(device) => ostd::info!(
            "registered {:?} for the eDP panel ({}x{})",
            device.node_name(k1_musebook_drm::DrmMinor::Primary),
            scanout.width(),
            scanout.height()
        ),
        Err(error) => ostd::warn!("failed to register a DRM device: {:?}", error),
    }
}

/// The DRM driver in front of the K1 display pipeline.
#[derive(Debug)]
struct K1Driver {
    mode_config: ModeConfig,
}

impl K1Driver {
    fn new(dpu: IoMem, scanout: &Arc<FrameBuffer>, pixel_clock_hz: u32) -> DrmResult<Arc<Self>> {
        let mode = panel_mode(
            scanout.width() as u16,
            scanout.height() as u16,
            pixel_clock_hz,
        );
        let ids = ObjectIds::new();

        let primary = Arc::new(K1Plane {
            id: ids.allocate(),
            plane_type: PlaneType::Primary,
        });
        // Probe before allocating the cursor ID so a failed probe leaves
        // the object-ID sequence identical to a Primary-only device.
        let cursor = if k1_cursor::requested() && k1_cursor::probe(&dpu) {
            Some(Arc::new(K1Plane {
                id: ids.allocate(),
                plane_type: PlaneType::Cursor,
            }))
        } else {
            None
        };
        let crtc = Arc::new(K1Crtc {
            id: ids.allocate(),
            primary: primary.clone(),
            cursor: cursor.clone(),
        });
        let encoder = Arc::new(K1Encoder { id: ids.allocate() });
        let connector = Arc::new(K1Connector {
            id: ids.allocate(),
            encoder: encoder.id,
            mode,
        });

        let crtc_id = crtc.id;
        let primary_id = primary.id;
        let cursor_id = cursor.as_ref().map(|plane| plane.id);
        let mut planes: Vec<Arc<dyn Plane>> = vec![primary];
        if let Some(cursor) = cursor {
            planes.push(cursor);
        }
        let atomic = Arc::new(K1Dpu {
            dpu,
            mode,
            programmed: SpinLock::new(Programmed {
                primary: Some((scanout.paddr(), scanout.line_size() as u32)),
                cursor: k1_cursor::CursorProgram::Off,
                generation: 0,
                latch_seen: false,
                latched_generation: None,
            }),
            primary: primary_id,
            cursor: cursor_id,
            crtc: crtc_id,
            vblank: Once::new(),
            irq_live: AtomicBool::new(false),
            pending_bits: SpinLock::new(PendingIrq::default()),
            irq: SpinLock::new(None),
            irq_entries: AtomicU32::new(0),
            irq_stormed: AtomicBool::new(false),
            storm_reported: AtomicBool::new(false),
            bottom_half: Once::new(),
            last_vsync_at: AtomicU64::new(0),
            vsync_stalled: AtomicBool::new(false),
            stall_reported: AtomicBool::new(false),
        });

        let connector_id = connector.id;
        let mode_config = ModeConfig::new(ModeConfigDesc {
            ids,
            min_size: (1, 1),
            max_size: (u32::from(mode.hdisplay), u32::from(mode.vdisplay)),
            crtcs: vec![crtc],
            planes,
            encoders: vec![encoder],
            connectors: vec![connector],
            atomic: atomic.clone(),
            // Boot programmed the DPU, the D-PHY, the bridge and the backlight
            // before this device existed. The committed state has to describe
            // that pipeline, or a client that only queries is told the CRTC is
            // off while the panel is lit.
            //
            // The primary plane is not bound: the console buffer is not a GEM
            // object and there is no owner for a framebuffer nobody created
            // via ADDFB2. GETCRTC therefore reports `fb_id = 0` until a
            // client adds one.
            initial: vec![InitialCrtc {
                crtc: crtc_id,
                mode,
                connectors: vec![connector_id],
            }],
        })?;

        atomic.bind_vblank(mode_config.vblank());
        atomic.install_irq();

        Ok(Arc::new(Self { mode_config }))
    }
}

/// Builds the panel's only mode.
///
/// The timings come from [`k1_edp`]; the pixel clock is what the clock tree
/// actually produced at boot rather than what was asked for.
fn panel_mode(width: u16, height: u16, pixel_clock_hz: u32) -> mode::ModeInfo {
    let timing = k1_edp::PANEL_TIMING;

    let hsync_start = width + timing.hfp;
    let hsync_end = hsync_start + timing.hsync;
    let htotal = hsync_end + timing.hbp;
    let vsync_start = height + timing.vfp;
    let vsync_end = vsync_start + timing.vsync;
    let vtotal = vsync_end + timing.vbp;

    mode_info(
        "1920x1080",
        pixel_clock_hz / 1000,
        (width, hsync_start, hsync_end, htotal),
        (height, vsync_start, vsync_end, vtotal),
        mode::mode_flag::PHSYNC | mode::mode_flag::PVSYNC,
        mode::mode_type::DRIVER | mode::mode_type::PREFERRED,
    )
}

/// Only the fixed timing programmed into the DPU/DSI/bridge is supported.
fn same_panel_timing(requested: &mode::ModeInfo, fixed: &mode::ModeInfo) -> bool {
    requested.clock == fixed.clock
        && requested.hdisplay == fixed.hdisplay
        && requested.hsync_start == fixed.hsync_start
        && requested.hsync_end == fixed.hsync_end
        && requested.htotal == fixed.htotal
        && requested.hskew == fixed.hskew
        && requested.vdisplay == fixed.vdisplay
        && requested.vsync_start == fixed.vsync_start
        && requested.vsync_end == fixed.vsync_end
        && requested.vtotal == fixed.vtotal
        && requested.vscan == fixed.vscan
        && requested.flags == fixed.flags
}

impl DrmDriver for K1Driver {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn info(&self) -> DriverInfo {
        DriverInfo {
            // Load-bearing: Mesa's loader matches on this name.
            name: "spacemit",
            date: "20260729",
            desc: "SpacemiT K1 display processing unit",
            major: 1,
            minor: 0,
            patchlevel: 0,
        }
    }

    fn features(&self) -> DriverFeatures {
        DriverFeatures::MODESET | DriverFeatures::GEM | DriverFeatures::ATOMIC
    }

    fn unique(&self) -> String {
        "platform:dpu@c0340000".to_string()
    }

    fn get_cap(&self, capability: u64) -> Option<u64> {
        match capability {
            cap::device::DUMB_BUFFER => Some(1),
            cap::device::DUMB_PREFERRED_DEPTH => Some(24),
            cap::device::DUMB_PREFER_SHADOW => Some(0),
            cap::device::ADDFB2_MODIFIERS => Some(1),
            _ => None,
        }
    }

    fn mode_config(&self) -> Option<&ModeConfig> {
        Some(&self.mode_config)
    }

    fn platform_identity(&self) -> Option<PlatformIdentity> {
        Some(PlatformIdentity {
            name: DEVICE_NAME.to_string(),
            of_fullname: OF_FULLNAME.to_string(),
            of_compatible: vec!["spacemit,dpu".to_string()],
        })
    }

    fn dumb_create(&self, args: DumbCreateArgs) -> DrmResult<DumbBuffer> {
        let bytes_per_pixel = args.bpp.div_ceil(8);
        let pitch = args
            .width
            .checked_mul(bytes_per_pixel)
            .and_then(|pitch| pitch.checked_next_multiple_of(PITCH_ALIGN))
            .ok_or(DrmError::InvalidArgument)?;
        let size = u64::from(pitch)
            .checked_mul(u64::from(args.height))
            .and_then(|size| usize::try_from(size).ok())
            .ok_or(DrmError::InvalidArgument)?;

        let object = ShmemObject::new(size, BufferUse::Scanout)?;
        if let Some(fill) = dumb_fill() {
            poison_dumb_buffer(&object, fill);
        }
        Ok(DumbBuffer {
            object,
            pitch,
            size: size as u64,
        })
    }

    /// Makes a buffer another device rendered into scannable by the DPU.
    ///
    /// The generic imported-object wrapper withholds the exporter's frames,
    /// and frames are the only thing the DPU needs. An import produces
    /// [`ImportedScanout`], which forwards them for that one purpose.
    fn import_buffer(&self, buffer: &Arc<DmaBuf>) -> DrmResult<Option<Arc<dyn GemObject>>> {
        if buffer.object().segment().is_none() {
            return Err(DrmError::InvalidArgument);
        }

        Ok(Some(Arc::new(ImportedScanout {
            buffer: buffer.clone(),
        })))
    }
}

/// A buffer another device rendered into, seen by the display controller.
///
/// Forwards the backing frames (the DPU walks memory linearly) and refuses
/// to be mapped into a process: a CPU alias of pages the exporter maps with
/// its own cache policy is how a buffer ends up written through two mappings
/// with different attributes, and this SoC has no DMA coherency to paper
/// over it.
#[derive(Debug)]
struct ImportedScanout {
    buffer: Arc<DmaBuf>,
}

impl GemObject for ImportedScanout {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync> {
        self
    }

    fn size(&self) -> usize {
        self.buffer.object().size()
    }

    fn buffer_use(&self) -> BufferUse {
        self.buffer.object().buffer_use()
    }

    fn segment(&self) -> Option<&Segment<()>> {
        self.buffer.object().segment()
    }

    fn fault(&self, _pgoff: usize) -> DrmResult<UFrame> {
        Err(DrmError::BadAddress)
    }

    fn is_mappable(&self) -> bool {
        false
    }
}

/// The DPU's single CRTC.
#[derive(Debug)]
struct K1Crtc {
    id: ObjectId,
    primary: Arc<K1Plane>,
    cursor: Option<Arc<K1Plane>>,
}

impl Crtc for K1Crtc {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn index(&self) -> u32 {
        0
    }

    fn primary_plane(&self) -> Arc<dyn Plane> {
        self.primary.clone()
    }

    fn cursor_plane(&self) -> Option<Arc<dyn Plane>> {
        self.cursor.clone().map(|plane| plane as _)
    }
}

/// One DPU plane.
///
/// Boot programs composer `nl0` / RDMA3 as the primary. A cursor plane is
/// only constructed when `k1.cursorplane=1` and [`k1_cursor::probe`] passes;
/// it is RDMA2 + composer `nl6` and never writes the primary registers.
#[derive(Debug)]
struct K1Plane {
    id: ObjectId,
    plane_type: PlaneType,
}

impl Plane for K1Plane {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn plane_type(&self) -> PlaneType {
        self.plane_type
    }

    fn possible_crtcs(&self) -> u32 {
        1
    }

    fn formats(&self) -> &[u32] {
        FORMATS
    }

    fn modifiers(&self) -> &[u64] {
        MODIFIERS
    }
}

/// The MIPI-DSI encoder feeding the eDP bridge.
#[derive(Debug)]
struct K1Encoder {
    id: ObjectId,
}

impl Encoder for K1Encoder {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn encoder_type(&self) -> u32 {
        mode::encoder_type::DSI
    }

    fn possible_crtcs(&self) -> u32 {
        1
    }
}

/// The internal panel.
#[derive(Debug)]
struct K1Connector {
    id: ObjectId,
    encoder: ObjectId,
    mode: mode::ModeInfo,
}

impl Connector for K1Connector {
    fn id(&self) -> ObjectId {
        self.id
    }

    fn connector_type(&self) -> u32 {
        mode::connector_type::EDP
    }

    fn connector_type_id(&self) -> u32 {
        1
    }

    fn detect(&self) -> ConnectionStatus {
        ConnectionStatus::Connected
    }

    fn modes(&self) -> Vec<mode::ModeInfo> {
        vec![self.mode]
    }

    fn physical_size_mm(&self) -> (u32, u32) {
        PANEL_SIZE_MM
    }

    fn encoders(&self) -> Vec<ObjectId> {
        vec![self.encoder]
    }

    fn edid(&self) -> Option<Vec<u8>> {
        None
    }
}

/// Last hardware image this driver wrote, used to skip a latch.
struct Programmed {
    // Address alone cannot identify a register image: a reused GEM buffer may
    // have a different scanout stride.
    primary: Option<(Paddr, u32)>,
    cursor: k1_cursor::CursorProgram,
    generation: u64,
    // Current-image latch acknowledgment determines flip-event eligibility.
    latch_seen: bool,
    // Keep the preceding ACK's latch across commits: a later VSYNC can retire
    // its old DMA owners even while a newer image is being acknowledged.
    latched_generation: Option<u64>,
}

impl Programmed {
    fn observe(&mut self, status: k1_vblank::IrqStatus) -> HardwareObservation {
        // Only a preceding ACK orders this VSYNC after a consumed shadow.
        // The BSP's flip policy also accepts a latch in the current ACK, but
        // that weaker event acknowledgment must not release its old buffers.
        let retired_update = status.vsync.then_some(self.latched_generation).flatten();
        if status.cfg_rdy_clr {
            self.latch_seen = true;
            self.latched_generation = Some(self.generation);
        }
        HardwareObservation {
            vblank: status.vsync,
            flipped_update: (status.vsync && self.latch_seen).then_some(self.generation),
            retired_update,
        }
    }
}

/// IRQ observations retain the hardware-update generation at acknowledgement.
/// A deferred observation must never complete a newer register image.
#[derive(Default)]
struct PendingIrq {
    vsync: Option<u64>,
}

/// The commit path: the scanout address, and the vblank interrupt that
/// completes a flip.
struct K1Dpu {
    dpu: IoMem,
    mode: mode::ModeInfo,
    // Shared by process commits and the Taskless watchdog.
    programmed: SpinLock<Programmed, LocalIrqDisabled>,
    primary: ObjectId,
    /// The cursor plane, if `k1.cursorplane=1` passed the register probe.
    cursor: Option<ObjectId>,
    crtc: ObjectId,
    vblank: Once<Vblank>,
    irq_live: AtomicBool,
    // Shared by the hard IRQ and deferred status drain.
    pending_bits: SpinLock<PendingIrq, LocalIrqDisabled>,
    irq: SpinLock<Option<MappedIrqLine>>,
    irq_entries: AtomicU32,
    irq_stormed: AtomicBool,
    storm_reported: AtomicBool,
    /// The deferred half, kept so the vsync watchdog can schedule it from
    /// the timer interrupt.
    bottom_half: Once<Arc<Taskless>>,
    /// Jiffies at the last vsync the deferred half processed.
    last_vsync_at: AtomicU64,
    /// Set by the watchdog, consumed by the deferred half: vsync has been
    /// silent for [`VSYNC_STALL_MS`] while flips were waiting on it.
    vsync_stalled: AtomicBool,
    /// One report per stall, not one per watchdog tick.
    stall_reported: AtomicBool,
}

/// Silence the watchdog tolerates before it declares vsync dead.
///
/// The panel refreshes at ~55 Hz, so vsync arrives every ~18 ms whether or
/// not anyone commits. Three hundred milliseconds is sixteen missed frames:
/// far beyond any legitimate gap, well under the point where a user starts
/// wondering whether the machine has hung.
///
/// A flip parked on [`FlipTiming::AtVblank`] cannot complete if the DPU
/// stops raising vsync. [`MISSED_LATCH_TICKS`] also requires ticks, so an
/// independent timer must detect a stopped scanout engine.
const VSYNC_STALL_MS: u64 = 300;

/// Interrupts tolerated without the deferred half seeing a vsync.
const IRQ_STORM_LIMIT: u32 = 256;

/// Ticks the core waits for a latch before delivering a flip anyway.
const MISSED_LATCH_TICKS: u32 = 3;

impl core::fmt::Debug for K1Dpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("K1Dpu")
            .field("mode", &self.mode)
            .field("crtc", &self.crtc)
            .field("cursor", &self.cursor)
            .field("irq_live", &self.irq_live)
            .finish_non_exhaustive()
    }
}

impl AtomicDriver for K1Dpu {
    fn atomic_check(&self, state: &AtomicState) -> DrmResult<()> {
        for crtc in &state.crtcs {
            // This wrapper cannot stop the boot pipeline or change its timing.
            let Some(mode) = crtc.mode.filter(|_| crtc.active) else {
                return Err(DrmError::NotSupported);
            };
            if !same_panel_timing(&mode, &self.mode) {
                return Err(DrmError::NotSupported);
            }
        }

        let primary = state
            .planes
            .iter()
            .find(|plane| plane.plane == self.primary)
            .ok_or(DrmError::NotSupported)?;
        let Some(framebuffer) = primary.framebuffer.as_ref() else {
            // ModeConfig::new seeds the firmware-owned boot scanout before
            // bind_vblank. That buffer has no userspace framebuffer ID. Once
            // this driver is published, the bound Once cannot become empty.
            return if self.vblank.get().is_none() {
                Ok(())
            } else {
                Err(DrmError::NotSupported)
            };
        };
        let width = u32::from(self.mode.hdisplay);
        let height = u32::from(self.mode.vdisplay);
        // The RDMA image size remains fixed. A smaller buffer or an ignored
        // crop/scale would make the hardware fetch outside the accepted image.
        if primary.crtc != Some(self.crtc)
            || primary.crtc_rect != (0, 0, width, height)
            || primary.src_rect != (0, 0, width << 16, height << 16)
            || framebuffer.width < width
            || framebuffer.height < height
        {
            return Err(DrmError::NotSupported);
        }

        for plane in &state.planes {
            let Some(framebuffer) = plane.framebuffer.as_ref() else {
                continue;
            };
            let address = scanout_address(framebuffer).ok_or(DrmError::NotSupported)?;
            if Some(plane.plane) == self.cursor {
                if k1_cursor::is_dead() {
                    return Err(DrmError::NotSupported);
                }
                let (src_x, src_y, _, _) = plane.src_rect;
                k1_cursor::plan_cursor(
                    true,
                    plane.crtc_rect,
                    (src_x >> 16, src_y >> 16),
                    k1_cursor::CursorBuffer {
                        address: address as u64,
                        stride: framebuffer.pitches[0],
                        pixel_format: framebuffer.pixel_format,
                        size: (framebuffer.width, framebuffer.height),
                    },
                    (u32::from(self.mode.hdisplay), u32::from(self.mode.vdisplay)),
                )
                .map_err(|_| DrmError::NotSupported)?;
            }
        }

        Ok(())
    }

    fn atomic_commit(&self, state: &AtomicState) -> DrmResult<()> {
        self.program_scanout(state)
    }

    fn flip_timing(&self) -> FlipTiming {
        if self.irq_live.load(Ordering::Acquire) {
            FlipTiming::AtVblank
        } else {
            FlipTiming::Immediate
        }
    }
}

impl K1Dpu {
    fn program_scanout(&self, state: &AtomicState) -> DrmResult<()> {
        let target = state
            .planes
            .iter()
            .find(|plane| plane.plane == self.primary)
            .ok_or(DrmError::NotSupported)?;
        let framebuffer = target.framebuffer.as_ref().ok_or(DrmError::NotSupported)?;
        let address = scanout_address(framebuffer).ok_or(DrmError::NotSupported)?;
        ostd::mm::dma::warn_if_addr_unreachable(
            address,
            framebuffer.pitches[0] as usize * framebuffer.height as usize,
        );

        let cursor = self.cursor_program(state)?;
        if let k1_cursor::CursorProgram::On(regs) = &cursor {
            ostd::mm::dma::warn_if_addr_unreachable(
                regs.addr as Paddr,
                regs.stride as usize * regs.img_h as usize,
            );
        }

        let mut programmed = self.programmed.lock();
        if k1_vblank::latch_pending(&self.dpu).map_err(k1_vblank::io_error)? {
            return Err(DrmError::Busy);
        }
        // Record the previous image's hardware proof before starting another
        // generation. A delayed bottom half must not be required to preserve it.
        let status = k1_vblank::ack_online2(&self.dpu).map_err(k1_vblank::io_error)?;
        if self
            .observe_irq(&mut programmed, status)
            .map_err(k1_vblank::io_error)?
            && let Some(bottom_half) = self.bottom_half.get()
        {
            bottom_half.schedule();
        }
        let vblank = self.vblank.get().ok_or(DrmError::Io)?;
        let previous_retired = vblank.is_update_retired(self.crtc, programmed.generation);
        let generation = vblank.begin_update(self.crtc)?;
        programmed.generation = generation;
        programmed.latch_seen = false;

        let primary_image = (address, framebuffer.pitches[0]);
        let primary_same = programmed.primary == Some(primary_image);
        let cursor_same = programmed.cursor == cursor;
        if primary_same && cursor_same {
            // The image is unchanged and CFG_RDY was clear before old status
            // was acknowledged. No new latch is needed for the flip event.
            programmed.latch_seen = true;
            programmed.latched_generation = Some(generation);
            if previous_retired {
                vblank.complete_update(self.crtc, generation);
            } else {
                vblank.observe_hardware(
                    self.crtc,
                    HardwareObservation {
                        vblank: false,
                        flipped_update: Some(generation),
                        retired_update: None,
                    },
                );
            }
            return Ok(());
        }

        if !primary_same {
            self.write(DPU_L0_ADDR_LO, address as u32)?;
            self.write(DPU_L0_ADDR_HI, (address >> 32) as u32)?;
            self.write(DPU_L0_STRIDE, framebuffer.pitches[0])?;
            programmed.primary = Some(primary_image);
        }

        if !cursor_same {
            match k1_cursor::apply(&self.dpu, cursor, programmed.cursor) {
                Ok(_) => {
                    programmed.cursor = if k1_cursor::is_dead() {
                        k1_cursor::CursorProgram::Off
                    } else {
                        cursor
                    };
                }
                Err(error) => {
                    ostd::warn!(
                        "[k1-kms] cursor apply failed ({:?}); leaving primary as programmed",
                        error
                    );
                    programmed.cursor = k1_cursor::CursorProgram::Off;
                }
            }
        }

        k1_vblank::request_latch(&self.dpu).map_err(k1_vblank::io_error)?;
        // The splash finish callback may take VT locks; release the register lock first.
        drop(programmed);
        if !primary_same {
            // Scanout moved off the boot FB: the compositor owns the panel.
            crate::splash::end(crate::splash::EndReason::Compositor);
        }
        Ok(())
    }

    /// Register image for the cursor plane, or `Off` when the plane is
    /// absent, detached, or the path has been marked dead.
    fn cursor_program(&self, state: &AtomicState) -> DrmResult<k1_cursor::CursorProgram> {
        let Some(id) = self.cursor else {
            return Ok(k1_cursor::CursorProgram::Off);
        };
        let Some(plane) = state.planes.iter().find(|plane| plane.plane == id) else {
            return Ok(k1_cursor::CursorProgram::Off);
        };
        let Some(framebuffer) = plane.framebuffer.as_ref() else {
            return Ok(k1_cursor::CursorProgram::Off);
        };
        if k1_cursor::is_dead() {
            return Ok(k1_cursor::CursorProgram::Off);
        }
        let address = scanout_address(framebuffer).ok_or(DrmError::NotSupported)?;
        let (src_x, src_y, _, _) = plane.src_rect;
        Ok(k1_cursor::plan_cursor(
            plane.crtc.is_some(),
            plane.crtc_rect,
            (src_x >> 16, src_y >> 16),
            k1_cursor::CursorBuffer {
                address: address as u64,
                stride: framebuffer.pitches[0],
                pixel_format: framebuffer.pixel_format,
                size: (framebuffer.width, framebuffer.height),
            },
            (u32::from(self.mode.hdisplay), u32::from(self.mode.vdisplay)),
        )
        .unwrap_or(k1_cursor::CursorProgram::Off))
    }

    fn write(&self, offset: usize, value: u32) -> DrmResult<()> {
        self.dpu
            .write_once(offset, &value)
            .map_err(|_| DrmError::Io)
    }

    fn bind_vblank(&self, vblank: Vblank) {
        self.vblank.call_once(|| vblank);
    }

    fn install_irq(self: &Arc<Self>) {
        if let Err(err) = k1_vblank::quiesce_all(&self.dpu) {
            ostd::warn!(
                "could not quiesce DPU interrupts ({:?}); not touching the PLIC",
                err
            );
            return;
        }

        let Some(interrupt) = k1_vblank::lookup_interrupt() else {
            ostd::warn!(
                "no DPU node in the DTB, so no vblank: page-flip events stay \
                 immediate. Add `dpu@c0340000` with interrupts = <{}>",
                k1_vblank::DPU_ONLINE_IRQ
            );
            return;
        };

        let Ok(line) = IrqLine::alloc() else {
            ostd::warn!("could not allocate an IRQ line for the DPU");
            return;
        };
        let Some(chip) = IRQ_CHIP.get() else {
            ostd::warn!("no interrupt controller; DPU vblank not installed");
            return;
        };
        let mut mapped = match chip.map_fdt_pin_to(interrupt, line) {
            Ok(mapped) => mapped,
            Err(err) => {
                ostd::warn!(
                    "could not map DPU interrupt {} on phandle {:#x}: {:?}",
                    interrupt.arguments[0],
                    interrupt.interrupt_parent,
                    err
                );
                return;
            }
        };

        let this = self.clone();
        let bottom_half = Taskless::new(move || this.bottom_half());
        self.bottom_half.call_once(|| bottom_half.clone());
        let this = self.clone();
        mapped.on_active(move |_trap| this.handle_irq(&bottom_half));
        *self.irq.lock() = Some(mapped);

        if let Err(err) = k1_vblank::enable_online2(&self.dpu) {
            ostd::warn!("could not enable DPU interrupt masks: {:?}", err);
            return;
        }

        // The watchdog only ever reads atomics and schedules the deferred
        // half, so it is safe in the timer interrupt; everything that takes
        // a lock or prints runs in `bottom_half`.
        let this = self.clone();
        ostd::timer::register_callback_on_cpu(move || this.watchdog_poll());

        ostd::info!(
            "DPU online irq {} installed; page-flip events wait for the first vsync \
             (watchdog: {} ms without vsync reverts to immediate flips)",
            interrupt.arguments[0],
            VSYNC_STALL_MS
        );
    }

    /// Timer-interrupt half of the vsync watchdog.
    ///
    /// Fires once vsync has been live and then silent for [`VSYNC_STALL_MS`].
    /// Nothing here takes a lock — the deferred half on this CPU may be
    /// holding `pending_bits` or the core's vblank lock when the timer
    /// interrupts it.
    fn watchdog_poll(&self) {
        if !self.irq_live.load(Ordering::Acquire) || self.irq_stormed.load(Ordering::Relaxed) {
            return;
        }
        if self.vsync_stalled.load(Ordering::Relaxed) {
            return;
        }
        let last = self.last_vsync_at.load(Ordering::Relaxed);
        let now = Jiffies::elapsed().as_u64();
        if now.saturating_sub(last) < VSYNC_STALL_MS {
            return;
        }
        self.vsync_stalled.store(true, Ordering::Release);
        if let Some(bottom_half) = self.bottom_half.get() {
            bottom_half.schedule();
        }
    }

    /// Deferred half of the watchdog: degrade to immediate flips, then
    /// retreat on the cursor if one is live.
    fn handle_vsync_stall(&self) {
        let Some(vblank) = self.vblank.get() else {
            return;
        };
        let silent_ms = Jiffies::elapsed()
            .as_u64()
            .saturating_sub(self.last_vsync_at.load(Ordering::Relaxed));
        ostd::warn!(
            "[k1-kms] vsync stalled: no DPU vsync for {} ms with the interrupt live; \
             reverting to immediate page-flip events and draining the parked flip",
            silent_ms
        );

        self.irq_live.store(false, Ordering::Release);
        for _ in 0..=MISSED_LATCH_TICKS {
            vblank.fallback_tick(self.crtc);
        }

        if self.cursor.is_some() && !k1_cursor::is_dead() {
            k1_cursor::mark_dead();
            let mut programmed = self.programmed.lock();
            match k1_cursor::apply(&self.dpu, k1_cursor::CursorProgram::Off, programmed.cursor) {
                Ok(_) => ostd::warn!(
                    "[k1-kms] vsync stalled: cursor path marked dead and RDMA2 disabled; \
                     if vsync resumes below, the cursor was what stopped the pipeline"
                ),
                Err(error) => ostd::warn!(
                    "[k1-kms] vsync stalled: could not disable the cursor path ({:?})",
                    error
                ),
            }
            programmed.cursor = k1_cursor::CursorProgram::Off;
            let _ = k1_vblank::request_latch(&self.dpu);
        }
    }

    /// Records hardware ordering while the register/IRQ lock is held.
    /// Event delivery remains deferred, but a later commit cannot erase proof.
    fn observe_irq(
        &self,
        programmed: &mut Programmed,
        mut status: k1_vblank::IrqStatus,
    ) -> Result<bool, ostd::Error> {
        // An old sticky CFG_RDY_CLR cannot acknowledge an unconsumed request.
        if status.cfg_rdy_clr && k1_vblank::latch_pending(&self.dpu)? {
            status.cfg_rdy_clr = false;
        }
        let observation = programmed.observe(status);
        if !status.vsync {
            return Ok(false);
        }
        if let Some(vblank) = self.vblank.get() {
            vblank.observe_hardware(self.crtc, observation);
            self.pending_bits.lock().vsync = Some(programmed.generation);
        }
        Ok(true)
    }

    fn handle_irq(&self, bottom_half: &Arc<Taskless>) {
        if self.irq_stormed.load(Ordering::Relaxed) {
            return;
        }

        let result = {
            let mut programmed = self.programmed.lock();
            k1_vblank::ack_online2(&self.dpu).and_then(|status| {
                let taken = self.irq_entries.fetch_add(1, Ordering::Relaxed) + 1;
                if taken > IRQ_STORM_LIMIT {
                    self.irq_stormed.store(true, Ordering::Relaxed);
                    self.irq_live.store(false, Ordering::Release);
                    k1_vblank::disable_all(&self.dpu)?;
                    return Ok(true);
                }
                self.observe_irq(&mut programmed, status)
            })
        };
        match result {
            Ok(true) => bottom_half.schedule(),
            Ok(false) => {}
            Err(err) => ostd::error!("could not acknowledge the DPU interrupt: {:?}", err),
        }
    }

    fn bottom_half(&self) {
        if self.vsync_stalled.load(Ordering::Acquire)
            && !self.stall_reported.swap(true, Ordering::AcqRel)
        {
            self.handle_vsync_stall();
        }
        self.complete_vblank();
    }

    fn complete_vblank(&self) {
        let status = {
            let mut pending = self.pending_bits.lock();
            core::mem::take(&mut *pending)
        };
        let Some(vblank) = self.vblank.get() else {
            return;
        };

        if self.irq_stormed.load(Ordering::Relaxed)
            && !self.storm_reported.swap(true, Ordering::AcqRel)
        {
            ostd::warn!(
                "DPU interrupt asserted {} times without a recognised status bit; \
                 masked it and reverted to immediate page-flip events",
                IRQ_STORM_LIMIT
            );
            for _ in 0..=MISSED_LATCH_TICKS {
                vblank.fallback_tick(self.crtc);
            }
            return;
        }

        if let Some(generation) = status.vsync {
            self.irq_entries.store(0, Ordering::Relaxed);
            self.last_vsync_at
                .store(Jiffies::elapsed().as_u64(), Ordering::Relaxed);
            let resumed = self.vsync_stalled.swap(false, Ordering::AcqRel);
            if !self.irq_live.swap(true, Ordering::AcqRel) {
                if resumed {
                    self.stall_reported.store(false, Ordering::Release);
                    ostd::info!(
                        "vsync resumed after the stall; page-flip events wait for vblank again"
                    );
                } else {
                    ostd::info!("first DPU vsync seen; page-flip events now wait for vblank");
                }
            }
            vblank.dispatch_observed_vblank(self.crtc, generation);
        }
    }
}

/// `k1.dumb.fill=0xAARRGGBB` — poison every new dumb buffer with this pixel
/// instead of leaving it zero. `0` turns the fill off.
///
/// Repeat the whole-buffer write and flush before exposing a cursor buffer.
/// This supplements `ShmemObject::new`'s zero-fill and cache flush to avoid
/// incomplete cursor scanout; the reason a single flush is insufficient is
/// unresolved.
///
/// Default `0x00000001`: alpha 0, so premultiplied blending adds nothing
/// the eye can see. `k1.dumb.fill=0` switches it off.
static DUMB_FILL: Once<k1_cursor::Hex> = Once::new();
aster_cmdline::define_kv_param!("k1.dumb.fill", DUMB_FILL);

/// The verified poison: alpha 0, blends to nothing, never written by a client.
const DUMB_FILL_DEFAULT: u32 = 0x0000_0001;

fn dumb_fill() -> Option<u32> {
    match DUMB_FILL.get().map(|hex| hex.0) {
        Some(0) => None,
        Some(fill) => Some(fill),
        None => Some(DUMB_FILL_DEFAULT),
    }
}

/// Writes `fill` over the whole object through the kernel's cached alias and
/// flushes it to DRAM. See [`DUMB_FILL`].
fn poison_dumb_buffer(object: &ShmemObject, fill: u32) {
    use ostd::mm::{HasSize, VmIo};

    let Some(segment) = object.segment() else {
        return;
    };
    let size = segment.size();
    let row: Vec<u32> = alloc::vec![fill; 1024];
    let mut offset = 0usize;
    while offset < size {
        let n = ((size - offset) / 4).min(row.len());
        if segment.write_slice(offset, &row[..n]).is_err() {
            break;
        }
        offset += n * 4;
    }
    if object.sync_to_device(0..size).is_err() {
        ostd::warn!(
            "[k1-kms] dumb pa={:#x} size={:#x}: poison flush failed",
            segment.paddr(),
            size
        );
    }
}

/// Returns the physical address the DPU would scan a framebuffer out from.
fn scanout_address(framebuffer: &Framebuffer) -> Option<Paddr> {
    let object = framebuffer.objects[0].as_ref()?;
    let segment = object.segment()?;
    Some(segment.paddr() + framebuffer.offsets[0] as usize)
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn the_dpu_and_the_gpu_are_distinct_devices() {
        const GPU_OF_FULLNAME: &str = "/soc/imggpu@cac00000";

        assert_ne!(OF_FULLNAME, GPU_OF_FULLNAME);
        assert!(OF_FULLNAME.starts_with('/'));
        assert!(OF_FULLNAME.ends_with(DEVICE_NAME));
    }

    #[ktest]
    fn an_imported_frame_is_scannable_but_not_cpu_mappable() {
        const FOUR_PAGES: usize = 4 * 4096;

        let exported = ShmemObject::new(FOUR_PAGES, BufferUse::Render).expect("frames");
        let expected = exported.segment().expect("frames").paddr();

        let buffer = k1_musebook_drm::prime::foreign_buffer_for_test(exported);
        let imported: Arc<dyn GemObject> = Arc::new(ImportedScanout { buffer });

        assert_eq!(imported.segment().expect("frames").paddr(), expected);
        assert_eq!(imported.size(), FOUR_PAGES);
        assert!(!imported.is_mappable());
        assert!(imported.fault(0).is_err());
    }

    #[ktest]
    fn the_panel_mode_matches_what_the_boot_sequence_programmed() {
        let mode = panel_mode(1920, 1080, 148_500_000);

        assert_eq!(mode.hdisplay, 1920);
        assert_eq!(mode.hsync_start, 1968);
        assert_eq!(mode.hsync_end, 2000);
        assert_eq!(mode.htotal, 2200);
        assert_eq!(mode.vdisplay, 1080);
        assert_eq!(mode.vsync_start, 1083);
        assert_eq!(mode.vsync_end, 1089);
        assert_eq!(mode.vtotal, 1120);
        assert_eq!(mode.clock, 148_500);
        assert_eq!(mode.vrefresh, 60);
        assert!(mode.name.contains(&0));
        assert!(mode.type_ & mode::mode_type::PREFERRED != 0);
    }

    fn empty_programmed(generation: u64) -> Programmed {
        Programmed {
            primary: None,
            cursor: k1_cursor::CursorProgram::Off,
            generation,
            latch_seen: false,
            latched_generation: None,
        }
    }

    #[ktest]
    fn coalesced_latch_and_vsync_acknowledge_flip_without_retiring_dma() {
        let mut programmed = empty_programmed(1);
        let both = k1_vblank::IrqStatus {
            vsync: true,
            cfg_rdy_clr: true,
        };
        let first = programmed.observe(both);
        assert_eq!(first.flipped_update, Some(1));
        assert_eq!(first.retired_update, None);
        let later = programmed.observe(both);
        assert_eq!(later.flipped_update, Some(1));
        assert_eq!(later.retired_update, Some(1));
    }

    #[ktest]
    fn an_independent_latch_then_vsync_proves_both_contracts() {
        let mut programmed = empty_programmed(1);
        let latch = programmed.observe(k1_vblank::IrqStatus {
            vsync: false,
            cfg_rdy_clr: true,
        });
        assert_eq!(latch.flipped_update, None);
        assert_eq!(latch.retired_update, None);
        let vsync = programmed.observe(k1_vblank::IrqStatus {
            vsync: true,
            cfg_rdy_clr: false,
        });
        assert_eq!(vsync.flipped_update, Some(1));
        assert_eq!(vsync.retired_update, Some(1));
    }

    #[ktest]
    fn continuous_coalesced_frames_keep_prior_latch_proof_across_commits() {
        let mut programmed = empty_programmed(1);
        for generation in 1..=64 {
            programmed.generation = generation;
            programmed.latch_seen = false;
            let observation = programmed.observe(k1_vblank::IrqStatus {
                vsync: true,
                cfg_rdy_clr: true,
            });
            assert_eq!(observation.flipped_update, Some(generation));
            assert_eq!(
                observation.retired_update,
                (generation > 1).then_some(generation - 1)
            );
        }
    }

    #[ktest]
    fn a_flip_loads_the_shadow_and_does_not_restart() {
        assert_eq!(DPU_CTRL_LOAD, 0x56c);
        assert_eq!(DPU_CTRL_LOAD, k1_vblank::DPU_CTL2_CFG_RDY);
        assert_ne!(DPU_CTRL_LOAD, k1_vblank::DPU_CTL2_SW_START);
    }

    #[ktest]
    fn primary_scanout_registers_are_rdma3() {
        // RDMA3 base is 0xd80; layer address/stride sit at +0x20 / +0x24 / +0x38.
        assert_eq!(DPU_L0_ADDR_LO, k1_cursor::rdma_base(3) + 0x20);
        assert_eq!(DPU_L0_ADDR_HI, k1_cursor::rdma_base(3) + 0x24);
        assert_eq!(DPU_L0_STRIDE, k1_cursor::rdma_base(3) + 0x38);
        assert_eq!(k1_cursor::rdma_base(k1_cursor::CURSOR_RDMA_ID), 0xc80);
    }

    #[ktest]
    fn cursor_plane_is_off_unless_the_knob_and_probe_agree() {
        assert!(!k1_cursor::requested());
        assert_eq!(k1_cursor::advertised_types(false), &[PlaneType::Primary]);
    }

    #[ktest]
    fn dumb_fill_defaults_to_the_verified_poison() {
        assert_eq!(dumb_fill(), Some(0x0000_0001));
        assert_eq!("0x1".parse::<k1_cursor::Hex>(), Ok(k1_cursor::Hex(1)));
        assert_eq!("0".parse::<k1_cursor::Hex>(), Ok(k1_cursor::Hex(0)));
    }
}
