// SPDX-License-Identifier: MPL-2.0

use alloc::{sync::Arc, vec::Vec};

use align_ext::AlignExt;
use ostd::{
    Error,
    Result,
    boot::boot_info,
    io::IoMem,
    // K1 MUSE Book --- (retain DMA backing and cache policy through framebuffer mappings)
    // mm::{CachePolicy, HasSize, PAGE_SIZE, VmIo},
    mm::{
        CachePolicy, FallibleVmRead, FallibleVmWrite, HasPaddr, HasSize, PAGE_SIZE, Paddr, VmIo,
        VmReader, VmWriter, dma::DmaCoherent, io::util::HasVmReaderWriter,
    },
    // K1 MUSE Book ---
    sync::Mutex,
};
use spin::Once;

use crate::pixel::{Pixel, PixelFormat, RenderedPixel};

/// Maximum number of colormap entries (standard 8-bit palette)
pub const MAX_CMAP_SIZE: usize = 256;

// K1 MUSE Book --- (DRAM scanout for K1 eDP; boot/EFI stays IoMem)
/// Physical backing store for a [`FrameBuffer`].
///
/// Boot/EFI framebuffers are typically MMIO ([`IoMem`]). Platform drivers such
/// as K1 eDP allocate DRAM and scan it out via DMA ([`DmaCoherent`]).
#[derive(Debug)]
enum FbMemory {
    IoMem(IoMem),
    Dma(Arc<DmaCoherent>),
}
// K1 MUSE Book ---

/// The framebuffer used for text or graphical output.
///
/// # Notes
///
/// It is highly recommended to use a synchronization primitive, such as a `SpinLock`, to
/// lock the framebuffer before performing any operation on it.
/// Failing to properly synchronize access can result in corrupted framebuffer content
/// or unspecified behavior during rendering.
#[derive(Debug)]
pub struct FrameBuffer {
    // K1 MUSE Book --- (FbMemory + mmap alias; original was a single IoMem)
    // io_mem: IoMem,
    memory: FbMemory,
    /// Userspace-mmap alias retaining the DMA allocation and its cache policy.
    ///
    /// `None` for IoMem-backed FBs — those already expose [`Self::io_mem`].
    dma_mmap: Option<IoMem>,
    // K1 MUSE Book ---
    width: usize,
    height: usize,
    line_size: usize,
    pixel_format: PixelFormat,
    cmap: Mutex<FbCmap>,
}

/// A single entry in the color map with 16-bit color values.
///
/// Linux framebuffer colormap uses 16-bit values (0-65535) for each color channel
/// to support high precision color mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColorMapEntry {
    /// Red color value (16-bit)
    pub red: u16,
    /// Green color value (16-bit)
    pub green: u16,
    /// Blue color value (16-bit)
    pub blue: u16,
    /// Transparency value (16-bit)
    pub transp: u16,
}

/// Internal framebuffer colormap structure.
#[derive(Clone, Debug)]
struct FbCmap {
    /// Color map entries
    entries: Vec<ColorMapEntry>,
}

pub static FRAMEBUFFER: Once<Arc<FrameBuffer>> = Once::new();

pub(crate) fn init() {
    let Some(framebuffer_arg) = boot_info().framebuffer_arg else {
        ostd::warn!("Framebuffer not found");
        return;
    };

    if framebuffer_arg.address == 0 {
        ostd::error!("Framebuffer address is zero");
        return;
    }

    if framebuffer_arg.address % PAGE_SIZE != 0 {
        ostd::error!("Framebuffer address is not page-aligned");
        return;
    }

    // FIXME: There are several pixel formats that have the same BPP. We lost the information
    // during the boot phase, so here we guess the pixel format on a best effort basis.
    let pixel_format = match framebuffer_arg.bpp {
        8 => PixelFormat::Grayscale8,
        16 => PixelFormat::Rgb565,
        24 => PixelFormat::Rgb888,
        32 => PixelFormat::BgrReserved,
        _ => {
            ostd::error!(
                "Unsupported framebuffer pixel format: {} bpp",
                framebuffer_arg.bpp
            );
            return;
        }
    };

    let framebuffer = {
        // FIXME: There can be more than `width` pixels per framebuffer line due to alignment
        // purposes. We need to collect this information during the boot phase.
        let line_size = framebuffer_arg
            .width
            .checked_mul(pixel_format.nbytes())
            .unwrap();
        let fb_size = framebuffer_arg
            .height
            .checked_mul(line_size)
            .unwrap()
            // The framebuffer should cover an entire set of pages. These pages can be mapped to
            // userspace upon request.
            .align_up(PAGE_SIZE);

        let fb_base = framebuffer_arg.address;
        // Use write-combining for framebuffer to enable faster write operations.
        // Write-combining allows the CPU to combine multiple writes into fewer bus transactions,
        // which is ideal for framebuffer access patterns (sequential writes).
        let io_mem = IoMem::acquire_with_cache_policy(
            fb_base..fb_base.checked_add(fb_size).unwrap(),
            CachePolicy::WriteCombining,
        )
        .unwrap();

        // K1 MUSE Book --- (construct via FbMemory so DMA-backed FBs share the type)
        // let default_cmap = FbCmap {
        //     entries: Vec::new(),
        // };
        //
        // FrameBuffer {
        //     io_mem,
        //     width: framebuffer_arg.width,
        //     height: framebuffer_arg.height,
        //     line_size,
        //     pixel_format,
        //     cmap: Mutex::new(default_cmap),
        // }
        FrameBuffer::new(
            FbMemory::IoMem(io_mem),
            None,
            framebuffer_arg.width,
            framebuffer_arg.height,
            line_size,
            pixel_format,
        )
        // K1 MUSE Book ---
    };

    framebuffer.clear();
    FRAMEBUFFER.call_once(|| Arc::new(framebuffer));
}

