# ADR-111 — Alt+Tab window switcher overlay

## Status

Accepted

## Context

The shared desktop already supports keyboard focus traversal, but `Alt+Tab` changed focus without
showing the user which managed window was selected. The focused title bar is useful in-place, but
it does not provide an explicit desktop-level switcher affordance during rapid traversal.

## Decision

`Alt+Tab` and `Shift+Alt+Tab` continue to delegate focus traversal to `WindowManager::cycle_focus`.
After each traversal the desktop raises a small compositor-owned switcher surface that lists the
managed windows and marks the current focus. The overlay is presentation-only: it is never focused
or routed application input, and it does not change window-management authority.

The overlay remains visible for a bounded 900 ms after the latest traversal, can be dismissed with
Escape or Enter, and is dismissed when a pointer press begins. Repeated `Alt+Tab` refreshes the same
surface rather than allocating another one.

## Invariants

- Window focus remains owned exclusively by `WindowManager`/`Compositor`.
- The switcher surface cannot receive application input.
- Repeated `Alt+Tab` does not create unbounded surfaces or buffers.
- The timeout is bounded and does not cause continuous repaint after the overlay disappears.
- Existing `Alt+Tab` modifier tracking and application-input isolation remain unchanged.
