# ADR-109 — Edge Snapping Uses a Forgiving Release Band

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-098, ADR-099, ADR-104, ADR-108

## Context

Pointer edge snapping previously required the release point to be within one pixel of a scanout
edge. That is deterministic, but it is unnecessarily difficult to hit with a normal pointer
gesture, especially when a window is being dragged quickly toward a corner.

## Decision

`WindowManager` treats the final pointer position as an edge-snap request when it is within a fixed
16-pixel band of any scanout edge. The same band applies to corners, with corner classification
winning when both axes are inside the band.

The policy remains in the window manager, is allocation-free, and uses the scanout dimensions as
the sole geometry authority. Releases outside the band retain ordinary free-form drag behavior.

## Verification

* `kernel-core/tests/wm.rs` proves a release at the edge-band boundary snaps and a release one
  pixel beyond the band does not.
* Existing half-screen, lower-half, and corner snap tests continue to exercise the same geometry
  paths.
