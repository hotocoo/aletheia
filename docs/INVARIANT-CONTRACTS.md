# Aletheia — Invariant Contracts

**As of:** 2026-08-23.

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

---

## INV-TASK — task lifecycle (REQ-SCHED-002, ALET-P1-015)

Four states, and until this was written down nothing said which transitions are impossible. A lifecycle
bug is usually a state that is only *briefly* wrong, so every test below drives long sequences and checks
its property after **every** event rather than at the end.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-TASK-1 | `Finished` is **terminal**: no event returns a finished task to Ready, Running or Blocked, and it is never dispatched again. | A resurrected task resumes a context that has been torn down. | `finished_is_terminal_whatever_arrives_afterwards` |
| INV-TASK-2 | A Blocked task is never dispatched, and only `unblock` makes it eligible — not the passage of rounds. | Dispatching a blocked task runs code waiting on something that has not happened. | `a_blocked_task_is_never_dispatched_until_it_is_unblocked` |
| INV-TASK-3 | At most ONE task is Running, and `current()` names exactly that task. | Two Running tasks on one core is a belief in something impossible; two disagreeing views resume the wrong context. | `at_most_one_task_is_running_after_every_event` |
| INV-TASK-4 | `runnable_len` equals the rotation — Ready **plus** the Running task (which rotates to the tail) — and never counts Blocked or Finished. | A drifting count makes a scheduler spin believing it has work, or idle with work pending. | `the_runnable_count_never_drifts_from_the_dispatchable_set` |
| INV-TASK-5 | An event naming a task that was never spawned changes **nothing** — no state appears for it, no real task is disturbed. | **This found a real defect:** `block`/`finish` used to INVENT a task in the state table from a stray id, and a scheduler that believes in a task it never created will try to resume a context that does not exist. | `an_event_for_an_unknown_task_changes_nothing` |

---

## INV-STORE-ERR — storage error semantics (REQ-STOR-004, ALET-P1-020)

A storage stack's error behavior *is* its safety story: a swallowed device error becomes silent data loss,
and an error that cannot be told apart from another one cannot be handled correctly.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-STORE-ERR-1 | The error kinds are **distinguishable** — wrong buffer size, block off the device, device failure, over-size transaction are four different values. | Collapsing them makes the only possible response to any error the same one. | `every_error_kind_is_distinguishable_at_its_own_boundary` |
| INV-STORE-ERR-2 | A device error is **surfaced**, never swallowed — including a failed **flush**, which is the durability barrier, so swallowing it would report durability that does not exist. And a failed commit leaves the home block untouched. | The difference between a reported failure and silent loss. | `a_device_error_surfaces_through_the_journal_rather_than_being_swallowed` |
| INV-STORE-ERR-3 | The filesystem **preserves** the device error (`FsError::Storage(Device)`) rather than flattening it, while its own refusals keep their own names. | A caller must be able to tell "your request was wrong" from "the hardware failed". | `the_filesystem_preserves_the_device_error_and_keeps_its_own_refusals_distinct` |
| INV-STORE-ERR-4 | Every refusal is a **no-op**, proven by comparing the whole device image byte-for-byte before and after — the strongest available form of "nothing happened". | A refusal that wrote something is worse than an accepted operation, because nobody goes looking for its effects. | `every_refusal_leaves_the_device_image_byte_identical` |

## INV-DEADVA — addresses that must be dead in every space (REQ-MM-007 / REQ-MM-008, ALET-P2-033)

Two pages are given no descriptor on purpose: VA 0, so a null dereference faults instead of touching real
state, and the ring-0 stack guard, so an overflow faults instead of writing what lies below the stack.
INV-LAYOUT proves both — of the map the kernel built for *itself*. A per-process root is a different tree,
and on x86-64 it is built by COPYING the live one, so a space copied before the kernel's own map was active
mapped the guard region as a single 2 MiB huge page. A user space able to reach an address the kernel's own
map deliberately cannot is the guard **inverted**: it protects the less privileged tree and not the more
privileged one. These invariants make the property hold of every root, not one.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-DEADVA-1 | A dead page has **no translation** in any address space — the kernel's own root and every derived per-process root alike. | Reachability is the breach; everything else is a precursor to it. | `a_reachable_dead_page_is_a_violation` + each target's boot invariant on a space it BUILDS |
| INV-DEADVA-2 | A dead page has **no descriptor at any level**, even when it does not translate. A live block/huge entry covering it is one split or permission change away from reviving the address with nothing having mapped it. | This is the exact shape of the hole ALET-P2-033 records: unreachable, yet described. | `a_described_but_unreachable_dead_page_is_still_a_violation` |
| INV-DEADVA-3 | An **empty declaration fails** the audit. A target that declares nothing has proved nothing, and must not be indistinguishable from one with no dead pages. | Fail-closed: otherwise deleting the declaration is the cheapest way to make every gate green. | `an_empty_declaration_proves_nothing_and_fails` + each target's live "empty declaration is refused" invariant |
| INV-DEADVA-4 | A **malformed or oversized** declaration is refused *before* any space is walked, and reports zero pages walked. | A partial walk that reported success would be the vacuous pass the whole module exists to refuse. | `a_malformed_or_oversized_declaration_is_refused_before_any_walk` |
| INV-DEADVA-5 | **Every** violation is counted, not only the first; and a report is clean only if it walked at least one page. | A space that revived two pages must say two, and `violations == 0` over an empty walk is not evidence. | `every_violation_is_counted_not_only_the_first` + `a_space_with_neither_page_mapped_is_clean` |
| INV-DEADVA-6 | A space-builder that cannot satisfy the audit returns **no space at all** and gives its frames back, rather than a space that can reach the guard. | Handing out a broken space is worse than failing to build one: the failure is visible, the space is not. | **x86-64 only:** `build_space_from(active_root())` in the virtual-memory suite — before `kmap::activate()` that IS a dirty tree, and the invariant requires the refusal *and* an unchanged frame count, so a builder that leaked its two table frames on the refusal fails too. On aarch64/RISC-V the same path exists in `build_identity` but is **not proved**: those builders take no source, so nothing can hand them a tree that fails. Stated rather than claimed from one target's test |

## INV-CAP-SCOPE — the authority lattice (REQ-CAP-007, ALET-P1-027, ADR-048)

