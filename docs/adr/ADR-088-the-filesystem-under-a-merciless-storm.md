# ADR-088 — The filesystem under a merciless storm

* Status: accepted
* Date: 2026-09-03
* Register: ALET-P2-018 (filesystem), ALET-P2-009 (soak), REQ-QUAL-007, REQ-FS-001, REQ-STOR-003
* Extends ADR-035 (the named-object namespace), ADR-062 (the scripted adversary),
  ADR-086 / ADR-087 (the storm discipline), ADR-063 (the boot heap never frees)

## Context

Two storms had already found per-event allocations the machine could not afford: the desktop's
(ADR-086) and the scheduler's (ADR-087). The filesystem is the third hot path a real machine
hammers — every console `write`, every component that persists anything, every transaction — and
it was by far the worst offender.

Measured before this wave: **16 498 bytes per write**. A twelve-megabyte kernel heap that never
frees survives about seven hundred writes; an interactive session that saves files would kill the
machine, and nothing in the tree would have said why.

Two causes, both invisible without measuring:

* Every transaction built a fresh `Vec<(usize, [u8; BLOCK_SIZE])>` — a vector of whole 4 KiB
  blocks — and dropped it. Three blocks for a 64-byte object is over twelve kilobytes.
* Every directory lookup called `decode`, which builds an owning `DirEntry` with a `String`, for
  each of up to `MAX_FILES` slots scanned. A lookup that allocates is a write that costs memory.

## Decision

1. **A resident staging buffer.** `Filesystem` owns one `Vec<(usize, [u8; BLOCK_SIZE])>`, cleared
   and refilled by every commit. It starts EMPTY rather than reserving the journal's 64-entry
   ceiling — that reservation is 262 KiB per mount, and a boot that mounts a dozen namespaces
   would pay for transactions it never runs. It settles at the largest transaction that
   filesystem actually commits, and never grows again.
2. **In-place directory reads.** `slot_is` compares a name against the raw slot bytes,
   `slot_used` tests the flag, and `slot_extent` returns (start, len) — no `String`, no
   `DirEntry`. `decode` stays exactly as it was for `list` and `stat`, where the caller keeps the
   answer and an owning type is the right shape.
3. **A storm that would have caught it.** `kernel-core/src/fsstorm.rs` is a boot suite on all
   three CPUs, measured on the platform's own heap:
   * five hundred writes in the steady state allocate NOTHING;
   * two thousand create/remove cycles leak no block and lose no slot (free-block count and
     directory population return exactly to their starting values);
   * every removed object's blocks read back ZERO — erase-on-delete (ADR-033's storage twin) at
     volume, not in one sampled case;
   * a fault placed at EVERY position of a commit (the ADR-062 adversary) leaves the object
     wholly old or wholly new, never a mixture;
   * the same storm twice leaves the namespace byte-for-byte identical.

Measured after: **0 bytes per write**, and the boot's total heap use at the desktop storm fell
from 7.55 MB to 6.81 MB — the old cost was being paid by every suite that touched storage.

## Consequences

* New boot-gate family on all three targets: `[fsstorm] ALL 5 FILESYSTEM-STORM INVARIANTS HOLD`
  (marker `fsstorm=5`, boot fails `780 + i`), with the heap measurement printed beside it.
* Five new cross-CPU conformance behaviours.
* The storm threads ONE 96-block device through every claim and reformats it, because a suite
  that allocated a fresh two-megabyte device per claim would be the disease it exists to catch.
* `Filesystem::read` still hands the caller an owned `Vec` — that is the caller's bytes, named
  rather than hidden.

## Named non-claims

* **Mounting still allocates** (the staging buffer, on first write). Mounts are a lifecycle
  operation, not a hot path.
* **No fragmentation or compaction claim.** The allocator is a first-fit bitmap; the storm proves
  blocks are not LEAKED, not that they are packed well.
* The fault sweep walks eight commit positions, not every write in a large transaction: enough to
  cover the journal's protocol pivots, and bounded so an emulated boot still finishes.
