# Aletheia — Invariant Contracts

**As of:** 2026-08-03.

Some subsystems were delivered as *code that works* without a written statement of **what must never
happen**. That gap is what GAPS4 rows ALET-P1-005 / P1-016 / P1-017 / P1-025 name: a passing test tells
you the case it runs; a contract tells you the cases that must never pass. This file is the written half.

Each invariant is numbered, stated as something that must hold (not as a description of the code),
carries the reason it is load-bearing, and names the test that adversarially attempts to violate it. A
test file may only cite an id that exists here; an id here without a proof is a bug in this document.

Scope note: this file covers the four clusters above. The broader "every architectural invariant in one
place" ask (ALET-P3-003) is larger — memory, W^X, the namespace, durability — and stays open; those
areas already carry their contracts in their ADRs and in `scripts/conformance.sh`.

---

## INV-TLB — cross-core TLB shootdown (REQ-SMP-004, ADR-021 Phase 3)

The requester is about to **reclaim or rewrite the physical frame** the stale mappings point at. Every
invariant below exists because a core that still holds a stale translation would then read or write
someone else's memory through it.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-TLB-1 | `request` returns `true` **only if every** addressed target has acknowledged draining past this request's item. | A partial barrier is worse than none: the requester proceeds believing all cores are clean. | `a_request_never_completes_while_any_target_is_silent` |
| INV-TLB-2 | An acknowledgement counts **only after** the target's invalidation has actually run (`perform` precedes the ack). | Acking first turns the barrier into a promise about the future. | `an_ack_never_precedes_the_invalidation_it_covers` |
| INV-TLB-3 | Every invalidation posted to a target is performed **exactly once** — none dropped, none doubled. | A dropped item leaves a stale entry; a doubled one hides a lost one during audits. | `every_posted_invalidation_is_performed_exactly_once` |
| INV-TLB-4 | A target's acknowledgement of *another* requester's item never satisfies this requester's watermark. | Concurrent shootdowns must not borrow each other's acks — that is a silent partial barrier. | `concurrent_requests_never_borrow_each_others_acknowledgements` |
| INV-TLB-5 | A caller whose deadline hook says stop gets `false`, and `false` never means "some but not all". | The caller's contract is: `false` ⇒ do **not** reclaim. | `an_aborted_wait_reports_failure_and_never_a_partial_success` |
| INV-TLB-6 | Targets outside the requested set, or `>= ncpus`, are never posted to and never waited on. | A bogus target id must not deadlock the barrier or invalidate an unrelated core. | `an_out_of_range_target_is_ignored_not_waited_on` |

**Not claimed:** this is the *protocol* contract, proven on the host against a model of cores. That the
backend's `perform` really invalidates the CPU's TLB is a per-target claim, proven in each VM gate.

---

## INV-PRIO — priority inheritance (REQ-IPC-009, ADR-020)

Priority inheritance exists so that a low-priority task holding an endpoint a high-priority task needs
cannot be starved by a medium-priority task. The failure mode is **unbounded priority inversion**, and
these invariants are what rule it out.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-PRIO-1 | A holder's effective priority is **never below** the effective priority of any task (transitively) blocked on an endpoint it holds. | This *is* the anti-inversion property. | `a_holder_is_never_weaker_than_anyone_waiting_on_it` |
| INV-PRIO-2 | Donation is **transitive** along a chain of holders. | A chain A→B→C leaves C starved unless C inherits A's priority. | `donation_follows_the_whole_chain_not_just_one_hop` |
| INV-PRIO-3 | Donation **ends** when the endpoint is released: the ex-holder returns to its base priority. | A holder that keeps donated priority forever is a privilege escalation of scheduling. | `donation_stops_the_moment_the_endpoint_is_released` |
| INV-PRIO-4 | A task's effective priority is **never above** the maximum base priority in the system. | Donation must not *manufacture* priority; it only lends existing priority. | `donation_never_manufactures_priority_above_the_highest_base` |
| INV-PRIO-5 | `schedule_next` never dispatches a Blocked task, and never dispatches a Ready task when a Ready task of strictly higher effective priority exists. | Correct donation with an incorrect dispatch rule still inverts. | `the_scheduler_never_runs_a_weaker_ready_task_over_a_stronger_one` |
| INV-PRIO-6 | A donation **cycle** (mutual blocking = deadlock) terminates the computation instead of recursing forever. | A deadlock must surface as a stuck system, not a kernel stack overflow. | `a_donation_cycle_terminates_instead_of_recursing` |
| INV-PRIO-7 | Every `acquire`/`wait` is capability-authorized; an unauthorized call changes **nothing**. | Scheduling state is authority-relevant: an unauthorized `wait` would let anyone force a donation. | `an_unauthorized_acquire_or_wait_changes_no_scheduling_state` |

