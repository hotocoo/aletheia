# ADR-243 — The block cache, measured against the optimum

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-STOR-004, REQ-AI-017 (row 4 of ADR-237's wave order)
**Builds on:** ADR-232 (write-through block cache, CLOCK, 16 blocks), ADR-237 (decision 4.4: measure before any learned policy).

## Context

ADR-237 put a System-1 eviction policy for the block cache on the list, with a condition: measure
first, and only build a policy if it beats CLOCK on that number. The number that bounds every
policy, learned or not, is Belady's optimum: with the future known, which block to drop (or not to
admit). If CLOCK is close to it, no model can pay for itself.

## Measurement

`kernel-core/tests/bcache.rs` records the device read trace of ADR-232's namespace workload and
simulates CLOCK (checked equal to the real cache's misses), LRU and the optimum with bypass.

| trace | blocks | CLOCK | LRU | optimum |
|---|---|---|---|---|
| console namespace, 12 objects, 778 reads | 8 | 168 | 169 | 97 |
| | **16** (ADR-232) | **49** | 48 | 29 |
| | **32** | **23** | 23 | 23 |
| larger namespace, 48 objects, 892 reads | 16 | 249 | — | 164 |
| | 32 | 184 | 181 | 111 |

S3-FIFO (Yang et al., SOSP 2023), the strongest simple policy we know of, was simulated on the
same traces: 38 vs CLOCK's 49 at 16 blocks on the console trace, equal at 32, and 179 vs 184 at
32 on the larger namespace.

## Decision

* **Double the cache, keep CLOCK.** At 32 blocks (128 KiB) CLOCK misses exactly the optimum on
  the console trace: 23 misses, which are the trace's compulsory first reads. No policy has a miss
  left to save there, and the old size's whole gap to the optimum (20 misses, 2.6 % of reads) is
  gone for 64 KiB of memory. Device reads for the workload: 778 → 23 (97 % saved; was 94 %).
* **No learned eviction policy.** Where the working set does not fit (the larger namespace), the
  gap to the optimum is the requests' randomness: the workload picks objects uniformly, so the
  future is not predictable from the past, and S3-FIFO recovers 5 of 73 possible misses. A model
  would be paying for the same thing. The row is closed by measurement, not by building.
* Both comparisons are tests: at the shipped size CLOCK must equal the optimum on the console
  trace, and must miss no more than 60 % of reads on the larger one.

## What this is not

The traces are synthetic namespace workloads from one generator; a recorded trace from real use
might have the locality these lack, and the simulation functions are there to re-run on one.
The cache is still the console namespace's only; the boot suites' storage paths run uncached.

## Consequences

* ADR-237 row "Block cache" is closed: the cache is at the optimum for its workload, and the
  measurement that would justify a policy is in the tree.
