// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: K1 eDP boot splash (logo, milestones, kernel log strip).

//! K1 eDP boot splash: logo, milestone status, progress bar, optional log strip.
//!
//! Drawing happens only from [`show`] (bring-up context) and [`run_loop`]
//! (the kernel thread). The `early_print` mirror only pushes bytes into a
//! fixed ring — it never touches the framebuffer.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ostd::{
    sync::{LocalIrqDisabled, SpinLock, WaitQueue},
    timer::{Jiffies, TIMER_FREQ},
};
use spin::Once;

use crate::{
    FrameBuffer, Pixel,
    font::BitmapFont,
    splash::{EndReason, SplashMode, TIMEOUT_SECS, mode},
};

const LOGO: &[u8] = include_bytes!("assets/logo_480x125.bgra");
const LOGO_W: usize = 480;
const LOGO_H: usize = 125;
const _: () = assert!(LOGO.len() == LOGO_W * LOGO_H * 4);

const BG: Pixel = Pixel {
    red: 0x10,
    green: 0x12,
    blue: 0x16,
};
const STATUS_FG: Pixel = Pixel {
    red: 0xd0,
    green: 0xd4,
    blue: 0xda,
};
const TRACK: Pixel = Pixel {
    red: 0x2a,
    green: 0x2e,
    blue: 0x36,
};
const FILL: Pixel = Pixel {
    red: 0x2f,
    green: 0x8f,
    blue: 0xff,
};
const SHIMMER: Pixel = Pixel {
    red: 0x7e,
    green: 0xc8,
    blue: 0xff,
};
const LOG_FG: Pixel = Pixel {
    red: 0x6b,
    green: 0x72,
    blue: 0x80,
};

const RING_LINES: usize = 16;
const LINE_CAP: usize = 160;
const TICK_MS: u64 = 250;
const TICK_PERIOD: u64 = TIMER_FREQ * TICK_MS / 1000;
const BAR_W_MAX: usize = 480;
const BAR_H: usize = 4;
const SHIMMER_W: usize = 64;
const STATUS_CAP: usize = 64;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static PROGRESS: AtomicU32 = AtomicU32::new(0);
static PHASE: AtomicU32 = AtomicU32::new(0);
static START_MS: AtomicU64 = AtomicU64::new(0);
static RING_GEN: AtomicU64 = AtomicU64::new(0);
static STATUS_GEN: AtomicU64 = AtomicU64::new(0);
static LAST_LOG_GEN: AtomicU64 = AtomicU64::new(u64::MAX);
static LAST_STATUS_GEN: AtomicU64 = AtomicU64::new(u64::MAX);
static TICKS: AtomicU64 = AtomicU64::new(0);
static PENDING: AtomicBool = AtomicBool::new(false);
static ON_END: Once<fn(EndReason)> = Once::new();
static FB: Once<Arc<FrameBuffer>> = Once::new();
static FONT_STATUS: Once<BitmapFont> = Once::new();
static FONT_LOG: Once<BitmapFont> = Once::new();
static WAIT: WaitQueue = WaitQueue::new();
static DRAW: SpinLock<()> = SpinLock::new(());
static RING: SpinLock<LogRing, LocalIrqDisabled> = SpinLock::new(LogRing::new());
static STATUS: SpinLock<StatusBuf, LocalIrqDisabled> = SpinLock::new(StatusBuf::new());

struct StatusBuf {
    bytes: [u8; STATUS_CAP],
    len: usize,
}

impl StatusBuf {
    const fn new() -> Self {
        Self {
            bytes: [0; STATUS_CAP],
            len: 0,
        }
    }

