# ADR-108 — Escape Cancels Pointer Window Manipulation

## Status

Accepted

## Context

The desktop supports pointer-driven window moves and resizes. Before this decision, once a
pointer drag had changed geometry, the only normal termination was pointer release. A user who
started a move or resize accidentally had no keyboard escape hatch.

## Decision

`Escape` cancels an active pointer move or resize. Cancellation restores the exact position and
size captured when the pointer press began, clears the interaction, and leaves focus and z-order
unchanged. The gesture is allocation-free and uses the window manager's existing owner token.

If no pointer manipulation is active, `Escape` is a no-op at the window-manager layer. Keyboard
resize mode retains its existing `Escape` behavior; an active pointer drag takes precedence so a
single Escape always cancels the in-flight pointer operation first.

## Consequences

* Accidental moves and resizes are reversible without requiring pointer precision.
* Both move and resize interactions have a deterministic press-time restore point.
* The compositor remains the authority for geometry changes; the window manager does not write
  pixels directly.
* The behavior is host-tested for move, resize, and no-op cancellation.

## Non-claim

This does not add multi-step undo history. Only the currently active pointer manipulation can be
cancelled.
