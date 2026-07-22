# ADR-027: Capability concurrency model (authorize/execute atomicity)

**Status:** Accepted (Option A implemented + hosted-proved; SMP integration deferred) · **Date:** 2026-07-22

## Context

The capability engine (`kernel-core::spine::CapEngine`) is the strongest part of the system, with an
adversarial regression suite (REQ-SEC-001) covering confused-deputy, laundering, single-threaded
TOCTOU, and scope confinement. But every one of those tests — and the whole kernel — runs on ONE
core. Safety there rests on an implicit assumption: Rust's borrow checker serializes access, so a
`&self` `evaluate` and a `&mut self` `revoke` cannot overlap within a thread.

SMP (ADR-021, gap-register Issue 4) breaks that assumption. Two cores holding the engine behind a
lock can interleave the deterministic pipeline's *authorize* and *execute* steps:

```text
  CPU 0: evaluate(cap) -> Allow      (time-of-check)
  CPU 1: revoke(cap)                 (interleaves in the gap)
  CPU 0: execute()                   (time-of-use — acts on a capability that is now dead)
```

The stored `Allow` cannot see a revoke that happened after it was computed. GAPS2 gap #9 ("the
capability model needs a formal concurrency specification before SMP") requires this guarantee be
**defined and proved before** SMP spreads the assumption throughout the kernel (#9 precedes #4).

## Decision

**The authorization check and the effect it authorizes commit inside ONE critical section.** An
effect executes only if its capability is live at the linearization point of the effect. Revocation
is immediate and permanent; a revoke that linearizes before that point prevents the effect, one that
linearizes after cannot un-happen it (revocation withdraws *future* authority — that is correct).

There is no cached authorization: a `Decision`/`AuthOutcome` value is a point-in-time result, valid
only while the engine lock under which it was produced is still held. Authority is never resurrected
— once a revoke completes, no later authorization of that capability returns `Allow`.

### Option A (chosen) — single critical section

The kernel holds one lock (or `&mut` borrow) across authorize→commit; the guarantee is simply
"re-check liveness inside the critical section." This needs **no epochs or generation counters**: the
lock already excludes a concurrent revoke.

`CapEngine::with_authorization(action, target, offered, commit)` makes the discipline structural — it
evaluates and, iff `Allow`, runs the `commit` closure, all within one `&self` call. Because
revocation requires `&mut self` (the write side), under the engine's lock no revoke can linearize
between the check and the effect. The TOCTOU gap becomes **unrepresentable**, not merely
policy-forbidden. The closure additionally receives `&CapEngine`, so an effect may perform further
capability reads or a post-condition *verify* within the same authorized section (mirroring the
pipeline's authorize→execute→verify), but it cannot mutate the engine (no `&mut`).

`CapEngine::authorize` is the read-only variant that also reports *which* token matched (an
`Authorization`); `evaluate` (unchanged) reports only the verdict. Both route through one private
`test_token` matcher, so the fast path and the token-naming path cannot drift.

The engine's `now` logical clock is fixed at construction (no setter), so it is not shared-mutable
state under concurrency; only `revoke`/`delegate`/`mint` (all `&mut self`) mutate the engine, and all
are excluded during any `&self` authorization by the engine lock.

### Option B (documented, NOT built) — generation/epoch tokens

For a future lock-free / optimistic authorize (no lock held across check→use), each capability would
carry a revocation generation; `authorize` snapshots it and `confirm` rejects a stale snapshot. This
earns its complexity ONLY when the single-lock discipline is abandoned. Per ADR-010 (no speculative
code) and YAGNI, it is not implemented: Option A is sufficient for the single-big-lock kernel-core
that SMP Phase 2 (ADR-021) will start from. Revisit if per-CPU lock-free authorization is adopted.

## Consequences

- **Proved, hosted.** `kernel-core/tests/cap_concurrency.rs` shows the naive `check(); … ; act();`
  pattern is stale by construction, then hammers `with_authorization` under real `std::thread`
  contention (an `RwLock`-guarded engine, committer threads vs. a revoker) and asserts the effect
  **never** commits under a revoked capability and revocation is **permanent** (no resurrection).
- **Additive.** `evaluate`/`revoke`/`mint`/`delegate` signatures are untouched, so `selftest`, the
  hosted invariant suite, and all three VM gates are unaffected.
- **Honesty boundary.** This proves the *mechanism* under host threads. It does **not** prove an
  SMP-safe kernel — none exists yet. Wiring `with_authorization` into each target's real trap/IPC
  path, plus the TLB-shootdown / atomic-ordering audit, is the SMP integration deferred under ADR-021
  and gap #4 (`REQ-SMP-001`). This ADR is the prerequisite spec that unblocks that work.
