//! fbcon — a software text console over a SCATTER-GATHER BGRA framebuffer (REQ-GFX-002).
//!
//! The GPU driver proved last wave that pixels can reach the device; what was missing was pixels
//! worth sending. This module owns everything between "characters to show" and "bytes in a frame
//! buffer": an embedded public-domain 8x8 font ([`crate::font8x8`]), double-struck vertically into
//! 8x16 cells, a cursor/wrap/scroll state machine, and a [`Surface`] whose pixels may be scattered
//! across hundreds of single frames — because a virtio-gpu backing store is scatter-gather BY
//! DESIGN (`virtio_gpu_mem_entry` lists arbitrary frames), not a bug to paper over with a
//! contiguity requirement no target allocator gives us.
//!
//! Everything here is pure computation over memory the caller already owns: no device, no locks, no
//! allocation. That is what makes the whole thing host-provable at PIXEL granularity — the VM gate
//! then proves the rendered frame really reaches hardware.
//!
//! Refusals, stated: a pixel outside the surface is [`FbError::OffSurface`] (never wrapped — a
//! wrapped pixel corrupts someone else's row); a control byte the console has no rule for is
//! [`FbError::UnknownControl`] (the same doctrine as the serial console's fail-closed filter);
//! geometry that cannot hold one cell is [`FbError::BadGeometry`].

use crate::dma::PAGE;
use crate::font8x8::{self, FONT8X8};

/// Foreground: opaque white, BGRA byte order (what VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM expects).
pub const FG: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
/// Background: opaque black.
pub const BG: [u8; 4] = [0x00, 0x00, 0x00, 0xFF];

/// Cell geometry: the 8-wide font double-struck to a classic 16-pixel line.
pub const CELL_W: u32 = 8;
pub const CELL_H: u32 = 16;

/// Why a framebuffer operation failed. Every variant is a refusal with a reason; none panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbError {
    /// A pixel outside the surface. Wrapping would corrupt another row silently.
    OffSurface,
    /// A control byte the console has no rule for. State is untouched.
    UnknownControl(u8),
    /// Geometry that cannot hold even one character cell.
    BadGeometry,
    /// The page list does not cover width x height x 4 bytes.
    TooSmall,
}

/// A pixel surface whose bytes live across `pages.len()` LOGICALLY-consecutive 4 KiB frames — the
/// exact shape of a virtio-gpu backing store. Pixel (x, y) is byte `((y * width + x) * 4)` of the
/// concatenation; its page is that offset divided by [`PAGE`].
///
/// # Safety
/// Every address in `pages` must be a WRITABLE, exclusively-owned 4 KiB frame for as long as the
/// surface exists (on the targets: identity-mapped allocator frames; on the host: real memory a
/// test owns). The surface reads and writes only inside those frames.
pub struct Surface<'a> {
    pages: &'a [usize],
    pub width: u32,
    pub height: u32,
}

impl<'a> Surface<'a> {
    /// Build a surface over `pages`. Refuses geometry that cannot hold one pixel and a page list
    /// that does not cover the bytes — fail-closed before any write happens.
    pub fn new(pages: &'a [usize], width: u32, height: u32) -> Result<Self, FbError> {
        if width == 0 || height == 0 {
            return Err(FbError::BadGeometry);
        }
        let need = width as usize * height as usize * 4;
        if pages.len() * PAGE < need {
            return Err(FbError::TooSmall);
        }
        Ok(Surface {
            pages,
            width,
            height,
        })
    }

    fn locate(&self, x: u32, y: u32) -> Result<(usize, usize), FbError> {
        if x >= self.width || y >= self.height {
            return Err(FbError::OffSurface);
        }
        let off = (y as usize * self.width as usize + x as usize) * 4;
        Ok((off / PAGE, off % PAGE))
    }

    /// Set one pixel to ink or background. Out-of-surface is a refusal, never a wrap.
    pub fn set(&mut self, x: u32, y: u32, ink: bool) -> Result<(), FbError> {
        let (p, o) = self.locate(x, y)?;
        let c = if ink { FG } else { BG };
        let addr = match self.pages.get(p) {
            Some(a) => *a + o,
            None => return Err(FbError::OffSurface),
        };
        let base = addr as *mut u8;
        // SAFETY: `locate` proved this byte lies inside frame `pages[p]`, exclusively ours for the
        // surface's lifetime (see the type's safety contract).
        for (i, b) in c.iter().enumerate() {
            unsafe {
                base.add(i).write_volatile(*b);
            }
        }
        Ok(())
    }

