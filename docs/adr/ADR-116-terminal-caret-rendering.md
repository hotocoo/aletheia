# ADR-116 — Visible terminal caret

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 text/desktop interaction · **Builds on:** ADR-083, ADR-105

## Context

ADR-083 made the terminal window a real composed text surface, but the grid exposed only the
console's output. The live input position was therefore implicit: a user could type or move the
line editor cursor without a visible caret in the window.

## Decision

`TextGrid` keeps its existing deterministic renderer for generic panels and exposes an opt-in
`render_packed_with_cursor` path. It renders the existing grid first, then marks the grid's own
write cursor with a one-pixel vertical caret. The live terminal window opts into this path; menus,
taskbar chrome and monitor/help panels retain the original renderer.

The caret is presentation only. It introduces no new input authority, timer, focus state, or
allocation, and it uses the grid's existing cursor coordinates and scanout bounds.

## Invariants

- Terminal caret position is derived exclusively from the terminal grid's existing cursor.
- Generic grid rendering remains byte-for-byte unchanged when the opt-in path is not used.
- The caret is clipped by the same packed-buffer bounds as every other pixel.
- Caret rendering performs no allocation and does not change surface geometry.
