# ADR-134 — Keep Qualification Telemetry Out of the Normal x86 Desktop Hot Path

**Status:** Accepted  
**Date:** 2026-09-09

## Context

The x86 desktop has an optional `input-msix` qualification build that measures interrupt-to-
foreground wake latency with TSC-backed counters. The normal interactive build uses the timer
wake path, but the telemetry objects and sampling calls were compiled into that build anyway.
That added dead code and atomics to a latency-sensitive foreground path while producing compiler
warnings for symbols that could never be observed in that configuration.

## Decision

Compile the MSI-X/timer wake-latency telemetry only when the `input-msix` feature is enabled.
The normal `interactive` build therefore contains no qualification sampling state or foreground
sampling calls. `shellio` reports no IRQ telemetry in that configuration rather than fabricating
zeroes. The `input-msix` build retains the complete measurement surface unchanged.

This is a build-time separation, not a change to interrupt semantics: the normal desktop remains
timer-woken, while the experimental MSI-X path remains explicitly feature-selected and measurable.

## Verification

- `scripts/vm-e2e-x86.sh`: **PASS**, including UEFI boot, W^X, SMP, ring-3, VT-d, persistence and
  rootless custody boots.
- The normal x86 release build completes without the former dead-code warnings.
- The `interactive,input-msix` release image builds successfully, preserving the qualification
  configuration.

## Non-claims

This ADR does not claim that the normal interactive build uses MSI-X, nor does it claim physical
hardware overclocking or lower end-to-end latency without hardware qualification.