Every capability guarantee in this repository rests on attenuation: a delegation may only produce
equal-or-narrower authority. Until ADR-048 that phrase had no definition anywhere — `delegate`
compared parent and child inline, with whichever predicate was at hand, and this document had no
section for it. Attenuation was the load-bearing property with the least written down about it.
`kernel-core/src/capalg.rs` states it as three partial orders and their conjunction; the proofs
sweep the whole finite universe the model can express, because a sampled proof of a partial order is
a proof about the samples.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CAP-SCOPE-1 | **Soundness.** If `attenuates(p, c)` then every request `c` authorizes, `p` would have authorized too — asserted directly against `evaluate`'s own covering functions, over the whole scope lattice × the whole target universe and the whole pattern alphabet. | This is the property the other six exist to serve. An order that drifts away from the authorization test it bounds is not bounding anything. | `scope_attenuation_never_grants_reach_the_parent_lacked`; `action_attenuation_never_grants_reach_the_parent_lacked` |
| INV-CAP-SCOPE-2 | **Covering is not attenuation.** `action_covers(pattern, action)` asks whether a concrete action is inside a pattern's reach; `action_attenuates(parent, child)` asks whether a child pattern's REACH is a subset of the parent's. They are different relations. | **This found a real defect:** `delegate` asked the first question with the child's pattern in the action slot. It agrees with the second on every pattern whose only `*` is trailing, and disagrees the moment one appears elsewhere — `entity.*.*` → `entity.*` was ACCEPTED, and the child then authorized `entity.delete`, which its parent could never authorize. Privilege amplification through the one mechanism the model says cannot amplify. Fixed in the kernel spine and the hosted Core, and pinned as spine invariant 12 in the conformance contract. | `the_covering_relation_is_not_the_attenuation_relation`; in-kernel `delegation: a child pattern reaching past its parent is denied` |
| INV-CAP-SCOPE-3 | **Reflexive.** `attenuates(x, x)` in every dimension. | A subject must be able to hand on exactly what it holds without laundering it through a wider intermediate. | `attenuation_is_reflexive_in_every_dimension` |
| INV-CAP-SCOPE-4 | **Transitive**, and therefore: no descendant of a legal chain exceeds its root. Proved on pairs exhaustively, then over 20 000 generated chains built by REJECTION — every candidate step is offered to `attenuates` and only accepted ones extend the chain, so the population is exactly the set of chains the engine would have permitted. | Makes "no descendant exceeds its root" a consequence of the per-step check rather than an audit `evaluate` would have to perform on every call. The campaign asserts a non-trivial number of steps were accepted, so it cannot pass vacuously. | `attenuation_is_transitive_in_every_dimension`; `no_descendant_of_a_legal_chain_exceeds_its_root` |
| INV-CAP-SCOPE-5 | A scope that reaches **nothing** — `None`, or an entity set with no members — is a legal narrowing of anything, and nothing can be widened back out of it. | The two spellings denote the same scope. A relation that refused the narrowest possible delegation on a spelling would push callers toward wider ones. | `a_scope_that_reaches_nothing_is_a_legal_delegation_from_anything`; hosted `an_empty_entity_scope_is_a_legal_narrowing_of_anything` |
| INV-CAP-SCOPE-6 | `Type(T)` and `Entities([…])` are refused in **both** directions, and the refusal is sound rather than merely conservative: there is a real target each reaches and the other does not. | A `Target` carries id and etype independently. Deciding the case for a particular entity needs a store lookup, and an authority check that reads the store is an authority check that can be starved. Deliberately incomplete, never unsound. | `type_and_entity_scopes_are_refused_in_both_directions_and_that_is_sound` |
| INV-CAP-SCOPE-7 | The conjunction holds **exactly** when all three dimensions do, and the refusal names a dimension that really failed. | A caller that re-derived the reason could report a dimension that in fact passed — the error message that sends someone to fix the wrong thing. | `the_conjunction_holds_exactly_when_all_three_dimensions_do`; `the_refusal_names_a_dimension_that_really_failed` |

## INV-CAP-LIFE — capability lifetime across a reboot (REQ-CAP-008, ALET-P1-026, ADR-048)

ADR-038 made entities durable and said plainly that authority is not: the capability engine is born
empty at every boot. Persisting authority is the dangerous direction, so these invariants are almost
all REFUSALS. A persisted registry is untrusted input — whoever can write the block can widen a
child, drop the revocation list, or point a record at a parent that does not exist — and there is no
partial load, because the parts a partial load would drop are the parts that make the rest safe.
Every forgery below is assembled through `capstore::encode_for_test`, the same encoder `save` uses,
so each is a well-formed image whose only defect is the one under test; a forgery refused for being
malformed would prove nothing about the check it was aimed at.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CAP-LIFE-1 | A round trip preserves every authority **and invents none** — the tokens held before the save still name the same authority, and the verdicts are identical either side. Saving an unchanged engine twice is byte-identical. | Without the first half the store is useless; without the second it is a minting path. Determinism follows the `sbom.py` doctrine: a store that differs on every save hides the saves that were real changes. | `a_round_trip_preserves_every_authority_and_invents_none`; `saving_an_unchanged_engine_twice_produces_identical_bytes` |
| INV-CAP-LIFE-2 | Revocation survives, and the **cascade is re-derived** — an image whose revocation list names only the root of a cascade comes back with the whole subtree still dead. | The smallest edit that resurrects the most authority. **This found a real defect:** the first loader re-applied the list with `revoke`, which descends only when its `insert` reports the id as newly revoked — every seed was already in the set, so the walk stopped at the first node and every descendant came back live. | `a_revoked_capability_is_still_revoked_after_the_reboot`; `the_cascade_is_recomputed_when_the_image_lists_only_its_root` |
| INV-CAP-LIFE-3 | The clock cannot go **backwards**: an image is refused under a clock earlier than the one it was saved under, and an expiry that has passed is still passed after the reload. | Expiry is relative to a logical clock. Reload under a clock that restarts at zero and every expired capability is live again — the expiry did not fail, the frame of reference moved. | `loading_under_an_earlier_clock_is_refused`; `an_expiry_that_has_passed_stays_passed_across_the_reboot` |
| INV-CAP-LIFE-4 | A **widened** child is refused — in each of the three dimensions separately, against a root that is genuinely narrower in all of them. So are an **orphan**, a **cycle** and a **duplicate id**. | This is INV-CAP-SCOPE applied to an input: a load that trusts the image is a delegation path with no `delegate` behind it. The widening fixture uses a narrow root on purpose — against an `All`/`*` root the test passes for the wrong reason, which is what its first version did. | `a_child_widened_in_the_image_is_refused`; `an_orphan_is_refused`; `a_cycle_in_the_parent_edges_is_refused`; `a_duplicate_id_is_refused` |
| INV-CAP-LIFE-5 | An id must never be **mintable twice**: an image whose counter could re-mint a stored id is refused, at every rewound value; and after a legal reload, 500 fresh mints collide with nothing the image carried. | Lose the counter and the next boot re-mints ids that are already held — including a REVOKED id, which hands a killed token back to whoever still holds it. | `an_image_whose_counter_could_re_mint_a_stored_id_is_refused`; `a_mint_after_the_reload_never_collides_with_a_stored_id` |
| INV-CAP-LIFE-6 | **Every** single-bit flip of the image is refused, over every byte including the checksum's own, and **every** truncation is refused. Trailing bytes are refused even when internally consistent. | A region the checksum did not cover would show up here as a load that succeeded. The bit sweep is what makes "checksummed" a measured claim rather than a design intention. | `every_single_bit_flip_of_the_image_is_refused`; `every_truncation_of_the_image_is_refused`; `trailing_bytes_are_refused` |
| INV-CAP-LIFE-7 | A refused load changes **nothing** — the running engine's verdicts and live count are unchanged, and the image itself is not mutated by the attempt. | Fail-closed means the error path yields no engine; what this adds is that it also yields no side effect, including on the bytes the next boot will read. | `a_refused_load_leaves_the_running_engine_untouched` |
| INV-CAP-LIFE-8 | Authority is stable across **ten** reboots with a mint, a delegation and a revocation between each — every killed token still dead, every live one still authorizing, and the live set exactly what was minted and not revoked. | One round trip proves the encoder. The point of a lifetime model is the tenth reboot, and that nothing accumulates in between. | `authority_is_stable_across_ten_reboots` |

