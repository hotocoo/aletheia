# ADR-113 — Pointer-selectable workspace strip in the taskbar

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-095, ADR-112, workspace switching

## Context

The desktop already exposed four workspaces and keyboard shortcuts for switching them, but the
taskbar only displayed the active workspace as text. Pointer users therefore had no direct visual
control for changing workspaces.

## Decision

Reserve the taskbar region immediately after the application buttons for four fixed workspace
buttons. A click delegates the workspace transition to `WindowManager::switch_workspace`; the
taskbar remains compositor chrome and never becomes a focus or ownership authority. The active
workspace is marked with `[*]`, while inactive workspaces retain their numbered labels.

The workspace hit map is a pure function shared by the pointer path and host-side tests. Cursor
hover uses the hand shape for the same four regions. The status portion is compact and bounded so
the one-row taskbar cannot wrap its presentation state.

## Invariants

- Workspace changes remain exclusively owned by `WindowManager`.
- Workspace buttons cannot receive application keyboard input or focus.
- All four hit regions are fixed, disjoint, and independent of dynamic window labels.
- Clicking the already-active workspace is a no-op.
- Existing menu and application taskbar regions remain unchanged.
