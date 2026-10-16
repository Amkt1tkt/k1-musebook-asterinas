// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: DPU → MIPI-DSI → LT8911 eDP bring-up and PWM14 backlight.

//! SpacemiT K1 internal eDP display bring-up (Muse Book panel).
//!
//! Pipeline: DPU -> MIPI-DSI -> LT8911EXB bridge -> eDP, fixed 1920x1080
//! panel timing (`lcd_lt8911ext_edp_1080p.c`). Ported from U-Boot:
//! - `drivers/power/domain/k1x-power-domain.c` (`K1X_PMU_LCD_PWR_DOMAIN`)
//! - `drivers/video/spacemit/dsi/drv/spacemit_dsi_drv.c` (`dsi_reset`,
//!   `spacemit_dsi_open`, `dsi_config_video_mode`,
//!   `spacemit_dsi_ready_for_datatx`)
//! - `drivers/video/spacemit/dsi/drv/spacemit_dphy.c` (`spacemit_dphy_init`)
//! - `drivers/video/spacemit/dsi/video/lcd/lcd_lt8911ext_edp_1080p.c`
//!   (`lt8911ext_edp_1080p_mipi_info`)
//! - `drivers/video/spacemit/spacemit_dpu.c` (`dsi_dpu_init`)
//! - `drivers/pwm/pwm-pxa.c` (PWM14 backlight, `k1x,pwm-disable-fd`)
//!
//! LT8911EXB I2C bring-up lives in `k1-musebook-i2c` (`lt8911_enable`) so
//! this crate does not own the bridge. The FB lives in DRAM (`DmaCoherent`,
//! non-coherent device → uncached CPU mapping) and is registered with
//! [`crate::framebuffer::FRAMEBUFFER`] for the VT console.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use k1_musebook_pinctrl::{MuxMode, write_pad};
use ostd::{
    arch::{boot::DEVICE_TREE, ccu, read_tsc, tsc_freq},
    io::IoMem,
    mm::{HasDaddr, HasPaddr, HasSize, PAGE_SIZE, VmIoOnce, dma::DmaCoherent},
    sync::{LocalIrqDisabled, SpinLock},
};
use spin::Once;

use crate::{
    framebuffer::{FRAMEBUFFER, FrameBuffer},
    pixel::PixelFormat,
    sysfs,
};

const DPU_BASE: usize = 0xc034_0000;
const DPU_SIZE: usize = 0x2a000;
const DSI_BASE: usize = 0xd421_a800;
const DSI_SIZE: usize = 0x200;

/// LCD power domain control (`K1X_PMU_LCD_PWR_DOMAIN.reg_pwr_ctrl` = `0x380`),
/// relative to the CCU's already-claimed APMU window — accessed via
/// [`ccu::apmu_read`]/[`ccu::apmu_rmw`] rather than a second `IoMem::acquire`
/// (`IoMem` ranges cannot overlap; this offset is inside `ccu`'s `0x3cc`
/// window).
const LCD_PWR_OFF: usize = 0x380;
const APMU_POWER_STATUS_OFF: usize = 0xf0;
const PD_BIT_AUTO_PWR_ON: u32 = 1 << 0;
const PD_BIT_HW_MODE: u32 = 1 << 4;
const PD_BIT_PWR_STAT: u32 = 1 << 4;
const PD_BIT_HW_PWR_STAT: u32 = 1 << 12;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const BPP: usize = 4;
const STRIDE: usize = WIDTH * BPP;
const FB_BYTES: usize = STRIDE * HEIGHT;
const OUTFMT_RGB888: u32 = 2;

// eDP panel timing (`lt8911ext_edp_1080p_mipi_info` / vendor DT): hfp48/
// hbp200/hsync32/vfp3/vbp31/vsync6, htotal=2200. Must stay in sync with the
// `k1_lt8911` MIPI_* constants.
const HFP: u32 = 48;
const HBP: u32 = 200;
const HSYNC: u32 = 32;
const VFP: u32 = 3;
const VBP: u32 = 31;
const VSYNC: u32 = 6;

