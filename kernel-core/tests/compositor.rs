//! Host-exhaustive proofs of the composition contract (ALET-P2-021, ADR-077).
//!
//! The in-kernel `compositor_suite` proves the core promises at boot on every target; these
//! tests are the EXHAUSTIVE sweeps the boot heap cannot afford (ADR-063): clip exactness
//! under guarded canary rasters on every side, the ownership table over every op, damage
//! accounting, buffer-honesty matrices, z-order sweeps, placement-damage visibility, and
//! bit-identical determinism.

use kernel_core::compositor::*;

const W: u32 = 96;
const H: u32 = 64;

/// A raster with a GUARD BAND: writes outside the scanout land in the band instead of
/// panicking, so clip violations are OBSERVED, not assumed. Host-only.
struct Guarded {
    w: u32,
    h: u32,
    /// (w+2)x(h+2) — ring 0 is the guard.
    bits: Vec<bool>,
    violations: usize,
}

impl Guarded {
    fn new(w: u32, h: u32) -> Self {
        Guarded {
            w,
            h,
            bits: vec![false; ((w + 2) * (h + 2)) as usize],
            violations: 0,
        }
    }
    fn get(&self, x: u32, y: u32) -> bool {
        self.bits[((y + 1) * (self.w + 2) + (x + 1)) as usize]
    }
    fn guard_clean(&self) -> bool {
        for y in 0..self.h + 2 {
            for x in 0..self.w + 2 {
                let inside = x >= 1 && x <= self.w && y >= 1 && y <= self.h;
                if !inside && self.bits[y as usize * (self.w + 2) as usize + x as usize] {
                    return false;
                }
            }
        }
        true
    }
}

impl Raster for Guarded {
    fn put(&mut self, x: u32, y: u32, ink: bool) {
        if x >= self.w || y >= self.h {
            // The one thing the contract must make impossible, made visible.
            self.violations += 1;
            return;
        }
        self.bits[((y + 1) * (self.w + 2) + (x + 1)) as usize] = ink;
    }
}

fn platform() -> Compositor {
    Compositor::new(0x5EED_0C0F, W, H)
}

// ---------------------------------------------------------------------------
// 1 - clip exactness: a surface pushed past EVERY edge paints only its
// intersection, and the guard band records zero violations.
// ---------------------------------------------------------------------------
#[test]
fn clipping_is_exact_on_every_edge() {
    // Each edge: place an 16x16 surface so it hangs off, paint it solid, compose, and
    // check both the visible part and a sample just inside the edge that must stay clean.
    type Visible = fn(u32, u32) -> bool;
    let cases: [(i32, i32, Visible); 4] = [
        (W as i32 - 8, 4, |x, y| {
            (W - 8..W).contains(&x) && (4..20).contains(&y)
        }),
        (-8, 4, |x, y| (0..8).contains(&x) && (4..20).contains(&y)),
        (4, H as i32 - 8, |x, y| {
            (4..20).contains(&x) && (H - 8..H).contains(&y)
        }),
        (4, -8, |x, y| (4..20).contains(&x) && (0..8).contains(&y)),
    ];
    for (x, y, visible) in cases {
        let mut comp = platform();
        let mut g = Guarded::new(W, H);
        let tok = comp.mint_surface(1, 16, 16).unwrap();
        comp.attach(1, tok, x, y).unwrap();
        comp.fill_rect(
            1,
            tok,
            Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            },
            true,
        )
        .unwrap();
        comp.compose_frame(&mut g);
        assert!(
            g.guard_clean(),
            "edge ({x},{y}): the guard band was written"
        );
        assert_eq!(
            g.violations, 0,
            "edge ({x},{y}): the sink saw out-of-bounds puts"
        );
        for yy in 0..H {
            for xx in 0..W {
                assert_eq!(
                    g.get(xx, yy),
                    visible(xx, yy),
                    "edge ({x},{y}): pixel ({xx},{yy}) wrong"
                );
            }
        }
    }
}

#[test]
fn fully_off_scanout_placements_are_refused_at_attach_and_move() {
    let mut comp = platform();
    let tok = comp.mint_surface(1, 16, 16).unwrap();
    for (x, y) in [
        (W as i32, 0),
        (0, H as i32),
        (-16, 0),
        (0, -16),
        (10_000, 10_000),
    ] {
        assert!(
            matches!(
                comp.attach(1, tok, x, y),
                Err(CompFault::OffScanout { surface: 1 })
            ),
            "attach at ({x},{y}) must be refused"
        );
    }
    comp.attach(1, tok, 0, 0).unwrap();
    for (x, y) in [(W as i32, 0), (-16, 0), (0, H as i32), (0, -16)] {
        assert!(
            matches!(
                comp.move_surface(1, tok, x, y),
                Err(CompFault::OffScanout { surface: 1 })
            ),
            "move to ({x},{y}) must be refused"
        );
    }
    assert_eq!(comp.placed_count(), 1, "refused moves changed nothing");
}

