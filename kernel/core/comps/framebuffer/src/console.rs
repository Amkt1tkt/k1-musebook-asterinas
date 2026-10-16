// SPDX-License-Identifier: MPL-2.0

use alloc::{sync::Arc, vec::Vec};

// K1 MUSE Book --- (size() covers DMA-backed K1 FB; io_mem() is now Option)
// use ostd::mm::HasSize;
// K1 MUSE Book ---
use crate::{
    // K1 MUSE Book --- (EL / CSI K for readline and getty)
    // ansi_escape::{EraseInDisplay, EscapeFsm, EscapeOp},
    ansi_escape::{EraseInDisplay, EraseInLine, EscapeFsm, EscapeOp},
    // K1 MUSE Book ---
    font::BitmapFont,
    framebuffer::FrameBuffer,
    mode::ConsoleMode,
    pixel::Pixel,
};

/// A text console rendered onto the framebuffer.
///
/// This console maintains an internal buffer mirroring the framebuffer content. By default, the
/// console is not activated (see [`Self::activate`]) and is in [`ConsoleMode::Text`] (see
/// [`Self::set_mode`]). The internal buffer and the framebuffer operate as follows:
///  - Whenever the console enters the `(activated, text)` state from any other state, the entire
///    internal buffer is flushed to the framebuffer. While this state holds, new text is drawn to
///    both the internal buffer and the framebuffer.
///  - Otherwise, text is drawn only to the internal buffer.
pub struct FramebufferConsole {
    state: ConsoleState,
    escape_fsm: EscapeFsm,
}

/// An error returned by [`FramebufferConsole::set_font`].
#[derive(Clone, Copy, Debug)]
pub enum ConsoleSetFontError {
    InvalidFont,
}

impl FramebufferConsole {
    /// Creates a new framebuffer console.
    pub fn new(framebuffer: Arc<FrameBuffer>) -> Self {
        let state = ConsoleState::new(framebuffer);
        let escape_fsm = EscapeFsm::new();

        Self { state, escape_fsm }
    }

    /// Activates the console.
    pub fn activate(&mut self) {
        let old_is_active = self.state.is_active;
        self.state.is_active = true;
        if !old_is_active {
            self.state.flush_fullscreen();
        }
    }

    /// Deactivates the console.
    pub fn deactivate(&mut self) {
        self.state.is_active = false;
    }

    /// Returns the current console mode.
    pub fn mode(&self) -> ConsoleMode {
        self.state.mode
    }

    /// Sets the console mode.
    pub fn set_mode(&mut self, mode: ConsoleMode) {
        let old_mode = self.state.mode;
        self.state.mode = mode;
        if old_mode == ConsoleMode::Graphics {
            self.state.flush_fullscreen();
        }
    }

    /// Sets the font for the framebuffer console.
    pub fn set_font(&mut self, font: BitmapFont) -> Result<(), ConsoleSetFontError> {
        self.state.set_font(font)
    }

    // K1 MUSE Book --- (TIOCGWINSZ for the 1080p VT)
    /// Returns `(rows, cols, width_px, height_px)` for TTY `TIOCGWINSZ`.
    pub fn window_geometry(&self) -> (u16, u16, u16, u16) {
        let cols = self.state.cols() as u16;
        let rows = self.state.rows() as u16;
        let width_px = self.state.backend.width() as u16;
        let height_px = self.state.backend.height() as u16;
        (rows, cols, width_px, height_px)
    }
    // K1 MUSE Book ---

    /// Sends a byte slice to the console.
    pub fn send(&mut self, buf: &[u8]) {
        for byte in buf {
            if self.escape_fsm.eat(*byte, &mut self.state) {
                // The character is part of an ANSI escape sequence.
                continue;
            }

            if *byte == 0 {
                // The character is a NUL character.
                continue;
            }

            self.state.send_char(*byte);
        }
        // K1 MUSE Book --- (coalesce eDP DMA writes per write() batch)
        self.state.flush_dirty();
        // K1 MUSE Book ---
    }
}

impl core::fmt::Debug for FramebufferConsole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FramebufferConsole").finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct ConsoleState {
    x_pos: usize,
    y_pos: usize,
    fg_color: Pixel,
    bg_color: Pixel,
    font: BitmapFont,

    is_active: bool,
    mode: ConsoleMode,

    bytes: Vec<u8>,
    backend: Arc<FrameBuffer>,
    // K1 MUSE Book --- (dirty bounding box; flushed once per send())
    /// Inclusive-exclusive dirty rect in pixels: `(x0, y0, x1, y1)`.
    dirty: Option<(usize, usize, usize, usize)>,
    // K1 MUSE Book ---
}

