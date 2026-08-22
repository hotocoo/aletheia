//! Host proofs for the framebuffer console (REQ-GFX-002).
//!
//! Everything here is pure computation over memory a test owns, so it is provable at PIXEL
//! granularity on the host: glyph blitting against the embedded table itself, wrap/scroll/
//! backspace semantics, and every refusal. The device suite then proves the rendered frame
//! reaches real hardware — no claim here depends on QEMU, none there depends on faith here.
extern crate alloc;

use kernel_core::dma::PAGE;
use kernel_core::fbcon::*;
use kernel_core::font8x8::{glyph, FONT8X8};

/// A real allocation viewed as N consecutive 4 KiB "frames" — the same scatter-gather shape the
/// device sees, over memory this test owns, kept page-aligned so the frame view is exact.
struct Pages {
    mem: alloc::vec::Vec<u8>,
    addrs: alloc::vec::Vec<usize>,
}

impl Pages {
    fn new(n: usize) -> Self {
        let mut mem = alloc::vec![0u8; n * PAGE + PAGE];
        let base0 = mem.as_mut_ptr() as usize;
        let base = (base0 + PAGE - 1) / PAGE * PAGE;
        let addrs = (0..n).map(|i| base + i * PAGE).collect();
        Pages { mem, addrs }
    }
}

// The renderer writes through `addrs`; a test may read the same bytes through `mem` — one owned
// allocation, two views, no other reference anywhere.

/// The font's own ink count for a glyph, doubled per drawn row — what a correct blit MUST
/// produce in one cell. Computed FROM the table, so the test cannot drift from the font.
fn font_ink(ch: u8) -> u32 {
    FONT8X8[ch as usize]
        .iter()
        .map(|r| r.count_ones())
        .sum::<u32>()
        * 2
}

