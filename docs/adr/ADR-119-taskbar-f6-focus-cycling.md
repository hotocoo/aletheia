# ADR-119 — F6 cycles the active taskbar selection

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-118

## Context

F6 enters taskbar keyboard navigation. Once active, pressing F6 again previously exited the mode,
which made repeated keyboard traversal less direct than the existing Tab and arrow interactions.
Reverse traversal also needs a direct function-key gesture so users can move backward without
switching to an arrow-key chord.

## Decision

F6 enters taskbar navigation when it is inactive. While navigation is already active, F6 advances
to the next taskbar affordance and Shift+F6 moves to the previous affordance, both using the same
canonical wrapping order as Tab/right-arrow and Shift+Tab/left-arrow navigation. Escape remains the
explicit way to leave the mode.

## Invariants

- F6 never leaks into the focused application.
- Forward F6 traversal wraps from the final workspace to the launcher.
- Reverse Shift+F6 traversal wraps from the launcher to the final workspace.
- F6 and forward Tab/right-arrow preserve the same affordance order.
- Shift+F6 and Shift+Tab/left-arrow preserve the same reverse affordance order.
- Enter and Space activation remain unchanged.
