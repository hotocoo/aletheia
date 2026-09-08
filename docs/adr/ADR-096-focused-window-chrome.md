# ADR-096 — Focus is visible in window chrome

**Status:** accepted

## Context

Keyboard focus can move independently of pointer position through `Ctrl+Tab`, taskbar actions,
minimize/restore, and close fallback. The desktop must make the active input target obvious without
introducing a second focus authority.

## Decision

The shared desktop marks the focused managed window in its existing title band with a leading `>`.
The compositor's existing focus id remains authoritative; the title is only a rendered projection
of that state. Both managed titles are repainted only when the focus id changes.

## Consequences

- Users can identify the keyboard destination without probing the windows.
- Focus remains owned by the compositor input session; title chrome cannot grant authority.
- No timer or per-tick repaint is introduced.
