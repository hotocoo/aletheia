//! Host proofs for the composition contract's REAL-pixel sink (REQ-GFX-003, ADR-078).
//!
//! `ComposeSink` is the bridge between the compositor's model and real backing frames, so the
//! proofs run it against real memory a test owns (the same page-view trick the fbcon proofs
//! use): what the model composed must be what the pages hold, pixel for pixel, and the sink's
//! refusal counter — the one number that says "the model's bound and the raster's bounds
//! disagree" — must stay zero even when a surface hangs far off the edge. The device leg then
//! proves the same frame reaches virtio-gpu; no claim here depends on QEMU.

extern crate alloc;

use kernel_core::compositor::{Compositor, Rect};
use kernel_core::dma::PAGE;
use kernel_core::fbcon::{ComposeSink, Surface};

/// A real allocation viewed as N consecutive 4 KiB "frames" — the scatter-gather shape the
/// device sees, over memory this test owns, kept page-aligned so the frame view is exact.
struct Pages {
    /// Owned. Never read through directly — it exists so the bytes the compositor writes
    /// have a real home the test controls for the struct's whole lifetime.
    #[expect(dead_code)]
    mem: alloc::vec::Vec<u8>,
    addrs: alloc::vec::Vec<usize>,
}

impl Pages {
    fn new(n: usize) -> Self {
        let mut mem = alloc::vec![0u8; n * PAGE + PAGE];
        let base0 = mem.as_mut_ptr() as usize;
        let base = base0.div_ceil(PAGE) * PAGE;
        let addrs = (0..n).map(|i| base + i * PAGE).collect();
        Pages { mem, addrs }
    }
}

/// The 640x240 shape the device suites use: 150 backing pages.
const W: u32 = 640;
const H: u32 = 240;
const PAGES: usize = (W as usize * H as usize * 4).div_ceil(PAGE);

/// Ink-pixel count over the whole raster — the checksum the no-op invariants re-read.
fn ink_total(surf: &Surface) -> u32 {
    let mut n = 0u32;
    for y in 0..H {
        for x in 0..W {
            if surf.get(x, y) == Ok(true) {
                n += 1;
            }
        }
    }
    n
}

/// The standard wallpaper + window pair, attached through their owner tokens: a full-scanout
/// wallpaper with an 8 px ink border and a 200x80 window with a 4 px ink border at (32, 24).
fn scene(comp: &mut Compositor) -> (u64, u64) {
    let tok_a = comp.mint_surface(1, W, H).unwrap();
    let tok_b = comp.mint_surface(2, 200, 80).unwrap();
    comp.fill_rect(
        1,
        tok_a,
        Rect {
            x: 0,
            y: 0,
            w: W,
            h: H,
        },
        true,
    )
    .unwrap();
    comp.fill_rect(
        1,
        tok_a,
        Rect {
            x: 8,
            y: 8,
            w: W - 16,
            h: H - 16,
        },
        false,
    )
    .unwrap();
    comp.fill_rect(
        2,
        tok_b,
        Rect {
            x: 0,
            y: 0,
            w: 200,
            h: 80,
        },
        true,
    )
    .unwrap();
    comp.fill_rect(
        2,
        tok_b,
        Rect {
            x: 4,
            y: 4,
            w: 192,
            h: 72,
        },
        false,
    )
    .unwrap();
    comp.attach(1, tok_a, 0, 0).unwrap();
    comp.attach(2, tok_b, 32, 24).unwrap();
    (tok_a, tok_b)
}

