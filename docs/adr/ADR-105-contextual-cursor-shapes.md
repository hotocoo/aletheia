# ADR-105 — Contextual cursor shapes belong to the compositor plane

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-079, ADR-084, ADR-104

## Context

The live desktop had a compositor-owned cursor, but it used one crosshair glyph everywhere. The
pointer already knows which window-local control it is over, so resize affordances and actionable
title/taskbar controls can communicate their meaning before a press.

## Decision

The compositor owns a bounded set of 8x8 cursor shapes: arrow, crosshair, horizontal/vertical
resize, the two diagonal resize directions, and hand. Only the existing input session may change
the shape. Shape changes damage the current cursor rectangle and are visible in the next composed
frame; selecting the current shape is an allocation-free no-op.

The desktop derives the shape from the window manager's existing hit-test geometry. Resize edges
select the matching resize glyph, title lifecycle controls and taskbar buttons select the hand,
and ordinary client/empty space selects the arrow. Hover never changes focus, z-order, geometry,
or ownership.

The compositor's default remains the historical crosshair so existing non-desktop callers retain
their cursor contract; the live desktop explicitly selects the arrow during installation.

## Verification

* `kernel-core/tests/input.rs` proves session-only shape changes, damage on a real transition, and
  idempotent no-damage behavior.
* Existing cursor clipping, z-order, and deterministic input proofs remain green.
* `cargo test --manifest-path kernel-core/Cargo.toml --lib` passes 108/108.
* `cargo test --manifest-path kernel-core/Cargo.toml --test wm` passes 34/34.
* `cargo test --manifest-path kernel-core/Cargo.toml --test input` passes 12/12.
