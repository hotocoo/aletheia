# ADR-081: memory is a boundary the advisor cannot cross — the allocator decides admissibility, the forest advises order

**Status:** Accepted · **Date:** 2026-09-02 · **Advances:** REQ-ML-006 (new; the memory boundary on the resident advisor's admission path) · **Builds on:** ADR-056 (the frozen integer forest, advisory by construction, INV-014), ADR-063 (the machine's own heap meter), ADR-064 (the machine measures itself), ADR-042 (kill the task, keep the system)

## Context

The resident advisor (ADR-056) answers one question at admission — *is this task going to die if I
admit it?* — with an ordering hint, and every invariant holds identically with the forest loaded,
absent or wrong. What nothing on that path asked was the question that precedes it: *can this
task be admitted at all?* A task's requested frames were a FEATURE (a fraction of the machine's
capacity, in the trainer's fixed point) and never a BOUND; the kernel would admit a task asking
for more memory than the machine had free and let the forest opine about its risk. The goal's
"RAM consumption" asks for memory to reach the scheduling decision. This rung makes it reach the
decision the only honest way available today: as a hard boundary the allocator states and the
advisor cannot cross — not as a new forest feature, because the 20-column contract is frozen by
the trainer and hash-checked by `mlrisk_contract`, and a column the forest was never fitted on
would be a number wearing a feature's name.

## Decision

### The allocator is told, never estimated

`MemoryMeter { total_pages, free_pages }` is one reading of the machine's own frame allocator.
`RiskService::observe_memory` records it — or refuses it BY NAME (`MeterInvalid`, naming both
numbers) when it cannot be true: zero frames, or more free than exist. A refused reading records
nothing. A valid one updates the last reading, the low-water mark (the fewest free frames any
reading showed), the sample count, and the pressure ledger: a reading that ENTERS the band below
`LOW_WATERMARK_PERMILLE` (10 % of total) counts one pressure event; staying in the band counts
nothing; leaving and re-entering counts again. Every target reads its allocator
(`frames::free_count`/`total_count`) into the resident service BEFORE anything is admitted
through it, and prints the reading the door will judge by
(`[mlsched] memory: F of T frames free - bounded admission ON`).

### The bounded door

`RiskService::admit_bounded` is the admission path. It refuses `Unmetered` while no allocator has
reported — fail closed, never "unbounded until told" — and refuses `MemoryExhausted { requested,
free }` when the task asks for more frames than are free right now. Both refusals happen BEFORE
the model is consulted (the advice census does not move), without touching the scheduler (the
task has no state, nothing is dispatched) and without touching the feature history (a refused
task never existed; the exclusive-history discipline of ADR-056 is not asked to forget it). A
task asking for exactly the free count is admissible: the boundary is `<=`. Past the door, the
path is exactly ADR-056's `admit` — the same features, the same margins, the same tie-break.

The resident seam (`resident::admit`) IS the bounded door — there is still no second admission
path that bypasses it — and gains `resident::observe_memory` and `resident::meter`. The three
targets' real ring-3 tasks read the allocator immediately before each admission, so the door
judges a REAL task against the frames actually free at that instant; a refusal there is a kernel
out of memory for a two-page task, printed and failed, never routed around. Commissioning
(`commission`) sizes its synthetic workload against the machine's OWN free frames — from a
rounding error to every free frame — and reports how many arrivals the door refused; a target
whose allocator never reported would see every arrival refused and the boot exits 187 rather
than reporting a census of nothing.

### Observability

`AdviceStats` carries the ledger: samples, total, last free, low-water, `MemoryExhausted`
refusals, `Unmetered` refusals, pressure crossings, and whether the last reading was inside the
band. `mlstat` prints one line for it — `memory: F of T frames free (low-water L, N reading(s));
R admission(s) refused MemoryExhausted, P pressure crossing(s)` — or names the unmetered state
and how many admissions it refused. Unmetered is a state a human can read, not an absence.

### Host proofs

`kernel-core/tests/mlsched.rs` gains six: an unmetered service admits nothing through the
bounded door (refusals counted, model never consulted, scheduler and history empty); a reading
that is not a meter is refused and recorded nowhere; the boundary refuses by name and leaves the
scheduler and history untouched, with exactly-free admissible; the meter is a ledger with an
exact low-water mark and crossings counted once per entry (at the watermark is not below it);
the boundary follows the latest reading deterministically (two services, identical verdicts and
counters); and the resident seam carries the same door, with a 256-task commissioning refused
nothing under a reading that covers it.

### Boot gate

`mlsched_suite` grows 12 → 17, on all three targets: a reading that is not a meter is refused by
name and records nothing; with no allocator reading the bounded door admits nothing — fail
closed, counted; a task asking for more than is free is refused by name before the model is
asked, the scheduler untouched, and exactly-free is admitted and dispatched; the meter is a
ledger — low-water exact, a pressure crossing counted once per entry; the boundary follows the
latest reading and refuses deterministically. The three QEMU gates also require the allocator's
reading line and a commissioning refused nothing. Boot fails 190+i on a suite invariant, 187 on a
refused commissioning arrival. Marker maps: `mlsched=17` on the three QEMU gates.

### Conformance

Five behaviors join the cross-CPU contract (165 → 170): the invalid-meter refusal, the unmetered
fail-closed door, the by-name refusal with the scheduler untouched, the ledger, and the
deterministic latest-reading boundary.

### What this changes about the running machine

The live advisory path on every target is now bounded: before each real admission the target
reads its allocator, and the door judges against it. Commissioning's synthetic workload is sized
by the machine's free frames instead of by the suite machine's fixture, so its arrivals are
admissible on a 128 MiB `virt` as on a 256 MiB q35, and the "enormous" arrival that exercises
the range guard asks for every free frame the machine has. The unsafe audit is UNCHANGED: the
rung adds no unsafe site.

### Named non-claims, in the register

RAM pressure is not a forest feature: the 20-column contract is frozen and hash-checked, and
adding a column means retraining. The second forest trained on the eviction event (REQ-ML-005,
`memrisk`) stays UNWIRED — nothing reclaims frames on the advisor's opinion; that is the next
rung. The boundary is per-admission against the latest reading; it does not reserve frames, so
two admissible tasks admitted back to back may together exceed what was free (the allocator,
not the door, refuses the second's actual allocation — ADR-030's ownership model). The
watermark is a constant (10 %), not a tuned or learned threshold. The resident service is still
installed with the suite machine's capacity for feature normalisation, not the real machine's —
a fixture the ADR-056 wave chose and this one leaves in place, stated.
