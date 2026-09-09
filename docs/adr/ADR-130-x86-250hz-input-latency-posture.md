# ADR-130 — Raise the x86-64 Desktop Timer Cadence to 250 Hz

**Date:** 2026-09-09  
**Status:** Accepted

## Decision

The x86-64 PIT remains the desktop's proven input pump, but its cadence is raised from 100 Hz to
250 Hz. The timer-driven input wait bound therefore falls from 10 ms to 4 ms without imposing the
10x global interrupt/compositor pressure of a 1 kHz tick.

This is an intermediate latency optimization, not the final interrupt-driven input design. The
bounded `EVENTS_PER_TICK` harvest, compose-only-on-change behavior, and single desktop writer remain
unchanged.

## Verification

- `scripts/vinput-e2e.sh`: PASS, including real QMP-injected keyboard/pointer workflows, focus,
  drag, close, terminal routing, and quiet-state checks.
- `scripts/e2e-all.sh`: PASS for aarch64, RISC-V, x86-64 image boot, and live GUI DT workflows.
- x86-64 boot reports `250 Hz` and the timer IRQ liveness gate still passes.
- VirtualBox remains a host-architecture SKIP on this arm64 machine.

## Rejected alternatives

- **1 kHz immediately:** deferred because the desktop pump is still timer-driven; multiplying the
  global interrupt/compositor cadence by ten should be measured before accepting its CPU/cache cost.
- **Claim interrupt-driven input now:** rejected because MSI-X delivery is not yet verified
  end-to-end on the current QEMU/VT-d path. Timer polling remains authoritative.

## Next step

Implement a proper interrupt-domain/MSI-X path with device-vector allocation and interrupt
remapping where required, then add an interrupt-to-compositor latency benchmark before removing
timer polling.