## INV-CAP-CUSTODY — authority custody is a lifecycle (ALET-P1-034, ADR-070)

`capvault` seals the persisted registry under keys the VAULT owns: a root handed in once at open
wraps a versioned keystore, rotation walks a one-way chain, and retirement deletes the only copy
of a key. There is no entropy at boot, so every nonce is CONSTRUCTED — deterministic per-key
prefix || monotone counter persisted in the keystore — and reuse is impossible by construction.
Both objects authenticate before they parse; there is no partial load of a keystore. The crash
model is ADR-062's: the host proof records the pivot's exact device-op sequence and fires the
right kind of refusal at EVERY position.

Proofs are HOST-side (`kernel-core/tests/capvault.rs`), mirroring ADR-069's posture for the
encryption-at-rest lifecycle: the boot heap never frees (ADR-063), and this suite's sweep churn
would starve later boot suites of exactly the allocations they need — a real failure mode the
first boot-gated version hit. The gate-marker map is therefore unchanged by this family.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CAP-CUSTODY-1 | A sealed registry **reopens under its root alone** with authority intact — across repeated save/reopen cycles verdicts are identical and reserved counters strictly increase. | Custody that cannot restore authority is decoration; counters that could repeat would reuse a nonce under the same key, the one failure AEAD cannot survive. | `sealed_round_trip_across_reopen_cycles_keeps_authority_and_counters_monotone` |
| INV-CAP-CUSTODY-2 | Rotation mints **max+1** and retains its predecessor; rekey retires every version below the newest, **destroying** the retired key inside the vault (`key_for_test` returns None); a replayed pre-pivot image names its dead version. | Retirement that leaves a usable key behind is a label, not retirement — the one-way chain makes deletion real, and replay is the smallest edit that revives the most authority. | `rekey_retires_by_name_and_destroys_the_retired_key`; `a_refusal_at_every_rekey_position_leaves_a_complete_world` sibling assertions in `kernel-core/tests/capvault.rs` |
| INV-CAP-CUSTODY-3 | Rolling the KEYSTORE back alone against a newer image names the future version (`FutureVersion{requested, newest}`); rolling back BOTH objects consistently OPENS — the documented residual an external anchor would be needed to catch. | Detectability must be stated per-object: keystore-alone rollback is visible, whole-world rollback is not. Both directions PINNED so doc and behavior cannot drift. | `rollback_semantics_are_pinned_in_both_directions`; also asserted inside the crash-position sweep's promised stages |
| INV-CAP-CUSTODY-4 | A wrong root refuses the WHOLE keystore with nothing loaded, nothing decoded, and **no byte of the medium changed** — verified block-for-block against a pre-attempt snapshot. | Authentication precedes parsing, so a failed open releases no bytes into any parser; fail-closed must also mean side-effect-free at the device level. | `a_wrong_root_refusal_is_a_total_noop_at_the_device_level` |
| INV-CAP-CUSTODY-5 | **Every** single-bit flip of EITHER object and **every** truncation of either object is refused, through real filesystem rewrites. | A region the AEAD or the structural checks did not cover would show up here as an open that succeeded; the sweeps make "authenticated" measured rather than intended. | `every_single_bit_flip_of_either_object_is_refused_through_the_filesystem`; `every_truncation_of_either_object_is_refused` |
| INV-CAP-CUSTODY-6 | Objects from ANOTHER store refuse under ours by name: the image fails authentication under our key (`ImageAuth`), the keystore under our root (`KeystoreAuth`). | Same machinery, different custody: derivation is domain-separated but custody-scoped, and the refusal names which layer caught it. | `another_stores_objects_refuse_under_ours_by_name` |
| INV-CAP-CUSTODY-7 | The counter protocol RESERVES first and exhaustion at u64::MAX is NAMED: MAX-1 seals and reserves MAX; MAX refuses BY NAME storing nothing and changing nothing; rotation escapes exhaustion without touching the root. | Reserve-first survives crashes (a gap wastes a number, reuse is impossible); wraparound would silently hand a new image a used nonce. | `counter_exhaustion_is_exact_named_and_escapable_by_rotation`; round-trip test asserts strictly increasing counters across cycles |
| INV-CAP-CUSTODY-8 | At EVERY device-operation position of the three-commit rekey pivot, a refusal of the RIGHT KIND surfaces as Err, the protocol ABORTS consuming nothing further, and the reopened world holds SOME complete stage ([1], [1,2] or [2]) with authority intact. | The pivot touches two objects across three commits; a half-pivot world would strand authority between versions. The op sequence is RECORDED from one clean run, so each fault aims correctly instead of silently missing. | `a_refusal_at_every_rekey_position_leaves_a_complete_world` (exhaustive over the recorded sequence) |
| INV-CAP-CUSTODY-9 | Layering: an image that AUTHENTICATES under a real retained key but widens a delegation inside is refused THROUGH the vault with the inner admission name preserved (`Image(Amplified)`). | Custody sits ON TOP of INV-CAP-LIFE's checks, never instead of them — the seal must not launder an admission failure into something vague. | `a_widened_registry_sealed_under_the_real_key_is_refused_through_the_vault` (forgery sealed via `seal_image_bytes_for_test`) |

## INV-CAP-DELIVERY — the custody anchor crosses the platform boundary (ALET-P1-034, ADR-072)

