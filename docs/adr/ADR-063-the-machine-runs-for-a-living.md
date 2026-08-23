# ADR-063: The machine runs for a living

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P2-009 · **Builds on:** ADR-056 (reported-not-gated timing), ADR-061 (the gate counts itself)

## Context

Every suite before this one proves a subsystem CORRECT on hand-picked cases. None answers the
operator's next question: does it still hold after the machine has been RUNNING — committing,
naming, sharing, dispatching — for a very long time? That is soak testing, and it was an open
register row precisely because on Aletheia it is not just "run it longer":

The kernel heap is a bump allocator that never frees (`kernel/src/heap.rs`). A churn loop that
allocates per operation does not soak — it counts down the heap until the machine dies. On this
kernel, long-running is a RESOURCE property before it is a correctness property.

## Decision

`kernel-core/src/soak.rs` runs four lifecycle campaigns to a steady state, defined once and shared
by all three CPU targets and the host:

* **journal churn** — two-update transactions committed, payloads rewritten IN PLACE in a buffer
  allocated before the meter starts, home blocks verified by read-back every eighth transaction;
* **namespace churn** — create/replace/remove cycles over a small live set of names, with the full
  structural contract (unique names, disjoint in-bounds extents, bitmap/directory tally) audited
  after EVERY mutation, contents verified byte-for-byte at every touch, and a fresh mount that must
  see exactly the survivors;
* **grant churn** — share/attenuate/write/read/revoke cycles over fixed regions: zero-copy observed
  every cycle, refcounts exact every cycle, and the refused paths (unauthorized, amplifying,
  revoked) attempted and counted at volume;
* **task generations** — both arch-independent schedulers driven generation after generation: a
  Finished task never dispatched again, Blocked tasks never dispatched, every priority drain
  exactly-once, unknown-id events changing nothing.

The load is deterministic (one fixed SplitMix64 seed; message placement and bytes are drawn from
it), so the suite's final check re-runs the whole campaign and requires identical checksums and
censuses.

What GATES the boot is only what holds at any scale, plus one measured claim the kernel alone can
make:

1. **Journal churn allocates nothing per transaction** — the target's own heap meter, read at the
   window's edges, must show EXACTLY zero. This is the load-bearing property that makes
   long-running possible on a never-freeing heap. A target that cannot meter its heap is UNPROVEN
   here, never silently exempt. Recovery rounds sit outside the window BY DESIGN (recovery's
   replay payload allocates) and are gated separately as idempotent-replay-mid-soak.
2-11. The scale-free properties above, stated once, proved at 384 transactions and at 50 000 alike.
12. The same seed replays the identical campaign.

What is REPORTED, never gated: transactions/second, namespace ops/second, per-phase nanoseconds —
QEMU-TCG nanoseconds are an emulator's numbers (ADR-056's rule). Each target also prints its heap
before and after the whole campaign, so the suite's own cost stays a measured fact rather than a
hope.

The boot load (`BOOT_LOAD`) is sized against the tightest constraint — a TCG-emulated riscv64
inside a 120 s watchdog, running the campaign TWICE on a bump heap. The hosted test
(`kernel-core/tests/soak.rs`) takes the same harness to loads tens to hundreds of times larger,
where long-running means what it means everywhere else.

## Consequences

* Three arch-neutral behaviors join the conformance contract (126 → 129): allocation-free churn,
  revocation-refused-by-name, Finished-never-runs-again — because repetition is where an
  arch-specific shortcut would eventually diverge.
* All three VM gates hold a new `soak=12` family in their marker maps (ADR-061); a soak failure
  exits `400+i`, a window no other suite uses.
* Scope stated: single-core policy modules only (no SMP contention soak — ALET-P1-014 remains
  open); the grant table's records accumulate by declared constant, not forever (a real kernel
  would free them with its heap); wall-clock soak duration is bounded by the boot watchdog, so
  "long-running" here means VOLUME under repetition, with true elapsed-time soaks belonging to a
  deployment with hours, not seconds.
