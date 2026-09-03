//! The terminal window's text: a character grid the console writes into and the compositor
//! paints from (ALET-P2-021's text rung, ADR-083).
//!
//! ADR-080 gave the live desktop a window nobody could read: keystrokes reached its queue, the
//! queue was drained by nobody, and the window showed a border. This module is the text INSIDE
//! the window — the console's own byte stream, rendered as glyphs — kept deliberately small and
//! exact:
//!
//! * **The alphabet is the console's.** The grid accepts the bytes the console EMITS: printable
//!   ASCII lands in a cell, `\n` ends the line, `\r` returns to the column, backspace erases,
//!   and the editor's `ESC [ ... <final>` control sequences are consumed without painting (the
//!   grid is not a terminal emulator with cursor addressing; it shows the console's output
//!   stream the way a paper teletype would). Anything else is dropped and COUNTED — nothing
//!   unknown is ever drawn.
//! * **Scrolling is exact.** A newline on the last row moves every row up one and clears the
//!   last; a wrap at the right edge is a newline. Pixel-exact, so a proof can predict the grid.
//! * **Rendering is a pure function of the cells.** [`TextGrid::render_packed`] writes the
//!   1-bpp row-major buffer [`crate::compositor::Compositor::fill_packed`] expects (bit `i` is
//!   pixel `i`, LSB first, a cell is one 8x8 glyph of [`crate::font8x8`]) into a caller-owned
//!   buffer — no allocation per frame, on a heap that never frees (ADR-063).
//! * **A title band above the text** (`TITLE_H` pixels) belongs to the window: solid ink with
//!   the window's name knocked out, the strip a pointer drags the window by (ADR-083).

use crate::font8x8;
use alloc::vec::Vec;

/// Glyph cell size in pixels (the font is 8x8, single-struck here).
pub const CELL: u32 = 8;
/// Height of the title band above the text rows.
pub const TITLE_H: u32 = 10;
/// Width of the CLOSE BOX at the right end of the title band (ADR-084). The band is painted
/// here and hit-tested in [`crate::wm`] from this same constant, so a user can never click a
/// close box that is drawn somewhere else.
pub const CLOSE_W: u32 = 10;

/// Does a window this wide carry a close box? A band with no room for a name beside the box
/// carries none at all — the alternative is a chrome that is nearly all close box, where every
/// press near the top destroys the window. One predicate, used by the painter here and by the
/// hit test in [`crate::wm`], so painted and clickable can never disagree.
pub fn has_close_box(width: u32) -> bool {
    width >= CLOSE_W * 2
}

/// The grid: `cols` x `rows` cells of the console's alphabet, a write cursor, and counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextGrid {
    cols: u32,
    rows: u32,
    cells: Vec<u8>,
    col: u32,
    row: u32,
    /// Inside an `ESC [` sequence: bytes are consumed until the final byte (0x40..=0x7E).
    csi: u8, // 0 = none, 1 = saw ESC, 2 = in CSI
    /// Lines completed (newlines seen or wraps taken) since creation.
    lines: u64,
    /// Bytes refused: not printable, not a control this grid models.
    refused: u64,
    /// Something changed since the last `take_dirty`.
    dirty: bool,
}