impl ConsoleState {
    // K1 MUSE Book --- (DMA-backed size, scaled glyphs, and deferred writes)
    fn new(backend: Arc<FrameBuffer>) -> Self {
        // let buffer_size = backend.io_mem().size();
        // Self {
        //     x_pos: 0,
        //     y_pos: 0,
        //     fg_color: Pixel::WHITE,
        //     bg_color: Pixel::BLACK,
        //     font: BitmapFont::new_basic8x8(),
        //     is_active: false,
        //     mode: ConsoleMode::Text,
        //     bytes: alloc::vec![0; buffer_size],
        //     backend,
        // }
        let buffer_size = backend.size();
        Self {
            x_pos: 0,
            y_pos: 0,
            fg_color: Pixel::WHITE,
            bg_color: Pixel::BLACK,
            font: default_font_for_framebuffer(&backend),
            is_active: false,
            mode: ConsoleMode::Text,
            bytes: alloc::vec![0; buffer_size],
            backend,
            dirty: None,
        }
    }
    // K1 MUSE Book ---

    // K1 MUSE Book --- (dirty-rect tracking for uncached eDP scanout)
    fn mark_dirty(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.dirty = Some(match self.dirty {
            None => (x0, y0, x1, y1),
            Some((ox0, oy0, ox1, oy1)) => (ox0.min(x0), oy0.min(y0), ox1.max(x1), oy1.max(y1)),
        });
    }

    /// Flushes the dirty rect as per-row sub-spans.
    fn flush_dirty(&mut self) {
        let Some((x0, y0, x1, y1)) = self.dirty.take() else {
            return;
        };
        if !self.is_output_enabled() {
            return;
        }
        let pixel_size = self.backend.render_pixel(self.fg_color).nbytes();
        let span = (x1 - x0) * pixel_size;
        for y in y0..y1 {
            let off = self.backend.calc_offset(x0, y).as_usize();
            if off + span > self.bytes.len() {
                break;
            }
            let _ = self
                .backend
                .write_bytes_at(off, &self.bytes[off..off + span]);
        }
    }
    // K1 MUSE Book ---

    /// Flushes the entire console buffer to the framebuffer.
    fn flush_fullscreen(&mut self) {
        // K1 MUSE Book --- (clear dirty before a full-screen flush)
        self.dirty = None;
        // K1 MUSE Book ---
        if self.is_output_enabled() {
            self.backend.write_bytes_at(0, &self.bytes).unwrap();
        }
    }

    /// Sends a single character to be drawn on the framebuffer.
    fn send_char(&mut self, ch: u8) {
        if ch == b'\n' {
            self.newline();
            return;
        } else if ch == b'\r' {
            self.carriage_return();
            return;
        } else if ch == b'\x08' {
            self.backspace();
            return;
        }

        // K1 MUSE Book --- (wrap on the text-area width, not the raw panel)
        // if self.x_pos > self.backend.width() - self.font.width() {
        if self.x_pos + self.font.width() > self.text_width_px() {
            // K1 MUSE Book ---
            self.newline();
        }

        self.draw_char(ch);

        self.x_pos += self.font.width();
    }

    fn newline(&mut self) {
        self.y_pos += self.font.height();
        self.x_pos = 0;

        // K1 MUSE Book --- (wrap on the text-area height, not the raw panel)
        // if self.y_pos > self.backend.height() - self.font.height() {
        if self.y_pos + self.font.height() > self.text_height_px() {
            // K1 MUSE Book ---
            self.shift_lines_up();
        }
    }

    fn shift_lines_up(&mut self) {
        // K1 MUSE Book --- (scroll only the text area; keep the overscan margin)
        // let offset = self.backend.calc_offset(0, self.font.height()).as_usize();
        // self.bytes.copy_within(offset.., 0);
        // self.bytes[self.backend.io_mem().size() - offset..].fill(0);
        //
        // if self.is_output_enabled() {
        //     self.backend.write_bytes_at(0, &self.bytes).unwrap();
        // }
        //
        // self.y_pos -= self.font.height();
        self.flush_dirty();

        let offset = self.backend.calc_offset(0, self.font.height()).as_usize();
        let text_bytes = self.text_height_px() * self.backend.line_size();
        if text_bytes > offset {
            self.bytes.copy_within(offset..text_bytes, 0);
            self.bytes[text_bytes - offset..text_bytes].fill(0);
        }

        if self.is_output_enabled() {
            self.backend
                .write_bytes_at(0, &self.bytes[..text_bytes])
                .unwrap();
        }

        self.y_pos -= self.font.height();
        // K1 MUSE Book ---
    }

