# ADR-125 — Compositor repaint hot path is allocation-free

**Status:** Accepted · **Date:** 2026-09-09 · **Advances:** ALET-P2-021 performance/GUI rung · **Builds on:** ADR-064, ADR-077, ADR-086

## Decision

`Compositor::compose_frame` keeps repaint-region collection on the stack with the existing
`MAX_DAMAGE_RECTS` bound. It no longer clones the z-order or allocates a temporary `Vec<Rect>` for
each frame. Surface damage is consumed directly from its bounded ledger.

The overflow rule remains unchanged: once the bounded region budget is exhausted, damage is
coalesced to the whole scanout. No repaint is silently dropped.

## Security and performance consequences

- Repaint execution has no heap allocation in its hot path, reducing pressure on Aletheia's
  never-freeing boot heap.
- Z-order remains owned by `Compositor`; removing the clone does not create a second mutable copy of
  authority-bearing state.
- Region count remains structurally bounded by `MAX_DAMAGE_RECTS`.
- Existing pixel clipping, z-order, cursor, damage, and determinism contracts remain unchanged.

## Verification

- `cargo test --lib desktop::tests` — 24 passed.
- `cargo test --lib` — 132 passed before this hot-path edit; compositor-focused tests passed after it.
- `scripts/vm-e2e.sh` — PASS; aarch64 boot, persistent reboot, custody absence, and all marker families.
- `scripts/vm-e2e-riscv.sh` — PASS; RISC-V boot, persistent reboot, custody absence, and all marker families.
- `scripts/vm-e2e-x86.sh` — PASS; UEFI boot, VT-d enforcement, persistent reboot, and custody absence.
- `scripts/vinput-e2e.sh` — PASS; real virtio keyboard/tablet → cursor → focus → window queue → close/focus recovery.
- `scripts/e2e-all.sh` — PASS; aarch64 PASS, RISC-V PASS, x86-64 PASS, VirtualBox SKIP because host is arm64.
- `scripts/comparative-bench.sh` — PASS on same QEMU/TCG host: Aletheia idle median 0.3% CPU vs Linux 0.9%; typed echo 35 ms/op vs Linux 266 ms/op in this run. These are workload measurements, not a claim of overall OS superiority.

## Non-claims

This does not close ALET-P2-021 or ALET-P2-022. GPU isolation, broader real-hardware graphics,
hardware frequency/voltage programming, battery, sleep/wake, and thermal sensor integration remain
open or deferred according to the gap register.
