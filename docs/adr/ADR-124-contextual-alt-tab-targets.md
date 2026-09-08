# ADR-124 — Alt+Tab lists only reachable desktop targets

## Status

Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-084, ADR-111, ADR-122

## Context

The desktop's Alt+Tab overlay previously rendered a fixed list of all three built-in applications.
That presentation could advertise a closed or minimized window even though
`WindowManager::cycle_focus` can only select a visible managed window. The overlay therefore
could disagree with the actual focus traversal state.

## Decision

Build the switcher presentation from the same live compositor/window-manager state used by focus
traversal. Only open, visible managed windows are listed; the focused target keeps the leading
`>` marker. The overlay retains its existing bounded compositor surface and timeout, and its
keyboard actions remain presentation-only.

## Consequences

- Alt+Tab no longer advertises closed or minimized targets that cannot receive focus.
- Closing or minimizing an application immediately changes the next switcher presentation without
  adding a second window-state authority.
- The switcher remains allocation-bounded because it reuses its existing fixed `TextGrid` and
  packed buffer.
- A pure label-mapping helper is unit-tested so non-window compositor surfaces cannot enter the
  switcher list.