    // K1 MUSE Book --- (cell geometry for TIOCGWINSZ and wrap)
    fn cols(&self) -> usize {
        self.backend.width() / self.font.width()
    }

    fn rows(&self) -> usize {
        self.backend.height() / self.font.height()
    }

    fn text_width_px(&self) -> usize {
        self.cols() * self.font.width()
    }

    fn text_height_px(&self) -> usize {
        self.rows() * self.font.height()
    }
    // K1 MUSE Book ---

    fn carriage_return(&mut self) {
        self.x_pos = 0;
    }

    fn backspace(&mut self) {
        if self.x_pos < self.font.width() {
            // TODO: What should we do if we're at the beginning of the line?
            return;
        }

        self.x_pos -= self.font.width();
        self.draw_char(b' ');
    }

    fn draw_char(&mut self, ch: u8) {
        let Some(font_ch) = self.font.char(ch) else {
            return;
        };

        let fg_pixel = self.backend.render_pixel(self.fg_color);
        let bg_pixel = self.backend.render_pixel(self.bg_color);

        let pixel_size = fg_pixel.nbytes();
        // K1 MUSE Book --- (shadow only; FB updated in flush_dirty)
        // let is_output_enabled = self.is_output_enabled();
        let y0 = self.y_pos;
        // K1 MUSE Book ---

        let mut offset = self.backend.calc_offset(self.x_pos, self.y_pos);

        for row in font_ch.rows() {
            let off_st = offset.as_usize();
            let off_ed = off_st + pixel_size * self.font.width();
            let render_buf = &mut self.bytes[off_st..off_ed];

            // Write pixels to the console buffer.
            let chunks = render_buf.chunks_exact_mut(pixel_size);
            for (chunk, is_fg) in chunks.zip(row.bits()) {
                let pixel = if is_fg { fg_pixel } else { bg_pixel };
                chunk.copy_from_slice(pixel.as_slice());
            }

            // K1 MUSE Book --- (do not write each glyph row; flush_dirty batches DMA)
            // if is_output_enabled {
            //     self.backend.write_bytes_at(off_st, render_buf).unwrap();
            // }
            // K1 MUSE Book ---

            offset.y_add(1);
        }

        // K1 MUSE Book --- (record the glyph's dirty rect)
        self.mark_dirty(
            self.x_pos,
            y0,
            (self.x_pos + self.font.width()).min(self.backend.width()),
            y0 + self.font.height(),
        );
        // K1 MUSE Book ---
    }

    /// Sets the font for the framebuffer console.
    fn set_font(&mut self, font: BitmapFont) -> Result<(), ConsoleSetFontError> {
        // Note that the font height cannot exceed the half the height of the framebuffer.
        // Otherwise, `shift_lines_up` will underflow `x_pos`.
        if font.width() > self.backend.width() || font.height() > self.backend.height() / 2 {
            return Err(ConsoleSetFontError::InvalidFont);
        }

        self.font = font;

        // K1 MUSE Book --- (wrap on the text-area height after a font change)
        // if self.y_pos > self.backend.height() - self.font.height() {
        if self.y_pos + self.font.height() > self.text_height_px() {
            // K1 MUSE Book ---
            self.shift_lines_up();
        }

        Ok(())
    }

    /// Fills a rectangular pixel region `[x0, x1) × [y0, y1)` with the given color.
    ///
    /// The caller must pass arguments that form a valid rectangle within the framebuffer, i.e.,
    /// `x0 ≤ x1 ≤ w` and `y0 ≤ y1 ≤ h`, where `w` and `h` are the framebuffer width and height.
    fn fill_rect_pixels(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, color: Pixel) {
        debug_assert!(x1 <= self.backend.width());
        debug_assert!(y1 <= self.backend.height());

        let rendered_pixel = self.backend.render_pixel(color);
        let rendered_pixel_size = rendered_pixel.nbytes();
        let row_bytes = (x1 - x0) * rendered_pixel_size;
        // K1 MUSE Book --- (defer FB write to flush_dirty)
        // let is_output_enabled = self.is_output_enabled();
        // K1 MUSE Book ---

        for y in y0..y1 {
            let off = self.backend.calc_offset(x0, y).as_usize();
            let buf = &mut self.bytes[off..off + row_bytes];

            // Write pixels to the console buffer.
            for chunk in buf.chunks_exact_mut(rendered_pixel_size) {
                chunk.copy_from_slice(rendered_pixel.as_slice());
            }

            // K1 MUSE Book --- (do not write each fill row; flush_dirty batches DMA)
            // if is_output_enabled {
            //     self.backend.write_bytes_at(off, buf).unwrap();
            // }
            // K1 MUSE Book ---
        }
        // K1 MUSE Book --- (record the filled rect as dirty)
        self.mark_dirty(x0, y0, x1, y1);
        // K1 MUSE Book ---
    }

