// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: DPU hardware cursor plane (RDMA2 + composer nl6).

//! Hardware cursor plane for the K1 DPU (`spacemit,dpu-online2`).
//!
//! # Why this is optional
//!
//! `k1_edp::dsi_dpu_init` programs one RDMA (RDMA3) and one composer layer
//! (CMP2 `nl0`). That path is what lights the Muse Book panel. A cursor plane
//! writes a *different* RDMA and a *different* composer layer; the default
//! is off so a wrong guess cannot touch the working scanout.
//!
//! Enable with `k1.cursorplane=1`. At registration a probe reads the boot
//! composer / `rch_en` values; any mismatch falls back to Primary-only and
//! logs one warning. The release DTS turns the knob on.
//!
//! # Hardware (Saturn LE, DSI / eDP at `0xc0340000`)
//!
//! | Block | Base | Boot use | Cursor use |
//! |---|---|---|---|
//! | RDMA3 | `0xd80` | primary scanout | only `FBC_MEM_SIZE`, once, to shrink its line-buffer window |
//! | RDMA2 | `0xc80` | unused | cursor scanout |
//! | CMP2 | `0x4c00` | `nl0` → RDMA3 | primary moved to `nl7`, cursor on `nl6` |
//! | `ctl2_nml_rch_en` | `0x560` | bit 3 + outctl | RMW bit 2 only |
//!
//! The composer stacks lower `nl` on top, so the primary must leave `nl0`
//! for the cursor. RDMA2/RDMA3 reset to the same line-buffer window
//! (`FBC_MEM_SIZE = 0x440` each); partition it to keep cursor pixels from
//! overlapping primary scanout at x=0.
//!
//! Cursor register configuration: `alphasel=1`, `blend=1`,
//! `alpha=0xff`, `afactor=0`, fbcmem Auto (RDMA3 `0x00080438`, RDMA2
//! `0x10000008`), vendor z-order, `xcrop=0`, `LAYER_CTRL=0x1e203c`.
//!
//! Sources: BSP `dpu/saturn_regs/reg_map.h`, `rdma_path_x.h`, `cmps_x.h`,
//! `dpu_ctl.h`; boot values from `k1_edp.rs` `dsi_dpu_init`.

use core::{
    str::FromStr,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use k1_musebook_drm::{
    error::{DrmError, DrmResult},
    uapi::fourcc,
};
use ostd::{io::IoMem, mm::VmIoOnce};
use spin::Once;

/// `k1.cursorplane=` defaults to off, advertising only the primary plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorPlaneKnob {
    /// Do not advertise a cursor plane. Register writes stay on RDMA3 / `nl0`.
    Off,
    /// Advertise `PlaneType::Cursor` if [`probe`] succeeds.
    On,
}

impl FromStr for CursorPlaneKnob {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1" | "on" | "yes" | "true" => Ok(Self::On),
            "0" | "off" | "no" | "false" => Ok(Self::Off),
            _ => Err(()),
        }
    }
}

static CURSOR_PLANE: Once<CursorPlaneKnob> = Once::new();
aster_cmdline::define_kv_param!("k1.cursorplane", CURSOR_PLANE);

/// A `u32` that also parses `0x...`. Used by `k1.dumb.fill`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Hex(pub u32);

impl FromStr for Hex {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u32::from_str_radix(h, 16).map(Hex).map_err(|_| ())
        } else {
            s.parse::<u32>().map(Hex).map_err(|_| ())
        }
    }
}

/// Composer layer index the cursor is programmed on. Probe sets this to 6
/// after the vendor z-order relayout; 1 is only the pre-relayout default.
static CURSOR_NL: AtomicU32 = AtomicU32::new(1);

pub(super) fn cursor_nl() -> u32 {
    CURSOR_NL.load(Ordering::Relaxed)
}

/// `FBC_MEM_SIZE` in `rdma_path_x.h` (register index 30).
const RDMA_FBC_MEM_SIZE: usize = 0x78;

/// Set when a live write/readback says the cursor layer is not safe to keep
/// programming. [`super::k1_kms`] then refuses an enabled cursor plane in
/// `atomic_check` so smithay can fall back to a software cursor.
static CURSOR_DEAD: AtomicBool = AtomicBool::new(false);

/// SATURN_LE reports 4 RDMAs (`dpu_saturn.c` `saturn_le_rdmas`). RDMA3 is
/// the boot primary; RDMA2 is RGB-only (`FORMAT_RGB`, no YUV) and unused.
pub(super) const CURSOR_RDMA_ID: u32 = 2;
/// Boot primary, `k1_edp` `0xd80`.
pub(super) const PRIMARY_RDMA_ID: u32 = 3;

/// `RDMA0_BASE_ADDR` in `reg_map.h`.
const RDMA0_BASE: usize = 0xa80;
/// `RDMA_SIZE` in `reg_map.h`.
const RDMA_SIZE: usize = 0x100;