impl FrameBuffer {
    // K1 MUSE Book --- (shared constructor for IoMem and DMA backing)
    fn new(
        memory: FbMemory,
        dma_mmap: Option<IoMem>,
        width: usize,
        height: usize,
        line_size: usize,
        pixel_format: PixelFormat,
    ) -> Self {
        Self {
            memory,
            dma_mmap,
            width,
            height,
            line_size,
            pixel_format,
            cmap: Mutex::new(FbCmap {
                entries: Vec::new(),
            }),
        }
    }

    /// Builds a framebuffer backed by coherent DMA memory (DRAM scanout).
    ///
    /// The caller chooses the allocation's device-coherency contract. The
    /// userspace IoMem alias retains that allocation and the same cache policy;
    /// on K1 with Svpbmt, noncoherent allocations use PBMT_NC mappings.
    pub fn from_dma(
        dma: DmaCoherent,
        width: usize,
        height: usize,
        line_size: usize,
        pixel_format: PixelFormat,
    ) -> Self {
        let dma = Arc::new(dma);
        let dma_mmap = dma.as_iomem_alias();
        Self::new(
            FbMemory::Dma(dma),
            Some(dma_mmap),
            width,
            height,
            line_size,
            pixel_format,
        )
    }

    /// Registers this framebuffer as the global [`FRAMEBUFFER`] if none is set.
    ///
    /// Returns `true` when registration succeeds. Clears the buffer to black.
    pub fn register(self) -> bool {
        if FRAMEBUFFER.is_completed() {
            return false;
        }
        self.clear();
        FRAMEBUFFER.call_once(|| Arc::new(self));
        true
    }
    // K1 MUSE Book ---

    /// Returns the width of the framebuffer in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Returns the height of the framebuffer in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Returns the line size of the framebuffer in bytes.
    pub fn line_size(&self) -> usize {
        self.line_size
    }

    // K1 MUSE Book --- (size/paddr/io_mem for DMA-backed K1 FB and /dev/fb0)
    /// Returns the size of the framebuffer in bytes.
    pub fn size(&self) -> usize {
        match &self.memory {
            FbMemory::IoMem(m) => m.size(),
            FbMemory::Dma(m) => m.size(),
        }
    }

    /// Returns the physical base address of the framebuffer.
    pub fn paddr(&self) -> Paddr {
        match &self.memory {
            FbMemory::IoMem(m) => m.paddr(),
            FbMemory::Dma(m) => m.paddr(),
        }
    }

    /// Returns an [`IoMem`] suitable for userspace `mmap` of this framebuffer.
    ///
    /// IoMem-backed FBs return the boot MMIO region. DMA-backed FBs return the
    /// allocation-backed alias created in [`Self::from_dma`].
    // pub fn io_mem(&self) -> &IoMem {
    //     &self.io_mem
    // }
    pub fn io_mem(&self) -> Option<&IoMem> {
        match &self.memory {
            FbMemory::IoMem(m) => Some(m),
            FbMemory::Dma(_) => self.dma_mmap.as_ref(),
        }
    }
    // K1 MUSE Book ---

    /// Returns the pixel format of the framebuffer.
    pub fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    /// Renders the pixel according to the pixel format of the framebuffer.
    pub fn render_pixel(&self, pixel: Pixel) -> RenderedPixel {
        pixel.render(self.pixel_format)
    }

