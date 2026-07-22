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
- **Zero-copy shared-memory channels** (REQ-IPC-008, delivered 2026-07-22, `kernel-core/src/grant.rs`).
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

**Deferred (design here; no blind code — ADR-010):**
- **Priority inheritance / donation.** When a high-priority task blocks on an endpoint held by a
  lower-priority task, the holder temporarily inherits the waiter's priority to avoid priority
  inversion. Requires the scheduler (`kernel-core::sched`) in the IPC blocking path — landed after the
  per-target `usermode.rs` is wired to the shared scheduler (see REQ-KERN-005).
- **Cross-address-space wiring.** `send_transfer` is proved in the shared spine; wiring it into each
  target's cross-AS `usermode.rs` fast-path (aarch64/x86-64/RISC-V invariants 11–13 already deliver
  the basic kernel-mediated endpoint) is the remaining per-target integration.

## Consequences

- Higher-level services can be built on real capability-authorized IPC, not ad-hoc hosted APIs.
- The fail-closed discipline is re-proved at each new boundary (notifications, timeout, transfer).
- Zero-copy shared memory is now delivered as the arch-independent `GrantTable` (REQ-IPC-008); the
  per-target page-mapping of a grant rides each target's existing `vm.rs`. Priority inheritance
  (REQ-IPC-009) stays deferred until the extracted scheduler is wired into the IPC blocking path;
  its status in `docs/TRACEABILITY.md` remains honest until then.