/// CMP2, the online2 composer `k1_edp` programs at `0x4c00`.
pub(super) const CMP2_BASE: usize = 0x4c00;
/// Composer layer 0 enable (`dpu_cmps_reg_14`). Boot writes `0x7`.
pub(super) const CMP_NL0_EN: usize = 0x38;
/// Bytes from `nl0` to `nl1` (`dpu_cmps_reg_22` − `reg_14`).
const CMP_LAYER_STRIDE: usize = 0x20;

/// `ctl2_nml_rch_en` — `dpu_ctl_reg_24`.
pub(super) const CTL2_RCH_EN: usize = 0x560;
/// RDMA3, the boot primary. Must never be cleared by the cursor path.
pub(super) const CTL2_RCH_PRIMARY_BIT: u32 = 1 << PRIMARY_RDMA_ID;
/// RDMA2, the cursor channel.
pub(super) const CTL2_RCH_CURSOR_BIT: u32 = 1 << CURSOR_RDMA_ID;
/// `ctl2_nml_outctl_en`. Boot `0x40008` has this plus the primary bit.
pub(super) const CTL2_OUTCTL_EN: u32 = 1 << 18;
/// `0x40008`: RDMA3 + outctl. Probe requires these bits to still be set.
pub(super) const BOOT_RCH_EN: u32 = CTL2_RCH_PRIMARY_BIT | CTL2_OUTCTL_EN;

/// `LAYER_CTRL` value `k1_edp` writes to RDMA3 (`0xd80`). Composer id 2,
/// `rdma_burst_len=16` (bit 21), `outstanding_num=16` (bit 6). Kept so
/// ktests can name the boot word next to the vendor cursor word.
#[cfg(ktest)]
pub(super) const BOOT_LAYER_CTRL: u32 = 0x202040;
/// `LAYER_CTRL` the vendor writes for a linear layer on online2
/// (`dpu_saturn.c`: `0xf<<17 | channel<<12 | 0xf<<2 | is_afbc`, channel 2):
/// `rdma_burst_len=15`, composer id 2, `outstanding_num=15`, not AFBC.
///
/// A 1-line cursor window (8 fbcmem units) needs this shallower prefetch;
/// the boot word over-reads the tip.
pub(super) const VENDOR_LAYER_CTRL: u32 = (0xf << 17) | (2 << 12) | (0xf << 2);

/// CMP2 `nl0`: enable + `layer_id = 3` + not solid.
pub(super) const BOOT_NL0_EN: u32 = 0x7;

/// Units the hardware reports at reset for the RDMA2/RDMA3 pool.
const FBCMEM_POOL_UNITS: u32 = 0x440;
/// 64 px × 4 B, rounded to 64 B, in 32-byte units (vendor
/// `get_raw_data_plane_rdma_mem_size`).
const FBCMEM_CURSOR_UNITS: u32 = 8;

/// `(RDMA2, RDMA3)` `FBC_MEM_SIZE` words for a cursor window of `units`.
///
/// Vendor layout (`saturn_adjust_rdma_fbcmem`): the even RDMA is
/// `map=1, start=0`, the odd one follows it with `map=0`. For the verified
/// 8 units: RDMA2 `0x10000008`, RDMA3 `0x00080438`.
pub(super) const fn fbcmem_windows(units: u32) -> (u32, u32) {
    (
        (1 << 28) | units,
        (units << 16) | (FBCMEM_POOL_UNITS - units),
    )
}

/// RDMA offsets from `rdma_path_x.h` (one `UINT32` per named register).
const RDMA_LAYER_CTRL: usize = 0x00;
const RDMA_COMPSR_Y_OFST: usize = 0x04;
const RDMA_ADDR_LO: usize = 0x20;
const RDMA_ADDR_HI: usize = 0x24;
const RDMA_STRIDE: usize = 0x38;
const RDMA_IMG_SIZE: usize = 0x3c;
const RDMA_CROP_START: usize = 0x40;
const RDMA_CROP_END: usize = 0x44;
const RDMA_ROT_MODE: usize = 0x70;

/// Hardware format ids from `dpu_saturn.c` `primary_fmts`.
const RDMA_FMT_ARGB8888: u32 = 4;
const RDMA_FMT_XRGB8888: u32 = 8;

/// Largest cursor framebuffer this path will accept.
///
/// The 8-unit line-buffer partition holds 64 pixels at four bytes each,
/// matching DRM_CAP_CURSOR_WIDTH/HEIGHT. Larger planes require a different
/// partition and are rejected before register programming.
pub(super) const CURSOR_MAX: u32 = FBCMEM_CURSOR_UNITS * 32 / 4;

/// Visible rectangle after clipping a dest/src pair to the panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorBlit {
    pub dest_x: u32,
    pub dest_y: u32,
    pub dest_w: u32,
    pub dest_h: u32,
    pub src_x: u32,
    pub src_y: u32,
}