/// The panel's blanking intervals, for the KMS wrapper in [`super::k1_kms`].
///
/// The same numbers as the constants above, re-exported rather than copied: the
/// mode reported to user space and the mode programmed into the DPU, the D-PHY and
/// the bridge must not be able to drift apart.
pub(super) struct PanelTiming {
    /// Horizontal front porch.
    pub hfp: u16,
    /// Horizontal back porch.
    pub hbp: u16,
    /// Horizontal sync width.
    pub hsync: u16,
    /// Vertical front porch.
    pub vfp: u16,
    /// Vertical back porch.
    pub vbp: u16,
    /// Vertical sync width.
    pub vsync: u16,
}

/// The Muse Book panel's timings.
pub(super) const PANEL_TIMING: PanelTiming = PanelTiming {
    hfp: HFP as u16,
    hbp: HBP as u16,
    hsync: HSYNC as u16,
    vfp: VFP as u16,
    vbp: VBP as u16,
    vsync: VSYNC as u16,
};

const DSI_LANE_NUMBER: u32 = 4;
const DSI_BPP_RGB888: u32 = 24;
const DSI_PHY_ESC_CLOCK: u32 = 51_200_000;

const PWM14_BASE: usize = 0xd402_1800;
const PWM14_SIZE: usize = 0x10;
const PWM14_GPIO: u32 = 44;
const PWMDCR: usize = 0x04;

struct Pwm14 {
    io: IoMem,
    max_duty: u32,
}

static PWM14: Once<Pwm14> = Once::new();
static BRIGHTNESS: AtomicU32 = AtomicU32::new(sysfs::MAX_BRIGHTNESS);
static BL_POWER: AtomicU32 = AtomicU32::new(sysfs::FB_BLANK_UNBLANK);
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Serializes the state update and its PWM write; getters remain lock-free.
static PWM_UPDATE: SpinLock<(), LocalIrqDisabled> = SpinLock::new(());
/// Linux `K1X_PADCONF(GPIO_44, MUX_MODE4, EDGE_NONE|PULL_UP|PAD_1V8_DS2)`
/// (`pinctrl_pwm14_1` in `k1-x_pinctrl.dtsi`).
const MFPR_PWM14: u32 = 0x0000_d044;

