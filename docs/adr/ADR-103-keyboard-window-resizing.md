# ADR-103 — Keyboard window resizing belongs to the window manager

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-084, ADR-093, ADR-095, ADR-101, ADR-102

## Context

The desktop can resize a managed window through its pointer resize grip, and it can move or snap
windows entirely from the keyboard. A keyboard-only user should not need to switch input methods
just to change a window's size.

## Decision

`Ctrl+Alt+R` toggles keyboard resize mode. While the mode is active, the four cursor arrows resize
the focused visible window by a deterministic 16-pixel step. `Enter` or `Escape` exits the mode
without changing geometry.

The resize policy remains in `WindowManager`, not the input decoder or compositor. It uses the
manager-held owner token and refuses hidden or maximized/snapped windows without disturbing their
saved state. The result is allocation-free on the desktop hot path and bounded by both scanout
geometry and the compositor's surface-pixel ceiling.

The desktop consumes resize-mode events before application routing, while still feeding them to
the keyboard decoder so modifier state remains faithful.

## Verification

* `kernel-core/tests/wm.rs` proves fixed-step resizing, scanout/surface bounds, and refusal of
  hidden or snapped windows.
* `cargo test --test wm` passes 33/33.
* `cargo test --test experience_gui` passes 2/2 after the shared desktop change.
