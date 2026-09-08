# ADR-093 — Managed windows can resize without changing their authority

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-021 graphics/compositor milestone · **Builds on:** ADR-077, ADR-084, ADR-085, ADR-086, ADR-092

## Context

ADR-084 deliberately left resize out because a surface and its terminal grid were allocated at
open time. That made the desktop usable but fixed-size. A resize operation must not create a second
authority: the same window owner must retain the token, the input queue must survive, and a failed
geometry change must leave the old surface untouched.

## Decision

The compositor gains an owner-token-gated `resize_surface` operation. It validates the new geometry
before mutation, preserves pixels in the old/new intersection, blanks newly exposed pixels, and
damages the old and new screen extents so the next compose removes stale pixels and paints the new
geometry. The owner token and input queue remain unchanged.

The window manager exposes the bottom-right resize grip. A press returns `Press::Resizing`, pointer
motion changes the surface geometry, and release ends the operation. The grip is painted by the
text grid from the same geometry predicate used by hit testing. Tiny windows omit the grip rather
than making the control consume most of the client area.

The live terminal grid follows the resized surface, preserving its top-left cell intersection and
clamping the cursor. The minimum interactive geometry keeps both the close box and resize grip
available.

## Proof

Host proofs cover owner refusal, geometry refusal, pixel preservation, resize damage, resize-grip
routing, and window-manager geometry synchronization. The full `kernel-core` test suite passes.

## Non-claims

Resize is a control-plane allocation on Aletheia's never-freeing heap; repeated growth can consume
additional heap capacity. There is still no application-defined layout policy, maximize, minimize,
tiling, or user-mode window ownership.
