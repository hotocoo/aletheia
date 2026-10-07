# ADR-218 — A program draws in colour

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-014 (new)
**Builds on:** ADR-215 (a program draws into its own window), ADR-175 (the photograph behind the
desktop, the first colour the display carried).

## Context

Every surface the compositor owns is one bit per pixel; colour reached the display only for the
wallpaper photograph, through the compose sink's per-surface hook (ADR-175). A program limited to
ink and paper cannot show an image, a chart or a game.

## Decision

* **RGB332 frames.** `SYS_PRESENT`'s width argument carries a flag, `progout::PRESENT_RGB332`
  (bit 32): the buffer is one byte per pixel, `RRRGGGBB`, `width x height` bytes (a 320 x 200 frame
  is 64,000 bytes, inside a program's 16 data pages). No new syscall, no change to the trap paths.
* **The window keeps a colour plane.** `AppWindow` holds the RGB332 frame (allocated when the
  window opens or changes format, never per frame) and derives the window's one-bit plane from
  it by luminance, so the compositor's damage, clipping, z-order and every one-bit readback keep
  working unchanged.
* **The compose sink paints it.** `ComposeSink::with_colour` (the ADR-175 hook, generalised)
  paints that surface's pixels from the plane, expanding each channel's bits to eight (0x00 is
  black, 0xFF white exactly), at any scale; title chrome and other surfaces are untouched.
* **`draw colour`** shows the scene in white over red, green and blue bands.

## Proof

* Host: a colour frame keeps its bytes, its one-bit plane follows brightness, a one-bit frame
  drops the plane (`kernel-core/src/appwin.rs`); RGB332 sizes and bounds (`progout.rs`).
* Live, aarch64 and riscv64 (`scripts/desktop-e2e-dt.sh`): with `draw colour` running, the dumped
  scanout holds 4,867 pure-red pixels - a colour neither the one-bit desktop nor the photograph
  ever produces - and `q` still ends it.

## Non-claims

* 256 fixed colours (RGB332), no palette of the program's choosing, no alpha; one program window.
