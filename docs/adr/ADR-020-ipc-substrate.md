# ADR-020: Capability-secure kernel IPC substrate

**Status:** Accepted (partially implemented) · **Date:** 2026-07-22

## Context

Aletheia's microkernel direction (ADR-001, ADR-019) requires IPC to be a first-class kernel
primitive: every service, component, application, and AI agent must communicate through explicit,
capability-authorized boundaries — never ambient APIs or a hosted Unix-socket abstraction
(gap-register Issue 2). The M1 spine already had a synchronous, capability-gated `Channel` with
bounded queues and attenuated capability transfer (`Channel::send_transfer`, committed de307e3). What
remained was the fuller substrate a real kernel builds services on.

## Decision

Consolidate IPC in one arch-independent `kernel-core::ipc` module (so all three targets inherit it
from a single source, per ADR-019's shared-substrate principle) and authorize every operation through
the SAME `CapEngine` the deterministic pipeline uses. The substrate provides:

**Implemented + hosted-proved (`kernel-core/tests/ipc.rs`, `invariants.rs`):**
- Synchronous capability-gated message passing; unauthorized send dropped fail-closed.
- Bounded message queues (a full inbox refuses fail-closed).
- Capability transfer with attenuation (a transfer can never amplify beyond the sender's authority).
- **Asynchronous notifications** — non-blocking, coalescing signal bits (the seL4 badge model);
  signalling is capability-gated, waiting is by possession of the notification object.
- **Deadline / timeout semantics** — a message past its deadline is dropped, never delivered late.
- **Cancellation** — an undelivered message can be cancelled by its channel-assigned id.
- **Tracing + deterministic replay** — every op is logged; `replay()` reconstructs the exact delivery
  sequence from the trace alone (auditable + reproducible).
- **Zero-copy shared-memory channels** (REQ-IPC-008, **delivered** 2026-07-22). kernel-core policy in
  `kernel-core/src/grant.rs`; driven through the REAL aarch64 MMU path in `kernel/src/usermode.rs`
  (EL0 invariants 14-16, VM-gated by `scripts/vm-e2e.sh`): a `memory.share` grant maps one physical
  frame into two distinct TTBR0 address spaces (both resolve to the same frame — zero-copy across AS),
  establishing it is capability-gated fail-closed, and revocation unmaps the grantee's page. The same
  `run_shared_memory` invariants (14-16) are now proved on **all three targets** — aarch64 (TTBR0),
  RISC-V (Sv39 satp, `vm-e2e-riscv.sh`), and x86-64 (PML4, `smoke-test.sh`) — one arch-independent
  `GrantTable` authority layer over three real MMU backends. Follow-on: EL0/ring-3/U-mode code itself
  accessing the shared page across the boundary (GAPS2 #3).
  A `GrantTable` shares one physical frame region between endpoints under an explicit `memory.share`
  capability: establishing a share is gated by the SAME `CapEngine` (no capability ⇒ no grant,
  fail-closed); a grant can only **attenuate** the grantor's access (a read-only holder can never mint
  a read-write grant); the region's bytes live exactly once so a read-write holder's write is observed
  by every reader with no copy through any queue (the zero-copy property, made observable via
  `region_refcount`); every access is bounded to `[0, len)`; and revocation drops the endpoint's
  mapping fail-closed and releases its share of the backing. This is the **arch-independent**
  authority/lifecycle layer; turning a granted region into a real page-table mapping in each
  endpoint's address space stays each target's `vm.rs` seam (map/unmap already delivered) — the same
  split by which `kernel-core::sched` owns the scheduling policy and each target owns the context
  switch. Proved on the host in `kernel-core/tests/grant.rs` (8 tests, no QEMU).
- **Priority inheritance / donation** (REQ-IPC-009, kernel-core policy delivered 2026-07-22,
  `kernel-core/src/priosched.rs`; traceability `partial` — per-target `usermode.rs` wiring + VM gate pending).
  A `PriorityScheduler` tracks task base priorities and an endpoint ownership + wait graph. When a task
  `wait`s on an endpoint held by another, the holder's **effective** priority rises to that of the
  waiter (and, transitively, of anything blocked behind the waiter across a chain of held endpoints),
  so `schedule_next` runs the boosted holder ahead of an unrelated medium-priority task — bounding
  priority inversion to the holder's critical section. Acquiring/waiting on an endpoint is gated by the
  SAME `CapEngine` (fail-closed); donation is derived on read, so `release` (which hands the endpoint to
  its highest-priority waiter) withdraws it automatically. Cycle-safe (a deadlock breaks the donation
  recursion via a visited set, not a hang). Arch-independent policy; the context switch stays each
  target's `TaskContext` seam. Proved on the host in `kernel-core/tests/priosched.rs` (9 tests).

- **Priority inheritance / donation** (REQ-IPC-009, **delivered** 2026-07-22). kernel-core policy in
  `kernel-core/src/priosched.rs`; proved end-to-end through the REAL blocking-IPC path in
  `kernel/src/usermode.rs::run_priority_ipc` (EL0 invariants 20-22, VM-gated by `scripts/vm-e2e.sh`):
  a HIGH EL0 receiver blocks on the endpoint a LOW task services, donates its priority so the boosted
  LOW is dispatched ahead of a Ready MEDIUM (inversion avoided at the real dispatch point), LOW
  services, and HIGH wakes. Depends on **blocking IPC** (REQ-IPC-010, `run_blocking_ipc`), the
  substrate that lets an EL0 task genuinely block on an endpoint. aarch64 dev backend; x86-64/RISC-V
  spread is the follow-on.

**Deferred (design here; no blind code — ADR-010):**
- **Cross-address-space wiring.** `send_transfer` is proved in the shared spine; wiring it into each
  target's cross-AS `usermode.rs` fast-path (aarch64/x86-64/RISC-V invariants 11–13 already deliver
  the basic kernel-mediated endpoint) is the remaining per-target integration.

## Consequences

- Higher-level services can be built on real capability-authorized IPC, not ad-hoc hosted APIs.
- The fail-closed discipline is re-proved at each new boundary (notifications, timeout, transfer).
- Zero-copy shared memory (REQ-IPC-008, `GrantTable`) and priority inheritance (REQ-IPC-009,
  `PriorityScheduler`) are now implemented as arch-independent policy in `kernel-core` with green
  hosted proofs, but — like REQ-KERN-005 — no target drives them and no VM gate exercises them yet, so
  `docs/TRACEABILITY.md` marks both **`partial`**, not delivered, until they are wired into a target
  (grant → `vm.rs` mapping; priority scheduler → `usermode.rs`) and VM-gated. The remaining IPC
  integration work is therefore: per-target wiring of this policy, plus cross-address-space
  `send_transfer` in each target's `usermode.rs` fast-path.