    fn set(&mut self, text: &str) {
        let t = text.as_bytes();
        let n = t.len().min(STATUS_CAP);
        self.bytes[..n].copy_from_slice(&t[..n]);
        self.len = n;
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

struct LogRing {
    lines: [[u8; LINE_CAP]; RING_LINES],
    lens: [u8; RING_LINES],
    start: u8,
    count: u8,
    col: u8,
    seq: u64,
}

impl LogRing {
    const fn new() -> Self {
        Self {
            lines: [[0; LINE_CAP]; RING_LINES],
            lens: [0; RING_LINES],
            start: 0,
            count: 0,
            col: 0,
            seq: 0,
        }
    }

    fn clear_line(&mut self, idx: usize) {
        self.lines[idx] = [0; LINE_CAP];
        self.lens[idx] = 0;
    }

    fn current_idx(&self) -> usize {
        (self.start as usize + self.count as usize - 1) % RING_LINES
    }

    fn ensure_line(&mut self) {
        if self.count == 0 {
            self.start = 0;
            self.count = 1;
            self.col = 0;
            self.clear_line(0);
        }
    }

    fn finish_line(&mut self) {
        if self.count == 0 {
            return;
        }
        if self.count as usize == RING_LINES {
            self.start = (self.start + 1) % RING_LINES as u8;
            self.clear_line(self.current_idx());
        } else {
            self.count += 1;
            self.clear_line(self.current_idx());
        }
        self.col = 0;
        self.bump();
    }

    fn bump(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        RING_GEN.store(self.seq, Ordering::Release);
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        for &b in bytes {
            match b {
                b'\n' => self.finish_line(),
                b'\r' => {
                    if self.count > 0 {
                        self.col = 0;
                        self.lens[self.current_idx()] = 0;
                        self.bump();
                    }
                }
                b if b < 0x20 || b == 0x7f => {}
                b => {
                    self.ensure_line();
                    let col = self.col as usize;
                    if col < LINE_CAP {
                        let idx = self.current_idx();
                        self.lines[idx][col] = b;
                        self.col = self.col.saturating_add(1);
                        self.lens[idx] = self.col;
                        self.bump();
                    }
                }
            }
        }
    }
}

struct Layout {
    logo_x: usize,
    logo_y: usize,
    logo_w: usize,
    logo_h: usize,
    status_y: usize,
    bar_x: usize,
    bar_y: usize,
    bar_w: usize,
    log_top: usize,
    log_rows: usize,
    log_margin: usize,
}

impl Layout {
    fn new(fb: &FrameBuffer, status_h: usize, log_line_h: usize) -> Self {
        let w = fb.width();
        let h = fb.height();
        let logo_w = LOGO_W.min(w);
        let logo_h = LOGO_H.min(h);
        let logo_x = w.saturating_sub(logo_w) / 2;
        let logo_y = h.saturating_mul(38) / 100;
        let logo_y = logo_y.min(h.saturating_sub(logo_h));

        let status_h = status_h.min(h);
        let status_y = logo_y
            .saturating_add(logo_h)
            .saturating_add(40)
            .min(h.saturating_sub(status_h.saturating_add(BAR_H).saturating_add(8)));

        let bar_w = BAR_W_MAX.min(w.saturating_sub(32));
        let bar_x = w.saturating_sub(bar_w) / 2;
        let bar_y = status_y
            .saturating_add(status_h)
            .saturating_add(24)
            .min(h.saturating_sub(BAR_H.min(h)));

        let gap = 16;
        let bar_bottom = bar_y.saturating_add(BAR_H);
        let floor = bar_bottom.saturating_add(gap);
        let want = 14usize.saturating_mul(log_line_h);
        let raw_top = h.saturating_sub(want);
        let log_top = raw_top.max(floor).min(h);
        let log_rows = h
            .saturating_sub(log_top)
            .checked_div(log_line_h)
            .unwrap_or(0);

        Self {
            logo_x,
            logo_y,
            logo_w,
            logo_h,
            status_y,
            bar_x,
            bar_y,
            bar_w,
            log_top,
            log_rows,
            log_margin: 24.min(w),
        }
    }
}

pub(crate) fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub(crate) fn set_on_end(f: fn(EndReason)) {
    ON_END.call_once(|| f);
}

/// Paint the splash and start owning the scanout. Called from `k1_edp` right
/// after a successful [`FrameBuffer::register`].
pub(crate) fn show(fb: &Arc<FrameBuffer>) {
    if mode() == SplashMode::Off {
        return;
    }
    if ACTIVE.load(Ordering::Acquire) {
        return;
    }

    let status_scale = if fb.height() >= 720 { 3 } else { 2 };
    let log_scale = if fb.height() >= 720 { 2 } else { 1 };
    FONT_STATUS.call_once(|| BitmapFont::new_basic8x8_scaled(status_scale));
    FONT_LOG.call_once(|| BitmapFont::new_basic8x8_scaled(log_scale));
    FB.call_once(|| fb.clone());

    if mode() == SplashMode::Log {
        ostd::console::set_early_print_mirror(mirror_hook);
    }

    START_MS.store(now_ms(), Ordering::Relaxed);
    set_status("Starting Asterinas");
    PROGRESS.store(5, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Release);

    paint_full(fb);

    let layout = Layout::new(
        fb,
        FONT_STATUS.get().map(|f| f.height()).unwrap_or(24),
        FONT_LOG.get().map(|f| f.height()).unwrap_or(16),
    );
    // Board DTB is `ostd.log_level=error`, so `ostd::info!` never reaches the
    // UART. These one-shot lines stay on `early_println!`.
    ostd::early_println!(
        "[k1-splash] shown {}x{} logo at ({},{})",
        fb.width(),
        fb.height(),
        layout.logo_x,
        layout.logo_y
    );
    ostd::early_println!("[k1-splash] milestone panel 5%");
}

pub(crate) fn on_exec(basename: &str) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some((name, pct, status)) = milestone_for(basename) else {
        return;
    };
    let old = PROGRESS.fetch_max(pct, Ordering::Relaxed);
    if pct <= old {
        return;
    }
    set_status(status);
    ostd::early_println!("[k1-splash] milestone {} {}%", name, pct);
    request_redraw();
}

pub(crate) fn end(reason: EndReason) {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    WAIT.wake_all();
    // Wait out an in-flight paint so the VT flush does not race us.
    drop(DRAW.lock());
    let elapsed = now_ms().saturating_sub(START_MS.load(Ordering::Relaxed));
    ostd::early_println!("[k1-splash] end reason={:?} after {} ms", reason, elapsed);
    if let Some(cb) = ON_END.get() {
        cb(reason);
    }
}

pub(crate) fn run_loop() {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    ostd::timer::register_callback_on_cpu(on_timer);
    loop {
        let keep_going = WAIT.wait_until(|| {
            if !ACTIVE.load(Ordering::Acquire) {
                return Some(false);
            }
            if PENDING.swap(false, Ordering::AcqRel) {
                return Some(true);
            }
            None
        });
        if !keep_going || !ACTIVE.load(Ordering::Acquire) {
            return;
        }
        if now_ms().saturating_sub(START_MS.load(Ordering::Relaxed)) >= TIMEOUT_SECS * 1000 {
            end(EndReason::Timeout);
            return;
        }
        tick();
    }
}

fn on_timer() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_multiple_of(TICK_PERIOD) {
        PENDING.store(true, Ordering::Release);
        WAIT.wake_all();
    }
}

