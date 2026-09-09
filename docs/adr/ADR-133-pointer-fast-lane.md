# ADR-133 — Pointer Fast Lane in the Interactive Desktop Pump

**Status:** Accepted
**Date:** 2026-09-09

## Context

The interactive desktop pump is deliberately bounded: keyboard and pointer virtio-input queues are
drained from foreground context rather than inside a hard timer interrupt. Before this ADR, the
keyboard budget was serviced completely before the pointer budget. A burst of keyboard events could
therefore delay a pointer motion or click until the keyboard budget was exhausted, even though the
pointer path is latency-sensitive because it controls cursor feedback and focus decisions.

## Decision

Service at most one pointer event before beginning the keyboard burst on every desktop pump. The
remaining pointer work is still drained by the existing bounded pointer budget. The fast lane calls
the exact same `route_pointer_motion` and `pointer_batch` path as normal pointer service.

This does not increase the pump's bounded work budget beyond one additional pointer event; it changes
the ordering so pointer latency is protected from keyboard bursts. The device remains DMA-gated and
all routing/focus authority remains unchanged.

## Verification

- `cargo test --manifest-path kernel-core/Cargo.toml`: **133 unit + all integration/doc tests passed**.
- `bash scripts/vinput-e2e.sh`: **PASS** after the change.
- The live workflow still proves real virtio keyboard/tablet input, pointer mapping, click-to-focus,
  keyboard routing, terminal echo, window drag/close/focus, and quiet-input stability.

## Non-claims

This ADR does not claim interrupt-driven virtio-input. The current desktop still uses the 1 kHz timer
as its wake cadence; true device-interrupt delivery remains a separate hardware/interrupt-controller
wave.
