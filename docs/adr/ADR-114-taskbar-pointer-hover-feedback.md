# ADR-114 — Pointer hover feedback for taskbar controls

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-113

## Context

ADR-113 made the taskbar's application and workspace controls pointer-selectable, but a pointer
could move over a control without any visual indication of which control would receive the next
press. The hand cursor communicates that some region is actionable, but does not identify the
specific target in the dense one-row taskbar.

## Decision

Track a compact, presentation-only taskbar hover id in `TaskbarFacts`. The id is derived from the
same fixed taskbar geometry used for activation. Repainting occurs only when the hover target or
another taskbar fact changes. Application controls show the existing leading `>` focus marker when
hovered, while a hovered workspace receives a leading `>` before its `[n]` marker.

Hover never changes focus, z-order, workspace state, ownership, or application input. The cursor
shape remains independently derived from the same hit geometry.

## Invariants

- Hover geometry and activation geometry remain identical.
- Hover is presentation state only and cannot invoke a desktop action.
- Taskbar chrome outside actionable regions has hover id zero.
- Workspace hit regions remain fixed, disjoint, and unchanged from ADR-113.
- Taskbar repaint remains fact-driven rather than timer-driven.