fn request_redraw() {
    PENDING.store(true, Ordering::Release);
    WAIT.wake_all();
}

fn now_ms() -> u64 {
    Jiffies::elapsed().as_duration().as_millis() as u64
}

fn milestone_for(path: &str) -> Option<(&'static str, u32, &'static str)> {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base == "e2fsck" || base.starts_with("e2fsck.") {
        return Some(("e2fsck", 15, "Checking root filesystem"));
    }
    if base == "systemd" || path.ends_with("/lib/systemd/systemd") {
        return Some(("systemd", 35, "Starting system services"));
    }
    if base == "cosmic-session" {
        return Some(("cosmic-session", 60, "Starting desktop session"));
    }
    if base == "cosmic-comp" {
        return Some(("cosmic-comp", 80, "Starting compositor"));
    }
    None
}

fn set_status(text: &str) {
    STATUS.lock().set(text);
    STATUS_GEN.fetch_add(1, Ordering::Release);
}

fn mirror_hook(bytes: &[u8]) {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    RING.lock().push_bytes(bytes);
}

fn paint_full(fb: &FrameBuffer) {
    let _g = DRAW.lock();
    fill_rect(fb, 0, 0, fb.width(), fb.height(), BG);
    blit_logo(fb);
    paint_status(fb);
    paint_bar(fb);
    if mode() == SplashMode::Log {
        paint_log(fb);
    }
}

fn tick() {
    let Some(fb) = FB.get() else {
        return;
    };
    let _g = DRAW.lock();
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    PHASE.fetch_add(24, Ordering::Relaxed);
    let status_gen = STATUS_GEN.load(Ordering::Acquire);
    if LAST_STATUS_GEN.swap(status_gen, Ordering::AcqRel) != status_gen {
        paint_status(fb);
    }
    paint_bar(fb);
    if mode() == SplashMode::Log {
        let ring_gen = RING_GEN.load(Ordering::Acquire);
        if LAST_LOG_GEN.swap(ring_gen, Ordering::AcqRel) != ring_gen {
            paint_log(fb);
        }
    }
}

