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

**Deferred (design here; no blind code — ADR-010):**
- **Zero-copy shared-memory channels.** A grant-table maps a physical frame region into both
  endpoints' address spaces under an explicit `memory.share` capability; the sender transfers a
  read/read-write grant, the receiver maps it, and revocation unmaps. Requires the per-target MMU
  (delivered) + a frame grant-table (new) — brought up behind the same `CapEngine` authorization, one
  target at a time, VM-gated.
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
- Zero-copy and priority inheritance are unblocked once the frame grant-table and scheduler-wiring
  land; both are honestly marked deferred in `docs/TRACEABILITY.md` (REQ-IPC-008/009) until then.