/// Packed register values for one cursor update, or "layer off".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorProgram {
    /// Composer cursor `nl` disabled, RDMA2 bit clear in `rch_en`.
    Off,
    /// RDMA2 + cursor `nl` programmed to this snapshot.
    On(CursorRegs),
}

/// One self-contained cursor register image. Compared for the same-addr skip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CursorRegs {
    pub addr: u64,
    pub stride: u32,
    pub img_w: u32,
    pub img_h: u32,
    pub layer_ctrl: u32,
    pub y_ofst: u32,
    pub crop_start: u32,
    pub crop_end: u32,
    pub rot_mode: u32,
    pub nl_en: u32,
    pub ltopx: u32,
    pub ltopy_rbotx: u32,
    pub rboty_blend: u32,
    pub alpha: u32,
}

/// Why [`plan_cursor`] refused to build a program.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorReject {
    Format,
    TooBig,
}

/// Command-line request. Unset is [`CursorPlaneKnob::Off`].
pub(super) fn knob() -> CursorPlaneKnob {
    CURSOR_PLANE.get().copied().unwrap_or(CursorPlaneKnob::Off)
}

/// Whether the user asked to try a cursor plane.
pub(super) fn requested() -> bool {
    matches!(knob(), CursorPlaneKnob::On)
}

/// Whether a previous programming attempt marked the path unusable.
pub(super) fn is_dead() -> bool {
    CURSOR_DEAD.load(Ordering::Acquire)
}

pub(super) fn mark_dead() {
    CURSOR_DEAD.store(true, Ordering::Release);
}

/// Plane types the KMS device should report for a given (knob, probe) pair.
///
/// Disabling the cursor or failing its probe advertises only `[Primary]`.
#[cfg(ktest)]
pub(super) fn advertised_types(live: bool) -> &'static [k1_musebook_drm::kms::PlaneType] {
    use k1_musebook_drm::kms::PlaneType;
    if live {
        &[PlaneType::Primary, PlaneType::Cursor]
    } else {
        &[PlaneType::Primary]
    }
}

/// RDMA block base for `rdma_id` 0..=11.
pub(super) const fn rdma_base(rdma_id: u32) -> usize {
    RDMA0_BASE + rdma_id as usize * RDMA_SIZE
}

/// Composer `nlN` enable register, relative to [`CMP2_BASE`].
pub(super) const fn cmp_nl_en(layer: u32) -> usize {
    CMP_NL0_EN + layer as usize * CMP_LAYER_STRIDE
}

/// `m_nlN_en | layer_id | solid_en=0`.
pub(super) const fn pack_nl_en(rdma_id: u32) -> u32 {
    1 | (rdma_id << 1)
}

/// `LEFT_IMG_SIZE`: height << 16 | width.
pub(super) const fn pack_img_size(width: u32, height: u32) -> u32 {
    (height << 16) | width
}

/// `LEFT_CROP_POS_START`: y << 16 | x.
pub(super) const fn pack_crop_start(x: u32, y: u32) -> u32 {
    (y << 16) | x
}

/// `LEFT_CROP_POS_END`: inclusive (y+h-1) << 16 | (x+w-1).
pub(super) const fn pack_crop_end(x: u32, y: u32, w: u32, h: u32) -> u32 {
    ((y + h - 1) << 16) | (x + w - 1)
}

/// CMP `nlN` dest: `color_key=0`, `ltopx` in bits [23:8].
pub(super) const fn pack_ltopx(x: u32) -> u32 {
    x << 8
}

/// CMP `nlN`: `ltopy` in [15:0], `rbotx` in [31:16].
pub(super) const fn pack_ltopy_rbotx(top: u32, right: u32) -> u32 {
    (right << 16) | top
}

/// CMP `nlN`: `rboty` in [15:0], `blend_mode` in [17:16], `alpha_sel` in [18].
///
/// Premultiplied (`blend_mode = 1`) when the format has an alpha channel;
/// that is what smithay's pixman cursor path produces (`Argb8888`).
/// ARGB requires `alphasel=1` and `blend=1`.
pub(super) const fn pack_rboty_blend(bottom: u32, has_alpha: bool) -> u32 {
    let blend_mode = if has_alpha { 1 } else { 0 };
    let alpha_sel = if has_alpha { 1 } else { 0 };
    bottom | (blend_mode << 16) | (alpha_sel << 18)
}

/// CMP `nlN` layer alpha 0xff in bits [23:16], matching boot `nl0`.
/// `afactor` (bits [7:0]) stays 0; the vendor never writes it.
pub(super) const fn pack_alpha() -> u32 {
    0x00ff_0000
}

