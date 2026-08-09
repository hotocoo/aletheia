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