---

## INV-IPC-CANCEL — message cancellation (REQ-IPC-006, ADR-020)

Cancellation is the sender withdrawing an *undelivered* command. The failure mode is a command the
sender believes withdrawn being executed anyway — or, symmetrically, a cancel that silently swallows a
message that was already delivered.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-IPC-CANCEL-1 | A cancelled message is **never** delivered by any later receive. | This is the whole point: a withdrawn command must not execute. | `a_cancelled_message_is_never_delivered_afterwards` |
| INV-IPC-CANCEL-2 | Cancelling an id that was already delivered, expired, or cancelled returns `false` and changes nothing. | A `true` return is the sender's evidence it won the race; a lie there is worse than a refusal. | `cancelling_something_already_gone_is_a_refusal_not_a_lie` |
| INV-IPC-CANCEL-3 | Cancellation removes **exactly one** message — the one named — and preserves the order of the rest. | Removing the wrong message executes a command the sender never withdrew. | `cancellation_removes_exactly_the_named_message_and_keeps_the_order` |
| INV-IPC-CANCEL-4 | Every message ends in exactly one terminal trace event (`Recv`, `Expired`, or `Cancel`). | The trace is the audit record; a message with two fates or none makes it useless. | `every_message_reaches_exactly_one_terminal_trace_event` |
| INV-IPC-CANCEL-5 | A cancelled slot is reusable: cancelling frees capacity for a later send, and never beyond the bound. | A queue that leaks slots on cancel eventually refuses all sends. | `cancelling_frees_the_slot_for_a_later_send` |
| INV-IPC-CANCEL-6 | A deadline and a cancel never both claim the same message. | Deadline and cancellation must not disagree about a message's fate. | `a_deadline_and_a_cancel_never_both_claim_the_same_message` |

---

## INV-CAP-REVOKE — revocation under concurrency (REQ-CAP-006, ADR-027)

`with_authorization` makes authorize+execute atomic so no *stale* authorization can act. These
invariants extend that to the revocation side: what a revoker is entitled to conclude.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CAP-REVOKE-1 | Once `revoke` returns, **no** subsequent `with_authorization` on that token may execute its effect. | Revocation that is not immediately total is not revocation. | `after_revoke_returns_no_later_attempt_can_ever_act` |
| INV-CAP-REVOKE-2 | Revocation is **permanent**: no later mint, delegation, or re-presentation makes the revoked token authoritative again. | Authority resurrection defeats every containment story. | `a_revoked_token_is_never_authoritative_again_however_it_is_presented` |
| INV-CAP-REVOKE-3 | Revocation is **idempotent**, and revoking an unknown/forged token is a no-op that grants nothing. | A revoker must be able to retry without side effects, and a forged handle must not be a channel. | `revoking_twice_or_revoking_a_forged_token_changes_nothing` |
| INV-CAP-REVOKE-4 | Revoking a **parent** revokes every delegated descendant, transitively. | Attenuated children outliving their parent is authority the revoker cannot see. | `revoking_a_parent_kills_every_descendant_transitively` |
| INV-CAP-REVOKE-5 | A revoke interleaved with an in-flight authorize+execute yields **either** a completed effect **or** a denial — never a partial effect, and never an effect ordered after the revoke returned. | This is the linearization point the whole model rests on. | `an_interleaved_revoke_yields_a_clean_before_or_after_never_a_partial` |
| INV-CAP-REVOKE-6 | Revoking one token leaves every **sibling** token's authority intact. | Over-broad revocation is an availability bug that pushes callers toward broader capabilities. | `revoking_one_capability_never_disturbs_its_siblings` |

**Not claimed:** these are proved on the host, deterministically, by interleaving operations at every
step — not by racing real threads (`kernel-core` is `no_std` and its tests are single-threaded). The SMP
suites prove the same primitive on real cores.
