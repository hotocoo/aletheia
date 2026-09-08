# ADR-092 — Keyboard focus traversal belongs to the window manager

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 keyboard-focus rung · **Builds on:** ADR-084 (managed windows), ADR-085 (shared desktop), ADR-086 (allocation-free desktop hot paths)

## Context

The desktop had pointer focus and window raising, but no keyboard-only way to move between
managed windows. The existing `Tab` byte is part of the console's command-completion grammar and
must remain available to the focused terminal. Adding a second focus authority at the input-device
layer would violate the window-manager boundary.

## Decision

`Ctrl+Tab` cycles focus forward and `Ctrl+Shift+Tab` cycles backward. The shortcut is consumed by
the shared desktop before the focused surface receives the `Tab` byte; plain `Tab` remains unchanged.

The `WindowManager` walks the compositor's existing placement table without allocating, skips
surfaces it does not own, raises the selected window, and changes focus through the existing input
session. Consequently visual stacking and keyboard focus stay aligned, and the existing
`FocusLost` queue semantics remain authoritative.

The keyboard decoder exposes held modifier state only; it does not invent a new byte alphabet.

## Proof

`kernel-core/tests/wm.rs` proves forward/backward cycling across the managed windows and the
existing `vinput` suite continues to prove the keyboard decoder's modifier and alphabet contract.
The implementation uses the same bounded placement table as pointer routing, so the shortcut path
does not recreate the per-event allocation that ADR-086 removed.

## Non-claims

There is still no application-defined focus order, focus ring rendering, mouse wheel routing,
resize/minimize/maximize, or user-mode application window ownership.