pub(super) fn init() {
    let Some(dt) = DEVICE_TREE.get() else {
        return;
    };
    if dt
        .find_compatible(&["spacemit,k1-x", "spacemit,k1"])
        .is_none()
    {
        return;
    }

    ostd::info!("Muse Book internal eDP panel (1920x1080) bring-up");
    if !power_on_lcd() {
        ostd::warn!("LCD power domain failed");
        return;
    }

    let (pxclk_hz, bitclk_hz) = ccu::enable_lcd_dsi();
    if pxclk_hz == 0 {
        ostd::warn!("LCD/DSI clock bring-up failed");
        return;
    }
    delay_us(100);

    let htotal = WIDTH as u32 + HFP + HBP + HSYNC;
    let vtotal = HEIGHT as u32 + VFP + VBP + VSYNC;
    let refresh_x100 = (u64::from(pxclk_hz) * 100) / (u64::from(htotal) * u64::from(vtotal));
    ostd::info!(
        "htotal={} vtotal={} pxclk={}Hz bitclk={}Hz ~{}.{:02}Hz",
        htotal,
        vtotal,
        pxclk_hz,
        bitclk_hz,
        refresh_x100 / 100,
        refresh_x100 % 100
    );

    let Ok(dpu) = IoMem::acquire(DPU_BASE..DPU_BASE + DPU_SIZE) else {
        ostd::warn!("failed to map DPU at {:#x}", DPU_BASE);
        return;
    };
    let Ok(dsi) = IoMem::acquire(DSI_BASE..DSI_BASE + DSI_SIZE) else {
        ostd::warn!("failed to map DSI at {:#x}", DSI_BASE);
        return;
    };

    let nframes = FB_BYTES.div_ceil(PAGE_SIZE);
    // Non-coherent device: CPU mapping is uncached so DPU sees writes immediately
    // (no sync_to_device), which the VT console needs for per-glyph updates.
    let Ok(fb) = DmaCoherent::alloc(nframes, false) else {
        ostd::warn!("FB DMA alloc failed ({} pages)", nframes);
        return;
    };
    if fb.daddr() >= ostd::mm::dma_addr_limit() {
        ostd::warn!("FB above DMA limit, unsupported");
        return;
    }
    let fb_pa = fb.paddr();
    ostd::info!(
        "FB pa={:#x} daddr={:#x} size={:#x} pages={}",
        fb_pa,
        fb.daddr(),
        fb.size(),
        nframes
    );

    dsi_open(&dsi, bitclk_hz);
    dsi_config_video_mode(&dsi);

    ostd::mm::dma::warn_if_addr_unreachable(fb_pa, fb.size());
    dsi_dpu_init(&dpu, fb_pa as u64);

    // Give MIPI HS video a moment to stabilize before LT8911 PCR / link train.
    delay_us(50_000);

    let ok = k1_musebook_i2c::lt8911_enable(pxclk_hz / 10_000);
    if ok {
        ostd::info!("LT8911EXB eDP bring-up OK");
    } else {
        ostd::warn!("LT8911EXB eDP bring-up reported failure");
    }

    // Backlight after bridge (matches U-Boot: LT8911 then PWM/`lcd_bl_enable`).
    pwm14_backlight_init();

    // Register for VT / FramebufferConsole even if link-train reported failure —
    // the panel may still be usable; clear happens inside `register()`.
    let framebuffer = FrameBuffer::from_dma(fb, WIDTH, HEIGHT, STRIDE, PixelFormat::BgrReserved);
    let registered = framebuffer.register();
    if registered {
        ostd::info!(
            "registered FRAMEBUFFER {}x{} stride={} fmt=BgrReserved",
            WIDTH,
            HEIGHT,
            STRIDE
        );
        if let Some(fb) = FRAMEBUFFER.get() {
            super::k1_splash::show(fb);
        }
    } else {
        ostd::warn!("FRAMEBUFFER already registered; skipped");
    }

    // Hand the programmed pipeline to the KMS wrapper. The DPU window moves
    // there because the page-flip path needs it. Only when this driver owns
    // the global framebuffer, since that is the buffer the DPU was pointed at.
    if let Some(scanout) = FRAMEBUFFER.get().filter(|_| registered) {
        super::k1_kms::register(dpu, scanout.clone(), pxclk_hz);
    }

    ostd::info!("eDP bring-up done (lt8911_ok={})", ok);
}

/// `K1X_PMU_LCD_PWR_DOMAIN` (`use_hw = 1`): set `bit_auto_pwr_on`(0) +
/// `bit_hw_mode`(4), poll `bit_hw_pwr_stat`(12) in `APMU_POWER_STATUS_REG`.
fn power_on_lcd() -> bool {
    if let Some(sts) = ccu::apmu_read(APMU_POWER_STATUS_OFF)
        && sts & PD_BIT_PWR_STAT != 0
    {
        ostd::info!("LCD power domain already on (status={:#010x})", sts);
        return true;
    }

    let Some(next) = ccu::apmu_rmw(LCD_PWR_OFF, 0, PD_BIT_AUTO_PWR_ON | PD_BIT_HW_MODE) else {
        ostd::warn!("failed to access LCD_PWR (CCU not initialized?)");
        return false;
    };
    ostd::info!("LCD_PWR (APMU+{:#x}) -> {:#010x}", LCD_PWR_OFF, next);
    delay_us(310);

    for _ in 0..10_000 {
        if let Some(sts) = ccu::apmu_read(APMU_POWER_STATUS_OFF)
            && sts & PD_BIT_HW_PWR_STAT != 0
        {
            ostd::info!("LCD power domain on (status={:#010x})", sts);
            return true;
        }
        delay_us(6);
    }
    let sts = ccu::apmu_read(APMU_POWER_STATUS_OFF).unwrap_or(0);
    ostd::warn!("LCD power domain timeout (status={:#010x})", sts);
    false
}

// ---------------------------------------------------------------------------
// MIPI-DSI (`spacemit_dsi_drv.c` / `spacemit_dphy.c`), DSI_REG_BASE relative.