/// Saturn RDMA pixel-format field for a FourCC, if this path supports it.
pub(super) fn rdma_format(pixel_format: u32) -> Option<u32> {
    match pixel_format {
        fourcc::ARGB8888 => Some(RDMA_FMT_ARGB8888),
        fourcc::XRGB8888 => Some(RDMA_FMT_XRGB8888),
        _ => None,
    }
}

/// Clips a dest rectangle to the panel, shifting the source origin with it.
///
/// Dest coordinates are signed so the plane may hang off any edge. Composer
/// dest registers are unsigned, so a negative origin has to become a crop
/// rather than a programmed negative `ltopx`. Fully off-screen is `None`.
pub(super) fn clip_cursor(
    (dest_x, dest_y, dest_w, dest_h): (i32, i32, u32, u32),
    (src_x, src_y): (u32, u32),
    (screen_w, screen_h): (u32, u32),
) -> Option<CursorBlit> {
    if dest_w == 0 || dest_h == 0 || screen_w == 0 || screen_h == 0 {
        return None;
    }

    let x0 = dest_x;
    let y0 = dest_y;
    let x1 = dest_x.checked_add(dest_w as i32)?;
    let y1 = dest_y.checked_add(dest_h as i32)?;

    let vis_x0 = x0.max(0);
    let vis_y0 = y0.max(0);
    let vis_x1 = x1.min(screen_w as i32);
    let vis_y1 = y1.min(screen_h as i32);
    if vis_x1 <= vis_x0 || vis_y1 <= vis_y0 {
        return None;
    }

    let dx = (vis_x0 - x0) as u32;
    let dy = (vis_y0 - y0) as u32;
    Some(CursorBlit {
        dest_x: vis_x0 as u32,
        dest_y: vis_y0 as u32,
        dest_w: (vis_x1 - vis_x0) as u32,
        dest_h: (vis_y1 - vis_y0) as u32,
        src_x: src_x.checked_add(dx)?,
        src_y: src_y.checked_add(dy)?,
    })
}

/// Scanout storage and layout for one cursor buffer.
pub(super) struct CursorBuffer {
    pub address: u64,
    pub stride: u32,
    pub pixel_format: u32,
    pub size: (u32, u32),
}

/// Builds the register image for one cursor plane state.
///
/// Source coordinates are in pixels; buffer dimensions are written to
/// `LEFT_IMG_SIZE`.
pub(super) fn plan_cursor(
    attached: bool,
    dest: (i32, i32, u32, u32),
    src_px: (u32, u32),
    buffer: CursorBuffer,
    screen: (u32, u32),
) -> Result<CursorProgram, CursorReject> {
    let CursorBuffer {
        address: addr,
        stride,
        pixel_format,
        size: (fb_w, fb_h),
    } = buffer;
    if !attached {
        return Ok(CursorProgram::Off);
    }
    let Some(rot) = rdma_format(pixel_format) else {
        return Err(CursorReject::Format);
    };
    if dest.2 > CURSOR_MAX || dest.3 > CURSOR_MAX || fb_w > CURSOR_MAX || fb_h > CURSOR_MAX {
        return Err(CursorReject::TooBig);
    }
    if dest.2 == 0 || dest.3 == 0 {
        return Ok(CursorProgram::Off);
    }

    let Some(blit) = clip_cursor(dest, src_px, screen) else {
        return Ok(CursorProgram::Off);
    };

    let has_alpha = pixel_format == fourcc::ARGB8888;
    let right = blit.dest_x + blit.dest_w - 1;
    let bottom = blit.dest_y + blit.dest_h - 1;
    Ok(CursorProgram::On(CursorRegs {
        addr,
        stride,
        img_w: fb_w,
        img_h: fb_h,
        layer_ctrl: VENDOR_LAYER_CTRL,
        y_ofst: blit.dest_y,
        crop_start: pack_crop_start(blit.src_x, blit.src_y),
        crop_end: pack_crop_end(blit.src_x, blit.src_y, blit.dest_w, blit.dest_h),
        rot_mode: rot,
        nl_en: pack_nl_en(CURSOR_RDMA_ID),
        ltopx: pack_ltopx(blit.dest_x),
        ltopy_rbotx: pack_ltopy_rbotx(blit.dest_y, right),
        rboty_blend: pack_rboty_blend(bottom, has_alpha),
        alpha: pack_alpha(),
    }))
}

