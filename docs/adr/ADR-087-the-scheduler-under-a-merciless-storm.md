# ADR-087 — The scheduler under a merciless storm

* Status: accepted
* Date: 2026-09-03
* Register: ALET-P3-007 (scheduler), ALET-P2-009 (soak), REQ-QUAL-007, REQ-ML-002, REQ-SCHED-002
* Extends ADR-020 (priority inheritance), ADR-056 (the forest advises the order),
  ADR-086 (the desktop under a merciless storm), ADR-063 (the boot heap never frees)

## Context

ADR-086 stormed the desktop and found two per-event allocations on a heap that never frees. The
scheduler is the other half of that question and the more serious one: it runs on EVERY dispatch,
it is where machine learning actually touches the machine (ADR-056: the forest advises the order,
it never invents a task), and a bug there is not a stutter — it is a task nobody ever runs.

Every scheduler invariant so far was proved on small, hand-built pools. Nothing measured the
machine at dispatch volume, and nothing at all measured what one dispatch COSTS in memory.

## Decision

`kernel-core/src/schedstorm.rs` floods the priority scheduler with a deterministic workload (the
same LCG stream ADR-086 storms with, so a failure fails identically on every CPU) and holds it to
five claims:

1. **Strict priority is strict, at volume.** Over 8192 dispatches, no READY task of a higher
   effective priority was ever passed over — checked on every dispatch against the scheduler's
   own view, not sampled.
2. **Equals are served FIFO and nobody starves inside a band.** Within one priority, service
   counts differ by at most one turn.
3. **The advisor reorders; it never changes MEMBERSHIP.** A full drain with a decisive advisor is
   a PERMUTATION of the model-free drain: the same tasks, each exactly once, in a different
   order. A model that changed membership would be deciding WHAT runs, not what runs first —
   the line ADR-056 draws (INV-014).
4. **A workload lifecycle at volume allocates NOTHING.** Four thousand admit → dispatch → finish
   cycles must not move the platform's own heap watermark; both numbers are printed on the boot
   log, pass or fail.
5. **The same storm twice is the same machine twice** — the whole dispatch SEQUENCE, folded into
   one number, compared.

**What the storm found, and what changed because of it.** Claim 4 failed: about sixty bytes per
dispatch. `PriorityScheduler::ready_dequeue` deleted a band's decisive-`Low` census the moment it
emptied (`low_seq.remove(&prio)`), and the next `Low` admission at that priority allocated a whole
fresh `BTreeSet`. On a bump heap that never frees, that is a machine that dies of its own
scheduling. The census is now KEPT when it empties: an empty census costs one map entry per
priority band (bounded by the 256 bands that can exist), `pick` already reads an empty band as
"no Low member", and the measured cost of a dispatch went from ~66 bytes to **zero**.

## Consequences

* New boot-gate family on all three targets: `[schedstorm] ALL 5 SCHEDULER-STORM INVARIANTS HOLD`
  (marker `schedstorm=5`, boot fails `760 + i`), with the heap measurement printed beside it.
* Five new cross-CPU conformance behaviours.
* `kernel-core/tests/schedstorm.rs` runs the same suite on the host under a counting allocator
  that IGNORES frees — the kernel heap cannot give bytes back, and a net-bytes measurement would
  hide exactly the churn this wave found.
* The storm reporter installed by each kernel now carries the FAMILY that measured, so
  `[wmstorm]` and `[schedstorm]` lines cannot be mistaken for each other.

## Named non-claims

* **No preemption is measured here.** This suite storms the POLICY (`schedule_next`, `admit`,
  `finish`, the advisory census), not the context switch — that is the SMP/user-mode gates'
  business.
* **No starvation claim across bands.** Strict priority means a lower band can wait forever while
  a higher band stays busy; that is the policy, not a bug, and claim 2 is deliberately scoped to
  service WITHIN a band. Aging/decay is not implemented and is not claimed.
* **Admission's memory boundary is ADR-081's**, proved there; this storm does not re-litigate it.
* Single-CPU by construction (the SMP suite parks its APs), so the storm proves volume, not
  concurrent dispatch.
