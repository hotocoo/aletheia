# ADR-238 — The running machine reclaims under pressure

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017 (row 1 of ADR-237's wave order), REQ-ML-005
**Builds on:** ADR-081 (memory boundary), ADR-082 (reclaim policy and eviction forest), ADR-213 (background programs), ADR-237 (gap register).

## Context

ADR-082 built the reclaimer and proved it twice: a suite over synthetic candidates and a storm
against each CPU's real allocator. Neither ran while the machine was in use. MATURITY said so:
"the reclaimer is not resident … a running machine's own pressure does not yet consult it."
ADR-237 put this first in the wave order because everything it needs already exists: the
allocator meter, the eviction forest, and, since ADR-213, programs left running in the background
that hold frames the machine could take back.

## Decision

* **A resident reclaimer.** `reclaim::resident` holds one verified `memrisk` forest for the
  machine's uptime, installed at boot beside the risk advisor on all three CPUs. A refused blob
  installs a model-free reclaimer and the boot log says which.
* **The job table is the candidate set.** `Jobs::reclaim(reclaimer, meter, pages)` turns every
  running background program into a candidate (its frames, its admission time, and the feature
  vector it was admitted with) and lets ADR-082's policy rank them: forest tier, then largest
  footprint, then oldest. The chosen jobs are taken out of the table and handed back; the target
  gives their frames back through the same `finish` that `kill` uses, reports the outcome to the
  advisor as `Evicted`, and the console prints the program as ended, `reclaimed under memory
  pressure`.
* **Asked after every turn, free when the machine is not short.** Each target's job tick reads
  the allocator after the slice. Not under the watermark: one comparison, nothing built, nothing
  counted. Under it: the reading goes to the risk advisor's pressure ledger and a round runs.
* **A job is judged on what it was admitted with.** The risk advisor now keeps the vector of its
  last admission (`last_features`); `start` stores it with the job. The submission also states the
  frames the placed program really holds (`Slot::pages`: code, stack, writable pages) instead of
  the constant 2 it used before.
* **The foreground is never a candidate.** A program run with `run`/`together` holds the console
  until it ends; only background jobs are in the table, so the console and the kernel's own
  frames cannot be chosen.
* `mlstat` prints the resident ledger: rounds, programs evicted, frames back, shortfalls, and how
  many candidates the forest gave a decisive tier.

## Evidence

* Host: `jobs` unit tests (no pressure: refused, table untouched, ledger zero; pressure: the
  largest job goes first and exactly the need is met; too little to take: everything goes and
  the shortfall is named; an empty table is refused `NothingEvictable`). kernel-core release
  suite: 933 passed.
* Boot: the reclaim family grows 9 → 11 on all three CPUs. Invariant 10: a table that is not
  short loses nothing and counts nothing. Invariant 11: under the suite's pressure the table
  evicts in exactly the order `Reclaimer::rank` gives with the shipped forest, the jobs taken out
  are exactly the ones evicted, and the frames counted are theirs.

## What this is not

No gate drives a live machine into pressure with real programs: a background program's writable
memory is capped at 16 pages and four jobs fit in the table, so on the gates' memory sizes the
running programs cannot reach the 10 % watermark. The live path is exercised by every job turn
(the not-short answer) and its decision logic by the host and boot proofs above. Eviction is of
whole programs; there is no swap, no compression, no partial reclaim. The forest's features are
admission-time (ADR-082's frozen contract).

## Consequences

* ADR-237 row "Reclaim under pressure" is live.
* The next pressure-relevant change (larger program memory, more jobs) inherits this path
  without new wiring.
