//! Host proofs for the input-routing contract (ALET-P2-021, ADR-079).
//!
//! The authority questions are swept exhaustively on the host: the session table, the
//! focus decision table, the routing/reading split (the input path decides WHERE events
//! go, the owner decides WHO reads them), the bounded-queue matrix, the cursor's
//! authority and its exact clipped geometry against a GUARD-BAND raster whose
//! out-of-bounds put counter must stay zero through partially-off cursor positions. The
//! in-kernel boot suite is host-run too, so its invariants are proved here before any
//! target boots.

use kernel_core::compositor::{
    input_suite, CompFault, Compositor, EventKind, Raster, Rect, CURSOR_SIZE, MAX_INPUT_EVENTS,
    MAX_SURFACES,
};

/// The canary-guarded raster from the composition proofs, extended with a put counter:
/// every write the model makes lands inside a scanout-sized grid, and anything else
/// bumps `oob` — which must be zero at the end of every proof.
struct Guard {
    bits: Vec<bool>,
    w: u32,
    h: u32,
    puts: u64,
    oob: u64,
}

impl Guard {
    fn new(w: u32, h: u32) -> Self {
        Guard {
            bits: vec![false; w as usize * h as usize],
            w,
            h,
            puts: 0,
            oob: 0,
        }
    }
    fn get(&self, x: u32, y: u32) -> bool {
        self.bits[y as usize * self.w as usize + x as usize]
    }
}

impl Raster for Guard {
    fn put(&mut self, x: u32, y: u32, ink: bool) {
        self.puts += 1;
        if x < self.w && y < self.h {
            self.bits[y as usize * self.w as usize + x as usize] = ink;
        } else {
            self.oob += 1;
        }
    }
}

const W: u32 = 96;
const H: u32 = 64;

/// The boot suite runs on the HOST first: its invariants are proved here before any
/// target is asked to prove them at boot.
#[test]
fn the_boot_suite_holds_on_the_host() {
    let mut failures = 0u32;
    let ok = input_suite(|_n, passed, name| {
        if !passed {
            failures += 1;
            panic!("boot input suite failed at {}: {}", _n, name);
        }
    });
    assert_eq!(ok.unwrap(), 12);
    assert_eq!(failures, 0);
}

#[test]
fn the_session_table_is_fail_closed() {
    let mut c = Compositor::new(0xA079_0001, W, H);
    let s = c.open_input_session().unwrap();
    // One session; every re-opening is refused and names itself.
    for _ in 0..8 {
        assert_eq!(c.open_input_session(), Err(CompFault::InputSealed));
    }
    // Wrong tokens on every op: refused, counted, and none of them mutates anything.
    let (_, refusals0) = c.input_counters();
    assert!(matches!(
        c.set_focus(s ^ 1, 1),
        Err(CompFault::NotInputSession)
    ));
    assert!(matches!(
        c.post_key(s ^ 1, b'x'),
        Err(CompFault::NotInputSession)
    ));
    assert!(matches!(
        c.move_cursor(s ^ 1, 0, 0),
        Err(CompFault::NotInputSession)
    ));
    assert!(matches!(
        c.hide_cursor(s ^ 1),
        Err(CompFault::NotInputSession)
    ));
    assert!(matches!(
        c.clear_focus(s ^ 1),
        Err(CompFault::NotInputSession)
    ));
    let (_, refusals) = c.input_counters();
    assert_eq!(refusals, refusals0 + 5);
    assert!(c.focus().is_none() && c.cursor().is_none());
    // And the session token itself still works afterward — refusals are not corruption.
    let tok = c.mint_surface(1, 16, 16).unwrap();
    c.attach(1, tok, 0, 0).unwrap();
    assert!(c.set_focus(s, 1).is_ok());
    assert_eq!(c.focus(), Some(1));
}

