# ADR-028: Kernel lock hierarchy and atomic-ordering audit

**Status:** Accepted — the lock-hierarchy + atomic-ordering audit slice of REQ-SMP-001, delivered
alongside CPU affinity + cross-core migration (REQ-SMP-005). · **Date:** 2026-07-24

## Context

REQ-SMP-002/003/004 brought multiple cores online and put real concurrent state on them: a shared
capability engine, per-CPU run queues, TLB-shootdown inboxes, a bump frame allocator, and a handful
of release/acquire mailboxes. ADR-021 named the remaining risk plainly — "a documented lock ordering;
an atomic-ordering audit of all shared scheduler/memory paths" — and left it open under REQ-SMP-001.

A deadlock needs a cycle in the *held-while-acquiring* graph. A data race needs a shared location
touched without an ordering that establishes happens-before. This ADR audits both across every
shared path the kernel runs on real cores, states the total lock order, and installs a debug tripwire
that makes the load-bearing scheduler invariant self-enforcing rather than merely documented.

## Decision

### 1. Total lock order (deadlock-freedom by acyclicity)

Every kernel lock is assigned a rank. A CPU may acquire a lock only when it holds no lock of equal or
higher rank. Lower rank = acquired first (outer).

| Rank | Lock | Type | Held across another lock? |
|-----:|------|------|---------------------------|
| 10 | `ENGINE` (capability engine + minted cap) | `SpinLock<Option<CapState>>` | **No** — `with_authorization` runs the authorize+effect entirely inside this one lock; it acquires no run-queue, inbox, or allocator lock while held (ADR-027). |
| 20 | run-queue lock (one per CPU, `SmpSched::queues[i]`) | `SpinLock<VecDeque<…>>` | **No** — see §2. At most ONE is held per CPU at any instant. |
| 20 | TLB-shootdown inbox (one per CPU, `TlbShootdown`) | per-CPU FIFO + count | **No** — `service()` runs the local invalidation callback; that callback holds no other lock, and the initiator's `request()` waits on acknowledgements without holding a queue lock. |
| 30 | frame allocator (bump) | lock-free CAS, not a `SpinLock` | n/a — a single `compare_exchange` loop, no lock to nest. |

The two rank-20 locks are never held simultaneously by the same CPU (a run-queue steal and a shootdown
service never nest — they run in distinct phases and neither calls into the other). Because no path
holds a lower-or-equal-rank lock while taking another, the held-while-acquiring graph is a forest: no
cycle, so no deadlock. The rank-10 engine lock is the only lock ever held across a non-trivial body,
and that body (the authorization effect) takes no further lock — by ADR-027 design.

### 2. The scheduler's "≤1 queue lock per CPU" invariant (stronger than a two-lock order)

`kernel_core::smpsched::SmpSched` is deliberately stricter than the rank order requires: a CPU **never
holds two run-queue locks at once.**

- A **local dispatch** locks only the acting CPU's own queue.
- A **steal** first snapshots victim loads with brief, independent single locks (`load()` — lock,
  read length, unlock), then locks *exactly one* victim to take from it. The most-loaded scan does not
  hold any queue lock while reading another's length in a way that overlaps a take.
- Affinity placement (`enqueue_least_loaded_affine`) locks each candidate queue one at a time to read
  its length, then locks the chosen queue once to push — never two at once.

With at most one queue lock held per CPU, no lock-order cycle among the rank-20 queue locks can exist
*regardless* of index order — the invariant subsumes an ordered hierarchy over the queues.

**Enforcement (not just documentation).** `SmpSched` carries a per-instance, per-CPU held-lock counter
(`held: Vec<AtomicU8>`, compiled only under `debug_assertions`). Every dispatch-path queue lock goes
through `with_queue(acting_cpu, …)`, which increments the acting CPU's counter before acquiring and
asserts the previous value was 0 — so any future edit that nests two queue locks on one CPU panics
immediately with `lock-hierarchy violation (ADR-028)`. It is per-instance (not a `static`) so parallel
host tests never alias, and it is **live** in both the contention stress test
(`kernel-core/tests/smpsched.rs::exactly_once_under_mixed_affinity_contention`, 4 threads × 4000 tasks)
and every `-smp 4` VM gate (the gates build debug). A `#[should_panic]` test
(`nested_queue_lock_trips_the_audit_tripwire`) proves the tripwire is armed rather than a silent no-op.

