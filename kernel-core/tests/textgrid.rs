//! Host proofs for the terminal window's text grid (ALET-P2-021's text rung, ADR-083).
use kernel_core::font8x8;
use kernel_core::textgrid::{textgrid_suite, TextGrid, CELL, TITLE_H};

#[test]
fn the_boot_suite_passes_on_the_host() {
    let mut seen = 0;
    let n = textgrid_suite(|k, ok, name| {
        seen += 1;
        assert_eq!(k, seen);
        assert!(ok, "{name}");
    })
    .unwrap();
    assert_eq!(n, 6);
}

#[test]
fn scrolling_is_exact_over_many_rows() {
    let mut g = TextGrid::new(3, 3);
    for i in 0..10u8 {
        g.write(&[b'0' + i, b'\n']);
    }
    // Ten lines through a three-row grid: the last newline scrolled once more, so the bottom
    // row is blank and the two above hold the last two completed lines.
    assert_eq!(
        (g.line(0), g.line(1), g.line(2)),
        (&b"8"[..], &b"9"[..], &b""[..])
    );
    assert_eq!(g.lines(), 10);
    assert_eq!(g.cursor(), (0, 2));
}

#[test]
fn the_editors_sequences_never_paint_and_a_bare_escape_swallows_one_byte() {
    let mut g = TextGrid::new(16, 1);
    g.write(b"x\x1b[2J\x1bz\x1b[Ay");
    assert_eq!(g.line(0), b"xy");
    assert_eq!(g.refused(), 0);
}

#[test]
fn backspace_at_column_zero_changes_nothing() {
    let mut g = TextGrid::new(4, 2);
    g.write(b"ab\n");
    g.put(0x08);
    assert_eq!(g.line(0), b"ab");
    assert_eq!(g.cursor(), (0, 1));
}

#[test]
fn the_rendered_title_knocks_the_name_out_of_solid_ink() {
    let g = TextGrid::new(6, 1);
    let mut out = Vec::new();
    assert!(g.render_packed(b"A", &mut out));
    let (w, _) = g.pixel_size();
    let px = |x: u32, y: u32| out[((y * w + x) / 8) as usize] & (1 << ((y * w + x) % 8)) != 0;
    let glyph = font8x8::glyph(b'A').unwrap();
    // Row 1..=8 of the band carries the glyph inverted at x offset 4.
    for (r, bits) in glyph.iter().enumerate() {
        for bit in 0..8u32 {
            let ink_expected = bits & (1 << bit) == 0; // knocked out where the glyph has ink
            assert_eq!(px(4 + bit, 1 + r as u32), ink_expected, "r={r} bit={bit}");
        }
    }
    // Outside the name the band is solid; the separator row below is blank.
    assert!(px(w - 1, 0) && px(w - 1, TITLE_H - 2));
    assert!((0..w).all(|x| !px(x, TITLE_H - 1)));
    // A byte the font cannot serve is reported.
    let mut out2 = Vec::new();
    assert!(!g.render_packed(&[0x80], &mut out2));
}

#[test]
fn pixel_size_and_packed_len_agree_with_fill_packed() {
    use kernel_core::compositor::Compositor;
    let g = TextGrid::new(40, 12);
    let (w, h) = g.pixel_size();
    assert_eq!((w, h), (40 * CELL, 12 * CELL + TITLE_H));
    let mut comp = Compositor::new(7, 640, 240);
    let tok = comp.mint_surface(1, w, h).unwrap();
    let mut out = Vec::new();
    g.render_packed(b"aletheia", &mut out);
    assert_eq!(out.len(), g.packed_len());
    comp.fill_packed(1, tok, &out).unwrap();
}

#[test]
fn rendering_is_deterministic() {
    let mut a = TextGrid::new(10, 3);
    let mut b = TextGrid::new(10, 3);
    for g in [&mut a, &mut b] {
        g.write(b"aletheia> help\ncommands:\n  ls\n");
    }
    let (mut oa, mut ob) = (Vec::new(), Vec::new());
    a.render_packed(b"term", &mut oa);
    b.render_packed(b"term", &mut ob);
    assert_eq!(oa, ob);
    assert_eq!(a, b);
}