    /// Calculates the pixel coordinates for the cursor cell.
    fn cursor_cell_rect(&self) -> (usize, usize, usize, usize) {
        let cx0 = self.x_pos;
        // K1 MUSE Book --- (clip to the text area, not the raw panel)
        // let cx1 = cx0 + self.font.width();
        // let cx1 = cx1.min(self.backend.width());
        let cx1 = (cx0 + self.font.width()).min(self.text_width_px());
        // K1 MUSE Book ---

        let cy0 = self.y_pos;
        let cy1 = cy0 + self.font.height();
        // This will always hold because we will scroll if it is no longer true after a new line or
        // font change.
        // K1 MUSE Book --- (assert against the text area, not the raw panel)
        // debug_assert!(cy1 <= self.backend.height());
        debug_assert!(cy1 <= self.text_height_px());
        // K1 MUSE Book ---

        (cx0, cy0, cx1, cy1)
    }

    /// Returns whether the output characters will be drawn in the framebuffer.
    fn is_output_enabled(&self) -> bool {
        self.is_active && self.mode == ConsoleMode::Text
    }
}

// K1 MUSE Book --- (scale console glyphs to the panel resolution)
/// Picks a readable default glyph size for the panel resolution.
fn default_font_for_framebuffer(fb: &FrameBuffer) -> BitmapFont {
    let scale = match fb.height() {
        0..720 => 1,
        720..1440 => 2, // 1080p → 16×16, ~120×67
        _ => 3,
    };
    BitmapFont::new_basic8x8_scaled(scale)
}
// K1 MUSE Book ---

impl EscapeOp for ConsoleState {
    fn set_cursor(&mut self, x: usize, y: usize) {
        // K1 MUSE Book --- (cursor limits use cell geometry, not raw pixels)
        // let max_x = self.backend.width() / self.font.width() - 1;
        // let max_y = self.backend.height() / self.font.height() - 1;
        let max_x = self.cols().saturating_sub(1);
        let max_y = self.rows().saturating_sub(1);
        // K1 MUSE Book ---

        // Note that if the Y (or X) position is too large, the cursor will move to the last line
        // (or the line end).
        self.x_pos = x.min(max_x) * self.font.width();
        self.y_pos = y.min(max_y) * self.font.height();
    }

    fn set_fg_color(&mut self, val: Pixel) {
        self.fg_color = val;
    }

    fn set_bg_color(&mut self, val: Pixel) {
        self.bg_color = val;
    }

    fn erase_in_display(&mut self, mode: EraseInDisplay) {
        let bg = self.bg_color;
        // K1 MUSE Book --- (clear the text area, not the overscan)
        // let w = self.backend.width();
        // let h = self.backend.height();
        let w = self.text_width_px();
        let h = self.text_height_px();
        // K1 MUSE Book ---

        let (cx0, cy0, cx1, cy1) = self.cursor_cell_rect();

        match mode {
            EraseInDisplay::CursorToEnd => {
                // Clear from the cursor to the end of the line, within the cursor row.
                self.fill_rect_pixels(cx0, cy0, w, cy1, bg);

                // Clear all rows below the cursor row.
                if cy1 < h {
                    self.fill_rect_pixels(0, cy1, w, h, bg);
                }
            }
            EraseInDisplay::CursorToBeginning => {
                // Clear all rows above the cursor row.
                if cy0 > 0 {
                    self.fill_rect_pixels(0, 0, w, cy0, bg);
                }

                // Clear from the start of the line to the cursor, within the cursor row.
                self.fill_rect_pixels(0, cy0, cx1, cy1, bg);
            }
            EraseInDisplay::EntireScreen | EraseInDisplay::EntireScreenAndScrollback => {
                // Clear the entire text area.
                self.fill_rect_pixels(0, 0, w, h, bg);
            }
        }
    }

    // K1 MUSE Book --- (CSI n K; readline/bash)
    fn erase_in_line(&mut self, mode: EraseInLine) {
        let bg = self.bg_color;
        let w = self.text_width_px();
        let (cx0, cy0, cx1, cy1) = self.cursor_cell_rect();

        match mode {
            EraseInLine::CursorToEnd => self.fill_rect_pixels(cx0, cy0, w, cy1, bg),
            EraseInLine::CursorToBeginning => self.fill_rect_pixels(0, cy0, cx1, cy1, bg),
            EraseInLine::EntireLine => self.fill_rect_pixels(0, cy0, w, cy1, bg),
        }
    }
    // K1 MUSE Book ---
}