const DSI_CTRL_0: usize = 0x0;
const DSI_CTRL_1: usize = 0x4;
const DSI_CPU_CMD_1: usize = 0x24;
const DSI_LCD_BDG_CTRL0: usize = 0x84;
const DSI_VPN_CTRL_0: usize = 0x100;
const DSI_VPN_CTRL_1: usize = 0x104;
const DSI_VPN_TIMING_0: usize = 0x110;
const DSI_VPN_TIMING_1: usize = 0x114;
const DSI_VPN_TIMING_2: usize = 0x118;
const DSI_VPN_TIMING_3: usize = 0x11c;
const DSI_VPN_WC_0: usize = 0x120;
const DSI_VPN_WC_1: usize = 0x124;
const DSI_VPN_WC_2: usize = 0x128;
const DSI_VPN_SLOT_CNT_0: usize = 0x130;
const DSI_VPN_SLOT_CNT_1: usize = 0x134;
const DSI_PHY_CTRL_1: usize = 0x184;
const DSI_PHY_CTRL_2: usize = 0x188;
const DSI_PHY_TIME_0: usize = 0x1c0;
const DSI_PHY_TIME_1: usize = 0x1c4;
const DSI_PHY_TIME_2: usize = 0x1c8;
const DSI_PHY_TIME_3: usize = 0x1cc;
const DSI_PHY_ANA_PWR_CTRL: usize = 0x1e0;

/// `dsi_reset`: soft-reset the whole DSI module, then release.
fn dsi_reset(dsi: &IoMem) {
    const CFG_SOFT_RST: u32 = 1 << 31;
    const CFG_SOFT_RST_REG: u32 = 1 << 30;
    const CFG_CLR_PHY_FIFO: u32 = 1 << 29;
    const CFG_RST_TXLP: u32 = 1 << 28;
    const CFG_RST_CPU: u32 = 1 << 27;
    const CFG_RST_CPN: u32 = 1 << 26;
    const CFG_RST_VPN: u32 = 1 << 24;
    const CFG_DSI_PHY_RST: u32 = 1 << 23;
    let reg = CFG_SOFT_RST
        | CFG_SOFT_RST_REG
        | CFG_CLR_PHY_FIFO
        | CFG_RST_TXLP
        | CFG_RST_CPU
        | CFG_RST_CPN
        | CFG_RST_VPN
        | CFG_DSI_PHY_RST;
    w(dsi, DSI_CTRL_0, reg);
    delay_us(100);
    w(dsi, DSI_CTRL_0, 0);
}

