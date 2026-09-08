# ADR-104 — Pointer edge and corner window resizing belongs to the window manager

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-084, ADR-103

## Context

The desktop previously exposed only a bottom-right resize grip. That makes resizing awkward for
windows whose opposite edge is the convenient one to grab and leaves corner resizing dependent on
one particular corner.

## Decision

The window manager recognizes eight pointer resize targets: four edges and four corners. A resize
keeps the opposite edge fixed; a corner changes both axes. The minimum geometry remains the same
as the existing bottom-right contract, and every resulting rectangle must remain inside the
scanout before the compositor is asked to resize it.

The text-grid painter exposes matching edge/corner marks using the same `RESIZE_W` constant. The
manager remains the sole geometry authority and continues to use the manager-held surface token.
Maximized/snap restore state is cleared by a free-form resize, exactly as before.

## Verification

* `kernel-core/tests/wm.rs` proves edge and corner geometry, fixed opposing edges, and the existing
  bottom-right behavior.
* `cargo test --test wm` passes 34/34.
* `cargo test --lib` passes 108/108.
* `cargo test --tests` passes all kernel-core test binaries, including the 34-test WM suite.