The vault root is no longer a caller-supplied argument. The platform delivers it over its
firmware configuration channel (fw_cfg: ioports under q35, MMIO windows on both virt
machines), and exactly one door opens a vault. Every non-delivered shape is a NAMED refusal;
absence seals the vault while the machine continues. Paired commits record the vault's
keystore generation inside the durable entity-store record (under its trailing checksum), and
custody-open enforces the monotone rule witnessed <= found — which converts ADR-070's pinned
"consistent older pair rolls back undetected" residual into a NAMED refusal.

Proofs are split by ADR-063 doctrine: the exhaustive sweeps (lying directories, wrong sizes,
pair rollback, fault-at-every-pair-position) run on the host in kernel-core/tests/bootroot.rs;
the boot carries 14 invariants against REAL firmware and the REAL persistent medium on all
three targets, and every QEMU gate proves ABSENCE live with a third rootless boot.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CAP-DELIVERY-1 | Only Delivered(exactly 32 bytes) opens a vault; RootNotProvided, FirmwareAbsent and MalformedRoot(size) are refused BY NAME before any byte is decrypted. | Custody that accepts "whatever the bus says" is a minting path; the shapes must be facts, not moods. | open_custody_names_every_undelivered_shape_without_touching_the_medium; every_wrong_size_is_refused_before_a_byte_is_wanted |
| INV-CAP-DELIVERY-2 | The directory walker is fail-closed against lying firmware: counts past the data, truncated entries, prefix-name lookalikes and dead buses all end in a named outcome, never a panic and never garbage accepted as a root. | Firmware is untrusted input to the trust ANCHOR; a walker that overreads parses attacker structure. | a_directory_count_past_the_data_ends_the_walk_fail_closed; a_truncated_entry_never_matches_and_never_panics; a_prefix_lookalike_name_must_not_match; a_dead_bus_is_named_firmware_absent_never_a_root |
| INV-CAP-DELIVERY-3 | The sealed registry refuses a FOREIGN root while opening under the delivered one, on the same medium. | Proves the disk alone confers nothing and delivery is load-bearing, not decorative. | suite check 2; the_boot_suite...on_reboot |
| INV-CAP-DELIVERY-4 | Paired commits witness the vault generation inside the entity-store record; rolled-back custody (a consistent older PAIR) is refused by name naming both sides; recovery at the witnessed generation keeps authority intact. | Closes ADR-070's pinned residual; availability cost of divergence is a lockout pending a forward commit, never authority destruction. | rolling_both_vault_objects_back_is_caught_by_the_entity_store_witness |
| INV-CAP-DELIVERY-5 | At EVERY device-op position of a paired commit, whatever lands leaves witnessed NEVER ahead of found — forward-safe by commit order. | An interrupted pair must be a retryable state, never a refusal trap that bricks the machine. | a_fault_at_every_pair_position_leaves_witnessed_never_ahead_of_found |
| INV-CAP-DELIVERY-6 | Booted without the platform item, the vault stays sealed BY NAME, the durable store still witnesses, and the machine reaches e2e PASS. | One missing authority must not kill the machine — nor be silently papered over. | third rootless boot asserted by all three QEMU gates |


## INV-IOMMU — the IOMMU contract (ALET-P1-018, ADR-071)

SoftIommu models what a hardware IOMMU enforces: per-device translation spaces that are deny-by-default,
faults NAMED by device and address, kernel-image protection on both sides of every mapping, bounded
mapping tables, and revocation-as-unmap. Proved host-exhaustive plus a compact boot suite on every
target.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-IOMMU-1 | An unattached device has NO address space - its translations fault by name (`UnknownDevice`). | Deny-by-default means absence is visible, not silently permissive. | suite check 1 |
| INV-IOMMU-2 | A mapped window translates each page to EXACTLY its own physical page - an offset IOVA lands on the offset PA. | This is what makes it TRANSLATION rather than pass-through: if the model just echoed addresses back it would prove nothing about a hardware IOMMU's page-table walk. | suite check 2 |
| INV-IOMMU-3 | An unmapped page INSIDE an attached device's space faults BY NAME (`NotMapped{device, iova}`). | Deny-by-default firing mid-space proves holes between mappings exist and are detected, not papered over. | suite check 3; fuzz sweep in tests/iommu.rs |
| INV-IOMMU-4 | One device's windows DO NOT EXIST for another device - cross-device isolation is structural. | Device A's buffers are invisible to device B without an explicit map for B; this is the isolation a real IOMMU exists to provide. | suite check 4 |
| INV-IOMMU-5 | The kernel image is refused as BOTH sides of any mapping (IOVA and PA), named `KernelImage`. | Closes the write-to-code path arriving via DMA, complementing W^X which closes it at the MMU layer. | suite check 5 |
| INV-IOMMU-6 | Overlapping IOVA windows AND physical aliasing inside one device are named `DoubleMap` refusals. | Two windows reaching one frame within one device is the DMA twin of a double free. | suite check 6 |
| INV-IOMMU-7 | Unmapping ends access IMMEDIATELY: after unmap, translations fault by name. | Revocation that leaves access alive is worse than none - it looks revoked but isn't. | suite check 7 |
| INV-IOMMU-8 | A read-only page refuses stores under its own name (`PermDenied`, distinct from `NotMapped`). | Permission enforcement must be distinguishable from absence of mapping, or debugging a fault is guesswork. | suite check 8 |
| INV-IOMMU-9 | Every fault and translation is COUNTED - the boundary did measurable work, not silence. | A silent boundary cannot be audited; counters make the enforcement observable at boot. | suite check 9 |

## INV-VTD — the IOMMU contract meets hardware (ALET-P1-018, ADR-073)

