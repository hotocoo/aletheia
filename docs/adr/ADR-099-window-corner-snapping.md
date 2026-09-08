# ADR-099 — Dragged windows snap to desktop corners

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-084, ADR-093, ADR-095, ADR-097, ADR-098

## Context

ADR-098 made edge drops deterministic, but a desktop with multiple applications also needs a
quick way to claim a quarter of the screen. A corner is a more specific edge target than a side,
so corner recognition must happen before the existing full-edge policy.

## Decision

When a **move** drag is released at a scanout corner, the window manager claims the corresponding
quarter: top-left, top-right, bottom-left, or bottom-right. Non-corner edge releases retain ADR-098's
top-maximize, left/right-half, and bottom-half behavior. Resize drags are never snapped.

The snap is still a manager decision. Geometry passes through the compositor's owner-token checks,
the pre-snap geometry remains the maximize/restore target, and focus and z-order stay aligned with
the existing window lifecycle.

## Proof

Host window-manager tests cover all four corner quadrants and verify that restoring the snapped
window returns its original geometry. Existing edge-snap tests continue to prove that non-corner
edge drops retain their previous layouts.
