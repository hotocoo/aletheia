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

---

## INV-FAULT — page-fault classification (REQ-FAULT-001, ADR-039)

A handler's first job is to decide **what just happened**, and the decision is security-relevant: "a
user task touched a page it may not touch" is routine, while "a translation structure contains a bit the
architecture reserves" means the page tables are corrupt and nothing the kernel believes about memory is
trustworthy. Before this, each target printed the raw architectural code and exited — honest, but not a
model: there was nowhere to state that a reserved-bit fault must never be resumed.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-FAULT-1 | Classification is **total**: every input any decoder can produce maps to exactly one kind, and every kind to exactly one verdict. | A fault the model cannot name is a fault nobody decided about. | `classification_is_total_over_every_input_the_architectures_can_present` |
| INV-FAULT-2 | A reserved-bit fault is `CorruptTranslation` and **never** survivable, whatever else was reported. | The page tables are the thing being doubted, so the other bits are not interpretable. | `a_reserved_bit_fault_is_never_survivable_whatever_else_is_set` |
| INV-FAULT-3 | A fault from **kernel** privilege is never survivable. | "Kill the task" is meaningless when the kernel made the bad access. | `a_kernel_fault_is_never_survivable` |
| INV-FAULT-4 | An **unrecognized** report is `Unknown` and fatal — never classified from the bits that happen to be understood. | This is what makes the model safe to extend: a new architectural bit degrades to fatal, not to "routine". | `an_unrecognized_report_is_never_classified_by_the_bits_it_happens_to_understand` |
| INV-FAULT-5 | Each decoder maps architectural fields to the **facts** they mean, including where the architectures disagree in shape (an aarch64 instruction abort is never a "write"; RISC-V's `present` is the caller's fact because `scause` cannot report it). | A decoder that invents a field the ISA does not report makes the classification a guess. | `each_decoder_maps_its_architectural_fields_to_the_facts_they_mean` |
| INV-FAULT-6 | The model's **default** value is the strict one: an unfilled `Fault` is a kernel fault. | A decoder that forgets a field must not thereby make a fault look routine. | `the_default_fault_is_the_strict_one` |

**Wired live on x86-64** (`kernel-x86_64/src/idt.rs` classifies before reporting; boot invariants 56–58
prove the model is compiled into the kernel and behaves there). The aarch64 and RISC-V decoders are
host-proved but **not yet wired** — those handlers still print the raw `ESR` / `scause`. Stated, not
implied.

---

## INV-REENTRY — shared trap state (REQ-FAULT-002, ADR-039)

A trap handler runs on top of whatever it interrupted. If both touch one structure, the handler can
observe it **half-updated** — and the failure is not a crash at the moment of re-entry but silent
corruption found much later. The answer is not "be careful": it is to make re-entry detectable and fatal.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-REENTRY-1 | A nested entry is **refused**, never granted. | A granted nested entry is exactly the half-updated read. | `a_nested_entry_is_refused_and_leaves_evidence` |
| INV-REENTRY-2 | Leaving reopens the section — the guard is not a one-shot latch. | Otherwise the first trap disables fault handling for the rest of the boot. | `leaving_reopens_the_section_exactly_once_per_entry` |
| INV-REENTRY-3 | Every refusal is **counted**, so a caller that swallows one still leaves evidence. | A re-entry that happened once will happen again; the count is how a boot log says so. | `a_nested_entry_is_refused_and_leaves_evidence` |
| INV-REENTRY-4 | Two CPUs entering at once (a missing lock, not a re-entry — same consequence) yields at most one inside. | The compare-exchange is what makes this the same mechanism rather than a second one. | `two_threads_entering_at_once_produce_exactly_one_winner` |
| INV-REENTRY-5 | The section is never left active once the last token drops. | A leaked active state wedges fault handling permanently. | `the_section_is_never_left_active_after_the_last_token_drops` |

---

## INV-LAYOUT — address-space layout (REQ-MM-007 / REQ-MM-008, ADR-040)

Each target knew its layout as scattered literals, so nothing could check the properties a layout must
have. A layout you cannot check is a layout that drifts — and writing the check found a real hole.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-LAYOUT-1 | Declared regions never overlap, are page-aligned, and none contains the null page. | One address belonging to two things is a bug nobody can reason about. | `overlap_unaligned_null_and_missing_guards_are_all_refused` |
| INV-LAYOUT-2 | A user-reachable region never merely ABUTS a kernel-only one: there is at least a guard page between them. | Something that grows (a stack, a heap) would cross the boundary without ever being unmapped. | `overlap_unaligned_null_and_missing_guards_are_all_refused` |
| INV-LAYOUT-3 | The page below every kernel stack has **no translation**, and the stack's own pages still work. | A stack overflow must fault at the first byte past the stack rather than corrupt `.bss` silently — and a guard that cost the kernel its stack would be worse than none. | boot invariants `guard: …` on all three targets |
| INV-LAYOUT-4 | **VA 0 has no translation in the live map.** | `vmaddr` refused mapping the null page through the mapping APIs, but every target's boot identity map COVERED page 0 — as device memory on aarch64/RISC-V and as RAM on x86-64 — so a kernel null dereference read or wrote real state instead of faulting. Found by writing INV-LAYOUT-1. | boot invariant `layout: VA 0 has NO translation …` on all three targets, and in the `conformance.sh` core contract |

**KASLR posture (stated, not implied):** there is none, deliberately. Every target identity-maps, which is
what keeps the DMA story auditable (a driver hands the device the address it writes through); randomizing
the kernel's virtual base is therefore a different memory model, not a flag. And KASLR defends against an
attacker who can read a pointer and use it, whereas Aletheia gates every effect on a capability, so a
leaked kernel pointer is not itself authority. What it would take is recorded in `kernel-core/src/layout.rs`
and ADR-040: a higher-half split, an offset-mapped physical window for DMA translation, and PIE images.