fn paint_status(fb: &FrameBuffer) {
    let Some(font) = FONT_STATUS.get() else {
        return;
    };
    let layout = Layout::new(
        fb,
        font.height(),
        FONT_LOG.get().map(|f| f.height()).unwrap_or(16),
    );
    let text = {
        let g = STATUS.lock();
        let mut buf = [0u8; STATUS_CAP];
        let n = g.len;
        buf[..n].copy_from_slice(g.as_bytes());
        (buf, n)
    };
    let band_w = fb.width().min(960);
    let band_x = fb.width().saturating_sub(band_w) / 2;
    fill_rect(fb, band_x, layout.status_y, band_w, font.height(), BG);
    let tw = text.1.saturating_mul(font.width());
    let tx = fb.width().saturating_sub(tw) / 2;
    draw_text(
        fb,
        font,
        tx,
        layout.status_y,
        &text.0[..text.1],
        STATUS_FG,
        BG,
    );
}

fn paint_bar(fb: &FrameBuffer) {
    let status_h = FONT_STATUS.get().map(|f| f.height()).unwrap_or(24);
    let log_h = FONT_LOG.get().map(|f| f.height()).unwrap_or(16);
    let layout = Layout::new(fb, status_h, log_h);
    if layout.bar_w == 0 {
        return;
    }
    let pct = PROGRESS.load(Ordering::Relaxed).min(100) as usize;
    let filled = layout.bar_w.saturating_mul(pct) / 100;
    let phase = PHASE.load(Ordering::Relaxed) as usize;
    let cycle = layout.bar_w.saturating_add(SHIMMER_W).max(1);
    let shimmer_head = phase % cycle;
    let bpp = fb.pixel_format().nbytes();
    if bpp == 0 {
        return;
    }
    let fill_p = fb.render_pixel(FILL);
    let track_p = fb.render_pixel(TRACK);
    let shim_p = fb.render_pixel(SHIMMER);
    let shim_track = fb.render_pixel(Pixel {
        red: 0x3a,
        green: 0x3e,
        blue: 0x48,
    });
    let mut row = vec![0u8; layout.bar_w * bpp];
    for x_off in 0..layout.bar_w {
        let in_fill = x_off < filled;
        let shimmer_x = x_off + SHIMMER_W;
        let in_shimmer = shimmer_x >= shimmer_head && shimmer_x < shimmer_head + SHIMMER_W;
        let pix = if in_shimmer {
            if in_fill { shim_p } else { shim_track }
        } else if in_fill {
            fill_p
        } else {
            track_p
        };
        let dst = x_off * bpp;
        let sl = pix.as_slice();
        let n = sl.len().min(bpp);
        row[dst..dst + n].copy_from_slice(&sl[..n]);
    }
    let y1 = layout.bar_y.saturating_add(BAR_H).min(fb.height());
    for yy in layout.bar_y..y1 {
        if layout.bar_x >= fb.width() {
            break;
        }
        let off = fb.calc_offset(layout.bar_x, yy).as_usize();
        let _ = fb.write_bytes_at(off, &row);
    }
}

fn paint_log(fb: &FrameBuffer) {
    let Some(font) = FONT_LOG.get() else {
        return;
    };
    let layout = Layout::new(
        fb,
        FONT_STATUS.get().map(|f| f.height()).unwrap_or(24),
        font.height(),
    );
    if layout.log_rows == 0 {
        return;
    }
    let line_h = font.height();
    let mut snapshot = Vec::with_capacity(RING_LINES);
    {
        let g = RING.lock();
        let take = (g.count as usize).min(layout.log_rows);
        let skip = (g.count as usize).saturating_sub(take);
        for i in 0..take {
            let idx = (g.start as usize + skip + i) % RING_LINES;
            snapshot.push((g.lines[idx], g.lens[idx]));
        }
    }
    for row in 0..layout.log_rows {
        let y = layout.log_top.saturating_add(row.saturating_mul(line_h));
        let text: &[u8] = if row < snapshot.len() {
            let (bytes, len) = &snapshot[row];
            &bytes[..*len as usize]
        } else {
            b""
        };
        fill_text_line(fb, font, y, layout.log_margin, text, LOG_FG, BG);
    }
}

