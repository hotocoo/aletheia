# ADR-217 — A program feels the pointer over its window

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-013 (new)
**Builds on:** ADR-216 (a program hears what is typed at its window), ADR-081 (the pointer).

## Context

After ADR-216 a program heard keys but not the pointer: a click on its window focused it and
nothing more. Anything with buttons, a canvas or a playfield needs where the pointer is and when
it is pressed.

## Decision

* **The desktop forwards the pointer over a program window's client area** (below its title
  band) to that window, in the window's own coordinates, before its own handling - focus, drag
  and resize behave exactly as before.
* **A small fixed ring in `AppWindow`** (16 events, no allocation) keeps them for the owner: a
  move right after a move replaces it, so a program slow to poll sees where the pointer is rather
  than a backlog; a full ring drops the event. Pointer events are handed out before keys.
* **Encoding** through the same `SYS_POLL_INPUT`: `appwin::POINTER | x | y << 16`, with `HELD`
  while the left button is down after the event and `CLICK` when the event is that button
  changing. No new syscall, no new grant: the pointer, like the keys, belongs to the window.
* **`draw`** parks its bar under a click, and its `q` exit status reports the last click's x
  (1000 + x).

## Proof

* Host: moves coalesce, clicks do not, the held state follows press and release, a full ring
  keeps what it has, nothing encodes as the refusal value, and only the owner may poll
  (`kernel-core/src/appwin.rs`).
* Live, aarch64 and riscv64 (`scripts/desktop-e2e-dt.sh`): a click injected at scanout
  (200, 100) on a window opened at (160, 40), then `q`: `draw` exits with status 1040.

## Non-claims

* Left button only; no wheel; no pointer capture outside the window (a drag that leaves the window
  stops reporting).