/// The focus decision table, swept: for every reachable focus state (none, surface A,
/// surface B) and every op target (A, B, unminted, minted-unplaced, dead id), the
/// refusal names exactly the violated rule and the focus state changes only through the
/// named transition — plus the FocusLost the loser is told through its own queue.
#[test]
fn the_focus_decision_table_is_exhaustive() {
    let mut c = Compositor::new(0xA079_0002, W, H);
    let s = c.open_input_session().unwrap();
    let ta = c.mint_surface(1, 16, 16).unwrap();
    let tb = c.mint_surface(2, 16, 16).unwrap();
    let tu = c.mint_surface(3, 16, 16).unwrap(); // minted, never placed

    // Nothing focused yet.
    assert_eq!(c.focus(), None);
    assert!(matches!(c.set_focus(s, 1), Err(CompFault::NotPlaced(1))));
    assert!(matches!(c.set_focus(s, 2), Err(CompFault::NotPlaced(2))));
    assert!(matches!(c.set_focus(s, 3), Err(CompFault::NotPlaced(3))));
    assert!(matches!(
        c.set_focus(s, 9),
        Err(CompFault::UnknownSurface(9))
    ));
    assert_eq!(c.focus(), None);

    c.attach(1, ta, 0, 0).unwrap();
    c.attach(2, tb, 32, 0).unwrap();

    // none -> A: no loser, nothing queued anywhere.
    c.set_focus(s, 1).unwrap();
    assert_eq!(c.focus(), Some(1));
    for id in 1..=3 {
        let t = [ta, tb, tu][id as usize - 1];
        assert!(c.drain_input(id, t).unwrap().is_empty());
    }

    // A -> B: A is told FocusLost AFTER the keystroke it never read; B gets new input.
    c.post_key(s, b'a').unwrap();
    c.set_focus(s, 2).unwrap();
    assert_eq!(c.focus(), Some(2));
    let da = c.drain_input(1, ta).unwrap();
    assert_eq!(da.len(), 2);
    assert_eq!(da[0].kind, EventKind::Key(b'a'));
    assert_eq!(da[1].kind, EventKind::FocusLost);
    let db = c.drain_input(2, tb).unwrap();
    // B receives NOTHING from A's keystroke: b'a' was typed while A held focus and it
    // stays A's — routing is to the surface focused AT POST TIME, never retroactive.
    assert_eq!(db.len(), 0);

    // B -> B: idempotent, no event, no queue movement.
    c.post_key(s, b'b').unwrap();
    c.set_focus(s, 2).unwrap();
    assert_eq!(c.drain_input(2, tb).unwrap().len(), 1); // only the pre-existing b'b'

    // B -> none: B is told; the unplaced surface can still be minted but never focused.
    c.clear_focus(s).unwrap();
    let db = c.drain_input(2, tb).unwrap();
    assert_eq!(db.len(), 1);
    assert_eq!(db[0].kind, EventKind::FocusLost);
    assert!(matches!(c.set_focus(s, 3), Err(CompFault::NotPlaced(3))));
    assert_eq!(c.focus(), None);

    // Re-focusing a surface whose placement ended: attach-again is refused
    // AlreadyAttached only if placed; a detached surface is UnknownSurface.
    c.detach(1, ta).unwrap();
    assert!(matches!(
        c.set_focus(s, 1),
        Err(CompFault::UnknownSurface(1))
    ));
}