/// Reads the boot composer / `rch_en` values and pokes unused `nl1`.
///
/// Failure means the register map we think we have is not what is running,
/// so a cursor plane must not be advertised. Writes only `nl1_en = 0`,
/// which is the reset value; RDMA3 and `nl0` are not touched until the
/// vendor z-order relayout, which copies `nl0` to `nl7` first.
pub(super) fn probe(dpu: &IoMem) -> bool {
    let Ok(nl0) = read(dpu, CMP2_BASE + CMP_NL0_EN) else {
        ostd::warn!("[k1-kms] cursor probe: cannot read CMP2 nl0");
        return false;
    };
    if nl0 != BOOT_NL0_EN {
        ostd::warn!(
            "[k1-kms] cursor probe: CMP2 nl0 is {:#x}, expected {:#x} (boot primary); \
             reporting Primary only",
            nl0,
            BOOT_NL0_EN
        );
        return false;
    }

    let Ok(rch) = read(dpu, CTL2_RCH_EN) else {
        ostd::warn!("[k1-kms] cursor probe: cannot read ctl2_nml_rch_en");
        return false;
    };
    if rch & BOOT_RCH_EN != BOOT_RCH_EN {
        ostd::warn!(
            "[k1-kms] cursor probe: rch_en {:#x} missing boot bits {:#x}; \
             reporting Primary only",
            rch,
            BOOT_RCH_EN
        );
        return false;
    }

    let nl1 = CMP2_BASE + cmp_nl_en(1);
    if write(dpu, nl1, 0).is_err() {
        ostd::warn!("[k1-kms] cursor probe: cannot write CMP2 nl1");
        return false;
    }
    match read(dpu, nl1) {
        Ok(0) => {}
        Ok(value) => {
            ostd::warn!(
                "[k1-kms] cursor probe: nl1 read back {:#x} after writing 0; \
                 reporting Primary only",
                value
            );
            return false;
        }
        Err(()) => {
            ostd::warn!("[k1-kms] cursor probe: cannot read CMP2 nl1");
            return false;
        }
    }

    if !relayout_vendor_z(dpu) {
        ostd::warn!(
            "[k1-kms] cursor probe: vendor z-order relayout failed; \
             staying on nl0/nl1"
        );
    }

    // Carve the shared line buffer once, before the first cursor frame.
    // Written here rather than per commit because RDMA3's window is the
    // primary's and should change exactly once, at a frame boundary the
    // latch below provides.
    let (cursor_word, primary_word) = fbcmem_windows(FBCMEM_CURSOR_UNITS);
    let base2 = rdma_base(CURSOR_RDMA_ID) + RDMA_FBC_MEM_SIZE;
    let base3 = rdma_base(PRIMARY_RDMA_ID) + RDMA_FBC_MEM_SIZE;
    let fbc3 = read(dpu, base3).unwrap_or(0);
    let fbc2 = read(dpu, base2).unwrap_or(0);
    if write(dpu, base3, primary_word).is_err()
        || write(dpu, base2, cursor_word).is_err()
        || super::k1_vblank::request_latch(dpu).is_err()
    {
        ostd::warn!("[k1-kms] cursor fbcmem: window write failed; reporting Primary only");
        return false;
    }

    ostd::info!(
        "[k1-kms] cursorplane=1: probe ok (nl0={:#x} rch={:#x}); \
         advertising Cursor on RDMA{} + CMP2 nl{} \
         (fbcmem 8 units: RDMA3 {:#010x} -> {:#010x}, RDMA2 {:#010x} -> {:#010x})",
        nl0,
        rch,
        CURSOR_RDMA_ID,
        cursor_nl(),
        fbc3,
        primary_word,
        fbc2,
        cursor_word,
    );
    true
}

/// Copy the boot `nl0` (primary) layer to `nl7`, disable `nl0`, latch, and
/// move the cursor to `nl6`.
///
/// This is the vendor's layout (`spacemit_planes.c`: primary `hw_pid` 7,
/// cursor `hw_pid` 6). The composer stacks lower indices on top, so a
/// cursor can only sit above the primary if the primary leaves `nl0`.
/// Every write here is to composer registers only; RDMA3 and `rch_en`
/// are untouched.
fn relayout_vendor_z(dpu: &IoMem) -> bool {
    let nl0 = CMP2_BASE + cmp_nl_en(0);
    let nl7 = CMP2_BASE + cmp_nl_en(7);
    let mut words = [0u32; 5];
    for (i, w) in words.iter_mut().enumerate() {
        let Ok(v) = read(dpu, nl0 + [0x00, 0x10, 0x14, 0x18, 0x1c][i]) else {
            return false;
        };
        *w = v;
    }
    for (i, off) in [0x10usize, 0x14, 0x18, 0x1c].iter().enumerate() {
        if write(dpu, nl7 + off, words[i + 1]).is_err() {
            return false;
        }
    }
    if write(dpu, nl7, words[0]).is_err() {
        return false;
    }
    if write(dpu, nl0, 0).is_err() {
        // Put nl7 back off so two copies of the primary do not fight.
        let _ = write(dpu, nl7, 0);
        return false;
    }
    if super::k1_vblank::request_latch(dpu).is_err() {
        return false;
    }
    CURSOR_NL.store(6, Ordering::Relaxed);
    true
}

