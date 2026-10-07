# ADR-222 — A game, and a clock to pace it

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-USER-015 (new)
**Builds on:** ADR-215..218 (a program's window, its keys, its pointer, colour), ADR-213 (programs
left running), ADR-200 (the machine's monotonic clock).

## Context

ADR-215..218 gave a program everything an interactive application needs to see and be steered,
but nothing to pace itself by: the only way to wait was a busy loop, whose speed is the emulator's
and the host's. The first attempt at a game ran all its frames in about 38 ms and was over before
anyone saw it.

## Decision

* **`SYS_CLOCK` (15)** returns nanoseconds since boot from the machine's monotonic clock (the
  same one `uptime` reads, ADR-200), answered in the trap handler itself - it reads a counter and
  touches nothing of the program's, so it needs no grant, like `SYS_YIELD`.
* **`snake`**, seeded in every new namespace and built from `userland/src/snake.rs`: a 40 x 30
  board in a 160 x 120 RGB332 window, steered with `w`/`a`/`s`/`d` or a click to one side, one
  move every 120 ms of the machine's clock, food from a deterministic generator. `q` quits; the
  exit status is 2000 + the score. With `clock` as its argument it is the boot suite's clock probe.

## Proof

* Boot, every target (`usermode` 61/61/69): two clock readings a program takes move forward
  (`snake clock` exits 1); `snake` in a gate image, with no desktop, ends cleanly with 2000.
* Live, aarch64 and riscv64 (`scripts/desktop-e2e-dt.sh`): `start snake` puts its yellow head
  and grey wall on the dumped scanout (16 and 2,176 pixels), a `w` reaches it, and it ends itself
  at the wall reporting `exited with status 2000`.

## Non-claims

* A program still waits by asking the clock in a loop: there is no sleep, so a waiting game spends
  its turns. Polling only; no audio; one-bit-per-key input (ADR-216).
