# ADR-082: reclaim under pressure — the allocator triggers, the policy chooses, the forest advises

**Status:** Accepted · **Date:** 2026-09-02 · **Advances:** REQ-ML-005 (the eviction-event forest, trained and measured in ADR-056's wave, now WIRED; residency under live pressure stays scoped) · **Builds on:** ADR-081 (the memory boundary and its meter), ADR-056 (the frozen integer forest, advisory by construction, INV-014), ADR-030 (frame ownership), ADR-032 (address-space teardown returns everything), ADR-042 (kill the task, keep the system), ADR-064 (the machine measures itself)

## Context

ADR-081 made memory a boundary at the door: a task asking for more frames than are free is refused
before the model is consulted. It left the harder question open and said so: a machine that is
ALREADY under pressure must decide whose frames go. The register carried the answer's raw material
for weeks — a second forest, `memrisk`, trained on the eviction event specifically (REQ-ML-005:
PR-AUC 0.8818 at a 0.2561 base rate, cost-chosen threshold, integer/float agreement 1.000),
exported in the same `ALTM1` format under the same 20-column contract — and the honest words
"NOT CLAIMED AND NOT WIRED". This wave wires it the way ADR-056 wired the first forest: as an
ORDERING, never a verdict with authority, and proves the whole path against the machine's own
allocator under a real storm.

## Decision

### Three parties, three jobs, none shared

* **The allocator triggers.** A `MemoryMeter` reading (ADR-081) under the watermark is the only
  thing that opens a reclaim round. A reading that is not under pressure is refused
  `NotUnderPressure` by name and nothing moves; its `need` is zero. A round's NEED is the frame
  count that puts free frames back at `HEADROOM_FACTOR` (2) times the watermark share — leave the
  band with room, not by one frame.
* **The policy chooses.** Candidates are ranked by a TOTAL order: protected candidates are never
  chosen (the kernel's own frames, a task the caller shields) — skipped and COUNTED even when
  skipping them leaves the need unmet, which the round then reports as a SHORTFALL rather than
  hiding; among the rest, the forest's tier first, then the largest footprint (free the most with
  the fewest evictions), then the lowest priority, then the oldest submission, then the task id.
  Two rounds over the same inputs evict the same tasks in the same sequence.
* **The forest advises the tier.** `memrisk` predicts the EVICTION event. `Elevated` — the trace
  says this task would have been evicted anyway, so its work is the cheapest to lose — is tier 0.
  `Low` — a task likely to complete, whose frames taken now destroy work already done, the mistake
  the trainer priced at 4x — is tier 2. `Abstain` (inside the conformal band, outside the training
  box, degenerate input) and NO MODEL AT ALL are tier 1. A machine without the blob, or with one the
  loader refused BY NAME, ranks bit-identically to one whose forest abstains about everyone: the
  model changes the ORDER among candidates and never whether reclaim happens, how much is needed, or
  what protection means. The blob is verified by the SAME `RiskAdvisor::load` as the risk forest —
  no second loader, no second contract hash that could drift.

### Execution is a seam

`ReclaimOps::evict(task, owner) -> frames` is what a target does to a chosen task: terminate it
(ADR-042) and return its frames through the ownership table (ADR-030/032). The policy asks for it
exactly once per chosen task and counts what the seam SAYS came back, not what the candidate
claimed — an ops that returns fewer frames than promised makes the round keep evicting until the
need is met. The suite drives the seam with a recording mock; every target drives it against its
REAL allocator under a REAL storm.

### The storm

On every target, after the advisor suites: a storm owner (`Owner::address_space(199)`) takes frames
from the machine's own allocator one at a time until the meter is under the watermark; the reading
goes to the resident advisor (its pressure ledger counts the crossing, ADR-081); the reclaimer is
handed the storm as its one candidate and the real ops seam, which walks the ownership table and
returns every frame the owner holds; the free count after must equal the free count before EXACTLY,
and the frames reclaimed must equal the frames taken. `StormReport::holds` is the verdict —
pressure really entered, every frame back, the machine where it started — and a storm that took
nothing proves nothing. On the 128 MiB `virt` machines the storm takes roughly 25 800 frames; on the
256 MiB q35 roughly 58 000; the boot log prints the numbers the gates read.

### Host proofs

`kernel-core/tests/reclaim.rs` (7 tests): the boot suite host-run first; the need arithmetic at
every edge (zero at and above the watermark, exact shortfall to twice it below); a reclaimer without
a forest ranks like one whose forest abstains (degenerate zero vectors → `Unknown` for all, model
free and model loaded identical); a refused blob is named and the reclaimer still reclaims; the
policy counts what the seam returns, not what the candidate claimed (a stingy ops → 15 evictions to
cover a need of 1500 at 100 each); the ledger sums across rounds and refusals; a storm report holds
only when pressure was entered, every frame came back, and the machine returned to where it started.

### Boot gate

`reclaim_suite` (9 invariants, all three targets): the eviction-event forest verifies under the risk
forest's own loader and contract (shape read back, not asserted); a machine not under pressure
reclaims nothing — refused by name, counted; with nothing evictable the round is refused by name and
no frame moves; a protected task is never taken and the shortfall it causes is named; the largest
footprint goes first and the round stops the moment the need is met; below the tier the order is
footprint, priority, age, id — total; the forest sets the tier and nothing else (24 candidates with
ADR-056-derived vectors: tiers monotone along the ranking, the model-free order preserved within each
tier, every advised candidate accounted); the seam is asked once per chosen task in rank order and
the ledger sums exactly; the same pressure over the same tasks evicts the same tasks in the same
order. Then the storm line the gates require: `storm: pressure entered and cleared, every frame
back EXACTLY`. Boot fails 700+i on a suite invariant, 699 on a storm that did not come back. Marker
maps gain `reclaim=9` on the three QEMU gates.

### Conformance

Five behaviors join the cross-CPU contract (170 → 175): the forest verifies under the shared loader,
not-under-pressure reclaims nothing, nothing-evictable is refused by name, largest-footprint-first
stopping at the need, and determinism.

### Named non-claims, in the register

The reclaimer is not yet RESIDENT: it runs at boot (the suite, the storm) and is not consulted by a
running machine's allocator on its own pressure — the resident seam is the next rung, and with it
the choice of which live tasks are candidates and which are protected (today protection is a flag
the caller sets, not a capability). The forest's features are the candidate's SUBMISSION-time
vector; no run-time footprint signal reaches it, because the contract is frozen. A round evicts whole
tasks — no partial reclaim, no swap, no compression. The watermark and headroom are constants. The
storm's candidate carries a zero feature vector, so the forest abstains about it and the live path
exercises the policy, the seam and the allocator, not the forest's opinion — the opinion is
exercised by invariant 7 over ADR-056-shaped tasks. The unsafe audit is UNCHANGED.
