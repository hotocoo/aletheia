# ADR-107 — Keyboard taskbar launchers belong to the desktop

## Status

Accepted.

## Context

The desktop taskbar already provides mouse-accessible controls for the terminal, monitor and
shortcut reference windows. Keyboard focus traversal exists, but reaching a known window still
requires cycling through the current z-order. A keyboard-only user should have a deterministic
direct route to the same three desktop applications.

## Decision

`Alt+1`, `Alt+2` and `Alt+3` are desktop-level launch/focus shortcuts for the terminal, monitor and
shortcut reference respectively. The desktop uses the same managed-window lifecycle as the
taskbar: a closed target is reopened, a minimized target is restored, and an already visible
target is raised and focused. The shortcut is consumed by the desktop after modifier state is
fed to the decoder, so the focused application never receives the number-row byte.

`F6` enters taskbar keyboard navigation. Left/Right moves through the launcher, three application
targets and the workspace strip; Enter activates the selected target and Escape leaves the mode.
The selection is presentation state only and activation delegates to the same taskbar/window-
manager operations used by pointer input.

## Consequences

- Keyboard and pointer access reach the same taskbar targets.
- A keyboard-only user can reach every taskbar affordance without requiring pointer input.
- Reopening uses the existing manager-owned token and lifecycle path; no second launcher authority
  is introduced.
- The hot path remains bounded and allocation-free for an already-open window.
- The shortcut reference remains the source of truth for the user-visible key binding.