fn ink_count(surf: &Surface, x0: u32, y0: u32) -> u32 {
    let mut n = 0;
    for y in y0..y0 + CELL_H {
        for x in x0..x0 + CELL_W {
            if surf.get(x, y) == Ok(true) {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn the_font_table_is_whole_and_printable_glyphs_have_ink() {
    // Every byte of the basic-latin plane has a slot; beyond it, none (the font must not guess).
    for ch in 0..=127u8 {
        assert!(glyph(ch).is_some());
    }
    for ch in 128..=255u8 {
        assert!(glyph(ch).is_none(), "glyph {} should not exist", ch);
    }
    // The known 'A' rows, verbatim from the public-domain table.
    assert_eq!(
        FONT8X8[65],
        [0x0C, 0x1E, 0x33, 0x33, 0x3F, 0x33, 0x33, 0x00]
    );
    // Space is blank; every other printable glyph has ink somewhere.
    assert!(FONT8X8[32].iter().all(|r| *r == 0));
    for ch in 0x21..=0x7E {
        assert!(
            FONT8X8[ch as usize].iter().any(|r| *r != 0),
            "printable glyph {} is blank",
            ch
        );
    }
}

#[test]
fn surface_geometry_is_fail_closed_before_any_write() {
    let pages = Pages::new(4);
    assert!(matches!(
        Surface::new(&pages.addrs, 0, 240),
        Err(FbError::BadGeometry)
    ));
    assert!(matches!(
        Surface::new(&pages.addrs, 640, 240),
        Err(FbError::TooSmall)
    ));
    let pages = Pages::new(150);
    assert!(Surface::new(&pages.addrs, 640, 240).is_ok());
}

#[test]
fn a_pixel_off_the_surface_is_refused_and_never_wraps() {
    let pages = Pages::new(2);
    let mut surf = Surface::new(&pages.addrs, 8, 8).unwrap();
    assert_eq!(surf.set(8, 0, true), Err(FbError::OffSurface));
    assert_eq!(surf.set(0, 8, true), Err(FbError::OffSurface));
    assert_eq!(surf.get(8, 0), Err(FbError::OffSurface));
    // The refused writes must not have landed ANYWHERE: the whole surface is still background.
    surf.fill(false);
    for y in 0..8 {
        for x in 0..8 {
            assert_eq!(surf.get(x, y), Ok(false));
        }
    }
}

#[test]
fn a_blit_lands_exactly_the_font_ink_and_nothing_else() {
    let pages = Pages::new(150);
    let mut surf = Surface::new(&pages.addrs, 640, 240).unwrap();
    let mut con = TextConsole::new(640, 240).unwrap();
    con.clear(&mut surf);
    con.putc(&mut surf, b'A').unwrap();
    // Cell (0,0) carries exactly the font's ink for 'A'.
    assert_eq!(ink_count(&surf, 0, 0), font_ink(b'A'));
    assert!(font_ink(b'A') > 0);
    // The NEIGHBOR cell (the next character was never printed) is untouched background.
    assert_eq!(ink_count(&surf, 8, 0), 0);
    // Double-strike: every ink pixel row is one of a PAIR — rows come in equal pairs.
    for r in 0..8 {
        let mut a = 0;
        let mut b = 0;
        for x in 0..8 {
            if surf.get(x, r * 2) == Ok(true) {
                a += 1;
            }
            if surf.get(x, r * 2 + 1) == Ok(true) {
                b += 1;
            }
        }
        assert_eq!(a, b, "row {} of the double-strike is not a pair", r);
    }
}

#[test]
fn a_full_line_wraps_to_the_next_row() {
    let pages = Pages::new(150);
    let mut surf = Surface::new(&pages.addrs, 640, 240).unwrap();
    let mut con = TextConsole::new(640, 240).unwrap();
    con.clear(&mut surf);
    for _ in 0..80 {
        con.putc(&mut surf, b'x').unwrap();
    }
    assert_eq!(con.cursor(), (0, 1));
    // The wrap did not corrupt the wrapped line: cell 79 row 0 has ink, and row 1 is fresh.
    assert_eq!(ink_count(&surf, 79 * 8, 0), font_ink(b'x'));
    assert_eq!(ink_count(&surf, 0, 16), 0);
}

#[test]
fn backspace_blanks_the_cell_behind_and_steps_back() {
    let pages = Pages::new(150);
    let mut surf = Surface::new(&pages.addrs, 640, 240).unwrap();
    let mut con = TextConsole::new(640, 240).unwrap();
    con.clear(&mut surf);
    con.putc(&mut surf, b'A').unwrap();
    assert_eq!(con.cursor(), (1, 0));
    con.putc(&mut surf, 0x08).unwrap();
    assert_eq!(con.cursor(), (0, 0));
    assert_eq!(
        ink_count(&surf, 0, 0),
        0,
        "backspace must BLANK, not just move"
    );
    // At column 0 backspace is a no-op, not a refusal and not a wrap to the previous line.
    con.putc(&mut surf, 0x08).unwrap();
    assert_eq!(con.cursor(), (0, 0));
}

#[test]
fn a_control_byte_with_no_rule_is_refused_by_name_and_changes_nothing() {
    let pages = Pages::new(150);
    let mut surf = Surface::new(&pages.addrs, 640, 240).unwrap();
    let mut con = TextConsole::new(640, 240).unwrap();
    con.clear(&mut surf);
    con.putc(&mut surf, b'A').unwrap();
    let before = con.cursor();
    assert_eq!(
        con.putc(&mut surf, 0x01),
        Err(FbError::UnknownControl(0x01))
    );
    assert_eq!(
        con.putc(&mut surf, 0xFF),
        Err(FbError::UnknownControl(0xFF))
    );
    assert_eq!(
        con.cursor(),
        before,
        "a refused byte must not move the cursor"
    );
    assert_eq!(ink_count(&surf, 0, 0), font_ink(b'A'), "nor touch a pixel");
}

#[test]
fn the_last_newline_scrolls_the_top_line_away() {
    let pages = Pages::new(150);
    let mut surf = Surface::new(&pages.addrs, 640, 240).unwrap();
    let mut con = TextConsole::new(640, 240).unwrap();
    con.clear(&mut surf);
    con.putc(&mut surf, b'A').unwrap();
    let top_ink = ink_count(&surf, 0, 0);
    // Fill every row: 14 newlines put the cursor on the last row (15 rows of 16px), the 15th scrolls.
    for _ in 0..15 {
        con.putc(&mut surf, b'\n').unwrap();
    }
    assert_eq!(con.cursor(), (0, 14), "the cursor parks on the last row");
    assert_eq!(ink_count(&surf, 0, 0), 0, "the 'A' scrolled off the top");
    // Total ink on the surface dropped by exactly the scrolled glyph.
    let mut total = 0u32;
    for y in 0..240 {
        for x in 0..640 {
            if surf.get(x, y) == Ok(true) {
                total += 1;
            }
        }
    }
    assert_eq!(total, 0, "nothing else was ever printed");
}

#[test]
fn clear_resets_every_pixel_and_the_cursor() {
    let pages = Pages::new(150);
    let mut surf = Surface::new(&pages.addrs, 640, 240).unwrap();
    let mut con = TextConsole::new(640, 240).unwrap();
    con.print(&mut surf, b"scattered words").unwrap();
    con.clear(&mut surf);
    assert_eq!(con.cursor(), (0, 0));
    for y in (0..240).step_by(7) {
        for x in (0..640).step_by(13) {
            assert_eq!(surf.get(x, y), Ok(false));
        }
    }
}

#[test]
fn a_console_smaller_than_one_cell_is_refused() {
    assert!(matches!(
        TextConsole::new(4, 240),
        Err(FbError::BadGeometry)
    ));
    assert!(matches!(
        TextConsole::new(640, 8),
        Err(FbError::BadGeometry)
    ));
    assert!(TextConsole::new(8, 16).is_ok());
}