The x86-64 target programs a REAL DMA-remapping unit (Intel VT-d as emulated on q35) and proves
ADR-071's contract from the machine's own fault bank. Discovery is from firmware declaration
(ACPI DMAR/DRHD) and the unit's capability words — never hardcoded offsets; evidence comes from
the fault-record BANK because QEMU serves FSTS at 0x34 against the spec's 0x30.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-VTD-1 | The remapping unit is found through the ACPI DMAR table's DRHD structure and its register page passes the device-memory admission rule. | A base nobody declared is a base somebody guessed; discovery must be firmware's word, walked by declared lengths. | boot invariant 1 |
| INV-VTD-2 | The unit answers sane identification (VER nibble-BCD, CAP/ECAP non-degenerate), AGAW picked from CAP.SAGAW, variable registers located via ECAP.IRO/CAP.FRO; RWBF-demanding units are REFUSED. | Programming an unidentified or flush-dependent unit is programming by hope; refusal names what was unreadable or unsupportable. | host tests/vtd.rs probe suite; boot invariant 2 |
| INV-VTD-3 | Translation starts OFF - firmware hands the machine quiet. | Enablement must be OURS to prove; inheriting TES would make every later claim about ordering untestable. | boot invariant 3 |
| INV-VTD-4 | The identity domain is built entirely from OWNED frames and the allocator's count balances exactly (tree + root + context). | Tables the ownership model cannot account for are exactly the memory DMA protection exists to guard. | boot invariant 4 |
| INV-VTD-5 | The kernel image has NO leaf: a live audit of every present entry counts ZERO image violations. | ADR-071's sharpest promise on real silicon - a device inventing kernel-text addresses faults at the unit, not reads code. | host walker proofs; boot invariants 5+6 with SoftIommu mirror agreement |
| INV-VTD-6 | Model and machine agree: SoftIommu refuses the image and translates spans identically to what was programmed. | Two enforcement stories that can diverge are one story too many; the mirror keeps the model honest against the tables. | boot invariant 6 |
| INV-VTD-7 | The root table is adopted through the SRTP handshake (RTPS observed, CFR distinguished from timeout). | A pointer the unit never adopted protects nothing; the handshake is the difference between published and in-effect. | boot invariant 7 |
| INV-VTD-8 | Enforcement turns ON through TE and TES is OBSERVED before any live probe runs. | Proofs under assumed enforcement prove nothing; the bit must be read back first. | boot invariant 8 |
| INV-VTD-9 | A GRANTED function kicked repeatedly under enforcement leaves the fault bank EMPTY. | Silence here means every descriptor, buffer and ring access translated cleanly - the positive half of enforcement, taken from the unit itself. | boot invariant 9 |
| INV-VTD-10 | Revoking one function's context entry (then global CCMD+IOTLB invalidation) makes its next kick produce an ACTIVE record naming ITS source-id with reason CONTEXT_ENTRY_P. | Denial must be per-function and attributable, or revocation is theater; repeat faults compress, so the probe re-kicks until evidence lands. | boot invariant 10 |
| INV-VTD-11 | Restoring the saved entry and re-invalidating returns the SAME function to silence after the record is retired write-one-to-clear. | Recovery must be real: the cleared bank separates old evidence from new behavior, so silence proves the restored walk. | boot invariant 11 |
| INV-VTD-12 | Enforcement REMAINS on to halt - TES still set, root pointer unchanged - LAYERED OVER the software registry which still refuses unregistered addresses. | An IOMMU that silently drops, and a boundary that trusts it, fail together; latching + layering makes both visible forever after. | boot invariant 12 |

## INV-KEYMAP — what a scancode means (REQ-CON-003, ALET-P2-039, ADR-049)

A keyboard is a device someone else may be holding. The interesting properties are therefore not
"does `a` type an `a`" but: can any sequence of bytes from this device make the line editor see
something it has no rule for, or leave the decoder in a state the user cannot get out of. The
decoder is arch-independent and proved on all three targets and the host; the i8042 that feeds it is
x86-64's alone.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-KEYMAP-1 | **The output alphabet is closed.** Every scancode, in every reachable modifier state, decodes to bytes the line editor has a rule for — the alphabet `shell::editor_accepts` defines, which since ADR-050 is the SINGLE definition both modules use — or to nothing. Exhaustive over all 256 codes × every state, not sampled. | This is the security boundary. The editor refuses bytes it has no rule for (`console: a non-printable byte never enters the line`); a decoder free to emit arbitrary bytes would be a way to hand it one anyway, from hardware an attacker controls. A `Ctrl` chord is delivered only when the editor has a rule for the byte it produces; `Ctrl-G` and `Ctrl-Z` produce nothing. Widening the editor's alphabet is the only thing that may widen what the keyboard can send. | `every_byte_the_map_can_emit_is_one_the_console_accepts`; `no_chord_invents_a_control_byte`; in-kernel `keymap: no scancode in any modifier state emits a byte the console refuses` |
| INV-KEYMAP-2 | **A press types once.** Every release, for every code, produces nothing. | Without it every key types twice, which is not a subtle failure but is a very easy one to write. | `a_release_never_types`; `a_word_types_exactly_its_letters` |
| INV-KEYMAP-3 | **Modifiers are held state**, ended only by their own break code — never by a character key, and unaffected by a release that was never pressed. | Holding shift through a word is the ordinary case; a decoder that cleared shift after the first letter is right once and wrong four times. And a device can send anything, so state must not be steerable by codes that mean nothing. | `shift_is_held_state_and_either_key_works`; `a_character_key_does_not_release_a_modifier`; `a_spurious_release_changes_nothing` |
| INV-KEYMAP-4 | **Caps affects letters only**, cancels against shift, and toggles on the press only. | Running caps over the punctuation row makes the number row unusable; toggling on the release too cancels itself and the key looks dead. | `caps_affects_letters_only_and_cancels_against_shift`; `caps_toggles_on_the_press_only` |
| INV-KEYMAP-5 | **An extended prefix consumes exactly one code and cannot be re-armed forever.** `E0 2A` (the fake shift the controller wraps some keys in) must not touch the real shift state, and `E0 E0` must RESOLVE rather than re-arm. | **This found a real defect.** The prefix test used to run before the pending-prefix test, so a device emitting a stream of `E0` swallowed every real key after it: a keyboard permanently dead with nothing crashed and no error anywhere. Resolving first bounds a malformed stream to eating one code per prefix. | `an_extended_prefix_consumes_exactly_one_code_and_sticks_nothing`; `the_extended_control_key_still_cancels` |
| INV-KEYMAP-6 | Enter is a **carriage return** and Backspace is **0x08** — the two bytes the line editor's contract is written against, so the editor needs no second rule for a second input source. | The whole design is one editor behind two sources. A keyboard that delivered `\n` would need the editor to grow a rule that the serial path does not have. | `enter_and_backspace_are_the_bytes_the_editor_expects` |
| INV-KEYMAP-7 | **No sequence of scancodes can wedge the decoder.** After 100 000 arbitrary bytes, releasing the modifiers a user can actually release leaves a keyboard that types a clean word. | A device that can wedge the decoder locks the user out of their own console without crashing anything — the failure mode with no error message. | `no_sequence_of_scancodes_can_wedge_the_decoder` |
| INV-KEYMAP-8 | **A navigation key moves the cursor and never types into the line.** The arrows, `Home`, `End` and `Delete` emit the ANSI sequences the editor parses (`ESC [ D`, `ESC [ 3 ~`), and feeding what the decoder emits to a real `LineEditor` leaves the LINE unchanged, the cursor moved, and the parser unarmed. | The two modules must agree about the GRAMMAR, not merely about the alphabet. A decoder that emitted a sequence one byte different from what the editor parses would leak its tail into the operator's command — which is exactly the defect ALET-P2-040 recorded, in the other direction. | `a_navigation_key_moves_the_cursor_and_never_types_into_the_line`; `the_left_arrow_really_moves_the_insertion_point`; in-kernel `keymap: the navigation keys emit the sequences the editor parses` |

