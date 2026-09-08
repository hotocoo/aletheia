# ADR-102 — Keyboard window nudging belongs to the window manager

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction rung · **Builds on:** ADR-092, ADR-093, ADR-098, ADR-101

## Context

Keyboard snapping gives the desktop deterministic coarse layouts, but it cannot make a small
spatial correction without returning to pointer dragging. That makes keyboard-only placement less
useful and encourages applications to invent their own geometry authority.

## Decision

`WindowManager::nudge_focused` moves the focused visible managed window by a fixed 16-pixel step in
one of the four cardinal directions. The manager-held owner token performs the move and the
operation is allocation-free. The desktop consumes `Ctrl+Alt+Shift+Arrow` before the focused
application sees the key.

Maximized or snapped windows are intentionally not nudged. Their saved restore geometry represents
the user's pre-layout placement; silently turning a nudge into a restore-and-move operation would
destroy that contract. Hidden windows are likewise left untouched.

## Proof

`kernel-core/tests/wm.rs` proves all four directions, fixed-step movement, scanout clamping, and
the no-effect behavior for minimized and snapped windows.

## Non-claims

This is fixed-step keyboard placement, not arbitrary keyboard geometry editing or a persistent
layout profile.
