# ADR-121 — Start-menu accelerator labels

## Status

Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-120

## Context

ADR-120 added direct number-row selection for the nine start-menu commands, but the menu did not
visually expose those accelerators. The shortcut existed, but its discoverability depended on the
separate shortcut reference window.

## Decision

Render each start-menu command with its `1`–`9` accelerator beside the existing selection marker.
The menu width is increased only enough to keep the longest numbered command fully visible. The
accelerator remains presentation-only: Enter still performs activation and the menu remains
compositor-owned rather than becoming a focus authority.

## Invariants

- All nine existing commands retain their original order and activation behavior.
- The displayed accelerator maps one-to-one to the existing `menu_number_selection` policy.
- No dynamic menu registry, surface, or per-event allocation is introduced.
- Number-row input remains consumed only while the menu is visible.
