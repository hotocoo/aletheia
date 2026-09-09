# ADR-131 — Cross-target 250 Hz desktop cadence

**Status:** accepted

**Date:** 2026-09-09

## Decision

Run the interactive desktop pump at **250 Hz (4 ms nominal period)** on aarch64 and RISC-V,
matching the x86-64 PIT posture established by ADR-130.

The platform timer remains the wake source on each target:

| Target | Timer source | 250 Hz programming |
| --- | --- | --- |
| x86-64 | PIT IRQ0 | existing 4 ms cadence |
| aarch64 | EL1 physical timer PPI | `CNTFRQ_EL0 / 250` |
| RISC-V | S-mode timer via SBI | `40_000` ticks at the QEMU 10 MHz timebase |

This is a latency posture, not a claim that hardware interrupt delivery is sub-4 ms under all
loads. The desktop remains timer-driven until the interrupt-driven virtio-input rung is implemented
and measured.

## Rationale

The previous DT-target desktop cadence was 100 Hz. That imposed a nominal 10 ms scheduling quantum
on interactive input even though the compositor and input path were already bounded and allocation-free.
250 Hz reduces the timer contribution to the same 4 ms class already selected for x86-64 without
introducing a 1 kHz global desktop pump and its unnecessary wake/power cost.

## Verification

`bash scripts/desktop-e2e-dt.sh` passes after the change on both aarch64 and RISC-V. The live workflow
drives real QEMU virtio keyboard/tablet events and exercises cursor mapping, focus routing, shell input,
and a desktop-only keyboard action.

The existing x86-64 250 Hz posture is unchanged. No claim is made about physical silicon frequency,
interrupt latency, or overclocking from this ADR; those require hardware-qualified measurements.
