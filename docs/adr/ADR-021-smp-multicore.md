# ADR-021: SMP and multicore scheduling

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

The kernel today assumes a single core/hart (`-smp 1`). Real hardware — and the future CPU/GPU/NPU
scheduling ADR-022 depends on — needs multicore execution (gap-register Issue 4). This ADR is the
phased plan; per ADR-010 no untested SMP code ships until it is brought up and VM-gated.

## Decision

Evolve to symmetric multiprocessing behind the existing arch seams (`Hal`, `kernel-core::sched`), so
the shared scheduler policy generalizes to per-CPU run-queues rather than each target re-inventing SMP.

**Phase 1 — secondary bring-up.** Boot secondary cores/harts to a parked idle loop: aarch64 PSCI
`CPU_ON`; x86-64 APIC INIT-SIPI-SIPI; RISC-V HSM SBI `hart_start`. Gate: each secondary reaches a
known marker and halts. Per-CPU data via a CPU-local base register (aarch64 `TPIDR_EL1`, x86-64
`GS`/`swapgs`, RISC-V `tp`).

**Phase 2 — per-CPU scheduling.** Give each CPU its own `RoundRobin` run-queue + `current`; the
arch-independent policy (REQ-KERN-005) already models one queue, so SMP is N instances + a
migration/balancing policy on top. Idle CPUs steal from busy queues (work-stealing) under an explicit
lock hierarchy.

**Phase 3 — cross-CPU correctness.** IPIs for cross-core wakeups + reschedule; TLB shootdown on
unmap (sender broadcasts, waits for ack); a documented lock ordering; an atomic-ordering audit of all
shared scheduler/memory paths; CPU affinity; NUMA abstraction as a later refinement.

## Consequences

- No single-core assumption remains in shared scheduler/memory paths once complete.
- Each phase is independently VM-gatable (secondary reaches marker → tasks run cross-CPU → stress
  test survives). Until then `REQ-SMP-001` stays `deferred` in `docs/TRACEABILITY.md`.
- Concurrency correctness (TLB shootdown, atomic ordering) becomes the dominant risk and is covered by
  the behaviour/stress tests planned in gap-register Issue 11.