/// Applies [`CursorProgram`] to RDMA2 / CMP2 cursor `nl` / `rch_en`.
///
/// Never writes RDMA3, CMP2 `nl0`/`nl7` (primary), or any primary scanout
/// register. On `rch_en` readback losing the primary bits, restores the
/// previous word and marks the path dead.
///
/// Returns whether something was written (caller should `cfg_rdy`).
pub(super) fn apply(
    dpu: &IoMem,
    program: CursorProgram,
    previous: CursorProgram,
) -> DrmResult<bool> {
    if program == previous {
        return Ok(false);
    }

    match program {
        CursorProgram::Off => {
            disable(dpu)?;
            Ok(true)
        }
        CursorProgram::On(regs) => {
            if let Err(error) = enable(dpu, &regs) {
                let _ = disable(dpu);
                mark_dead();
                ostd::warn!(
                    "[k1-kms] cursor program failed ({:?}); disabled cursor layer, \
                     further cursor commits will be refused",
                    error
                );
                return Ok(true);
            }
            Ok(true)
        }
    }
}

fn enable(dpu: &IoMem, regs: &CursorRegs) -> Result<(), ()> {
    let base = rdma_base(CURSOR_RDMA_ID);
    write(dpu, base + RDMA_LAYER_CTRL, regs.layer_ctrl)?;
    write(dpu, base + RDMA_COMPSR_Y_OFST, regs.y_ofst)?;
    write(dpu, base + RDMA_ADDR_LO, regs.addr as u32)?;
    write(dpu, base + RDMA_ADDR_HI, (regs.addr >> 32) as u32)?;
    write(dpu, base + RDMA_STRIDE, regs.stride)?;
    write(
        dpu,
        base + RDMA_IMG_SIZE,
        pack_img_size(regs.img_w, regs.img_h),
    )?;
    write(dpu, base + RDMA_CROP_START, regs.crop_start)?;
    write(dpu, base + RDMA_CROP_END, regs.crop_end)?;
    write(dpu, base + RDMA_ROT_MODE, regs.rot_mode)?;

    let nl = CMP2_BASE + cmp_nl_en(cursor_nl());
    write(dpu, nl, regs.nl_en)?;
    write(dpu, nl + 0x10, regs.ltopx)?;
    write(dpu, nl + 0x14, regs.ltopy_rbotx)?;
    write(dpu, nl + 0x18, regs.rboty_blend)?;
    write(dpu, nl + 0x1c, regs.alpha)?;

    rmw_rch(dpu, true)?;

    match read(dpu, nl) {
        Ok(value) if value == regs.nl_en => Ok(()),
        Ok(value) => {
            ostd::warn!(
                "[k1-kms] cursor nl{} read back {:#x}, wrote {:#x}",
                cursor_nl(),
                value,
                regs.nl_en
            );
            Err(())
        }
        Err(()) => Err(()),
    }
}

fn disable(dpu: &IoMem) -> Result<(), DrmError> {
    let nl = CMP2_BASE + cmp_nl_en(cursor_nl());
    write(dpu, nl, 0).map_err(|()| DrmError::Io)?;
    rmw_rch(dpu, false).map_err(|()| DrmError::Io)?;
    Ok(())
}

/// Sets or clears only the cursor RDMA bit. Restores `old` if the primary
/// bits disappear after the write.
fn rmw_rch(dpu: &IoMem, enable_cursor: bool) -> Result<(), ()> {
    let old = read(dpu, CTL2_RCH_EN)?;
    if old & BOOT_RCH_EN != BOOT_RCH_EN {
        ostd::warn!(
            "[k1-kms] cursor rch_en {:#x} missing boot bits {:#x}; not touching it",
            old,
            BOOT_RCH_EN
        );
        return Err(());
    }
    let new = if enable_cursor {
        old | CTL2_RCH_CURSOR_BIT
    } else {
        old & !CTL2_RCH_CURSOR_BIT
    };
    if new == old {
        return Ok(());
    }
    write(dpu, CTL2_RCH_EN, new)?;
    let back = read(dpu, CTL2_RCH_EN)?;
    if back & BOOT_RCH_EN != BOOT_RCH_EN {
        let _ = write(dpu, CTL2_RCH_EN, old);
        ostd::warn!(
            "[k1-kms] cursor rch_en read back {:#x} after {:#x}; restored {:#x}",
            back,
            new,
            old
        );
        return Err(());
    }
    Ok(())
}

fn read(dpu: &IoMem, offset: usize) -> Result<u32, ()> {
    // Order earlier shadow writes before their validation readback.
    ostd::mm::dma::dma_io_fence();
    dpu.read_once(offset).map_err(|_| ())
}

fn write(dpu: &IoMem, offset: usize, value: u32) -> Result<(), ()> {
    dpu.write_once(offset, &value).map_err(|_| ())
}

