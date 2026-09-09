# ADR-127 — Live desktop E2E parity on DT targets

**Status:** Accepted · **Date:** 2026-09-09 · **Advances:** ALET-P2-021 desktop interaction · **Builds on:** ADR-085, ADR-080, ADR-126

## Context

The aarch64 and RISC-V kernels already contain the shared desktop, real virtio-input devices, a
timer-driven pump, and an interactive console. Their normal VM gates intentionally build without
`interactive` so boot verdicts can terminate deterministically. That proves the contracts and
installation path, but not a human-style GUI workflow on those targets.

## Decision

Add `scripts/desktop-e2e-dt.sh` to build the actual interactive kernels and drive the real QEMU
virtio keyboard/tablet while the desktop timer pump is live. The gate verifies the live desktop,
initially quiet input ledger, hardware cursor movement, pointer focus routing, keyboard-driven
`help`, and a desktop-only Alt+F9 window-management action. The existing deterministic boot gates
remain unchanged and continue to provide the full invariant suite.

## Security

The test supplies only emulated hardware events through QEMU's input path. It introduces no kernel
authority and does not bypass the compositor input session or window-owner checks.

## Verification

The gate reports a named SKIP when the target QEMU emulator is unavailable; otherwise failure is
fatal. A successful run proves a live timer-pumped desktop workflow rather than only boot-time
component tests.
