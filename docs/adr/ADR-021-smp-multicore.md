# ADR-021: SMP and multicore scheduling

**Status:** Accepted — Phase 1 DELIVERED on ALL THREE targets (aarch64 + RISC-V + x86-64,
REQ-SMP-002); Phase 2 scheduling POLICY delivered (kernel-core + all-target VM-gated, REQ-SMP-003);
Phase 3 cross-core TLB shootdown DELIVERED on ALL THREE targets (REQ-SMP-004); preemptive cross-core
task migration + CPU affinity + lock-hierarchy audit open under REQ-SMP-001 · **Date:** 2026-07-22 ·
**Updated:** 2026-07-24

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
  `-smp 4` so CI cannot silently skip.

## Delivery (2026-07-24, same day) — Phase 2 scheduling policy (REQ-SMP-003)

`kernel-core/src/smpsched.rs` (`SmpSched`): one run queue per CPU, each behind its own `SpinLock`;
dispatch is local-first, an empty CPU **steals from the most-loaded** victim. **Lock discipline:**
never two queue locks at once — local pop locks only the local queue; a steal snapshots loads via
brief single locks then locks exactly ONE victim; with at most one queue lock held per CPU, no
lock-order cycle can exist (deliberately stronger than an ordered two-lock hierarchy). The steal
path is **alloc-free** past construction (kernel CPUs spin on it; bump allocators never reclaim).

- **Host-proved** (`kernel-core/tests/smpsched.rs`, 5 tests, real threads, progress-gated):
  exactly-once dispatch under 4-thread contention with an all-on-one-queue seed (none lost, none
  duplicated), local-first, steal liveness + victim attribution, most-loaded victim preference,
  least-loaded placement balance.
- **VM-gated on real cores on ALL THREE targets** (`kernel/src/smp.rs`, `kernel-riscv64/src/smp.rs`,
  `kernel-x86_64/src/smp.rs` — each suite now 16 invariants at `-smp 4`): every task seeded on ONE
  secondary's queue alone (lowest started hartid on RISC-V — the boot-hart lottery makes ids
  arbitrary, so its scheduler is sized MAX_CPUS and indexed by hartid), so the boot CPU and the
  other secondaries progress ONLY by stealing; invariants: scheduling completes on every core,
  every task dispatched EXACTLY once, stealing drains the unbalanced queue (the boot CPU performs
  one uncontended steal before opening the phase, so the steal invariant is structural, never a
  race).
- **Honesty:** this is the *policy* on real cores dispatching kernel work items. Preemptive
  cross-core *task migration* (a stolen EL0 task resuming on the thief CPU via the `TaskContext`
  seam), TLB shootdown, and the lock-hierarchy/atomic-ordering audit remain open under
  REQ-SMP-001 (partial).

## Delivery (2026-07-24, same day) — Phase 3 cross-core TLB shootdown (REQ-SMP-004)

Closes gap-register **GAPS4 ALET-P1-005** ("SMP TLB shootdown semantics need a formal
implementation contract") and the TLB-shootdown item of **GAP3 §4.2** / **GAPS2 #4**. When one CPU
edits a page table (unmap, or remap to a different frame) in an address space another CPU has
active, that CPU may keep using a STALE TLB translation — a cross-address-space use-after-free. The
contract: a CPU about to **reclaim** a frame must not proceed until every CPU that could hold a
stale translation has **completed** its local invalidation.

- **Arch-independent coordination — `kernel_core::shootdown::TlbShootdown`** (defined ONCE, Issue 1):
  an all-acknowledged barrier. `request(targets, inv, keep_waiting)` posts an `Invalidation` to each
  target's inbox and blocks until every target has drained it, run its local invalidation via the
  `service(cpu, perform)` callback, AND acknowledged — the ack is bumped strictly AFTER `perform`,
  so a reached watermark proves the work happened-before the reclaim. This is the arch-independent-
  policy-over-arch-specific-mechanism split used by `smpsched`/`sched`.
- **Native mechanism per target** (deliberately NOT forced-uniform, because a hardware broadcast
  exists on only one target): **aarch64** — the initiator's `tlbi vaae1is` broadcasts across the
  inner-shareable domain in hardware (the real invalidation); the barrier proves per-core
  acknowledgement. **x86-64** — NO hardware broadcast; each AP runs its own core-local `invlpg` via
  the service callback (this IS the genuine software shootdown). **RISC-V** — the SBI RFENCE
  `remote_sfence_vma` firmware fence (OpenSBI fences remote harts and blocks) + each hart's own
  `sfence.vma` through the barrier.
- **Edge behaviour (ALET-P1-005 "must define"):** an **offline / unresponsive / failed** CPU makes
  `request` hit its `keep_waiting` deadline and return `false` — the reclaiming CPU treats that as
  failure and does NOT reclaim (fail closed), never hangs. **Concurrent unmaps** are serialized by
  each inbox's FIFO order + independent lock. **Address-space destruction** teardown ordering is the
  separate deferred ALET-P1-004 brick. Service runs with the shootdown polled (interrupts masked in
  the VM suites), so no reentrancy during the invalidation.
- **VM-gated on ALL THREE targets** (`-smp 4`, SMP invariants 16-18 each — total 19 SMP invariants
  per target): each Phase 6 maps a fresh VA to frame A in the shared root, every secondary primes
  its TLB, the initiator remaps the VA to frame B and shoots down (broadcast/IPI/RFENCE + the
  barrier), and every secondary re-reads and observes B coherently.
- **Honesty (load-bearing):** the VM gates prove the mechanism + barrier RAN on real cores and the
  initiator waited for every acknowledgement — they do NOT claim QEMU exhibits a stale entry, because
  TCG's softmmu TLB is a performance cache, not a faithful architectural retention model, so
  "a core reads stale without a shootdown" is not a deterministic observable there. The
  *discriminating* stale-vs-fresh proof (a broken barrier = a genuine failure) is the deterministic
  host-thread test `kernel-core/tests/shootdown.rs` (5 tests: barrier ordering, the use-after-free
  scenario, no-lost-request under concurrent requesters, fail-visible on an unresponsive target,
  FIFO+count). kernel-core hosted suite 79 → 84.

## Consequences

- No single-core assumption remains in shared scheduler/memory paths once complete.
- Each phase is independently VM-gatable (secondary reaches marker → tasks run cross-CPU → stress
  test survives). Until then `REQ-SMP-001` stays `deferred` in `docs/TRACEABILITY.md`.
- Concurrency correctness (TLB shootdown, atomic ordering) becomes the dominant risk and is covered by
  the behaviour/stress tests planned in gap-register Issue 11.
