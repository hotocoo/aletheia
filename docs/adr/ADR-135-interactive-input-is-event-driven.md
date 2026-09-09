# ADR-135 — Interactive input is event-driven, with a timer watchdog

**Status:** Accepted  
**Date:** 2026-09-09  
**Builds on:** ADR-079, ADR-080, ADR-126, ADR-133, ADR-134

## Decision

The x86-64 `interactive` build enables the existing virtio-input MSI-X wake path by default.

The MSI-X handler performs only timestamp/sequence accounting, requests foreground desktop service,
and acknowledges the local APIC. It does not drain queues, route windows, or touch the framebuffer.
The foreground desktop therefore retains the existing single-owner and interrupt-masked mutation
boundary while input no longer waits for the normal 1 kHz timer cadence.

When both live virtio-input functions successfully receive MSI-X delivery, the PIT desktop cadence
is reduced to a 100 Hz watchdog. A missed device interrupt can therefore still recover through the
timer, while the normal path is device-event driven.

`input-msix` remains independently selectable for qualification/A-B builds, and MSI-X is still
bounded by the CPU's supported interrupt-controller path and the device-advertised capability/table.

## Rationale

The prior interactive posture paid a timer wakeup cost even when no input arrived. Raising the timer
from 250 Hz to 1 kHz reduced the worst-case polling quantum, but it could not beat the fundamental
polling delay without increasing interrupt rate further. MSI-X removes that tradeoff for supported
virtio-input devices: an input event itself wakes the desktop, while the watchdog bounds recovery.

This is a latency optimization, not a throughput or physical-hardware-overclock claim.

## Verification

- `cargo test --manifest-path kernel-core/Cargo.toml`
- `bash scripts/desktop-e2e-dt.sh`
- `bash scripts/vm-e2e-x86.sh`
- `bash scripts/vinput-e2e.sh`
- `BOOT_SAMPLES=2 WORKLOAD_OPS=20 bash scripts/comparative-bench.sh`

The benchmark must report measured values; no latency improvement is claimed until the live workflow
passes and the post-change comparison is recorded.