/// D-PHY timing register values (`dphy_set_timing`), computed from the
/// panel's bit/esc clocks. Matches the U-Boot integer-truncating arithmetic
/// exactly (verified by hand against the vendor "brownstone" fallback
/// comments for `PHY_TIME_0`/`PHY_TIME_2`, which are clock-independent in
/// this range: `0x06010603` / `0x06040c04`).
fn dphy_compute_timing(bit_clk_hz: u32, esc_clk_hz: u32) -> (u32, u32, u32, u32) {
    // Defaults from U-Boot `dsi_get_dphy_setting` (`spacemit_dsi_drv.c`).
    const HS_PREP_CONSTANT: i64 = 40;
    const HS_PREP_UI: i64 = 4;
    const HS_ZERO_CONSTANT: i64 = 145;
    const HS_ZERO_UI: i64 = 10;
    const HS_TRAIL_CONSTANT: i64 = 60;
    const HS_TRAIL_UI: i64 = 4;
    const HS_EXIT_CONSTANT: i64 = 100;
    const HS_EXIT_UI: i64 = 0;
    const CK_ZERO_CONSTANT: i64 = 300;
    const CK_TRAIL_CONSTANT: i64 = 60;
    const REQ_READY: i64 = 0x3c;
    const WAKEUP_CONSTANT: i64 = 1_000_000;
    const LPX_CONSTANT: i64 = 60;

    let esc_clk = (esc_clk_hz as i64) / 1000 / 1000;
    let esc_clk_t = 1000 / esc_clk;
    let bit_clk = (bit_clk_hz as i64) / 1000 / 1000;
    let ui = 1000 / bit_clk + 1;

    let lpx_clk = LPX_CONSTANT / esc_clk_t + 1;
    let lpx_time = lpx_clk * esc_clk_t;
    let ta_get = lpx_time * 5 / esc_clk_t - 1;
    let ta_go = lpx_time * 4 / esc_clk_t - 1;
    let wakeup = WAKEUP_CONSTANT / esc_clk_t + 1;

    let hs_prep = (HS_PREP_CONSTANT + HS_PREP_UI * ui) / esc_clk_t + 1;

    let hs_zero_pre = HS_ZERO_CONSTANT + HS_ZERO_UI * ui - (hs_prep + 1) * esc_clk_t;
    let hs_zero = ((hs_zero_pre - ((3 * ui) << 3)) / esc_clk_t + 4).max(0);

    let hs_trail_pre = HS_TRAIL_CONSTANT + HS_TRAIL_UI * ui;
    let hs_trail_pre = (8 * ui).max(hs_trail_pre);
    let hs_trail_raw = hs_trail_pre / esc_clk_t + 1;
    let hs_trail = if hs_trail_raw > 3 {
        hs_trail_raw - 3
    } else {
        0
    };

    let hs_exit = (HS_EXIT_CONSTANT + HS_EXIT_UI * ui) / esc_clk_t + 1;

    let ck_zero = (CK_ZERO_CONSTANT - (hs_prep + 1) * esc_clk_t) / esc_clk_t + 1;
    let ck_trail = CK_TRAIL_CONSTANT / esc_clk_t + 1;
    let ck_exit = hs_exit;

    let t0 = ((hs_exit as u32) << 24)
        | ((hs_trail as u32) << 16)
        | ((hs_zero as u32) << 8)
        | (hs_prep as u32);
    let t1 = ((ta_get as u32) << 24) | ((ta_go as u32) << 16) | (wakeup as u32);
    let t2 = ((ck_exit as u32) << 24)
        | ((ck_trail as u32) << 16)
        | ((ck_zero as u32) << 8)
        | (lpx_clk as u32);
    let t3 = ((lpx_clk as u32) << 8) | (REQ_READY as u32);
    (t0, t1, t2, t3)
}

/// `spacemit_dphy_init`: analog power on, continuous HS clock, timing, lanes.
fn dphy_init(dsi: &IoMem, bit_clk_hz: u32) {
    set_bits(dsi, DSI_PHY_ANA_PWR_CTRL, 1 << 8); // CFG_DPHY_ANA_RESET
    set_bits(dsi, DSI_PHY_ANA_PWR_CTRL, 1 << 0); // CFG_DPHY_ANA_PU

    set_bits(dsi, DSI_PHY_CTRL_1, 1 << 0); // CFG_DPHY_CONT_CLK
    set_bits(dsi, DSI_PHY_CTRL_1, 1 << 17); // CFG_DPHY_ADD_VALID
    set_bits(dsi, DSI_PHY_CTRL_1, 1 << 16); // CFG_DPHY_VDD_VALID

    let (t0, t1, t2, t3) = dphy_compute_timing(bit_clk_hz, DSI_PHY_ESC_CLOCK);
    w(dsi, DSI_PHY_TIME_0, t0);
    w(dsi, DSI_PHY_TIME_1, t1);
    w(dsi, DSI_PHY_TIME_2, t2);
    w(dsi, DSI_PHY_TIME_3, t3);

    // 4 lanes -> `spacemit_dphy_lane[4]` = 0xf, `CFG_DPHY_LANE_EN_SHIFT` = 4.
    rmw(dsi, DSI_PHY_CTRL_2, 0xf << 4, 0xf << 4);

    delay_us(1000); // mdelay(1)
}

/// `spacemit_dsi_open(ready=false)`: reset, D-PHY init, split/LPTX/EOTP.
fn dsi_open(dsi: &IoMem, bit_clk_hz: u32) {
    dsi_reset(dsi);
    dphy_init(dsi, bit_clk_hz);

    clear_bits(dsi, DSI_LCD_BDG_CTRL0, 1 << 0); // dsi_enable_split_mode(false)
    // dsi_enable_lptx_lanes(spacemit_dsi_lane[4] = 0xf): CFG_TXLP_LPDT_SHIFT=20.
    rmw(dsi, DSI_CPU_CMD_1, 0xf << 20, 0xf << 20);
    set_bits(dsi, DSI_CTRL_1, 1 << 8); // dsi_enable_eotp(true): CFG_EOTP_EN
}

