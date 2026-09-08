# ADR-117 — Taskbar keyboard navigation completion

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-107

## Context

ADR-107 made the taskbar keyboard-selectable with `F6`, arrows, `Enter`, and `Escape`. Two
interaction gaps remained: pressing `F6` again while already navigating leaked the key to the
focused application, and reaching a distant workspace could require many arrow presses.

## Decision

`F6` is a true taskbar-navigation toggle: it enters when inactive and exits when active. While
active, `Home` selects the first affordance (the launcher) and `End` selects the last affordance
(the final workspace). The selected target remains presentation-only until `Enter` delegates the
action to the existing desktop/window-manager authority.

## Invariants

- `F6`, `Home`, and `End` are consumed by the desktop and never become application input while
  taskbar navigation is active.
- Repeated `F6` cannot leave a stale keyboard target behind.
- `Home` and `End` always select valid taskbar targets, independent of the current target.
- Existing arrow wrapping, `Enter` activation, `Escape` cancellation, pointer hit geometry, and
  window-manager ownership remain unchanged.
- Taskbar repaint remains fact-driven through `TaskbarFacts`.
