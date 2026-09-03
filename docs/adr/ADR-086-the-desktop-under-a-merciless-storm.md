# ADR-086 — The desktop under a merciless storm

* Status: accepted
* Date: 2026-09-03
* Register: ALET-P2-021 (graphics / compositor), ALET-P2-009 (soak), REQ-QUAL-007, REQ-GFX-009
* Extends ADR-084 (windows are a managed set), ADR-085 (one desktop, three CPUs),
  ADR-063 (the boot heap never frees)

## Context

Every window invariant so far was proved on a handful of events: one press, one drag, one close.
That proves the RULES. It says nothing about the machine at the ten-thousandth event — when a
queue nobody drained is full, a window has been opened and closed a thousand times, and a heap
that never frees (ADR-063) has had every chance to grow by a few bytes per event, forever.

An OS that is "fastest and most secure" has to be judged at volume, on its own hardware, by its
own numbers. So the storm is a BOOT suite, not a host benchmark: it runs on all three CPUs and
measures the platform's own heap.

## Decision

`kernel-core/src/wmstorm.rs` storms the composition + input + window stack with a deterministic
pseudo-random flood (a plain 64-bit LCG — nothing here needs entropy, it needs a stream that is
identical on every CPU) and holds it to what a desktop must actually do:

1. **Lifecycles close.** A thousand open/close cycles return the compositor to EXACTLY its
   starting surface count, placement count and window set, with zero refusals.
2. **Backlog is bounded and HONEST.** A window that stops draining fills to exactly
   `MAX_INPUT_EVENTS`; every further event is refused `Backlogged` AND counted, and the drop
   ledger equals the arithmetic (`flood - cap`), to the event.
3. **A drain restores capacity exactly** — `MAX_INPUT_EVENTS` more fit, and not one more.
4. **The steady state allocates NOTHING.** After a warm-up round, four thousand pointer events
   must not move the caller's own heap watermark. The platform passes `used_bytes()`; the boot
   log prints both numbers, pass or fail.
5. **A settled desktop is QUIET.** One frame repaints what the storm damaged; the next writes
   zero pixels and the damage ledger is empty.
6. **The same storm lands bit-identically** — z-order, placements, focus, the manager's ledger
   and the frame's own cost.

**What the storm found, and what changed because of it.** Claim 4 failed on the first run: the
window manager asked `Compositor::z_order()` on every pointer event, which builds a `Vec`, and
the live desktop's pump called `drain_input`, which builds another `Vec` every tick. On a heap
that never frees those are leaks with polite names. Both paths are now allocation-free —
`placed_len`/`placed_at` walk the compositor's own placement table in place, and `pop_input`
takes one event under the same owner-token authority `drain_input` enforces. The measured result
on x86-64: `heap watermark across the storm: 7550448 -> 7550448 bytes (0 moved)`.

## Consequences

* New boot-gate family on all three targets: `[wmstorm] ALL 6 WINDOW-STORM INVARIANTS HOLD`
  (marker `wmstorm=6`, boot fails `740 + i`), plus a printed heap measurement.
* Five new cross-CPU conformance behaviours.
* `kernel-core/tests/wmstorm.rs` runs the same suite on the host under a counting allocator that
  IGNORES frees — because the kernel heap cannot give bytes back, and a net-bytes measurement
  would let a doubling `Vec` through.
* The live pump drains with `pop_input`, so what the machine runs is what the storm proved.

## Named non-claims

* **Opening a window allocates, by design** — its pixels and its queue. The storm's allocation
  round therefore does not press close boxes; window lifecycles are claim 1's business, and on a
  never-freeing heap a user who opens and closes windows without end still grows the heap. That
  is ADR-063's posture, named here rather than hidden by a suite that avoids it.
* **The storm is arch-neutral and device-free.** It storms the model every CPU runs, not the
  virtio-input path (ADR-080's suites) and not the scanout (ADR-078's).
* No thread contention: this kernel runs one CPU at a time by construction (the SMP suite parks
  its APs), so the storm proves volume, not concurrency.
