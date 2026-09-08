# ADR-101 — Keyboard window snapping is a manager operation

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop layout rung · **Builds on:** ADR-084, ADR-098, ADR-099, ADR-100

## Context

Pointer edge and corner snapping make the managed desktop spatially useful, but they require a
pointer to be positioned at a scanout edge. Keyboard users need the same deterministic layout
operation without moving a pointer.

## Decision

`WindowManager::snap_focused` applies a four-way half-screen layout to the focused visible managed
window. `Ctrl+Alt+Left/Right` selects the left/right half and `Ctrl+Alt+Up/Down` selects the
upper/lower half. The keyboard event is consumed by the desktop after the decoder sees it, so
modifier state remains accurate and the application never receives the desktop shortcut.

The manager-held owner token performs the resize and move. Existing restore geometry is preserved
when the window enters its first snap state, so directional changes do not destroy the user's
pre-snap placement. Focus and z-order remain aligned with the snapped window.

## Proof

Host window-manager tests verify all four geometries, preserved size/focus contracts, restore
behavior, idempotent directional movement, minimized-window exclusion, and the no-focus case.

## Non-claims

This is a four-way half-screen keyboard layout command, not a general keyboard window-placement
language or persistent workspace layout system.