#[cfg(ktest)]
mod test {
    use k1_musebook_drm::kms::PlaneType;
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn cursorplane_parses_the_documented_tokens() {
        assert_eq!("1".parse::<CursorPlaneKnob>(), Ok(CursorPlaneKnob::On));
        assert_eq!("on".parse::<CursorPlaneKnob>(), Ok(CursorPlaneKnob::On));
        assert_eq!("0".parse::<CursorPlaneKnob>(), Ok(CursorPlaneKnob::Off));
        assert_eq!("off".parse::<CursorPlaneKnob>(), Ok(CursorPlaneKnob::Off));
        assert!("2".parse::<CursorPlaneKnob>().is_err());
        assert!("".parse::<CursorPlaneKnob>().is_err());
    }

    #[ktest]
    fn unset_cmdline_does_not_advertise_a_cursor() {
        assert_eq!(knob(), CursorPlaneKnob::Off);
        assert!(!requested());
        assert_eq!(advertised_types(false), &[PlaneType::Primary]);
        assert_eq!(
            advertised_types(true),
            &[PlaneType::Primary, PlaneType::Cursor]
        );
    }

    #[ktest]
    fn rdma_and_composer_offsets_match_the_boot_path() {
        assert_eq!(rdma_base(3), 0xd80);
        assert_eq!(rdma_base(2), 0xc80);
        assert_eq!(rdma_base(3) + RDMA_ADDR_LO, 0xda0);
        assert_eq!(rdma_base(3) + RDMA_ADDR_HI, 0xda4);
        assert_eq!(rdma_base(3) + RDMA_STRIDE, 0xdb8);
        assert_eq!(rdma_base(3) + RDMA_IMG_SIZE, 0xdbc);
        assert_eq!(rdma_base(3) + RDMA_CROP_START, 0xdc0);
        assert_eq!(rdma_base(3) + RDMA_CROP_END, 0xdc4);
        assert_eq!(rdma_base(3) + RDMA_ROT_MODE, 0xdf0);
        assert_eq!(CMP2_BASE + CMP_NL0_EN, 0x4c38);
        assert_eq!(CMP2_BASE + cmp_nl_en(1), 0x4c58);
        assert_eq!(CMP2_BASE + cmp_nl_en(6), 0x4cf8);
        assert_eq!(CMP2_BASE + cmp_nl_en(7), 0x4d18);
        assert_eq!(BOOT_RCH_EN, 0x40008);
        assert_eq!(pack_nl_en(3), BOOT_NL0_EN);
        assert_eq!(pack_nl_en(2), 0x5);
        assert_eq!(CTL2_RCH_EN, 0x560);
        assert_eq!(VENDOR_LAYER_CTRL, 0x1e203c);
        assert_eq!((VENDOR_LAYER_CTRL >> 12) & 7, 2);
        assert_eq!((BOOT_LAYER_CTRL >> 12) & 7, 2);
        assert_eq!(VENDOR_LAYER_CTRL & 3, 0);
    }

    #[ktest]
    fn clip_keeps_an_on_screen_cursor() {
        let blit = clip_cursor((10, 20, 64, 64), (0, 0), (1920, 1080)).unwrap();
        assert_eq!(
            blit,
            CursorBlit {
                dest_x: 10,
                dest_y: 20,
                dest_w: 64,
                dest_h: 64,
                src_x: 0,
                src_y: 0,
            }
        );
    }

    #[ktest]
    fn clip_handles_negative_origin() {
        let blit = clip_cursor((-10, -5, 64, 64), (0, 0), (1920, 1080)).unwrap();
        assert_eq!(
            blit,
            CursorBlit {
                dest_x: 0,
                dest_y: 0,
                dest_w: 54,
                dest_h: 59,
                src_x: 10,
                src_y: 5,
            }
        );
    }

    #[ktest]
    fn clip_handles_right_and_bottom_overflow() {
        let blit = clip_cursor((1900, 1040, 64, 64), (0, 0), (1920, 1080)).unwrap();
        assert_eq!(
            blit,
            CursorBlit {
                dest_x: 1900,
                dest_y: 1040,
                dest_w: 20,
                dest_h: 40,
                src_x: 0,
                src_y: 0,
            }
        );
    }

    #[ktest]
    fn clip_rejects_a_fully_off_screen_cursor() {
        assert!(clip_cursor((-80, 10, 64, 64), (0, 0), (1920, 1080)).is_none());
        assert!(clip_cursor((1920, 0, 64, 64), (0, 0), (1920, 1080)).is_none());
        assert!(clip_cursor((0, 1080, 64, 64), (0, 0), (1920, 1080)).is_none());
        assert!(clip_cursor((0, 0, 0, 64), (0, 0), (1920, 1080)).is_none());
    }

