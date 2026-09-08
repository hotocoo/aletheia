# ADR-120 — Start-menu number accelerators

## Status

Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-112, ADR-118, ADR-119

## Context

The desktop start menu already supported pointer selection and keyboard Up/Down navigation, but
keyboard users had to traverse the entire bounded command list to reach a command near the end.
The menu contains exactly nine commands, so a fixed number-row accelerator can provide direct
selection without introducing a dynamic command registry or another focus authority.

## Decision

While the compositor-owned start menu is visible, number-row keys `1` through `9` select the
corresponding menu item immediately. The selection is repainted but is not activated until the
existing Enter/activation path runs. The desktop consumes these number-row events only while the
menu is visible; outside the menu, existing application and desktop shortcuts retain their
behavior.

The taskbar launcher also uses the pointer hand cursor, matching its existing clickable hit region
and the application's/workspace taskbar affordances.

## Invariants

- The menu remains presentation-only and never becomes a second focus authority.
- Number accelerators select exactly one of the nine existing bounded commands.
- Number-row input is not leaked to the focused application while the menu is visible.
- Existing `Alt+number` launchers and `Ctrl+Alt+number` workspace shortcuts remain unchanged.
- No dynamic surfaces, buffers, or command entries are allocated by the accelerator path.