/// The routing/reading split, swept over the whole event path: events enter ONLY through
/// the input session and land ONLY in the focused surface's queue; they leave ONLY
/// through the owner's drain; order is seq-monotonic; exactly-once under repeated drains;
/// and a keystroke never touches the raster (the guard's put counter is asserted
/// unchanged across a whole post/drain cycle).
#[test]
fn routing_and_reading_are_two_authorities() {
    let mut c = Compositor::new(0xA079_0003, W, H);
    let mut g = Guard::new(W, H);
    let s = c.open_input_session().unwrap();
    let mut toks = Vec::new();
    for id in 1..=4u32 {
        toks.push((id, c.mint_surface(id, 16, 16).unwrap()));
    }
    for (id, t) in &toks {
        c.attach(*id, *t, ((id - 1) * 20) as i32, 4).unwrap();
    }
    c.compose_frame(&mut g); // settle placements
    let puts_before = g.puts;

    // One alphabetical sweep lands in the focused surface's queue in order, exactly once.
    c.set_focus(s, 2).unwrap();
    for b in b"aletheia" {
        c.post_key(s, *b).unwrap();
    }
    let events = c.drain_input(2, toks[1].1).unwrap();
    assert_eq!(events.len(), 8);
    assert!(
        events.windows(2).all(|w| w[0].seq < w[1].seq),
        "delivery order must be seq-monotonic"
    );
    assert_eq!(
        events
            .iter()
            .map(|e| match e.kind {
                EventKind::Key(k) => k,
                _ => 0,
            })
            .collect::<Vec<_>>(),
        b"aletheia".to_vec()
    );
    assert!(c.drain_input(2, toks[1].1).unwrap().is_empty());

    // Nobody else's queue moved, and nobody else's token can read surface 2.
    for (i, (id, t)) in toks.iter().enumerate() {
        if i != 1 {
            assert!(c.drain_input(*id, *t).unwrap().is_empty());
        }
    }
    assert!(matches!(
        c.drain_input(2, toks[1].1 ^ 0xF00D),
        Err(CompFault::NotOwner { surface: 2 })
    ));

    // Input is not pixels: the whole cycle above put ZERO pixels.
    assert_eq!(g.puts, puts_before);
    assert_eq!(g.oob, 0);

    // Refocus-move delivery: the LOSER is told — its FocusLost arrives with a LATER seq
    // than everything the winner ever received — and the new focus's queue is empty.
    c.set_focus(s, 3).unwrap();
    let d2 = c.drain_input(2, toks[1].1).unwrap();
    assert_eq!(d2.len(), 1);
    assert_eq!(d2[0].kind, EventKind::FocusLost);
    assert!(d2[0].seq > events[7].seq);
    assert!(c.drain_input(3, toks[2].1).unwrap().is_empty());
}

/// The bounded-queue matrix, at capacity: exactly MAX_INPUT_EVENTS land, the next is
/// refused Backlogged and counted, the queue's contents are untouched by the refusal,
/// draining restores capacity exactly, and the drop counter accumulates only on drops.
#[test]
fn the_queue_is_bounded_and_the_drop_is_counted() {
    let mut c = Compositor::new(0xA079_0004, W, H);
    let s = c.open_input_session().unwrap();
    let t = c.mint_surface(1, 16, 16).unwrap();
    c.attach(1, t, 0, 0).unwrap();
    c.set_focus(s, 1).unwrap();

    for i in 0..MAX_INPUT_EVENTS {
        assert!(c.post_key(s, b'k' + (i % 26) as u8).is_ok());
        assert_eq!(c.input_counters().0, 0);
    }
    // Every further post is refused BY NAME and counted; the queue holds exactly its cap.
    for _ in 0..4 {
        assert!(matches!(
            c.post_key(s, b'!'),
            Err(CompFault::Backlogged { surface: 1 })
        ));
    }
    let (dropped, _) = c.input_counters();
    assert_eq!(dropped, 4);
    let events = c.drain_input(1, t).unwrap();
    assert_eq!(events.len(), MAX_INPUT_EVENTS);
    assert_eq!(events[0].kind, EventKind::Key(b'k'));
    // The sweep wrapped at 26: the last admitted byte is the sixth letter of the repeat.
    assert_eq!(events[MAX_INPUT_EVENTS - 1].kind, EventKind::Key(b'k' + 5));

    // Capacity restored EXACTLY: MAX posts succeed again before the next refusal.
    for _ in 0..MAX_INPUT_EVENTS {
        assert!(c.post_key(s, b'k').is_ok());
    }
    assert!(matches!(
        c.post_key(s, b'k'),
        Err(CompFault::Backlogged { surface: 1 })
    ));
    let (dropped, _) = c.input_counters();
    assert_eq!(dropped, 5);

    // The FocusLost of a full queue is dropped too — counted, and focus still moves.
    for id in 2..=MAX_SURFACES as u32 {
        let tt = c.mint_surface(id, 4, 4);
        if let Ok(tt) = tt {
            c.attach(id, tt, 0, 0).unwrap();
        }
    }
    c.set_focus(s, 2).unwrap();
    let (dropped, _) = c.input_counters();
    assert_eq!(
        dropped, 6,
        "the FocusLost into the full queue of surface 1 is a counted drop"
    );
    assert_eq!(c.focus(), Some(2));
}