/// `dsi_config_video_mode` for the eDP `mipi_info` (burst mode, RGB888,
/// 4 lanes, no split). Ends with cmd-mode off / video-mode on, matching
/// `spacemit_dsi_ready_for_datatx` for `SPACEMIT_DSI_MODE_VIDEO`.
fn dsi_config_video_mode(dsi: &IoMem) {
    const LGP_OVERHEAD: u32 = 6;
    const HSS_BCNT: u32 = 4;

    let v_total = HEIGHT as u32 + VFP + VBP + VSYNC;
    let to_bcnt = |px: u32| (px * DSI_BPP_RGB888) >> 3;

    let hact_b = to_bcnt(WIDTH as u32);
    let hfp_b = to_bcnt(HFP);
    let hbp_b = to_bcnt(HBP);
    let hsync_b = to_bcnt(HSYNC);
    let hex_b = 0u32;
    let httl_b = hact_b + hsync_b + hfp_b + hbp_b + hex_b;
    let slot_cnt = (httl_b - hact_b) / DSI_LANE_NUMBER + 3;

    let hact = hact_b / DSI_LANE_NUMBER;
    let hbp = hbp_b / DSI_LANE_NUMBER;
    let hsync = hsync_b / DSI_LANE_NUMBER;
    let httl = (hact_b + hfp_b + hbp_b + hsync_b) / DSI_LANE_NUMBER;

    // DSI_BURST_MODE_BURST (not NON_BURST_SYNC_PULSE):
    let hsa_wc = 0u32;
    let hbp_wc = hsync_b + hbp_b - HSS_BCNT - LGP_OVERHEAD;
    let hfp_wc = hfp_b + hex_b - LGP_OVERHEAD - LGP_OVERHEAD; // burst && !hex_en
    let hact_wc = (WIDTH as u32 * DSI_BPP_RGB888) >> 3;
    let hex_wc = 0u32;
    let hlp_wc = httl_b - HSS_BCNT - LGP_OVERHEAD;

    w(dsi, DSI_VPN_CTRL_0, (0x50 << 16) | 0xc08); // lane_number != 1
    w(dsi, DSI_VPN_TIMING_0, (hact << 16) | httl);
    w(dsi, DSI_VPN_TIMING_1, (hsync << 16) | hbp);
    w(dsi, DSI_VPN_TIMING_2, ((HEIGHT as u32) << 16) | v_total);
    w(dsi, DSI_VPN_TIMING_3, (VSYNC << 16) | VBP);
    w(dsi, DSI_VPN_WC_0, (hbp_wc << 16) | hsa_wc);
    w(dsi, DSI_VPN_WC_1, (hfp_wc << 16) | hact_wc);
    w(dsi, DSI_VPN_WC_2, (hex_wc << 16) | hlp_wc);
    w(dsi, DSI_VPN_SLOT_CNT_0, (slot_cnt << 16) | slot_cnt);
    w(dsi, DSI_VPN_SLOT_CNT_1, (slot_cnt << 16) | slot_cnt);

    // Advanced-setting defaults (`dsi_get_advanced_setting`) + burst_mode(2) +
    // rgb_mode(DSI_INPUT_DATA_RGB_MODE_888 = 3): only vsync_rst_en, hact_wc_en
    // and hbp_pkt_en are set among the packet-enable bits for burst mode.
    const VSYNC_RST_EN: u32 = 1 << 31;
    const HACT_WC_EN: u32 = 1 << 26;
    const HBP_PKT_EN: u32 = 1 << 18;
    const BURST_MODE: u32 = 2 << 2;
    const RGB_MODE_888: u32 = 3;
    w(
        dsi,
        DSI_VPN_CTRL_1,
        VSYNC_RST_EN | HACT_WC_EN | HBP_PKT_EN | BURST_MODE | RGB_MODE_888,
    );

    rmw(dsi, DSI_LCD_BDG_CTRL0, 0xfff << 16, 0); // CFG_VPN_FIFO_AFULL_CNT = 0
    set_bits(dsi, DSI_LCD_BDG_CTRL0, 1 << 6); // CFG_VPN_FIFO_AFULL_BYPASS
    set_bits(dsi, DSI_LCD_BDG_CTRL0, 1 << 1); // CFG_PIXEL_SWAP

    clear_bits(dsi, DSI_CTRL_0, 1 << 2); // dsi_enable_cmd_mode(false)
    set_bits(dsi, DSI_CTRL_0, (1 << 8) | (1 << 4) | (1 << 0)); // dsi_enable_video_mode(true)
}

