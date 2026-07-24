# ADR-021: SMP and multicore scheduling

**Status:** Accepted — Phase 1 DELIVERED on ALL THREE targets (aarch64 + RISC-V + x86-64,
REQ-SMP-002); Phases 2-3 open under REQ-SMP-001 · **Date:** 2026-07-22 · **Updated:** 2026-07-24

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

## Delivery (2026-07-24) — Phase 1 + concurrency-substrate slice (REQ-SMP-002)

`kernel/src/smp.rs` + `kernel/src/boot.s::_secondary_start`, VM-gated by `scripts/vm-e2e.sh` at
`-smp 4` (13 invariants, marker `ALL 13 SMP INVARIANTS HOLD`):

- **Bring-up:** PSCI `CPU_ON` (HVC conduit) powers on every present secondary; each gets a private
  16 KiB stack, sets `TPIDR_EL1` (per-CPU identity, proved distinct), and enables its MMU over the
  SAME kernel tables core 0 built (shared address space).
- **Cross-core memory model:** exact atomic accounting under 4-core contention (the bump allocator
  moved from load-then-store to CAS — the first removed single-core assumption), and a
  release/acquire mailbox observed exactly by every core.
- **ADR-027 on real cores:** the `with_authorization` primitive runs under a new kernel `SpinLock`
  hammered by 3 secondaries while core 0 revokes: commits flow pre-revoke (progress-gated), the
  revoke linearizes inside the lock, ZERO commits land after it, and every post-revoke attempt on
  every core fails closed. This upgrades GAPS2 #9 from host-thread proof to real-SMP proof.
- **IPI:** GICv2 SGI 0 from core 0 is claimed on each secondary's banked CPU interface (polled IAR,
  masked PSTATE — never re-enters the core-0-owned vector table) and EOI'd.
- **RISC-V parity (same day):** `kernel-riscv64/src/smp.rs` + `boot.s::_secondary_start` replicate
  the suite through SBI HSM `hart_start` (boot-hart lottery handled by an atomic first-comer claim
  in `_start` — never assume hart 0), per-hart `tp` identity, Sv39 enable over the shared tables,
  and the SBI IPI (`send_ipi` → polled `sip.SSIP`). Same 13 invariants, gated by
  `scripts/vm-e2e-riscv.sh` at `-smp 4`. The `SpinLock` moved to `kernel-core/src/sync.rs`
  (Issue 1: defined once, host-proved in `kernel-core/tests/sync.rs`, used by both targets).
- **x86-64 parity (same day):** `kernel-x86_64/src/smp.rs` replicates the suite with NO firmware
  bring-up service — after `ExitBootServices` the OS itself is the protocol: the ACPI **MADT**
  (RSDP stashed from the UEFI config table pre-exit) enumerates the APs, LAPIC **INIT-SIPI-SIPI**
  wakes each into a 16-bit real-mode **trampoline** at physical `0x8000` (`global_asm!`, copied +
  parameterized at runtime; PTE made present/writable/executable through a manual CR3 walk since
  the low megabyte may sit under a 2 MiB leaf) that climbs real→long mode in one hop by cloning the
  BSP's CR4/CR3/EFER/CR0 over the SHARED page tables. Per-CPU identity via `IA32_GS_BASE` + LAPIC
  ID; the IPI is a fixed-vector LAPIC interrupt taken through a dedicated AP IDT (handler tags the
  CPU and writes EOI — the BSP's IDT stays untouched for the later ring-3 suite). Same 13
  invariants, gated by `kernel-x86_64/scripts/smoke-test.sh` at `-smp 4`. Ordering is load-bearing:
  the suite runs BEFORE the ring-3 suite, which repoints IRQ0 and strands the PIT deadline clock.
- **Honesty line:** with `-smp 1` the suite skips green (like virtio with no disk); the VM gates pin
  `-smp 4` so CI cannot silently skip. Phase 2 (per-CPU run queues, work stealing), TLB shootdown,
  and the lock-hierarchy/atomic-ordering audit remain open under REQ-SMP-001 (partial).

## Consequences

- No single-core assumption remains in shared scheduler/memory paths once complete.
- Each phase is independently VM-gatable (secondary reaches marker → tasks run cross-CPU → stress
  test survives). Until then `REQ-SMP-001` stays `deferred` in `docs/TRACEABILITY.md`.
- Concurrency correctness (TLB shootdown, atomic ordering) becomes the dominant risk and is covered by
  the behaviour/stress tests planned in gap-register Issue 11.