## INV-CONSOLE-EDIT — the console parses its input (REQ-CON-004, ALET-P2-040, ADR-050)

ADR-044's editor was a *filter* over single bytes, and a terminal sends *sequences*. Dropping `ESC`
and then admitting the printable `[` and `A` that follow it is how every arrow key came to type
`[A` into the middle of the operator's command. These invariants are about what a byte inside a
sequence may do (nothing) and what the parser may be left in (never an armed state), because both
failure modes are silent — one corrupts the line, the other eats the next keystroke.

Arch-independent and proved on all three targets plus the host, since which bytes become a command
must not vary by CPU.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CONSOLE-EDIT-1 | **No byte inside an escape sequence reaches the line**, recognized or not. Arrows, `Home`, `End`, `Delete`, Page Up and a bracketed paste all leave the line's text exactly as it was. | THE regression. The old filter admitted the tail of every sequence as printable text, so a cursor key corrupted the command being typed and there was nothing on screen to explain it. | in-kernel `console: an arrow key moves the cursor and types nothing into the line`; `scripts/keyboard-e2e.sh` presses `left` at the emulated i8042 and asserts on the filesystem |
| INV-CONSOLE-EDIT-2 | **An unrecognized sequence is consumed whole and leaves the parser unarmed** — any final byte in `0x40..=0x7e` closes the sequence, whether or not the editor has a meaning for it. | A parser left waiting for a final byte eats the operator's next real keystroke. Losing one key in ten with no error is harder to diagnose than typing garbage. | in-kernel `console: an unrecognized sequence is consumed whole and leaves the parser unarmed` |
| INV-CONSOLE-EDIT-3 | **Sequence parameters are bounded**: `ESC [` followed by 4096 digits still terminates at its final byte, having remembered at most `CSI_PARAM_MAX` bytes. | A peer on a serial line can send digits forever. The bound is what makes that cost a fixed eight bytes instead of an allocation in the kernel. | in-kernel `console: an over-long escape sequence is bounded and still terminates` |
| INV-CONSOLE-EDIT-4 | **A control byte inside a sequence abandons it and means what it says** — a return arriving mid-escape still executes the line. | Otherwise one stray `ESC [` on a noisy wire makes the console ignore every line the operator types until a letter happens to arrive: a machine that looks hung while working perfectly. | in-kernel `console: a return arriving inside a sequence still executes the line` |
| INV-CONSOLE-EDIT-5 | **The cursor is real**: text inserts where the cursor is, backspace erases before it, `Delete` removes under it, `Home`/`End` reach the ends, and the cursor stops at both ends however hard a terminal pushes. | A cursor key that is merely *swallowed* satisfies INV-CONSOLE-EDIT-1 and is still not a line editor. These are what separate "the sequence was ignored" from "the sequence did what the operator meant". | in-kernel `console: text is inserted where the cursor is, not always at the end`; `console: backspace erases before the cursor, not at the end of the line`; `console: Delete removes under the cursor and Home reaches the start of the line`; `console: the cursor stops at both ends of the line` |
| INV-CONSOLE-EDIT-6 | **History is bounded (32), records neither blank lines nor an immediate repeat, and walking down past the newest entry restores the half-typed line the walk interrupted.** | A console is a long-lived session on a machine with no swap, so "remember everything" is a leak with a human-shaped fuse. Losing the half-typed line is the classic history bug, and what it loses is the operator's work. | in-kernel `console: history is bounded, and records neither blank lines nor an immediate repeat`; `console: walking history down past the newest entry restores the half-typed line` |
| INV-CONSOLE-EDIT-7 | **Tab completes against what exists** — command names from the same table `help` prints, object names from the live namespace — and an ambiguous Tab shows the candidates and keeps the line intact. | Completion that guesses, or that eats the line while redrawing it, is worse than no completion. Completing commands from the `help` table is also what stops a command being completable and undocumented. | in-kernel `console: Tab completes a command name from its prefix`; `console: Tab completes an object name from the namespace that exists`; `console: an ambiguous Tab shows the candidates and keeps the line` |
| INV-CONSOLE-EDIT-8 | **A decoded key enters the input ring whole or not at all.** With two slots free, a three-byte arrow sequence is refused entirely and counted as three drops. | A truncated sequence is worse than a dropped one: the parser would be mid-sequence when the next real keystroke arrived. This is INV-CONSOLE-EDIT-2's failure mode reached through the ring instead of the wire. | in-kernel `conring: an escape sequence is admitted whole or not at all, never truncated` |
| INV-CONSOLE-EDIT-9 | **One alphabet, defined once.** `keymap::Keymap::is_console_byte` delegates to `shell::editor_accepts`, and the decoder's exhaustive sweep is run against it. | Two producers feed this editor. A second copy of the alphabet in the decoder is a second list that drifts, and the drift is invisible until a device someone else is holding sends the byte that fell through the gap. | `every_byte_the_map_can_emit_is_one_the_console_accepts`; `no_chord_invents_a_control_byte`; `no_sequence_of_scancodes_can_corrupt_the_next_line_typed` |

## INV-CONSOLE-CMD — what the commands do to the namespace (REQ-CON-005, ALET-P2-041, ADR-051)