    /// Read one pixel back. The renderer's readback is exactly what host tests and the device
    /// suite assert against — a renderer nobody can read cannot be proved.
    pub fn get(&self, x: u32, y: u32) -> Result<bool, FbError> {
        let (p, o) = self.locate(x, y)?;
        let addr = match self.pages.get(p) {
            Some(a) => *a + o,
            None => return Err(FbError::OffSurface),
        };
        // SAFETY: same region proof as `set`.
        Ok(unsafe { (addr as *const u8).read_volatile() } != 0)
    }

    /// Fill every pixel.
    pub fn fill(&mut self, ink: bool) {
        for y in 0..self.height {
            for x in 0..self.width {
                let _ = self.set(x, y, ink);
            }
        }
    }
}

/// The text console state machine: a cursor over a grid of 8x16 cells, with wrap, scroll,
/// backspace, and the fail-closed rule for control bytes. Pure layout logic — every pixel goes
/// through the [`Surface`] it is handed.
#[derive(Clone, Copy, Debug)]
pub struct TextConsole {
    cols: u32,
    rows: u32,
    col: u32,
    row: u32,
}

impl TextConsole {
    /// A console over a surface of the given pixel geometry. Refuses anything smaller than one
    /// cell.
    pub fn new(width: u32, height: u32) -> Result<Self, FbError> {
        if width < CELL_W || height < CELL_H {
            return Err(FbError::BadGeometry);
        }
        Ok(TextConsole {
            cols: width / CELL_W,
            rows: height / CELL_H,
            col: 0,
            row: 0,
        })
    }

    pub fn cursor(&self) -> (u32, u32) {
        (self.col, self.row)
    }

    /// Write one byte. Printable ASCII blits its glyph; LF moves down (scrolling at the bottom);
    /// CR returns; BS blanks the cell behind the cursor. ANY other control byte is refused BY
    /// NAME with the state untouched — the same doctrine as the serial console's editor.
    pub fn putc(&mut self, surf: &mut Surface, ch: u8) -> Result<(), FbError> {
        match ch {
            b'\n' => self.newline(surf)?,
            b'\r' => self.col = 0,
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                    self.blank_cell(surf)?;
                }
            }
            0x20..=0x7E => {
                self.blit(surf, ch)?;
                self.col += 1;
                if self.col >= self.cols {
                    self.newline(surf)?;
                }
            }
            _ => return Err(FbError::UnknownControl(ch)),
        }
        Ok(())
    }

    /// Write a whole slice, stopping at the first refusal (the cursor names where it stopped).
    pub fn print(&mut self, surf: &mut Surface, s: &[u8]) -> Result<(), FbError> {
        for &b in s {
            self.putc(surf, b)?;
        }
        Ok(())
    }

    /// Clear to background and home the cursor.
    pub fn clear(&mut self, surf: &mut Surface) {
        surf.fill(false);
        self.col = 0;
        self.row = 0;
    }

    fn newline(&mut self, surf: &mut Surface) -> Result<(), FbError> {
        self.col = 0;
        self.row += 1;
        if self.row >= self.rows {
            self.row = self.rows - 1;
            Self::scroll(surf)?;
        }
        Ok(())
    }

    fn blank_cell(&self, surf: &mut Surface) -> Result<(), FbError> {
        let x0 = self.col * CELL_W;
        let y0 = self.row * CELL_H;
        for y in y0..y0 + CELL_H {
            for x in x0..x0 + CELL_W {
                surf.set(x, y, false)?;
            }
        }
        Ok(())
    }

    /// Blit one glyph, each font row drawn TWICE — an 8-tall font on a 16-tall line, the classic
    /// double-strike.
    fn blit(&self, surf: &mut Surface, ch: u8) -> Result<(), FbError> {
        let glyph = match font8x8::glyph(ch) {
            Some(g) => *g,
            None => FONT8X8[0], // unreachable: glyph() serves every byte
        };
        let x0 = self.col * CELL_W;
        let y0 = self.row * CELL_H;
        for (r, bits) in glyph.iter().enumerate() {
            for bit in 0..8 {
                let ink = bits & (1 << bit) != 0;
                let px = x0 + bit;
                let py0 = y0 + (r as u32) * 2;
                surf.set(px, py0, ink)?;
                surf.set(px, py0 + 1, ink)?;
            }
        }
        Ok(())
    }

    /// Move rows 16.. up one line and clear the last line. Pixel-exact, so scrolling is provable.
    fn scroll(surf: &mut Surface) -> Result<(), FbError> {
        for y in 0..surf.height - CELL_H {
            for x in 0..surf.width {
                let v = surf.get(x, y + CELL_H)?;
                surf.set(x, y, v)?;
            }
        }
        for y in surf.height - CELL_H..surf.height {
            for x in 0..surf.width {
                surf.set(x, y, false)?;
            }
        }
        Ok(())
    }
}