#[test]
fn a_composed_frame_is_what_the_pages_hold() {
    let pages = Pages::new(PAGES);
    let mut comp = Compositor::new(0x0C0F_FEE1, W, H);
    let _ = scene(&mut comp);
    let mut surf = Surface::new(&pages.addrs, W, H).unwrap();

    let (st, puts, refused) = {
        let mut sink = ComposeSink::new(&mut surf);
        let st = comp.compose_frame(&mut sink);
        (st, sink.puts(), sink.refusals())
    };

    // The z-order read back through the REAL pages: window border over wallpaper interior,
    // window interior dark, wallpaper border showing where nobody covers it.
    assert_eq!(surf.get(0, 0), Ok(true), "wallpaper border");
    assert_eq!(surf.get(4, 4), Ok(true), "wallpaper border inner corner");
    assert_eq!(
        surf.get(33, 25),
        Ok(true),
        "window border over wallpaper interior"
    );
    assert_eq!(surf.get(40, 30), Ok(false), "window interior");
    assert_eq!(surf.get(300, 200), Ok(false), "wallpaper interior");
    assert_eq!(
        surf.get(639, 239),
        Ok(true),
        "wallpaper border reaches the raster corner"
    );
    // The sink's own measurement agrees with the model's, and nothing was refused: the model's
    // bound and the raster's bounds agree exactly.
    assert!(st.pixels_blitted > 0);
    assert_eq!(st.pixels_blitted, puts, "every counted write is a real put");
    assert_eq!(
        refused, 0,
        "a legal frame must never be refused by the raster"
    );
}

#[test]
fn a_move_leaves_no_ghost_in_the_real_pages() {
    let pages = Pages::new(PAGES);
    let mut comp = Compositor::new(0x0C0F_FEE1, W, H);
    let (_tok_a, tok_b) = scene(&mut comp);
    let mut surf = Surface::new(&pages.addrs, W, H).unwrap();

    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    let ghost_site = surf.get(33, 25).unwrap();
    assert!(ghost_site, "the window border was there");

    comp.move_surface(2, tok_b, 400, 120).unwrap();
    {
        let mut sink = ComposeSink::new(&mut surf);
        let st = comp.compose_frame(&mut sink);
        assert!(st.pixels_blitted > 0, "a move must be visible");
    }
    // The vacated area shows what the wallpaper paints there now — the border pixel that was
    // true is gone, replaced by the wallpaper's interior.
    assert_eq!(
        surf.get(33, 25),
        Ok(false),
        "a ghost of the moved window remained"
    );
    assert_eq!(
        surf.get(401, 121),
        Ok(true),
        "the window border did not arrive"
    );
    assert_eq!(
        surf.get(430, 150),
        Ok(false),
        "the window interior did not arrive"
    );
    // The wallpaper underneath was NOT rewritten where it was already correct is not checkable
    // pixel-wise here, but the moved-to site must not have corrupted its surroundings:
    assert_eq!(
        surf.get(399, 121),
        Ok(false),
        "pixel left of the moved window"
    );

    // And a quiet frame after the move writes nothing at all.
    let before = ink_total(&surf);
    let mut sink = ComposeSink::new(&mut surf);
    let st = comp.compose_frame(&mut sink);
    assert_eq!(
        st.pixels_blitted, 0,
        "the quiet frame must write zero pixels"
    );
    assert_eq!(sink.puts(), 0);
    assert_eq!(
        ink_total(&surf),
        before,
        "the quiet frame changed real bytes"
    );
}

#[test]
fn an_overhanging_surface_never_asks_outside_the_real_raster() {
    let pages = Pages::new(PAGES);
    let mut comp = Compositor::new(0x0C0F_FEE1, W, H);
    let (_tok_a, tok_b) = scene(&mut comp);
    let mut surf = Surface::new(&pages.addrs, W, H).unwrap();

    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }

    // Push the window 160 px past the right edge and 40 px past the bottom: only its
    // intersection may land, and the sink must never be asked for a pixel it does not have.
    comp.move_surface(2, tok_b, 600, 200).unwrap();
    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
        assert_eq!(
            sink.refusals(),
            0,
            "the model asked the real raster for an out-of-bounds pixel"
        );
    }
    assert_eq!(
        surf.get(603, 203),
        Ok(true),
        "window border inside the intersection"
    );
    assert_eq!(
        surf.get(620, 220),
        Ok(false),
        "window interior inside the intersection"
    );
    assert_eq!(
        surf.get(599, 220),
        Ok(false),
        "the column before the placement"
    );
    assert_eq!(surf.get(610, 199), Ok(false), "the row above the placement");
}