// ---------------------------------------------------------------------------
// DPU (`spacemit_dpu.c` `dsi_dpu_init`).

fn dsi_dpu_init(dpu: &IoMem, fbbase: u64) {
    let hsync = HSYNC;
    let vsync = VSYNC;
    let hbp = HBP;
    let hfp = HFP;
    let vbp = VBP;
    let vfp = VFP;
    let hsp: u32 = 1; // hsync_invert = 0
    let vsp: u32 = 1;
    let xres = WIDTH as u32;
    let yres = HEIGHT as u32;

    w(dpu, 0xa1c, 0x2223);
    w(dpu, 0x18000, (yres << 16) | xres);
    w(dpu, 0x18018, 0x20);
    w(dpu, 0x1807c, 0x100);
    w(dpu, 0x18080, (hbp << 16) | hfp);
    w(dpu, 0x18084, (vbp << 16) | vfp);
    w(
        dpu,
        0x18088,
        (vsp << 28) | (vsync << 16) | (hsp << 12) | hsync,
    );
    w(dpu, 0x1808c, (yres << 16) | xres);
    w(dpu, 0x18090, OUTFMT_RGB888);
    w(dpu, 0xd80, 0x202040);
    w(dpu, 0xda0, fbbase as u32);
    w(dpu, 0xda4, (fbbase >> 32) as u32);
    w(dpu, 0xdb8, xres * 4);
    w(dpu, 0xdbc, (yres << 16) | xres);
    w(dpu, 0xdc0, 0x0);
    w(dpu, 0xdc4, ((yres - 1) << 16) | (xres - 1));
    w(dpu, 0xdf0, 0x4);

    w(dpu, 0x4c00, (xres << 8) | 0x01);
    w(dpu, 0x4c04, yres);
    w(dpu, 0x4c10, 0xff0000);
    w(dpu, 0x4c14, 0xff);
    w(dpu, 0x4c38, 0x7);
    w(dpu, 0x4c48, 0x0);
    w(dpu, 0x4c4c, (xres - 1) << 16);
    w(dpu, 0x4c50, yres - 1);
    w(dpu, 0x4c54, 0xff0000);

    w(dpu, 0x560, 0x40008);
    w(dpu, 0x588, 0x821);
    super::k1_vblank::request_latch(dpu).unwrap();
    w(dpu, 0x58c, 0x1);
}

// ---------------------------------------------------------------------------
// PWM14 backlight (`pwm-pxa.c`, `k1-x_MUSE-Book.dts` `&backlight`).