The check runs *before* the actual `.lock()`, so a nested acquisition is caught before it could
self-deadlock on the spinlock.

### 3. Per-atomic-site ordering audit

Each shared atomic in the SMP path, and why its ordering is sufficient (not merely conservative):

| Site (`kernel/src/smp.rs` unless noted) | Ordering | Justification |
|---|---|---|
| `PHASE` (phase gate) | `SeqCst` store/load | The single global happens-before spine of the suite. Every phase's data is published/observed relative to a `SeqCst` `PHASE` transition, so a per-phase datum needs no ordering stronger than what makes it visible *before* the gate advances. |
| `SHARED_COUNTER` | `Relaxed` fetch_add | Counting only; correctness is the final *sum*, and `fetch_add` is atomic regardless of ordering. It is read for the assertion only after a `SeqCst` `PHASE`/`DONE_COUNTER` gate establishes all writers finished — the gate, not the counter, carries the happens-before. |
| `MAILBOX_DATA` / `MAILBOX_FLAG` | `Relaxed` data + `Release`/`Acquire` flag | Classic release/acquire publication: the data write happens-before the `Release` flag store, and a reader's `Acquire` flag load happens-before its data read. Data may be `Relaxed` because the flag fences it. |
| `REVOKED` | `Release` (set inside `ENGINE` lock) / `Acquire` (readers) | The linearization marker for revocation. Set with `Release` immediately after `revoke` *inside the engine lock*, so a reader observing `REVOKED` is guaranteed to see the revoked engine state — the ADR-027 "no commit after revoke" proof. |
| `SCHED_PTR` / `SHOOTDOWN_PTR` | `Release` publish / `Acquire` read | The leaked `&'static` is published with `Release` before `PHASE` advances; readers gate on `PHASE` (`SeqCst`) first, then `Acquire`-load the pointer — so the pointee is fully constructed before any dereference. |
| `ONLINE_MASK`, `SEEN_*`, `DONE_*`, `PARKED`, `IPI_SEEN`, `STEALS`, `EXEC_*` | `SeqCst` | Cross-core progress/observation flags read inside `wait_until` progress gates; `SeqCst` keeps the audit trivially correct and these are not on a hot path. |
| `SmpSched::held[cpu]` (`kernel-core`) | `AcqRel` | Debug tripwire; `AcqRel` gives the increment/decrement a total order per slot so a genuine nesting is observed, without claiming cross-slot ordering it does not need. |
| bump allocator cursor | `compare_exchange` (`AcqRel`/`Acquire`) | Lock-free bump: CAS loop makes each hand-out atomic; `AcqRel` on success orders the reservation against concurrent allocators (REQ-SMP-002). |

No shared SMP location is written without either (a) being behind a lock, or (b) an explicit
release/acquire (or `SeqCst`) that establishes happens-before to its readers.

### 4. What stays out of scope (honesty)

- **NUMA / rank-aware placement** — a later refinement (ADR-021 Phase 3 tail), not needed at ≤8 cores.
- **A general runtime rank lattice over all `SpinLock`s** — deliberately NOT built. The one invariant
  that carries real risk (≤1 queue lock per CPU) is enforced directly and is stronger than a lattice
  over the queues; adding rank fields to every `SpinLock` would be ceremony without a matching risk.
- **Affinity starvation** — a task pinned to a permanently-busy CPU waits for it; work stealing cannot
  rescue it, by design. Callers must supply satisfiable masks. This is inherent to affinity, documented
  in `smpsched`, not a deadlock.

## Consequences

- The lock order is a forest → the kernel's SMP paths are deadlock-free by construction, and the one
  invariant that a future edit could plausibly break is a compile-in-debug panic away from being caught.
- The atomic audit is per-site and re-checkable: adding a new shared atomic means adding a row and a
  justification, not re-reasoning the whole system.
- Combined with REQ-SMP-005 (affinity + migration), this closes the lock-hierarchy/atomic-ordering item
  of REQ-SMP-001. The umbrella REQ-SMP-001 flips to `delivered` only once affinity + migration + this
  audit are all green on all three targets (see `docs/TRACEABILITY.md`).
