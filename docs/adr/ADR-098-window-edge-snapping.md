# ADR-098 — Dragged windows snap to desktop edges

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-084, ADR-093, ADR-095, ADR-097

## Context

The desktop can already move, resize, minimize, maximize, close, focus, and tile managed
windows, but free-form dragging has no direct layout affordance. A user should be able to place a
window against an edge without manually matching exact coordinates.

## Decision

When a **move** drag is released at a scanout edge, the window manager applies a deterministic
edge layout: the top edge maximizes the window, while the left and right edges claim the
corresponding half of the scanout and the bottom edge claims the lower half. Resize drags are not
snapped. A snap records the pre-snap geometry as the restore geometry, preserving the existing
maximize/restore contract.

The policy is implemented in the window manager, not the pointer decoder or compositor. The
owner token, input queue, focus authority, and z-order remain unchanged; geometry still passes
through the compositor's owner-token checks and damage accounting.

## Proof

Host window-manager tests cover left-edge maximize/restore, right-edge half snapping, and
bottom-edge lower-half snapping. The desktop routes pointer release through the same policy, so
the live input path uses the tested manager decision.