fn pwm14_backlight_init() {
    if write_pad(PWM14_GPIO, MFPR_PWM14).is_err() {
        ostd::warn!("PWM14 pinctrl GPIO{} failed", PWM14_GPIO);
        return;
    }
    let _ = k1_musebook_pinctrl::set_mux(PWM14_GPIO, MuxMode::Mode4);

    ccu::enable(ccu::Periph::Pwm14);
    delay_us(10);

    let Ok(pwm) = IoMem::acquire(PWM14_BASE..PWM14_BASE + PWM14_SIZE) else {
        ostd::warn!("failed to map PWM14 at {:#x}", PWM14_BASE);
        return;
    };

    // `pxa_pwm_config`: period_cycles = clk_rate * period_ns / 1e9;
    // prescale = (period_cycles-1)/1024; pv = period_cycles/(prescale+1)-1.
    // Default mux after reset selects `pwm_parent_names[0]` =
    // `pll1_d192_12p8` (12.8 MHz, `ccu-spacemit-k1x.c`). Target period
    // ~2000us, full duty. The DT sets `k1x,pwm-disable-fd`, so the `dc =
    // PWMDCR_FD` shortcut is unavailable; clamp `dc`/`pv` just under
    // `PWMDCR_FD` (bit10) like the vendor driver does for 100% duty.
    const PWM_CLK_HZ: u64 = 12_800_000;
    const PERIOD_NS: u64 = 2_000_000;
    const PWMCR_SD: u32 = 1 << 6;
    const PWMDCR_FD: u32 = 1 << 10;

    let period_cycles = (PWM_CLK_HZ * PERIOD_NS / 1_000_000_000).max(1);
    let prescale = ((period_cycles - 1) / 1024) as u32;
    let pv = (period_cycles / (prescale as u64 + 1) - 1) as u32;
    let mut dc = pv + 1;
    let mut pv_reg = pv;
    if dc >= PWMDCR_FD {
        dc = PWMDCR_FD - 1;
        pv_reg = dc - 1;
    }

    w(&pwm, 0x00, prescale | PWMCR_SD); // PWMCR
    w(&pwm, 0x04, dc); // PWMDCR
    w(&pwm, 0x08, pv_reg); // PWMPCR
    ostd::info!(
        "PWM14 backlight: prescale={} pv={} dc={} (period~{}us, full duty)",
        prescale,
        pv_reg,
        dc,
        PERIOD_NS / 1000
    );

    // Keep the window so sysfs can change duty without touching period.
    // Software brightness starts at 255 so a later `echo 255` is a no-op
    // on the register (same `dc` as above).
    BRIGHTNESS.store(sysfs::MAX_BRIGHTNESS, Ordering::Relaxed);
    BL_POWER.store(sysfs::FB_BLANK_UNBLANK, Ordering::Relaxed);
    PWM14.call_once(|| Pwm14 {
        io: pwm,
        max_duty: dc,
    });
    PRESENT.store(true, Ordering::Release);
}

pub(crate) fn backlight_present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

pub(crate) fn backlight_brightness() -> u32 {
    BRIGHTNESS.load(Ordering::Relaxed)
}

pub(crate) fn backlight_bl_power() -> u32 {
    BL_POWER.load(Ordering::Relaxed)
}

pub(crate) fn set_backlight_brightness(value: u32) -> Result<(), crate::BacklightError> {
    if value > sysfs::MAX_BRIGHTNESS {
        return Err(crate::BacklightError::BrightnessOutOfRange);
    }
    let _guard = PWM_UPDATE.lock();
    BRIGHTNESS.store(value, Ordering::Relaxed);
    apply_pwm();
    Ok(())
}

pub(crate) fn set_backlight_bl_power(value: u32) -> Result<(), crate::BacklightError> {
    let _guard = PWM_UPDATE.lock();
    BL_POWER.store(value, Ordering::Relaxed);
    apply_pwm();
    Ok(())
}

fn apply_pwm() {
    let Some(pwm) = PWM14.get() else {
        return;
    };
    let brightness = sysfs::effective_brightness(
        BRIGHTNESS.load(Ordering::Relaxed),
        BL_POWER.load(Ordering::Relaxed),
    );
    let dc = sysfs::brightness_to_duty(brightness, sysfs::MAX_BRIGHTNESS, pwm.max_duty);
    w(&pwm.io, PWMDCR, dc);
    // Publish the register image before another CPU can update the state.
    ostd::mm::dma::dma_io_fence();
}

// ---------------------------------------------------------------------------
// Helpers.

fn w(io: &IoMem, off: usize, val: u32) {
    ostd::mm::dma::dma_io_fence();
    io.write_once(off, &val).unwrap();
}

fn rmw(io: &IoMem, off: usize, mask: u32, set: u32) {
    let cur: u32 = io.read_once(off).unwrap();
    w(io, off, (cur & !mask) | set);
}

fn set_bits(io: &IoMem, off: usize, bits: u32) {
    rmw(io, off, 0, bits);
}

fn clear_bits(io: &IoMem, off: usize, bits: u32) {
    rmw(io, off, bits, 0);
}

fn delay_us(us: u64) {
    // Start the minimum hardware delay after preceding device writes.
    ostd::mm::dma::dma_io_fence();
    let ticks = (tsc_freq() as u128 * us as u128 / 1_000_000) as u64;
    let start = read_tsc();
    while read_tsc().wrapping_sub(start) < ticks.max(1) {
        core::hint::spin_loop();
    }
}