fn blit_logo(fb: &FrameBuffer) {
    let status_h = FONT_STATUS.get().map(|f| f.height()).unwrap_or(24);
    let log_h = FONT_LOG.get().map(|f| f.height()).unwrap_or(16);
    let layout = Layout::new(fb, status_h, log_h);
    let w = layout.logo_w.min(fb.width().saturating_sub(layout.logo_x));
    let h = layout.logo_h.min(fb.height().saturating_sub(layout.logo_y));
    if w == 0 || h == 0 {
        return;
    }
    let bpp = fb.pixel_format().nbytes();
    if bpp == 0 {
        return;
    }
    let mut row = vec![0u8; w * bpp];
    for ly in 0..h {
        for lx in 0..w {
            let src = ((ly * LOGO_W + lx) * 4).min(LOGO.len().saturating_sub(4));
            let b = LOGO[src];
            let g = LOGO[src + 1];
            let r = LOGO[src + 2];
            let a = LOGO[src + 3];
            let (or, og, ob) = blend(r, g, b, a, BG.red, BG.green, BG.blue);
            let pix = fb.render_pixel(Pixel {
                red: or,
                green: og,
                blue: ob,
            });
            let dst = lx * bpp;
            let sl = pix.as_slice();
            let n = sl.len().min(bpp);
            row[dst..dst + n].copy_from_slice(&sl[..n]);
        }
        let off = fb.calc_offset(layout.logo_x, layout.logo_y + ly).as_usize();
        let _ = fb.write_bytes_at(off, &row);
    }
}

fn blend(sr: u8, sg: u8, sb: u8, a: u8, dr: u8, dg: u8, db: u8) -> (u8, u8, u8) {
    if a == 0 {
        return (dr, dg, db);
    }
    if a == 255 {
        return (sr, sg, sb);
    }
    let aa = a as u16;
    let ia = 255 - aa;
    (
        ((sr as u16 * aa + dr as u16 * ia) / 255) as u8,
        ((sg as u16 * aa + dg as u16 * ia) / 255) as u8,
        ((sb as u16 * aa + db as u16 * ia) / 255) as u8,
    )
}

fn fill_rect(fb: &FrameBuffer, x: usize, y: usize, w: usize, h: usize, color: Pixel) {
    let x1 = x.saturating_add(w).min(fb.width());
    let y1 = y.saturating_add(h).min(fb.height());
    let x0 = x.min(fb.width());
    let y0 = y.min(fb.height());
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let pix = fb.render_pixel(color);
    let n = pix.nbytes();
    if n == 0 {
        return;
    }
    let mut row = vec![0u8; (x1 - x0) * n];
    for chunk in row.chunks_exact_mut(n) {
        chunk.copy_from_slice(pix.as_slice());
    }
    for yy in y0..y1 {
        let off = fb.calc_offset(x0, yy).as_usize();
        let _ = fb.write_bytes_at(off, &row);
    }
}

fn draw_text(
    fb: &FrameBuffer,
    font: &BitmapFont,
    x: usize,
    y: usize,
    text: &[u8],
    fg: Pixel,
    bg: Pixel,
) {
    fill_text_line(fb, font, y, x, text, fg, bg);
}

fn fill_text_line(
    fb: &FrameBuffer,
    font: &BitmapFont,
    y: usize,
    margin: usize,
    text: &[u8],
    fg: Pixel,
    bg: Pixel,
) {
    let w = fb.width();
    let h = fb.height();
    if y >= h || w == 0 {
        return;
    }
    let gh = font.height().min(h - y);
    let n = fb.pixel_format().nbytes();
    if gh == 0 || n == 0 {
        return;
    }
    let fg_p = fb.render_pixel(fg);
    let bg_p = fb.render_pixel(bg);
    let mut bg_line = vec![0u8; w * n];
    for chunk in bg_line.chunks_exact_mut(n) {
        chunk.copy_from_slice(bg_p.as_slice());
    }
    let mut rows = Vec::with_capacity(gh);
    for _ in 0..gh {
        rows.push(bg_line.clone());
    }
    let mut cx = margin;
    for &ch in text {
        if cx >= w {
            break;
        }
        let glyph_ch = if (0x20..=0x7e).contains(&ch) {
            ch
        } else {
            b'?'
        };
        if let Some(glyph) = font.char(glyph_ch) {
            for (gy, grow) in glyph.rows().enumerate() {
                if gy >= gh {
                    break;
                }
                for (i, on) in grow.bits().enumerate() {
                    let px = cx + i;
                    if px >= w {
                        break;
                    }
                    if on {
                        let off = px * n;
                        rows[gy][off..off + n].copy_from_slice(fg_p.as_slice());
                    }
                }
            }
        }
        cx = cx.saturating_add(font.width());
    }
    for (gy, row) in rows.iter().enumerate() {
        let off = fb.calc_offset(0, y + gy).as_usize();
        let _ = fb.write_bytes_at(off, row);
    }
}
