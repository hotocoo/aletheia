//! The desktop under a merciless storm (ALET-P2-021 / REQ-QUAL-007, ADR-086).
//!
//! Every window invariant so far was proved on a handful of events: one press, one drag, one
//! close. That proves the RULES. It does not prove the machine — an OS is judged by what it does
//! on the ten-thousandth event, when a queue nobody drained is full, a window has been opened and
//! closed a thousand times, and the heap that never frees (ADR-063) has had every chance to grow
//! by a byte per event forever.
//!
//! This suite storms the composition + input + window stack with a deterministic pseudo-random
//! event flood and holds it to five claims a real desktop must meet:
//!
//! * **Lifecycles close.** Thousands of open/close cycles return the compositor to EXACTLY its
//!   starting surface, placement and queue population — no leaked surface, no orphan queue.
//! * **Backlog is bounded and HONEST.** A window that stops draining fills to exactly
//!   `MAX_INPUT_EVENTS` and every further event is refused `Backlogged` AND COUNTED; the drop
//!   ledger equals the arithmetic, and a drain restores capacity exactly.
//! * **The storm allocates NOTHING in its steady state.** The caller passes its own heap's
//!   watermark; after a warm-up round the storm must not move it. On a bump allocator that never
//!   frees, an allocation per event is a leak by another name — this is the invariant that
//!   catches it, on the machine, not in a review.
//! * **A settled desktop is QUIET.** After the storm stops, one frame repaints what the storm
//!   damaged and the NEXT frame writes zero pixels: no ghost damage, no perpetual repaint.
//! * **The same storm lands bit-identically.** Same seed, same machine state, twice.

use alloc::vec::Vec;

use crate::compositor::{CompFault, Compositor, FrameStats, Raster, MAX_INPUT_EVENTS};
use crate::textgrid::TITLE_H;
use crate::wm::{hit_at, Hit, Press, WindowManager};

/// Windows the storm keeps open.
const WINDOWS: u32 = 4;
/// Pointer events per storm round. Large enough that a per-event allocation is unmissable,
/// small enough that an emulated boot still finishes in the time a gate allows.
const EVENTS: u32 = 4096;
/// The storm's scanout, deliberately small: this suite is about EVENT volume, not pixel volume
/// (the pixel contract is ADR-077's, proved there).
const SW: u32 = 128;
const SH: u32 = 96;
const WIN_W: u32 = 40;
const WIN_H: u32 = 30;

/// A deterministic pseudo-random source: an ordinary 64-bit LCG. Nothing here needs entropy —
/// it needs a stream that is the SAME on every CPU, so a storm that fails fails identically.
struct Storm(u64);

impl Storm {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % n as u64) as u32
    }
}

/// A raster that counts what a frame wrote without owning any pixels — the storm cares how MUCH
/// moved, not what it looked like.
struct CountingRaster {
    puts: u64,
}

impl Raster for CountingRaster {
    fn put(&mut self, _x: u32, _y: u32, _ink: bool) {
        self.puts += 1;
    }
}

/// Open the storm's desktop: `WINDOWS` windows, spread so they overlap.
fn desktop(seed: u64) -> (Compositor, WindowManager, u64) {
    let mut comp = Compositor::new(seed ^ 0x5701_1086, SW, SH);
    let sess = comp.open_input_session().unwrap();
    let mut wm = WindowManager::new();
    for i in 0..WINDOWS {
        let x = (i * 20) as i32;
        let y = (i * 12) as i32;
        wm.open(&mut comp, i + 1, WIN_W, WIN_H, x, y).unwrap();
    }
    let _ = comp.set_focus(sess, 1);
    (comp, wm, sess)
}

