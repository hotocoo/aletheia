# ADR-112 — Desktop launcher in the taskbar

## Status

Accepted

## Context

The desktop already had a compositor-owned context menu reachable through right-click and
Shift+F10, while the taskbar contained only application buttons. Pointer users therefore had no
stable taskbar affordance for opening the same desktop command surface.

## Decision

Reserve the first eight text cells of the taskbar as a `[menu]` launcher. A press in that region
opens the existing compositor-owned menu above the taskbar; it does not create another menu or
focus authority. The menu continues to delegate window operations to `WindowManager`.

The launcher hit map is a pure function shared by the pointer path and its host-side tests. The
application button regions remain unchanged, so existing terminal/monitor/help taskbar behavior
and keyboard Alt+1/2/3 launchers retain their contracts.

## Invariants

- The launcher never focuses the menu as an application window.
- The existing menu surface and token are reused; no click can allocate an unbounded surface.
- Application taskbar hit regions remain disjoint from the launcher region.
- Window-management actions still execute exclusively through `WindowManager`.
- The menu is clamped to the scanout and opens above the taskbar when geometry permits.