impl TextGrid {
    /// A blank grid. `cols`/`rows` are clamped to at least 1 so a degenerate window still works.
    pub fn new(cols: u32, rows: u32) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        TextGrid {
            cols,
            rows,
            cells: alloc::vec![b' '; (cols * rows) as usize],
            col: 0,
            row: 0,
            csi: 0,
            lines: 0,
            refused: 0,
            dirty: true,
        }
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }
    pub fn rows(&self) -> u32 {
        self.rows
    }
    /// Pixel size of the surface this grid paints: text rows plus the title band.
    pub fn pixel_size(&self) -> (u32, u32) {
        (self.cols * CELL, self.rows * CELL + TITLE_H)
    }
    pub fn lines(&self) -> u64 {
        self.lines
    }
    pub fn refused(&self) -> u64 {
        self.refused
    }
    pub fn cursor(&self) -> (u32, u32) {
        (self.col, self.row)
    }
    /// Whether anything changed since the last call; clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.dirty, false)
    }

    /// One row's text, trailing blanks trimmed.
    pub fn line(&self, row: u32) -> &[u8] {
        if row >= self.rows {
            return &[];
        }
        let start = (row * self.cols) as usize;
        let s = &self.cells[start..start + self.cols as usize];
        let end = s
            .iter()
            .rposition(|&b| b != b' ')
            .map(|p| p + 1)
            .unwrap_or(0);
        &s[..end]
    }
    /// The row the cursor is on.
    pub fn current_line(&self) -> &[u8] {
        self.line(self.row)
    }
    /// The last row with any text on it, searching up from the cursor — what a reader sees as
    /// "the last thing the console said" even while the console is mid-line.
    pub fn last_nonblank_line(&self) -> &[u8] {
        let mut r = self.row;
        loop {
            let l = self.line(r);
            if !l.is_empty() || r == 0 {
                return l;
            }
            r -= 1;
        }
    }

    /// Accept one byte of the console's output stream.
    pub fn put(&mut self, b: u8) {
        match self.csi {
            1 => {
                // After ESC: `[` opens a CSI; anything else ends the escape unpainted.
                self.csi = if b == b'[' { 2 } else { 0 };
                return;
            }
            2 => {
                if (0x40..=0x7E).contains(&b) {
                    self.csi = 0;
                }
                return;
            }
            _ => {}
        }
        match b {
            0x1B => self.csi = 1,
            b'\n' => self.newline(),
            b'\r' => {
                self.col = 0;
            }
            0x08 | 0x7F => {
                if self.col > 0 {
                    self.col -= 1;
                    let i = (self.row * self.cols + self.col) as usize;
                    self.cells[i] = b' ';
                    self.dirty = true;
                }
            }
            0x20..=0x7E => {
                if self.col >= self.cols {
                    self.newline();
                }
                let i = (self.row * self.cols + self.col) as usize;
                self.cells[i] = b;
                self.col += 1;
                self.dirty = true;
            }
            _ => self.refused += 1,
        }
    }

    /// Blank every cell and put the write cursor home. A panel that REPAINTS itself (the
    /// desktop's monitor window, ADR-084) is not a teletype: it says what is true now, rather
    /// than scrolling a history of what was true.
    pub fn clear(&mut self) {
        for c in self.cells.iter_mut() {
            *c = b' ';
        }
        self.col = 0;
        self.row = 0;
        self.csi = 0;
        self.dirty = true;
    }

    /// Accept a whole string of the console's output.
    pub fn write(&mut self, s: &[u8]) {
        for &b in s {
            self.put(b);
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        self.lines += 1;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            let w = self.cols as usize;
            self.cells.copy_within(w.., 0);
            let last = (self.rows as usize - 1) * w;
            for c in &mut self.cells[last..] {
                *c = b' ';
            }
        }
        self.dirty = true;
    }

    /// Bytes `render_packed` needs for this grid's surface.
    pub fn packed_len(&self) -> usize {
        let (w, h) = self.pixel_size();
        (w as usize * h as usize).div_ceil(8)
    }

    /// Paint the title band and every cell into `out` (row-major 1-bpp, LSB first — exactly
    /// what `Compositor::fill_packed` consumes). `out` is resized to `packed_len` once and
    /// then reused; nothing else is allocated. Returns false if `title` had a byte the font
    /// cannot serve (it is painted with what it can).
    pub fn render_packed(&self, title: &[u8], out: &mut Vec<u8>) -> bool {
        let (w, h) = self.pixel_size();
        let need = self.packed_len();
        if out.len() != need {
            out.resize(need, 0);
        }
        for b in out.iter_mut() {
            *b = 0;
        }
        let mut set = |x: u32, y: u32, ink: bool| {
            if x < w && y < h {
                let i = (y * w + x) as usize;
                if ink {
                    out[i / 8] |= 1 << (i % 8);
                } else {
                    out[i / 8] &= !(1 << (i % 8));
                }
            }
        };
        // Title band: solid ink, one blank line below it, the name knocked out in the middle.
        for y in 0..TITLE_H - 1 {
            for x in 0..w {
                set(x, y, true);
            }
        }
        let mut all_served = true;
        for (k, &ch) in title.iter().enumerate() {
            let x0 = 4 + k as u32 * CELL;
            let title_limit = if has_close_box(w) { w - CLOSE_W } else { w };
            if x0 + CELL > title_limit {
                break;
            }
            let g = match font8x8::glyph(ch) {
                Some(g) => *g,
                None => {
                    all_served = false;
                    continue;
                }
            };
            for (r, bits) in g.iter().enumerate() {
                for bit in 0..8u32 {
                    if bits & (1 << bit) != 0 {
                        set(x0 + bit, 1 + r as u32, false);
                    }
                }
            }
        }
        // The close box: an 'x' knocked out of the right end of the band, over a knocked-out
        // gap that separates it from the name. The manager hit-tests exactly these pixels.
        if has_close_box(w) {
            let x0 = w - CLOSE_W;
            for y in 0..TITLE_H - 1 {
                set(x0, y, false);
            }
            if let Some(g) = font8x8::glyph(b'x') {
                for (r, bits) in g.iter().enumerate() {
                    for bit in 0..8u32 {
                        if bits & (1 << bit) != 0 {
                            set(x0 + 1 + bit, 1 + r as u32, false);
                        }
                    }
                }
            }
        }
        // Cells: glyphs in ink on the blank text area.
        for row in 0..self.rows {
            for col in 0..self.cols {
                let ch = self.cells[(row * self.cols + col) as usize];
                if ch == b' ' {
                    continue;
                }
                let g = match font8x8::glyph(ch) {
                    Some(g) => *g,
                    None => continue,
                };
                let x0 = col * CELL;
                let y0 = TITLE_H + row * CELL;
                for (r, bits) in g.iter().enumerate() {
                    for bit in 0..8u32 {
                        if bits & (1 << bit) != 0 {
                            set(x0 + bit, y0 + r as u32, true);
                        }
                    }
                }
            }
        }
        all_served
    }

    /// Is a window-local point inside the title band (the strip a pointer drags by)?
    pub fn in_title(&self, local_x: i32, local_y: i32) -> bool {
        let (w, _) = self.pixel_size();
        local_x >= 0 && (local_x as u32) < w && local_y >= 0 && (local_y as u32) < TITLE_H
    }
}