/// One storm round: `EVENTS` pointer events over the whole scanout, with keystrokes posted into
/// whatever holds focus. Returns (presses, keys accepted, keys refused).
fn round(
    comp: &mut Compositor,
    wm: &mut WindowManager,
    sess: u64,
    s: &mut Storm,
) -> (u64, u64, u64) {
    let (mut presses, mut accepted, mut refused) = (0u64, 0u64, 0u64);
    for i in 0..EVENTS {
        let x = s.below(SW);
        // A press, a move, a release — the shape a real pointer makes. The close box is
        // deliberately NOT stormed here: OPENING a window allocates its pixels and its queue, by
        // design, so a round that destroys and recreates windows could never hold the
        // allocation claim still. Window lifecycles are invariant 1's business.
        let mut y = s.below(SH);
        // Nudge off a close box, twice at most, and skip the press if the storm still insists:
        // the point is to hammer routing, focus and dragging, not to destroy and recreate
        // windows, and CREATING a window allocates its pixels and its queue by design.
        let closes_here = |wm: &WindowManager, comp: &Compositor, x: u32, y: u32| -> bool {
            match wm.window_at(comp, x, y) {
                Some((_, lx, ly)) => hit_at(WIN_W, WIN_H, lx, ly) == Some(Hit::Close),
                None => false,
            }
        };
        let mut skip = false;
        for _ in 0..2 {
            if !closes_here(wm, comp, x, y) {
                break;
            }
            if y + TITLE_H < SH {
                y += TITLE_H;
            } else {
                skip = true;
                break;
            }
        }
        if !skip && closes_here(wm, comp, x, y) {
            skip = true;
        }
        if !skip {
            match wm.press(comp, sess, x, y) {
                Press::Closed(id) => {
                    // Belt and braces: if a close still happened, reopen so the set stays whole.
                    let _ = wm.open(comp, id, WIN_W, WIN_H, 0, 0);
                }
                Press::Dragging(_) => {
                    let _ = wm.motion(comp, s.below(SW), s.below(SH));
                }
                _ => {}
            }
        }
        let _ = wm.release();
        presses += 1;
        if i % 3 == 0 {
            match comp.post_key(sess, b'a' + (i % 26) as u8) {
                Ok(()) => accepted += 1,
                Err(_) => refused += 1,
            }
        }
        // Drain often enough that the queue is not the thing under test here.
        if i % 64 == 0 {
            if let Some(f) = comp.focus() {
                if let Some(tok) = wm.token(f) {
                    // Allocation-free draining (ADR-086): a pump that drains every tick must not
                    // build a `Vec` it throws away.
                    while let Ok(Some(_)) = comp.pop_input(f, tok) {}
                }
            }
        }
    }
    (presses, accepted, refused)
}