A command table is a set of promises about someone's data. These invariants are the ones where the
wrong behavior is *plausible*: a `touch` that truncates, a `mv` that loses the bytes instead of the
name, an `append` that replaces, a count that silently defaults. Each is proved by an effect on a
real namespace, not by the command printing that it worked.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|
| INV-CONSOLE-CMD-1 | **A copy is an independent object.** Writing the source after `cp` leaves the copy unchanged. | A copy that shared an extent would look correct until the first edit, and then silently change data the operator believed was safe. Proved by writing the ORIGINAL afterwards, which a name-aliasing bug cannot survive. | in-kernel `console: a copy is an independent object, not a second name for the same bytes` |
| INV-CONSOLE-CMD-2 | **A rename moves the bytes and removes the old name** — copy first, remove second, and never the other order. | `mv` reads atomic and is not. In this order a crash leaves both names, which a person can fix; in the other it loses the data. | in-kernel `console: a rename moves the bytes and removes the old name`; `scripts/console-e2e.sh` re-reads the renamed object after a REBOOT |
| INV-CONSOLE-CMD-3 | **`append` keeps what was there**, and creates the object when it is absent — in one `replace` transaction. | Read-modify-write is the only command here that reads before it writes, so it is the only one that can lose what it read. One transaction means a crash leaves the old contents or the new ones (ADR-035), never a half-appended object. | in-kernel `console: append keeps what was there and creates what was not` |
| INV-CONSOLE-CMD-4 | **`touch` leaves an existing object's bytes alone.** | There is no modification time here, so the only other thing `touch` could do to an existing object is destroy it — a harmless-looking command that eats data. | in-kernel `console: touch leaves an existing object's bytes alone` |
| INV-CONSOLE-CMD-5 | **A bad numeric argument is refused, not defaulted.** `head NAME x` and `hexdump NAME x` say so. | Quietly reading ten lines because the count would not parse makes the output an answer to a question nobody asked, exactly when the operator is trying to understand something. | in-kernel `console: a bad count is refused rather than replaced with a default` |
| INV-CONSOLE-CMD-6 | **Every command that needs an argument refuses to run without one**, swept over the whole table rather than sampled. | A command whose usage line was forgotten acts on an empty name. Sweeping the table is what makes this a property of the console rather than of the six commands someone remembered to test. | in-kernel `console: every command that needs an argument refuses to run without one` |
| INV-CONSOLE-CMD-7 | **`hexdump` shows the bytes `cat` refuses to print.** | `cat` declines non-text contents so a terminal is not handed escape sequences from a device. Without a second way to look, that refusal makes the object unreadable rather than safe. | in-kernel `console: hexdump shows the bytes cat refuses to print` |
| INV-CONSOLE-CMD-8 | **`history` prints the same list the up arrow walks.** | One list, so what an operator is shown and what they can recall cannot diverge. | in-kernel `console: history shows the lines this session ran` |

## INV-PS2 — bringing the controller up (REQ-CON-003, ADR-049)

x86-64 only, and run on **every** boot including the non-interactive gate build: a driver that only
runs when someone is sitting at the machine is a driver no gate covers.

| Id | Invariant | Why it is load-bearing | Proof |
|----|-----------|------------------------|-------|
| INV-PS2-1 | The machine's own declaration is consulted **before any port is touched**, and which of the three answers it gave is legible. | On a legacy-free platform the i8042 ports are unclaimed, not empty: reading them is undefined on the bus rather than a read that returns nothing. And an ABSENT `IAPC_BOOT_ARCH` is not a ZERO one — absent means ACPI 1.0, where the controller is universal. | boot invariant `ps2: the firmware's 8042 declaration is legible` |
| INV-PS2-2 | Bring-up **terminates**. Every controller wait is spin-bounded. | An OS a missing device can stop forever is not a production OS. A keyboard that never answers must cost a bounded delay and a printed reason. | boot invariant `ps2: bring-up terminates` |
| INV-PS2-3 | Controller **translation is enabled, read back from the controller** rather than remembered from the write. | Translation is what makes the set-1 decoder correct. A controller that silently dropped the write would deliver set 2 into a set-1 decoder — every key wrong, presenting as a broken keymap rather than a broken assumption. | boot invariant `ps2: controller translation is enabled`; measured device id `AB 41` under QEMU is itself the evidence |
| INV-PS2-4 | The device **identified itself after passing its power-on self-test**, and the id is in the log. | A controller can pass its own self-test with a dead port, which is why the port test is separate; and the only way anyone learns what an unfamiliar keyboard answered is if the boot said so. | boot invariant `ps2: the keyboard identified itself` |
| INV-PS2-5 | The suite leaves **IRQ1 masked**, read from the PIC rather than from its own bookkeeping. | Arming an input source is the console's decision, made once beside the other source. A boot suite that armed one behind the console's back would hand every later suite a machine taking interrupts it was not written for. | boot invariant `ps2: the suite leaves IRQ1 masked` |
| INV-PS2-6 | End to end: a keystroke injected at the **emulated i8042** travels controller → IRQ1 → PIC → vector 0x21 → decoder → shared ring → line editor, and Aletheia's own filesystem changes. | The gate the old one could not be. `console-e2e.sh` types at the serial line, and under `-serial stdio` the terminal IS the wire — which is exactly why ALET-P2-039 survived every console gate. Here the serial line is a FILE with no writer. | `scripts/keyboard-e2e.sh` (7 checks); confirmed by hand on Oracle VirtualBox with `keyboardputstring` |
## INV-SOAK — lifecycles under repetition (REQ-QUAL-007, ALET-P2-009, ADR-063)

Every other contract on this page was proved on hand-picked cases; this one asks whether the properties
SURVIVE THE MACHINE RUNNING — committing, naming, sharing and dispatching for a very long time. On a
kernel whose heap never frees, endurance is a resource property before it is a correctness property:
one more cycle must cost nothing permanent.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
|----|-----------|------------------------|-------------------|

## INV-ATREST — encryption at rest is a lifecycle (REQ-STORE-002, ALET-P1-028/029/030, ADR-069)

The store encrypted every frame from its first commit; what it did not have was a LIFECYCLE. These are
the invariants that make the at-rest layer a system rather than a cipher call. All are proved on the
host by `aletheia/tests/encryption_at_rest.rs` (plus three unit tests in `aletheia/src/atrest.rs`);
they are hosted-store contracts — the kernel-side durable store has no crypto yet and claims none.