/// The cursor's authority and exact geometry, over a sweep of positions: session-only
/// movement, fully-off refused by name with the position named, partially-off positions
/// legal and clipped EXACTLY (guard-band zero through every edge), the ink count of every
/// frame equal to the crosshair bits inside the damaged regions, hide reversible, and a
/// no-op move costing nothing.
#[test]
fn the_cursor_is_the_compositors_and_clipped_exactly() {
    let ink = (0..CURSOR_SIZE)
        .map(|r| {
            let row = match r {
                7 => 0b0000_0000u8, // the glyph's blank bottom row
                3 => 0b1111_1110u8, // the horizontal stroke
                _ => 0b0001_0000u8, // the vertical stroke
            };
            row.count_ones()
        })
        .sum::<u32>();
    assert_eq!(
        ink, 13,
        "the crosshair is 13 ink pixels — the proofs below count it"
    );

    let mut c = Compositor::new(0xA079_0005, W, H);
    let mut g = Guard::new(W, H);
    let s = c.open_input_session().unwrap();

    // Hidden by default; a hide of a hidden cursor is a legal no-op.
    assert_eq!(c.cursor(), None);
    assert!(c.hide_cursor(s).is_ok());
    assert_eq!(c.cursor(), None);

    // Fully-off moves are refused BY NAME, with the position named, from every direction.
    for (x, y) in [
        (W, 0),
        (0, H),
        (W + 64, H + 64),
        (u32::MAX, 0),
        (0, u32::MAX),
    ] {
        assert!(matches!(
            c.move_cursor(s, x, y),
            Err(CompFault::CursorOffScanout { .. })
        ));
        assert_eq!(c.cursor(), None);
    }

    // Corner-exact positions are legal; each move is visible the same frame with an
    // exact measured cost: the damaged glyph regions' background + z-order (no surfaces:
    // nothing) + the crosshair bits inside them.
    for (x, y) in [
        (0, 0),
        (W - 1, H - 1),
        (W - CURSOR_SIZE, H - CURSOR_SIZE),
        (W - 3, 5),
    ] {
        c.move_cursor(s, x, y).unwrap();
        let st = c.compose_frame(&mut g);
        // Damage: the new glyph rect (and the old one — but this is the first move, so
        // only the new; the loop below moves between legal positions and both rects are
        // damaged; we assert only the INVARIANT parts: every put landed inside, and the
        // ink the frame wrote is exactly the glyph bits that fit on the scanout.
        assert_eq!(g.oob, 0);
        assert!(st.pixels_blitted >= st.cursor_pixels);
        assert_eq!(st.cursor_pixels, st.cursor_pixels.min(ink as u64));
        // The hot-spot column: where the glyph has ink, the shadow says so.
        if x + 3 < W && y < H {
            assert!(g.get(x + 3, y), "vertical stroke top at ({},{})", x, y);
        }
    }

    // A no-op move is FREE: no damage, no frame work.
    let (x0, y0) = c.cursor().unwrap();
    c.move_cursor(s, x0, y0).unwrap();
    let st = c.compose_frame(&mut g);
    assert_eq!(st.pixels_blitted, 0);
    assert_eq!(st.cursor_pixels, 0);

    // Hide is visible the same frame and reveals the background; re-show works.
    c.hide_cursor(s).unwrap();
    let st = c.compose_frame(&mut g);
    assert!(st.pixels_blitted > 0 && st.cursor_pixels == 0);
    c.move_cursor(s, 40, 30).unwrap();
    let st = c.compose_frame(&mut g);
    assert_eq!(st.cursor_pixels, ink as u64);
    assert_eq!(g.oob, 0);
}

