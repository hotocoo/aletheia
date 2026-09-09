# ADR-132 — Cross-target 1 kHz desktop cadence

**Status:** accepted

**Date:** 2026-09-09

## Decision

Run the interactive desktop pump at **1 kHz (1 ms nominal period)** on all three first-class
targets. x86-64 already uses a 1 kHz PIT; this change raises the aarch64 and RISC-V interactive
timer cadence from 250 Hz to the same 1 ms class.

| Target | Timer source | 1 kHz programming |
| --- | --- | --- |
| x86-64 | PIT IRQ0 | `FREQ_HZ = 1000` |
| aarch64 | EL1 physical timer PPI | `CNTFRQ_EL0 / 1000` |
| RISC-V | S-mode timer via SBI | `10_000` ticks at the QEMU 10 MHz timebase |

The timer remains only a **wake signal**. Device harvesting, input routing, window management and
framebuffer work stay in the foreground desktop pump, so increasing cadence does not put compositor
work into hard-IRQ context.

## Rationale

The desktop's interactive path is already bounded and allocation-free in steady state. At 250 Hz,
timer-driven virtio-input service has a nominal 4 ms scheduling contribution before device and host
jitter. A 1 kHz wake cadence reduces that contribution to 1 ms and aligns all first-class targets,
which is the better latency posture for an interactive OS.

This is deliberately a latency decision, not a claim of 1 ms end-to-end hardware input latency.
The virtio-input path is still timer-polled; interrupt-driven virtio-input remains the next lower-
latency hardware rung.

## Verification requirement

The full cross-target E2E suite must remain green after the cadence change. Comparative performance
measurements must report the cadence change alongside latency/CPU observations; no physical hardware
frequency or overclocking claim is inferred from QEMU.