/// The boot suite (ADR-086). `used_bytes` reports the CALLER's heap watermark — the platform's
/// own number, because a claim about allocation must be measured where allocation happens.
pub fn storm_suite(
    used_bytes: &mut dyn FnMut() -> usize,
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

    // 1 — LIFECYCLES CLOSE. A thousand open/close cycles return the compositor to exactly the
    //     surface, placement and queue population it started with: nothing leaks per window.
    {
        let (mut comp, mut wm, sess) = desktop(1);
        let (s0, p0) = (comp.surface_count(), comp.placed_count());
        for i in 0..1000u32 {
            let id = 1 + (i % WINDOWS);
            wm.close(&mut comp, sess, id).unwrap();
            wm.open(
                &mut comp,
                id,
                WIN_W,
                WIN_H,
                (i % 40) as i32,
                (i % 30) as i32,
            )
            .unwrap();
        }
        let (_, closes, _, refusals) = wm.counters();
        check!(
            comp.surface_count() == s0
                && comp.placed_count() == p0
                && wm.count() == WINDOWS as usize
                && closes == 1000
                && refusals == 0,
            "storm: a thousand open/close cycles leave the compositor exactly as they found it"
        );
    }
    // 2 — BACKLOG IS BOUNDED AND HONEST. A window nobody drains fills to exactly the cap; every
    //     further keystroke is refused BY NAME and counted; the ledger equals the arithmetic.
    {
        let (mut comp, wm, sess) = desktop(2);
        let flood = 4000u32;
        let mut refused = 0u64;
        let mut named = 0u64;
        for i in 0..flood {
            match comp.post_key(sess, b'x' + (i % 8) as u8) {
                Ok(()) => {}
                Err(CompFault::Backlogged { .. }) => {
                    refused += 1;
                    named += 1;
                }
                Err(_) => refused += 1,
            }
        }
        let (dropped, _) = comp.input_counters();
        let queued = comp.queued_len(1);
        check!(
            queued == MAX_INPUT_EVENTS
                && refused == (flood as u64 - MAX_INPUT_EVENTS as u64)
                && named == refused
                && dropped == refused,
            "storm: a window that stops draining backs up to exactly its cap, and every loss is named and counted"
        );
        // ... and a drain restores capacity EXACTLY: cap more events fit, the next one does not.
        let tok = wm.token(1).unwrap();
        let drained = comp.drain_input(1, tok).map(|e| e.len()).unwrap_or(0);
        let mut fit = 0;
        while comp.post_key(sess, b'y').is_ok() {
            fit += 1;
            if fit > MAX_INPUT_EVENTS + 1 {
                break;
            }
        }
        check!(
            drained == MAX_INPUT_EVENTS && fit == MAX_INPUT_EVENTS,
            "storm: draining a backlogged window restores exactly its capacity, not one event more"
        );
    }
    // 3 — THE STEADY STATE ALLOCATES NOTHING. Warm up (first-touch growth is a cost paid once),
    //     then storm and hold the caller's own heap watermark still.
    {
        let (mut comp, mut wm, sess) = desktop(3);
        let mut s = Storm(0xA1E7_4E1A);
        let _ = round(&mut comp, &mut wm, sess, &mut s); // warm-up: growth here is per-boot
        let before = used_bytes();
        let (presses, _, _) = round(&mut comp, &mut wm, sess, &mut s);
        let after = used_bytes();
        // The measurement is REPORTED whether it passes or not: a claim about allocation that
        // fails silently teaches nobody where the bytes went.
        crate::kprintln_storm(before, after);
        check!(
            presses == EVENTS as u64 && after == before,
            "storm: four thousand pointer events in the steady state allocate NOTHING"
        );
    }
    // 4 — A SETTLED DESKTOP IS QUIET. After the storm, one frame repaints what it damaged and
    //     the next writes zero pixels: no ghost damage, no perpetual repaint.
    {
        let (mut comp, mut wm, sess) = desktop(4);
        let mut s = Storm(0x0BAD_5EED);
        let _ = round(&mut comp, &mut wm, sess, &mut s);
        let mut r = CountingRaster { puts: 0 };
        let first: FrameStats = comp.compose_frame(&mut r);
        let painted = r.puts;
        r.puts = 0;
        let second = comp.compose_frame(&mut r);
        check!(
            first.pixels_blitted > 0
                && painted == first.pixels_blitted
                && second.pixels_blitted == 0
                && r.puts == 0
                && !comp.has_pending_damage(),
            "storm: a settled desktop repaints once and then writes nothing at all"
        );
    }
    // 5 — THE SAME STORM LANDS BIT-IDENTICALLY. Same seed, same result: z-order, placements,
    //     focus, the manager's ledger and the frame's own cost.
    {
        let (mut c1, mut w1, s1) = desktop(5);
        let (mut c2, mut w2, s2) = desktop(5);
        let mut r1 = Storm(0x00C0_FFEE_1234);
        let mut r2 = Storm(0x00C0_FFEE_1234);
        let a = round(&mut c1, &mut w1, s1, &mut r1);
        let b = round(&mut c2, &mut w2, s2, &mut r2);
        let mut k1 = CountingRaster { puts: 0 };
        let mut k2 = CountingRaster { puts: 0 };
        let f1 = c1.compose_frame(&mut k1);
        let f2 = c2.compose_frame(&mut k2);
        let placements = |c: &Compositor| -> Vec<Option<(i32, i32)>> {
            (1..=WINDOWS).map(|id| c.placement(id)).collect()
        };
        check!(
            a == b
                && c1.z_order() == c2.z_order()
                && placements(&c1) == placements(&c2)
                && c1.focus() == c2.focus()
                && w1.counters() == w2.counters()
                && f1 == f2
                && k1.puts == k2.puts,
            "storm: the same storm told twice lands bit-identically, down to the frame's cost"
        );
    }
    Ok(n)
}
