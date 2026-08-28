# ADR-079: The composed desktop meets input — focus is authority, the cursor is the compositor's own

**Status:** Accepted · **Date:** 2026-08-29 · **Advances:** ALET-P2-021 (the input-routing
rung; wiring a REAL input device's bytes through the session stays scoped) · **Builds on:**
ADR-077 (the composition contract — owner tokens, exact clipping, painter's z-order),
ADR-078 (the composition contract meets the scanout), ADR-049/050 (the keyboard and the
fail-closed console alphabet), ADR-063 (the boot heap never frees), ADR-064 (the machine
measures itself)

## Context

The graphics stack now has a device leg: ADR-077 defined who may draw and where, and ADR-078
put the verdict on real pixels through the virtio-gpu flush path. What it did not have was the
other half of any interactive GUI: INPUT. The gap register named it plainly — "no input
routing, no cursor queue" — and the omission is not cosmetic. A keystroke is an authority
decision about WHO may read what the user typed; a pointer is a device nobody else may steer.
A compositor that lets any surface read any event, or lets any token holder move the cursor,
would be an ambient-authority hole sitting exactly where the user's attention is.

The same question the composition contract answered twice applies a third time, with one new
possession: WHO decides where events go, WHO may read them, and WHO — if anyone — may steer
the pointer.

## Decision

### The input path is ONE session; focus is ONE surface; the owner alone reads

`kernel-core/src/compositor.rs` extends the contract with three possessions and one
compositor-owned plane:

* **The input session.** `open_input_session` mints exactly ONE session token per compositor
  (`next_serial ^ secret`, possession-based like every authority here). Every event post,
  focus change, and cursor move answers to it — a second opening is refused `InputSealed`,
  and absent, wrong, and forged session tokens are all `NotInputSession`, refused BY NAME and
  COUNTED. The decomposition is deliberate: the input path (the driver standing between the
  user's hardware and the desktop) is one principal, and a second opinion about where the
  user's keystrokes go is exactly the ambient authority this contract refuses to mint.
* **Focus.** At most ONE surface is focused, and only a PLACED one (minted-but-unplaced is
  `NotPlaced`, unknown is `UnknownSurface`). Refocusing moves it, and the surface that LOST
  focus is told — a synthesized `FocusLost` delivered through its own bounded queue, dropped
  AND COUNTED if that surface stopped draining. Refocusing the already-focused surface is
  idempotent: nothing was lost, so nothing is queued.
* **The routing/reading split.** `post_key` routes a decoded keystroke (the keymap's output
  alphabet — the same alphabet the console editor already rules on, never a raw device byte)
  to the FOCUSED surface's bounded queue; `drain_input` empties it — OWNER TOKEN ONLY. The
  input path decides WHERE events go; the owner decides WHO reads them; neither can act as
  the other. A wrong owner token on `drain_input` is the same refusal a forged draw token
  is. Delivery carries a per-compositor monotonic `seq`, so order is observable and
  reorder/replay is detectable. With nothing focused, a keystroke is refused `NoFocus` and
  exists NOWHERE — asserted, not assumed.
* **The bounded queue.** `MAX_INPUT_EVENTS` (32) per surface. The next keystroke past the
  cap is refused `Backlogged` AND counted as a drop — a window that stops draining loses
  input loudly, never silently, and the queue's existing events are never evicted to make
  room. Draining restores capacity exactly. A `FocusLost` into a full queue is a counted
  drop too — and focus still moves. Detaching the focused surface clears focus and the
  queue dies with the surface: no event outlives its surface, and a re-minted id starts
  EMPTY (events are never resurrected under a fresh mint).

And input is not pixels: a keystroke with no repaint damages NOTHING — a quiet frame stays
quiet (measured, ADR-064's posture).

### The cursor is the compositor's own plane

The cursor is NOT a surface: no token names it, no z-order slot holds it, no surface can
cover, read, or steer it. Only the input session may `move_cursor`/`hide_cursor` it. It is an
8x8 crosshair, 1-bit, transparent where 0 — a mask, not a paint-over: window ink shows
through the transparent bits. It paints LAST, above every surface (raising a window to the
top of the z-order changes nothing about it), and it obeys the SAME geometry contract as
everything else: a position whose glyph could never show a pixel is refused
`CursorOffScanout` with the position named; a partially-off position is legal and clipped
EXACTLY at compose time (the guard-band proofs hold through every edge). Its moves are
visible the SAME frame through the same damage machinery (old and new glyph rects both
damaged; a no-op move costs nothing), and its cost is REPORTED: `FrameStats` gains
`cursor_pixels`, so "the cursor is cheap" is a measurement, not an adjective.

### Host proofs

`kernel-core/tests/input.rs` (8 tests): the in-kernel boot suite is host-run first; the
session table is swept fail-closed (every re-opening and every wrong-token op refused and
counted, the token still valid afterward — refusals are not corruption); the focus decision
table over every reachable state (none, A, B) × every op target (A, B, minted-unplaced,
unknown, detached); the routing/reading split swept over a four-surface alphabet delivery
(seq-monotonic, exactly-once, no cross-queue leakage, zero raster puts across a whole
post/drain cycle); the bounded-queue matrix at capacity (drops counted, contents never
evicted, capacity restored exactly, the FocusLost-of-a-full-queue drop); the cursor's
authority and exact geometry over corner-exact and partially-off positions against the
guard-band raster (out-of-bounds puts zero, hide reversible, no-op move free); the punch-through
matrix (cursor ink over a zero-ink window, window ink through a transparent glyph bit, raise
changes nothing); and determinism — two engines fed an identical mixed input sequence land
bit-identical rasters with identical counters.

### Boot gate

`input_suite` (12 invariants, in `compositor.rs` beside the composition suite, running
UNCONDITIONALLY on every target — the contract is arch-neutral and needs no device): the
session mints once; focus requires placement; events route to the focused surface and only
its owner drains them, in order, exactly once; exactly one focus with the loser told; a
keystroke with nothing focused is queued nowhere; a wrong session token changes nothing; the
queue is bounded with the drop counted and capacity restored exactly; detaching the focused
surface clears focus and kills the queue; a re-minted id starts empty with the old token
dead; the cursor is session-moved, fully-off refused by name, partially-off clipped to an
EXACT measured cost (36 background puts + 11 crosshair bits on the 640x240-shaped shadow's
6x6 clipped region); the cursor paints above every surface and hide reveals what was below;
and a keystroke is not a pixel, with identical input sequences landing bit-identical. Boot
fails 660+i.

### Conformance and marker maps

Four behaviors join the cross-CPU contract (154 → 158): the session table, the routing/reading
split, the wrong-session no-op, and the cursor's authority/exact clip. Marker maps changed
deliberately: `input=12` joins the three QEMU gates' expected families, and the boot suite's
prose marker joins VirtualBox's REQUIRED list — the family is arch-neutral, so VirtualBox
proves it too (unlike the virtio-gpu families it lists SKIP).

### Named non-claims, in the register

No REAL input device is wired through the session yet — the console's PS/2/serial path still
feeds the serial console, and this rung routes the contract's events synthetically; the
hardware rung (keymap bytes entering the session on all three targets) is the next rung. No
pointer hardware exists — the cursor is moved through the session API. The queue is bounded
with drops counted, and no flow control beyond that (a surface that wants every keystroke
must drain — that is the contract). No text rendering, no IME, no alpha, no device-level GPU
isolation between surfaces, no interrupt-driven completion — all still open in the row.