    /// Calculates the offset of a pixel at the specified position.
    pub fn calc_offset(&self, x: usize, y: usize) -> PixelOffset<'_> {
        PixelOffset {
            fb: self,
            offset: (x * self.pixel_format.nbytes() + y * self.line_size) as isize,
        }
    }

    /// Writes a pixel at the specified position.
    pub fn write_pixel_at(&self, offset: PixelOffset, pixel: RenderedPixel) -> Result<()> {
        // K1 MUSE Book --- (dispatch through write_bytes_at for DMA backing)
        // self.io_mem.write_bytes(offset.as_usize(), pixel.as_slice())
        self.write_bytes_at(offset.as_usize(), pixel.as_slice())
        // K1 MUSE Book ---
    }

    /// Writes raw bytes at the specified offset.
    pub fn write_bytes_at(&self, offset: usize, bytes: &[u8]) -> Result<()> {
        // K1 MUSE Book --- (IoMem or DmaCoherent)
        // self.io_mem.write_bytes(offset, bytes)
        match &self.memory {
            FbMemory::IoMem(m) => m.write_bytes(offset, bytes),
            FbMemory::Dma(m) => m.write_bytes(offset, bytes),
        }
        // K1 MUSE Book ---
    }

    // K1 MUSE Book --- (/dev/fb0 read/write on DMA-backed scanout)
    /// Reads from the framebuffer into fallible memory; returns copied length.
    pub fn read_fallible(
        &self,
        offset: usize,
        writer: &mut VmWriter,
    ) -> core::result::Result<usize, (Error, usize)> {
        match &self.memory {
            FbMemory::IoMem(m) => m.read_fallible(offset, writer),
            FbMemory::Dma(m) => {
                let len = writer.avail();
                let mut reader = m.reader();
                let limit = offset.checked_add(len).ok_or((Error::Overflow, 0))?;
                if limit > reader.remain() {
                    return Err((Error::InvalidArgs, 0));
                }
                reader.skip(offset);
                reader.to_fallible().read_fallible(writer)
            }
        }
    }

    /// Writes from fallible memory into the framebuffer; returns copied length.
    pub fn write_fallible(
        &self,
        offset: usize,
        reader: &mut VmReader,
    ) -> core::result::Result<usize, (Error, usize)> {
        match &self.memory {
            FbMemory::IoMem(m) => m.write_fallible(offset, reader),
            FbMemory::Dma(m) => {
                let len = reader.remain();
                let mut writer = m.writer();
                let limit = offset.checked_add(len).ok_or((Error::Overflow, 0))?;
                if limit > writer.avail() {
                    return Err((Error::InvalidArgs, 0));
                }
                writer.skip(offset);
                writer.to_fallible().write_fallible(reader)
            }
        }
    }
    // K1 MUSE Book ---

    /// Clears the framebuffer with default color (black).
    pub fn clear(&self) {
        // K1 MUSE Book --- (size() covers both IoMem and DMA)
        // let frame = alloc::vec![0u8; self.io_mem().size()];
        let frame = alloc::vec![0u8; self.size()];
        // K1 MUSE Book ---
        self.write_bytes_at(0, &frame).unwrap();
    }

    /// Sets color map entries starting from the given index.
    ///
    /// For efifb devices, hardware color map is not supported, so we maintain
    /// an in-memory map for software emulation.
    pub fn set_color_map(&self, start: usize, entries: &[ColorMapEntry]) -> Result<()> {
        if start > MAX_CMAP_SIZE || entries.len() > MAX_CMAP_SIZE - start {
            return Err(Error::InvalidArgs);
        }

        let mut cmap = self.cmap.lock();
        let required_len = start + entries.len();

        // Ensure the colormap has enough space
        if cmap.entries.len() < required_len {
            cmap.entries.resize(
                required_len,
                ColorMapEntry {
                    red: 0,
                    green: 0,
                    blue: 0,
                    transp: 0,
                },
            );
        }

        // Copy the entries
        cmap.entries[start..start + entries.len()].copy_from_slice(entries);

        Ok(())
    }

    /// Gets color map entries from the given range.
    pub fn get_color_map(&self, start: usize, len: usize) -> Option<Vec<ColorMapEntry>> {
        let cmap = self.cmap.lock();

        if start >= cmap.entries.len() || len > cmap.entries.len() - start {
            return None;
        }

        Some(cmap.entries[start..start + len].to_vec())
    }
}

/// The offset of a pixel in the framebuffer.
#[derive(Clone, Copy, Debug)]
pub struct PixelOffset<'a> {
    fb: &'a FrameBuffer,
    offset: isize,
}

impl PixelOffset<'_> {
    /// Adds the specified delta to the x coordinate.
    pub fn x_add(&mut self, x_delta: isize) {
        let delta = x_delta * self.fb.pixel_format.nbytes() as isize;
        self.offset += delta;
    }

    /// Adds the specified delta to the y coordinate.
    pub fn y_add(&mut self, y_delta: isize) {
        let delta = y_delta * self.fb.line_size as isize;
        self.offset += delta;
    }

    /// Returns the offset value as a `usize`.
    pub fn as_usize(&self) -> usize {
        self.offset as usize
    }
}