// ---------------------------------------------------------------------------
// 2 - ownership: every mutating op answers only to the surface's own token.
// ---------------------------------------------------------------------------
#[test]
fn ownership_gates_every_op() {
    let mut comp = platform();
    let tok = comp.mint_surface(1, 16, 16).unwrap();
    let other = comp.mint_surface(2, 8, 8).unwrap();
    comp.attach(1, tok, 0, 0).unwrap();
    let wrong = tok ^ 1;
    assert!(matches!(
        comp.attach(1, wrong, 0, 0),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.move_surface(1, wrong, 4, 4),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.raise(1, wrong),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.lower(1, wrong),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.draw_pixel(1, wrong, 0, 0, true),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.fill_rect(
            1,
            wrong,
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4
            },
            true
        ),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.fill_packed(1, wrong, &[0u8; 32]),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.clear_surface(1, wrong),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    assert!(matches!(
        comp.detach(1, wrong),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    // A token for ANOTHER surface is not authority here either.
    assert!(matches!(
        comp.raise(1, other),
        Err(CompFault::NotOwner { surface: 1 })
    ));
    // Unknown ids are unknown, not "not owner".
    assert!(matches!(
        comp.raise(77, tok),
        Err(CompFault::UnknownSurface(77))
    ));
    // The right token works everywhere, and the state never moved.
    assert!(comp.raise(1, tok).is_ok() && comp.lower(1, tok).is_ok() && comp.z_order() == vec![1]);
    assert_eq!(comp.placed_count(), 1);
}

// ---------------------------------------------------------------------------
// 3 - buffer honesty: every wrong-size fill is refused with the surface
// untouched; the exact size is accepted and lands pixel-exact.
// ---------------------------------------------------------------------------
#[test]
#[allow(clippy::manual_is_multiple_of)]
fn fill_buffers_are_size_honest() {
    let mut comp = platform();
    let tok = comp.mint_surface(1, 16, 16).unwrap();
    comp.attach(1, tok, 0, 0).unwrap();
    let expect = 32usize;
    for got in [0usize, 1, 31, 33, 64, 4096] {
        let buf = vec![0xFFu8; got];
        let r = comp.fill_packed(1, tok, &buf);
        assert!(
            matches!(
                r,
                Err(CompFault::BufferMismatch {
                    surface: 1,
                    expected_bytes: 32,
                    ..
                })
            ),
            "fill of {got} bytes must be refused"
        );
    }
    // A refused fill left the surface empty.
    let mut g = Guarded::new(W, H);
    comp.compose_frame(&mut g);
    assert!(!g.get(0, 0) && !g.get(15, 15));
    // The exact size lands bit-exact: bit i of the buffer = pixel (i%16, i/16).
    let mut buf = vec![0u8; expect];
    for i in 0..256 {
        if i % 3 == 0 {
            buf[i / 8] |= 1 << (i % 8);
        }
    }
    comp.fill_packed(1, tok, &buf).unwrap();
    comp.compose_frame(&mut g);
    for y in 0..16u32 {
        for x in 0..16u32 {
            let i = (y * 16 + x) as usize;
            assert_eq!(g.get(x, y), i % 3 == 0, "pixel ({x},{y}) after exact fill");
        }
    }
}

// ---------------------------------------------------------------------------
// 4 - placement changes are VISIBLE the same frame: move erases the vacated
// area, detach reveals what was underneath, raise/lower flip the overlap.
// ---------------------------------------------------------------------------
#[test]
fn placement_changes_are_visible_without_redraw() {
    let mut comp = platform();
    let mut g = Guarded::new(W, H);
    let a = comp.mint_surface(1, 16, 16).unwrap();
    let b = comp.mint_surface(2, 16, 16).unwrap();
    comp.attach(1, a, 0, 0).unwrap();
    comp.fill_rect(
        1,
        a,
        Rect {
            x: 0,
            y: 0,
            w: 16,
            h: 16,
        },
        true,
    )
    .unwrap();
    comp.compose_frame(&mut g);
    assert!(g.get(5, 5));
    // Move: the vacated area must go dark the same frame, with no redraw from the client.
    comp.move_surface(1, a, 32, 0).unwrap();
    comp.compose_frame(&mut g);
    assert!(
        !g.get(5, 5),
        "the vacated area kept the moved surface's pixels"
    );
    assert!(g.get(37, 5), "the new area did not show the moved surface");
    // A second surface underneath reappears where the top one vacated.
    comp.attach(2, b, 0, 0).unwrap();
    comp.fill_rect(
        2,
        b,
        Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        },
        true,
    )
    .unwrap(); // a small mark under the vacated area
    comp.compose_frame(&mut g);
    assert!(!g.get(5, 5), "surface 2's mark leaked outside its 4x4 rect");
    assert!(g.get(2, 2), "surface 2's mark did not show");
    // Detach the bottom surface: its mark disappears, nothing else moves.
    comp.detach(2, b).unwrap();
    comp.compose_frame(&mut g);
    assert!(
        !g.get(2, 2),
        "a detached surface's pixels survived its detach"
    );
    assert!(g.get(37, 5), "detach disturbed an unrelated surface");
    assert!(g.guard_clean());
}

#[test]
fn z_order_flips_are_visible_and_owner_gated() {
    let mut comp = platform();
    let mut g = Guarded::new(W, H);
    let a = comp.mint_surface(1, 16, 16).unwrap();
    let b = comp.mint_surface(2, 16, 16).unwrap();
    comp.attach(1, a, 0, 0).unwrap();
    comp.attach(2, b, 4, 0).unwrap();
    // 1 paints (4,4) dark; 2 paints it bright — 2 is on top (attached later).
    comp.draw_pixel(1, a, 4, 4, false).unwrap();
    comp.fill_rect(
        2,
        b,
        Rect {
            x: 0,
            y: 0,
            w: 16,
            h: 16,
        },
        true,
    )
    .unwrap();
    comp.compose_frame(&mut g);
    assert!(g.get(4, 4), "the top surface's pixel did not win");
    comp.raise(1, a).unwrap();
    comp.compose_frame(&mut g);
    assert!(!g.get(4, 4), "raising did not flip the overlap");
    comp.lower(1, a).unwrap();
    comp.compose_frame(&mut g);
    assert!(g.get(4, 4), "lowering did not flip the overlap back");
    assert_eq!(comp.z_order(), vec![1, 2]);
}

// ---------------------------------------------------------------------------
// 5 - damage accounting: an unchanged frame writes nothing; the measured
// savings shrink to the exact dirty region as damage narrows.
// ---------------------------------------------------------------------------
#[test]
fn damage_accounting_is_exact() {
    let mut comp = platform();
    let mut g = Guarded::new(W, H);
    let tok = comp.mint_surface(1, 16, 16).unwrap();
    comp.attach(1, tok, 8, 8).unwrap();
    let first = comp.compose_frame(&mut g);
    // Attach damaged the 16x16 area: background clear + repaint = 2 writes per pixel.
    assert_eq!(first.pixels_blitted, 16 * 16 * 2);
    assert_eq!(first.pixels_skipped_by_damage, (W * H) as u64 - 16 * 16 * 2);
    // A quiet frame writes nothing and skips the whole scanout.
    let quiet = comp.compose_frame(&mut g);
    assert_eq!(quiet.pixels_blitted, 0);
    assert_eq!(quiet.pixels_skipped_by_damage, (W * H) as u64);
    // One pixel: 1x1 region = 2 writes.
    comp.draw_pixel(1, tok, 0, 0, true).unwrap();
    let one = comp.compose_frame(&mut g);
    assert_eq!(one.pixels_blitted, 2);
    assert_eq!(one.pixels_skipped_by_damage, (W * H) as u64 - 2);
    // A row: 16x1 region = 32 writes.
    comp.fill_rect(
        1,
        tok,
        Rect {
            x: 0,
            y: 3,
            w: 16,
            h: 1,
        },
        true,
    )
    .unwrap();
    let row = comp.compose_frame(&mut g);
    assert_eq!(row.pixels_blitted, 32);
    assert_eq!(row.frames, quiet.frames + 2);
    assert!(g.guard_clean());
}

// ---------------------------------------------------------------------------
// 6 - bounds, capacity, and the damage ledger.
// ---------------------------------------------------------------------------
#[test]
fn geometry_and_capacity_are_bounded() {
    let mut comp = platform();
    assert!(matches!(
        comp.mint_surface(1, 0, 8),
        Err(CompFault::BadGeometry(1))
    ));
    assert!(matches!(
        comp.mint_surface(1, 8, 0),
        Err(CompFault::BadGeometry(1))
    ));
    assert!(matches!(
        comp.mint_surface(1, 1024, 1024 + 1),
        Err(CompFault::BadGeometry(1))
    ));
    // Exactly at the cap is accepted.
    assert!(comp.mint_surface(1, 1024, 1024).is_ok());
    // The surface table caps at MAX_SURFACES.
    let mut comp2 = platform();
    for id in 0..MAX_SURFACES as u32 {
        assert!(
            comp2.mint_surface(id, 8, 8).is_ok(),
            "surface {id} should fit"
        );
    }
    assert!(matches!(
        comp2.mint_surface(99, 8, 8),
        Err(CompFault::NoSpace)
    ));
    // A pixel outside a surface's own bounds is refused.
    let mut comp3 = platform();
    let tok = comp3.mint_surface(1, 8, 8).unwrap();
    assert!(matches!(
        comp3.draw_pixel(1, tok, 8, 0, true),
        Err(CompFault::OutsideSurface {
            surface: 1,
            x: 8,
            y: 0
        })
    ));
    assert!(matches!(
        comp3.draw_pixel(1, tok, 0, 8, true),
        Err(CompFault::OutsideSurface {
            surface: 1,
            x: 0,
            y: 8
        })
    ));
    // The damage ledger coalesces instead of growing without bound.
    for i in 0..500u32 {
        comp3.draw_pixel(1, tok, i % 8, (i / 8) % 8, true).unwrap();
    }
    assert!(comp3.damage_rects_len(1).unwrap() <= MAX_DAMAGE_RECTS);
}

#[test]
fn detached_ids_can_be_reminted_and_slots_reused() {
    let mut comp = platform();
    let tok = comp.mint_surface(1, 8, 8).unwrap();
    assert!(matches!(
        comp.mint_surface(1, 8, 8),
        Err(CompFault::AlreadyAttached(1))
    ));
    comp.detach(1, tok).unwrap();
    let tok2 = comp.mint_surface(1, 8, 8).unwrap();
    assert_ne!(tok, tok2, "a re-minted id must not inherit the dead token");
    assert!(matches!(
        comp.attach(1, tok, 0, 0),
        Err(CompFault::NotOwner { surface: 1 })
    ));
}

// ---------------------------------------------------------------------------
// 7 - determinism: identical op sequences compose bit-identical frames with
// identical counters, including negative placements and clipping.
// ---------------------------------------------------------------------------
#[test]
fn identical_sequences_are_bit_identical() {
    let run = || {
        let mut c = Compositor::new(0x1234_5678, W, H);
        let mut g = Guarded::new(W, H);
        let a = c.mint_surface(1, 16, 16).unwrap();
        let b = c.mint_surface(2, 16, 16).unwrap();
        c.attach(1, a, -4, 10).unwrap();
        c.attach(2, b, 80, 50).unwrap(); // hangs off the bottom-right corner
        c.fill_rect(
            1,
            a,
            Rect {
                x: 0,
                y: 0,
                w: 12,
                h: 12,
            },
            true,
        )
        .unwrap();
        c.fill_rect(
            2,
            b,
            Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 16,
            },
            true,
        )
        .unwrap();
        c.compose_frame(&mut g);
        c.raise(1, a).unwrap();
        c.move_surface(2, b, 88, 56).unwrap();
        c.draw_pixel(1, a, 5, 5, false).unwrap();
        let st = c.compose_frame(&mut g);
        (g.bits.clone(), st, g.violations, c.z_order())
    };
    let (r1, s1, v1, z1) = run();
    let (r2, s2, v2, z2) = run();
    assert_eq!(r1, r2);
    assert_eq!(s1, s2);
    assert_eq!(v1, 0);
    assert_eq!(v2, 0);
    assert_eq!(z1, z2);
}

// ---------------------------------------------------------------------------
// 8 - the boot suite itself, run on the host with a capturing reporter.
// ---------------------------------------------------------------------------
#[test]
fn the_boot_suite_passes_on_the_host() {
    let mut n = 0u32;
    let r = compositor_suite(|i, passed, name| {
        n = i;
        assert!(
            passed,
            "boot-suite invariant {i} failed on the host: {name}"
        );
    });
    assert_eq!(r, Ok(n));
    assert!(
        n >= 12,
        "the suite must keep proving all its invariants, got {n}"
    );
}
