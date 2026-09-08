# ADR-095 — Managed window tiling belongs to the window manager

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop layout rung · **Builds on:** ADR-084 (managed windows), ADR-085 (shared desktop), ADR-086 (allocation-free desktop hot paths), ADR-093 (resize), ADR-094 (minimize)

## Context

The desktop can now resize, maximize/restore, minimize/restore, close and keyboard-cycle managed
windows. Those controls still leave layout entirely manual. A useful desktop needs a deterministic
way to arrange the windows without introducing an application-level layout authority.

## Decision

`WindowManager::tile_visible` tiles every visible managed window into equal-width columns spanning
the scanout. Minimized windows are excluded and retain their visibility, placement and geometry.
The manager changes geometry through the existing owner-token-gated compositor operations, keeps
the input queues and tokens intact, preserves focus and z-order, and clears maximize restore state
because the tiled geometry becomes the current layout.

The shared desktop consumes `Ctrl+Alt+T` as the tile shortcut. The keystroke is fed to the keyboard
decoder only to preserve modifier state and is never delivered to the focused application.

The manager validates scanout and minimum column geometry before changing any window, and the
layout walks its existing window table without creating a per-event allocation.

## Proof

`kernel-core/tests/wm.rs` proves two visible windows tile to exact scanout columns with focus
preserved, and that minimized windows are excluded without changing their state. Existing
compositor resize and visibility tests continue to prove the owner-token and damage contracts.

## Non-claims

This is deterministic column tiling, not a general-purpose application-defined layout engine.
There is no persistent user layout profile, arbitrary split tree, or drag-and-drop tiling policy.