| Id | Invariant | Why it is load-bearing | Adversarial proof |
| INV-CAP-CUSTODY-1 | A sealed registry **reopens under its root alone** with authority intact — across repeated save/reopen cycles verdicts are identical and reserved counters strictly increase. | Custody that cannot restore authority is decoration; counters that could repeat would reuse a nonce under the same key, the one failure AEAD cannot survive. | `sealed_round_trip_across_reopen_cycles_keeps_authority_and_counters_monotone` |
| INV-CAP-CUSTODY-2 | Rotation mints **max+1** and retains its predecessor; rekey retires every version below the newest, **destroying** the retired key inside the vault (`key_for_test` returns None); a replayed pre-pivot image names its dead version. | Retirement that leaves a usable key behind is a label, not retirement — the one-way chain makes deletion real, and replay is the smallest edit that revives the most authority. | `rekey_retires_by_name_and_destroys_the_retired_key`; `a_refusal_at_every_rekey_position_leaves_a_complete_world` sibling assertions in `kernel-core/tests/capvault.rs` |
| INV-CAP-CUSTODY-3 | Rolling the KEYSTORE back alone against a newer image names the future version (`FutureVersion{requested, newest}`); rolling back BOTH objects consistently OPENS — the documented residual an external anchor would be needed to catch. | Detectability must be stated per-object: keystore-alone rollback is visible, whole-world rollback is not. Both directions PINNED so doc and behavior cannot drift. | `rollback_semantics_are_pinned_in_both_directions`; also asserted inside the crash-position sweep's promised stages |
| INV-CAP-CUSTODY-4 | A wrong root refuses the WHOLE keystore with nothing loaded, nothing decoded, and **no byte of the medium changed** — verified block-for-block against a pre-attempt snapshot. | Authentication precedes parsing, so a failed open releases no bytes into any parser; fail-closed must also mean side-effect-free at the device level. | `a_wrong_root_refusal_is_a_total_noop_at_the_device_level` |
| INV-CAP-CUSTODY-5 | **Every** single-bit flip of EITHER object and **every** truncation of either object is refused, through real filesystem rewrites. | A region the AEAD or the structural checks did not cover would show up here as an open that succeeded; the sweeps make "authenticated" measured rather than intended. | `every_single_bit_flip_of_either_object_is_refused_through_the_filesystem`; `every_truncation_of_either_object_is_refused` |
| INV-CAP-CUSTODY-6 | Objects from ANOTHER store refuse under ours by name: the image fails authentication under our key (`ImageAuth`), the keystore under our root (`KeystoreAuth`). | Same machinery, different custody: derivation is domain-separated but custody-scoped, and the refusal names which layer caught it. | `another_stores_objects_refuse_under_ours_by_name` |
| INV-CAP-CUSTODY-7 | The counter protocol RESERVES first and exhaustion at u64::MAX is NAMED: MAX-1 seals and reserves MAX; MAX refuses BY NAME storing nothing and changing nothing; rotation escapes exhaustion without touching the root. | Reserve-first survives crashes (a gap wastes a number, reuse is impossible); wraparound would silently hand a new image a used nonce. | `counter_exhaustion_is_exact_named_and_escapable_by_rotation`; round-trip test asserts strictly increasing counters across cycles |
| INV-CAP-CUSTODY-8 | At EVERY device-operation position of the three-commit rekey pivot, a refusal of the RIGHT KIND surfaces as Err, the protocol ABORTS consuming nothing further, and the reopened world holds SOME complete stage ([1], [1,2] or [2]) with authority intact. | The pivot touches two objects across three commits; a half-pivot world would strand authority between versions. The op sequence is RECORDED from one clean run, so each fault aims correctly instead of silently missing. | `a_refusal_at_every_rekey_position_leaves_a_complete_world` (exhaustive over the recorded sequence) |
| INV-CAP-CUSTODY-9 | Layering: an image that AUTHENTICATES under a real retained key but widens a delegation inside is refused THROUGH the vault with the inner admission name preserved (`Image(Amplified)`). | Custody sits ON TOP of INV-CAP-LIFE's checks, never instead of them — the seal must not launder an admission failure into something vague. | `a_widened_registry_sealed_under_the_real_key_is_refused_through_the_vault` (forgery sealed via `seal_image_bytes_for_test`) |
|----|-----------|------------------------|-------------------|
| INV-ATREST-1 | The content address is **SHA-256(PLAINTEXT)**, and dedup happens above the crypto layer: two puts of equal content write exactly one frame, and the address equals the hash of the plaintext both times. | Identity is a semantic fact that must outlive the storage encoding. An address derived from ciphertext would change under rotation/rekey and break every stored reference to it. | `address_is_plaintext_sha256_and_dedup_survives_encryption` |
| INV-ATREST-2 | Equal plaintexts NEVER produce equal frames: not at two positions in one store, not across independent stores. | If ciphertext leaked equality of plaintext, the "no metadata leaks" claim of encryption at rest would be false in the one direction content addressing tempts you to reintroduce it. | `identical_plaintexts_produce_different_frames_and_cross_store_ciphertext_differs` |
| INV-ATREST-3 | Data-frame nonces are CONSTRUCTED prefix||counter pairs that are GLOBALLY distinct across reopen cycles, strictly increasing within a prefix, recovered from the authenticated log after any restart, and exhausted only as a NAMED refusal. | Nonce reuse under one key is the single catastrophic failure AEAD admits; "the CSPRNG was probably fine" is not a lifecycle, and a wraparound would be silent reuse. | `nonce_lifecycle_prefix_counter_never_repeats_across_reopen_cycles`; `nonce_exhaustion_is_a_named_refusal_not_a_wraparound`; unit `counters_advance_monotonically_across_instances` |
| INV-ATREST-4 | EVERY single-bit flip anywhere in the log image is refused at open — length prefixes, headers, nonces, ciphertext, tags, all of it, exhaustively. | A tamper-evidence claim with sampled coverage is a hope; the log is small enough to sweep every bit, so the claim is exact. | `every_single_bit_flip_of_the_whole_log_image_is_refused` |
| INV-ATREST-5 | Frames are POSITION-bound: transposition, middle deletion, duplication and resend-at-another-position each fail authentication; trailing partial bytes are a refusal, never silent residue. | The AAD binds sequence number + length + key version, because an unbound frame stream can be restructured without touching any tag — confidentiality is not integrity of ARRANGEMENT. | `transposed_deleted_and_duplicated_frames_are_refused_by_position_binding` |
| INV-ATREST-6 | Torn-tail truncation mid-frame is REFUSED; truncation at an EXACT frame boundary opens with the surviving prefix intact — the documented residual, pinned in BOTH directions. | Without an external anchor the boundary case is undecidable; pretending otherwise would be a false claim, and NOT testing it would let doc and behavior drift apart silently. | `torn_tail_refused_but_boundary_truncation_is_the_documented_residual` |
| INV-ATREST-7 | Keys are VERSIONED under the root-wrapped keystore: rotate mints max+1 with a fresh nonce prefix, writes use the newest, old frames stay readable while held, rekey collapses the log to ONE version and retires the rest with nothing orphaned, and a reopened store reads every retained version. | Rotation is the response to key compromise; a rotation that orphans data or strands the writer is a defect wearing a security name. | `rotate_write_reopen_reads_every_version_then_rekey_collapses_to_one` |
| INV-ATREST-8 | Wrong root key and tampered keystore are NAMED refusals with NO partial load; retirement above the newest version and stranding retirements refuse by name. | Fail-closed is only fail-closed if the failure says what failed; a wrong-key open that returned an empty store would masquerade as a fresh install and invite re-secretting into a broken state. | `wrong_root_key_is_a_named_refusal_not_an_empty_store`; `keystore_tamper_refuses_the_whole_image_with_no_partial_load` |
| INV-ATREST-9 | A pre-ADR-069 legacy log is recognized by trial-AUTHENTICATION (never by magic alone), migrated wholesale before any append, readable through the UNCHANGED public API, idempotent across crash, and steady-state logs are pure v2 thereafter. | Existing stores must upgrade without losing a byte, and format ambiguity must be decided by cryptography rather than by a byte pattern a random nonce could imitate. | `pre_adr069_legacy_log_is_detected_by_authentication_and_migrated_transparently` |
