# ADR-110 — Conventional keyboard window minimizing

## Status

Accepted

## Context

The shared desktop already exposes keyboard focus, close, maximize/restore, taskbar launchers,
and pointer lifecycle controls. Minimizing a focused window was available through its title bar,
taskbar, and `Ctrl+Alt+M`, but the conventional `Alt+F9` desktop gesture was not mapped.

## Decision

The desktop consumes `Alt+F9` on a key-press event and applies
`WindowManager::toggle_minimize` to the currently focused managed window. The shortcut is fed to
the key decoder only to preserve modifier state; it is never routed into the focused application's
input queue.

The existing window-manager contract remains authoritative: minimizing changes presentation only;
the surface token, input queue, window geometry, and lifecycle remain intact. Restoring follows the
same manager path and re-establishes focus and z-order normally.

## Invariants

- No second window-management authority is introduced in the desktop.
- A key-release does not trigger the action.
- `Alt+F9` never becomes application input.
- Closed or absent focus is a no-op.
- Terminal input state is retained while minimized and cleared only when the terminal is closed,
  matching the existing lifecycle policy.