#[test]
fn a_wrong_token_changes_nothing_in_the_real_pages() {
    let pages = Pages::new(PAGES);
    let mut comp = Compositor::new(0x0C0F_FEE1, W, H);
    let (tok_a, _tok_b) = scene(&mut comp);
    let mut surf = Surface::new(&pages.addrs, W, H).unwrap();

    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    let before = ink_total(&surf);

    // A forged token is refused by name; nothing is damaged; the recomposition is quiet.
    assert!(comp.draw_pixel(1, tok_a ^ 1, 1, 1, true).is_err());
    assert!(comp.move_surface(1, tok_a ^ 1, 10, 10).is_err());
    let mut sink = ComposeSink::new(&mut surf);
    let st = comp.compose_frame(&mut sink);
    assert_eq!(
        st.pixels_blitted, 0,
        "a refused op must not damage the frame"
    );
    assert_eq!(sink.puts(), 0);
    assert_eq!(ink_total(&surf), before, "a refused op changed real bytes");
}

#[test]
fn the_device_legs_move_clip_and_z_flips_are_visible_in_real_pages() {
    // The exact op sequence of `compose_suite` invariants 6-8, replayed on the host against
    // real pages: a move, a 160-px overhang clipped while the window is on top, then the
    // raise/lower/detach flips — each with the readback real memory must show. The device
    // suite re-proves the same sequence against virtio-gpu; this test proves the EXPECTED
    // readbacks are right before a boot ever runs it.
    let pages = Pages::new(PAGES);
    let mut comp = Compositor::new(0x0C0F_FEE1, W, H);
    let (tok_a, tok_b) = scene(&mut comp);
    let mut surf = Surface::new(&pages.addrs, W, H).unwrap();

    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }

    // move to (400, 120): the vacated area reverts to the wallpaper, the window arrives.
    comp.move_surface(2, tok_b, 400, 120).unwrap();
    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    assert_eq!(
        surf.get(33, 25),
        Ok(false),
        "move: vacated wallpaper interior"
    );
    assert_eq!(surf.get(401, 121), Ok(true), "move: window border arrived");

    // clip to (600, 200) with the window still ON TOP: only the intersection lands.
    comp.move_surface(2, tok_b, 600, 200).unwrap();
    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    assert_eq!(surf.get(401, 121), Ok(false), "clip: old area vacated");
    assert_eq!(surf.get(603, 203), Ok(true), "clip: window border inside");
    assert_eq!(
        surf.get(620, 220),
        Ok(false),
        "clip: window interior inside"
    );
    assert_eq!(
        surf.get(599, 220),
        Ok(false),
        "clip: wallpaper before the placement"
    );

    // raise the wallpaper: the window's border pixel disappears under it.
    comp.raise(1, tok_a).unwrap();
    assert_eq!(comp.z_order(), [2, 1], "raise puts the wallpaper on top");
    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    assert_eq!(
        surf.get(603, 203),
        Ok(false),
        "raise: the border is covered"
    );

    // lower it back: the border reappears.
    comp.lower(1, tok_a).unwrap();
    assert_eq!(comp.z_order(), [1, 2], "lower puts the wallpaper back");
    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    assert_eq!(surf.get(603, 203), Ok(true), "lower: the border is back");

    // detach the window: its last pixels leave the scanout for good.
    comp.detach(2, tok_b).unwrap();
    assert_eq!(comp.surface_count(), 1);
    {
        let mut sink = ComposeSink::new(&mut surf);
        comp.compose_frame(&mut sink);
    }
    assert_eq!(surf.get(603, 203), Ok(false), "detach: the wallpaper shows");
}