    #[ktest]
    fn plan_packs_an_argb_cursor_on_the_right_rdma() {
        let program = plan_cursor(
            true,
            (100, 50, 64, 64),
            (0, 0),
            CursorBuffer {
                address: 0x1_0000_2000,
                stride: 256,
                pixel_format: fourcc::ARGB8888,
                size: (64, 64),
            },
            (1920, 1080),
        )
        .unwrap();
        let CursorProgram::On(regs) = program else {
            panic!("expected On");
        };
        assert_eq!(regs.addr, 0x1_0000_2000);
        assert_eq!(regs.stride, 256);
        assert_eq!(regs.layer_ctrl, VENDOR_LAYER_CTRL);
        assert_eq!(regs.rot_mode, RDMA_FMT_ARGB8888);
        assert_eq!(regs.nl_en, pack_nl_en(CURSOR_RDMA_ID));
        assert_eq!(regs.ltopx, pack_ltopx(100));
        assert_eq!(regs.ltopy_rbotx, pack_ltopy_rbotx(50, 163));
        assert_eq!(regs.rboty_blend, pack_rboty_blend(113, true));
        assert_eq!(regs.crop_start, 0);
        assert_eq!(regs.crop_end, pack_crop_end(0, 0, 64, 64));
        assert_eq!(regs.alpha, pack_alpha());
        assert_eq!(pack_alpha(), 0x00ff_0000);
    }

    #[ktest]
    fn plan_turns_a_fully_off_screen_cursor_into_off() {
        let program = plan_cursor(
            true,
            (-100, 0, 64, 64),
            (0, 0),
            CursorBuffer {
                address: 0x1000,
                stride: 256,
                pixel_format: fourcc::ARGB8888,
                size: (64, 64),
            },
            (1920, 1080),
        )
        .unwrap();
        assert_eq!(program, CursorProgram::Off);
    }

    #[ktest]
    fn plan_rejects_unknown_formats_and_huge_cursors() {
        assert_eq!(
            plan_cursor(
                true,
                (0, 0, 64, 64),
                (0, 0),
                CursorBuffer {
                    address: 0x1000,
                    stride: 256,
                    pixel_format: 0x2020_384e, // NV12
                    size: (64, 64)
                },
                (1920, 1080)
            ),
            Err(CursorReject::Format)
        );
        assert_eq!(
            plan_cursor(
                true,
                (0, 0, 512, 64),
                (0, 0),
                CursorBuffer {
                    address: 0x1000,
                    stride: 2048,
                    pixel_format: fourcc::ARGB8888,
                    size: (512, 64)
                },
                (1920, 1080)
            ),
            Err(CursorReject::TooBig)
        );
        assert_eq!(
            plan_cursor(
                false,
                (0, 0, 64, 64),
                (0, 0),
                CursorBuffer {
                    address: 0x1000,
                    stride: 256,
                    pixel_format: fourcc::ARGB8888,
                    size: (64, 64)
                },
                (1920, 1080)
            ),
            Ok(CursorProgram::Off)
        );
    }

    #[ktest]
    fn cursor_limit_matches_the_fixed_line_buffer() {
        assert_eq!(CURSOR_MAX, 64);
        for width in [65, 128, 256] {
            assert_eq!(
                plan_cursor(
                    true,
                    (0, 0, width, 64),
                    (0, 0),
                    CursorBuffer {
                        address: 0x1000,
                        stride: width * 4,
                        pixel_format: fourcc::ARGB8888,
                        size: (width, 64)
                    },
                    (1920, 1080)
                ),
                Err(CursorReject::TooBig)
            );
        }
    }

    #[ktest]
    fn fbcmem_windows_do_not_overlap() {
        let (cursor_word, primary_word) = fbcmem_windows(FBCMEM_CURSOR_UNITS);
        assert_eq!(cursor_word, 0x1000_0008);
        assert_eq!(primary_word, 0x0008_0438);
        let cur_start = (cursor_word >> 16) & 0xfff;
        let cur_size = cursor_word & 0xffff;
        let pri_start = (primary_word >> 16) & 0xfff;
        let pri_size = primary_word & 0xffff;
        assert_eq!(cur_start + cur_size, pri_start);
        assert_eq!(pri_start + pri_size, FBCMEM_POOL_UNITS);
        assert_eq!(cursor_word >> 28, 1);
        assert_eq!("0x7800".parse::<Hex>(), Ok(Hex(0x7800)));
        assert_eq!("120".parse::<Hex>(), Ok(Hex(120)));
    }

    #[ktest]
    fn xrgb_has_no_pixel_alpha_in_the_blend_word() {
        assert_eq!(pack_rboty_blend(63, false), 63);
        assert_eq!(pack_rboty_blend(63, true), 63 | (1 << 16) | (1 << 18));
        assert_eq!(rdma_format(fourcc::XRGB8888), Some(RDMA_FMT_XRGB8888));
        assert_eq!(rdma_format(fourcc::ARGB8888), Some(RDMA_FMT_ARGB8888));
        assert!(rdma_format(0).is_none());
    }
}