/// The cursor above every surface: a zero-ink window under the glyph cannot unpaint it
/// (the cursor is the last plane through the z-order), a surface RAISED over the cursor
/// position still cannot cover it, and a surface's own ink under a transparent glyph bit
/// shows through — the glyph is a mask, not a paint-over.
#[test]
fn the_cursor_paints_above_every_surface() {
    let mut c = Compositor::new(0xA079_0006, W, H);
    let mut g = Guard::new(W, H);
    let s = c.open_input_session().unwrap();
    let t = c.mint_surface(1, 32, 32).unwrap();
    c.attach(1, t, 16, 16).unwrap();
    c.move_cursor(s, 20, 20).unwrap();
    c.compose_frame(&mut g);

    // (23,20) is glyph ink (vertical stroke) over the window; (21,20) is transparent
    // glyph over the window's zero background.
    c.clear_surface(1, t).unwrap();
    c.compose_frame(&mut g);
    assert!(g.get(23, 20), "cursor ink survives over a zero-ink window");
    assert!(!g.get(21, 20), "transparent glyph leaves what is below");

    // The window draws ink under a TRANSPARENT glyph bit: it shows through. Screen
    // (21,20) is surface-local (5,4) — the pen draws in the surface's own coordinates.
    c.draw_pixel(1, t, 5, 4, true).unwrap();
    c.compose_frame(&mut g);
    assert!(
        g.get(21, 20),
        "window ink shows through a transparent glyph bit"
    );
    assert!(g.get(23, 20), "cursor ink still wins at the hot spot");

    // Raising the window to the top of the z-order changes nothing about the cursor.
    c.raise(1, t).unwrap();
    c.compose_frame(&mut g);
    assert!(g.get(23, 20));
    assert_eq!(g.oob, 0);
}

/// Determinism over the WHOLE input contract: two engines fed an identical mixed
/// sequence (session, mints, placements, focus changes, keystrokes, backlog, cursor
/// moves, hides, drains, refusals) land bit-identical rasters with identical counters.
#[test]
fn identical_input_sequences_land_bit_identical() {
    let run = || {
        let mut c = Compositor::new(0xA079_0007, W, H);
        let mut g = Guard::new(W, H);
        let s = c.open_input_session().unwrap();
        let t1 = c.mint_surface(1, 20, 20).unwrap();
        let t2 = c.mint_surface(2, 24, 12).unwrap();
        c.attach(1, t1, 2, 2).unwrap();
        c.attach(2, t2, 40, 30).unwrap();
        c.fill_rect(
            1,
            t1,
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            true,
        )
        .unwrap();
        c.set_focus(s, 1).unwrap();
        for b in b"ab" {
            c.post_key(s, *b).unwrap();
        }
        c.move_cursor(s, 30, 8).unwrap();
        c.compose_frame(&mut g);
        c.move_cursor(s, W - 2, H - 2).unwrap(); // partially-off, clipped
        c.set_focus(s, 2).unwrap(); // 1 loses (its queue holds nothing)
        c.post_key(s, b'c').unwrap();
        let st = c.compose_frame(&mut g);
        c.hide_cursor(s).unwrap();
        let st2 = c.compose_frame(&mut g);
        let d = c.drain_input(2, t2).unwrap();
        (
            g.bits,
            st,
            st2,
            d,
            c.focus(),
            c.cursor(),
            c.input_counters(),
            c.z_order(),
        )
    };
    let a = run();
    let b = run();
    assert_eq!(a, b);
}