/// A grid accepts formatted output like any writer, so a caller with `write!` needs no
/// intermediate buffer on a heap that never frees (ADR-063). Every byte still goes through
/// [`TextGrid::put`], so the alphabet and the refusal counter are exactly the same.
impl core::fmt::Write for TextGrid {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.write(s.as_bytes());
        Ok(())
    }
}

/// The boot suite for the grid (ADR-083): arch-neutral, allocation-bounded, pixel-exact.
pub fn textgrid_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // 1 — printable bytes land in cells in order; `\r` returns; backspace erases exactly one.
    {
        let mut g = TextGrid::new(8, 2);
        g.write(b"abc\x08d\r");
        check!(
            g.line(0) == b"abd" && g.cursor() == (0, 0) && g.refused() == 0,
            "textgrid: printable bytes land in cells, carriage return returns, backspace erases one"
        );
    }
    // 2 — newline moves down; on the last row it SCROLLS exactly one row and the last is blank.
    {
        let mut g = TextGrid::new(4, 2);
        g.write(b"one\ntwo\nthr");
        check!(
            g.line(0) == b"two" && g.line(1) == b"thr" && g.lines() == 2,
            "textgrid: a newline on the last row scrolls exactly one row"
        );
    }
    // 3 — a wrap at the right edge is a newline: the fifth byte of a 4-wide row starts row 1.
    {
        let mut g = TextGrid::new(4, 3);
        g.write(b"12345");
        check!(
            g.line(0) == b"1234" && g.line(1) == b"5" && g.cursor() == (1, 1),
            "textgrid: a wrap at the right edge is a newline"
        );
    }
    // 4 — the editor's control sequences are consumed unpainted; unknown bytes are refused and
    //     counted, never drawn.
    {
        let mut g = TextGrid::new(8, 1);
        g.write(b"a\x1b[Kb\x1b[3~c\x01\xff");
        check!(
            g.line(0) == b"abc" && g.refused() == 2,
            "textgrid: control sequences are consumed unpainted and unknown bytes are refused, counted"
        );
    }
    // 5 — rendering is exact: the packed buffer has the fill_packed length, the title band is
    //     solid ink on its rows, the text area is blank where no glyph sits, and glyph 'A''s
    //     bits land at the cell's pixels in the font's own row order.
    {
        let mut g = TextGrid::new(2, 1);
        g.put(b'A');
        let mut out = Vec::new();
        let served = g.render_packed(b"", &mut out);
        let (w, _) = g.pixel_size();
        let px = |x: u32, y: u32| out[((y * w + x) / 8) as usize] & (1 << ((y * w + x) % 8)) != 0;
        let glyph = font8x8::glyph(b'A').unwrap();
        let mut glyph_ok = true;
        for (r, bits) in glyph.iter().enumerate() {
            for bit in 0..8u32 {
                glyph_ok &= px(bit, TITLE_H + r as u32) == (bits & (1 << bit) != 0);
            }
        }
        check!(
            served
                && out.len() == g.packed_len()
                && (0..w).all(|x| px(x, 0) && px(x, TITLE_H - 2))
                && (0..w).all(|x| !px(x, TITLE_H - 1))
                && glyph_ok
                && (0..8u32).all(|r| !px(CELL + 3, TITLE_H + r)),
            "textgrid: rendering is pixel-exact - solid title band, glyph bits at the cell, blank elsewhere"
        );
    }
    // 6 — the render buffer is reused, not reallocated: a second render into the same buffer
    //     keeps its capacity, and the dirty flag is a one-shot.
    {
        let mut g = TextGrid::new(3, 1);
        let mut out = Vec::new();
        g.render_packed(b"t", &mut out);
        let cap = out.capacity();
        g.put(b'x');
        let d1 = g.take_dirty();
        let d2 = g.take_dirty();
        g.render_packed(b"t", &mut out);
        check!(
            out.capacity() == cap
                && d1
                && !d2
                && g.in_title(0, 0)
                && !g.in_title(0, TITLE_H as i32),
            "textgrid: the render buffer is reused and dirty is a one-shot"
        );
    }
    // 7 — the close box is PAINTED where the window manager hit-tests it (ADR-084): the
    //     rightmost CLOSE_W pixels of the band carry the knocked-out 'x' glyph, the column
    //     that separates it from the name is clear, and the title text stops before it.
    {
        let g = TextGrid::new(8, 1);
        let mut out = Vec::new();
        g.render_packed(b"abcdefgh", &mut out);
        let (w, _) = g.pixel_size();
        let px = |x: u32, y: u32| out[((y * w + x) / 8) as usize] & (1 << ((y * w + x) % 8)) != 0;
        let glyph = font8x8::glyph(b'x').unwrap();
        let x0 = w - CLOSE_W;
        let mut box_ok = true;
        for (r, bits) in glyph.iter().enumerate() {
            for bit in 0..8u32 {
                if bits & (1 << bit) != 0 {
                    box_ok &= !px(x0 + 1 + bit, 1 + r as u32);
                }
            }
        }
        // The title's last glyph cell must end before the close box begins.
        let title_clear = (0..TITLE_H - 1).all(|y| !px(x0, y));
        check!(
            box_ok && title_clear && (0..TITLE_H - 1).all(|y| px(x0 - 1, y)),
            "textgrid: the close box is painted exactly where the window manager hit-tests it"
        );
    }
    Ok(n)
}
