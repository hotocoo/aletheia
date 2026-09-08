# ADR-115 — Taskbar workspace occupancy indicators

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-113, ADR-114

## Context

ADR-113 made the four workspaces directly selectable from the taskbar and ADR-114 made their
pointer targets visible on hover. The strip still exposed only the workspace number, so a user
could not tell whether another workspace contained managed windows without switching into it.

## Decision

Each fixed workspace button now renders `[n:c]`, where `n` is the workspace number (or `*` for
the active workspace) and `c` is the number of currently managed windows assigned to that
workspace, capped at nine for the one-row text affordance. Hover keeps the same information and
adds the existing leading `>` marker.

The count is derived from the WindowManager's managed set and is presentation-only. It does not
change workspace ownership, visibility, focus, z-order, or switching behavior. Taskbar repaint
remains fact-driven: occupancy changes are part of `TaskbarFacts`, so the desktop repaints only
when the displayed state changes.

## Invariants

- Workspace activation geometry is unchanged from ADR-113.
- Occupancy is derived from manager-owned window state; the taskbar does not own workspace state.
- Counts are bounded to one display digit and cannot overflow the fixed workspace cell.
- Hover remains presentation-only as specified by ADR-114.
- An idle desktop still performs no taskbar repaint when occupancy and all other facts are stable.
