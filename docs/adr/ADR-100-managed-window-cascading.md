# ADR-100 — Managed window cascading belongs to the window manager

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop layout rung · **Builds on:** ADR-084, ADR-086, ADR-093, ADR-094, ADR-095 (managed tiling)

## Context

Column tiling is useful when windows should share the screen, but it destroys the spatial
relationship that makes overlapping application windows useful. The desktop needs a second
deterministic layout operation that exposes each window while retaining its existing size.

## Decision

`WindowManager::cascade_visible` arranges every visible managed window diagonally in manager-table
order using a fixed 32-pixel offset. Each position is clamped to the scanout bounds so the window's
top-left never leaves the desktop. Window sizes, visibility, tokens, queues and z-order are not
changed. Minimized windows are excluded and retain their exact placement.

Moving a window through the cascade clears its maximize restore state because the resulting
geometry is the current layout. Focus is preserved when its focused window remains visible.
The operation uses the existing owner-token-gated compositor move path and performs no per-event
allocation.

The shared desktop consumes `Ctrl+Alt+C` as the cascade shortcut. The keyboard decoder still sees
the event so modifier state remains accurate, but the shortcut is never delivered to the focused
application.

## Proof

Host window-manager tests verify deterministic positions, preserved sizes, focus and z-order, plus
exclusion and preservation of minimized windows. Existing compositor geometry tests cover the
owner-token move contract.

## Non-claims

This is a deterministic desktop layout command, not a persistent layout manager or an application-
defined arrangement API.
