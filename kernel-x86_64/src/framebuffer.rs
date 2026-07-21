//! GOP framebuffer text console — the human-visible boot channel (what you watch in a VM window).
//!
//! The base pointer + geometry are captured from the UEFI Graphics Output Protocol BEFORE
//! `ExitBootServices`; the linear framebuffer is identity-mapped MMIO that stays valid after the
//! firmware is gone. We assume a 32-bpp RGB/BGR mode (the only formats OVMF/VMware expose with a
//! direct framebuffer); `BltOnly` has no linear buffer and disables framebuffer text (serial still
//! carries the full log). Glyphs come from the public-domain `font8x8` legacy set, drawn at 2x.

use font8x8::legacy::BASIC_LEGACY;

const SCALE: usize = 2;
const GLYPH: usize = 8;
const CELL_W: usize = GLYPH * SCALE;
const CELL_H: usize = GLYPH * SCALE;
const FG: u32 = 0x00FF_FFFF; // white (symmetric across RGB/BGR)
const BG: u32 = 0x0000_0000; // black

pub struct FrameBuffer {
    base: *mut u8,
    width: usize,
    height: usize,
    stride: usize, // pixels per scanline
    _bgr: bool,
    cx: usize,
    cy: usize,
}

impl FrameBuffer {
    /// # Safety
    /// `base` must be the linear framebuffer from GOP for a 32-bpp mode with `stride` pixels per
    /// scanline and at least `height` scanlines; the region must be exclusively ours (post-exit).
    pub unsafe fn new(base: *mut u8, width: usize, height: usize, stride: usize, bgr: bool) -> Self {
        let mut fb = FrameBuffer { base, width, height, stride, _bgr: bgr, cx: 0, cy: 0 };
        fb.clear();
        fb
    }

    fn put(&mut self, x: usize, y: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = (y * self.stride + x) * 4;
        // SAFETY: bounds checked above; stride*height*4 <= framebuffer size.
        unsafe { core::ptr::write_volatile(self.base.add(offset) as *mut u32, color) }
    }

    fn clear(&mut self) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put(x, y, BG);
            }
        }
        self.cx = 0;
        self.cy = 0;
    }

    fn glyph(&mut self, byte: u8) {
        let rows = BASIC_LEGACY[byte as usize];
        for (ry, bits) in rows.iter().enumerate() {
            for cx in 0..GLYPH {
                // font8x8 legacy: bit 0 (LSB) is the leftmost column.
                if bits & (1 << cx) != 0 {
                    for sy in 0..SCALE {
                        for sx in 0..SCALE {
                            self.put(self.cx + cx * SCALE + sx, self.cy + ry * SCALE + sy, FG);
                        }
                    }
                }
            }
        }
    }

    fn newline(&mut self) {
        self.cx = 0;
        self.cy += CELL_H;
        // No costly framebuffer scroll: on overflow, wrap to top and clear. Serial keeps the full
        // log; the framebuffer shows the most recent screenful.
        if self.cy + CELL_H > self.height {
            self.clear();
        }
    }

    pub fn write_str(&mut self, s: &str) {
        for ch in s.chars() {
            match ch {
                '\n' => self.newline(),
                '\r' => self.cx = 0,
                c => {
                    let b = if c.is_ascii() { c as u8 } else { b'?' };
                    self.glyph(b);
                    self.cx += CELL_W;
                    if self.cx + CELL_W > self.width {
                        self.newline();
                    }
                }
            }
        }
    }
}
