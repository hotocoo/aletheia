# ADR-095 — Desktop taskbar is compositor chrome

**Status:** accepted

## Context

Window minimization is reversible, but a shortcut-only restore path is not sufficient desktop
chrome. A user needs a persistent visual affordance for windows that are open, hidden, focused,
or closed.

## Decision

The shared desktop owns a small taskbar surface below the managed window stack. It is compositor
chrome, not a `WindowManager` window: it has no application input queue and cannot receive keyboard
focus. Two fixed button regions represent the terminal and monitor windows.

Pressing a button restores a hidden window, focuses and raises an unfocused visible window, or
minimizes the currently focused visible window. The taskbar label is derived from live window
state and is repainted only when that state changes, preserving the desktop's quiet-frame contract.

## Consequences

- Minimized windows have a visible restore path without re-minting a surface or token.
- Taskbar chrome cannot become an application authority or steal keyboard focus.
- Taskbar updates remain damage-driven rather than timer-driven.
- The desktop keeps the same taskbar behavior on every architecture using the shared desktop.
