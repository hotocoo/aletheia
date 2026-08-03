# ADR-035 — A name is as atomic as a block: the filesystem namespace over the journal

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P2-018 · REQ-FS-001 · builds directly on ADR-024 (journaled store, REQ-STOR-002)
and ADR-023 (virtio-blk, REQ-DRV-003); the storage twin of ADR-033 (erase on free)

## Context

The storage stack had a correct middle and no top. ADR-024 gave a write-ahead journal: a set of
`(home_block, contents)` updates either all land or none do, proven by a crash sweep at every prefix.
ADR-023 gave a real driver implementing the same `BlockDevice` seam. What sat above them was nothing —
callers addressed **raw block numbers**. Every layer that wanted to keep something durable (an
installed component, a policy set, a content-addressed object, anything a user could name) would have
had to invent its own block bookkeeping, and each such invention is an independent chance to leave a
half-written state the journal was designed to prevent.

The failure mode this decision exists to prevent is specific: an allocation map and a directory that
disagree. A conventional filesystem writes metadata in several steps, so a crash can leave a name
pointing at blocks the free map calls free (later handed to a second object) or blocks marked
allocated that no name owns (leaked until a repair pass runs). `fsck` exists because that window
exists.

## Decision

**One flat namespace whose every mutation is exactly one journal transaction.**

1. **All durable state is on the device, in ordinary home blocks.** A directory block (fixed 64-byte
   slots: header, then `name` NUL-padded, first block, byte length, flags) and one allocation bitmap
   block, both immediately after the journal area; file data after them. `Filesystem` itself holds only
   the journal, so any mount of a device sees exactly what its last committed transaction left, and
   there is no cached state that can disagree with the disk.

2. **Create and remove are single transactions.** A create commits the data blocks **and** the updated
   bitmap **and** the updated directory together; a remove commits the zeroed data blocks, the cleared
   bitmap bits and the cleared slot together. Therefore the disagreement above is not merely unlikely,
   it is unrepresentable: recovery yields the state before the mutation or the state after it, and both
   satisfy "every allocated block belongs to exactly one live name". **There is no repair pass, because
   there is no inconsistent state to repair.**

3. **The transaction bound is a refusal, not a truncation.** The journal carries at most `MAX_ENTRIES`
   blocks; two slots are always the directory and the bitmap, so an object is bounded at
   `MAX_ENTRIES - 2` blocks and a larger one is refused with `TooLarge`. Silently writing a prefix
   would be the one outcome the whole design exists to exclude.

4. **Erase on delete.** A removed object's data blocks are written back as zeros *inside the same
   transaction* — the storage-layer twin of ADR-033. A block returned to the free map carries none of
   the bytes of the object that used to live there, so reusing it cannot disclose them.

5. **The namespace is a mechanism; authority stays in the capability engine.** There is deliberately no
   capability check inside the module. Authority is applied by wrapping the device in
   `device::DeviceGuard` (REQ-DRV-002), so the same `CapEngine::evaluate` that authorizes an entity
   write or an IPC send authorizes the I/O beneath a name. Adding a second, parallel authorization
   point inside the filesystem is exactly how a system ends up with two answers to one question.

6. **Every target proves the same behaviors, and one target proves them against hardware.** The suite
   (`fs::selftest_on`) runs against any `BlockDevice`: all three CPU targets run it over the RAM-disk
   device in their boot gate, and aarch64 additionally runs the identical twelve behaviors over the
   real virtio-blk device through the virtqueue. The twelve are part of the `conformance.sh` core
   contract, so a CPU on which a create is not atomic is a conformance failure, not a footnote.

7. **The crash proof is a device fault, not a host trick.** `fs::FaultDevice` fails every mutation after
   the first *n*, which expresses a crash as something the `BlockDevice` trait can already report. That
   is why the atomicity invariants run identically in-kernel on real hardware and on the host — where
   `tests/fs.rs` sweeps **every** prefix of a create and of a remove, asserting after each that the
   namespace is structurally sound and that an unrelated object was never collateral damage.

## Consequences

* **What it enables.** Anything above storage can now name what it keeps: component persistence, a
  durable policy set, a content-addressed store with stable handles. None of them need block
  arithmetic, and none of them can invent a new torn-write bug.
* **Cost.** Two extra journal slots per mutation (the directory and the bitmap), and a mutation is
  bounded at `MAX_ENTRIES - 2` data blocks. A create reads two metadata blocks first, so an operation
  is two reads plus one transaction.
* **Explicitly NOT claimed.** One flat namespace (no directories — `/` is refused in names *now* so
  today's names stay valid when a hierarchy exists); one bitmap block, so at most `8 * BLOCK_SIZE` data
  blocks; contiguous extents only, so a create can be refused with `NoSpace` on fragmentation while the
  total free count would have fit; no rename, no in-place update (an object is created whole or
  removed whole); no per-object integrity beyond the journal's commit checksum, so post-commit bit rot
  in a home block is not detected on read (the named follow-on from ADR-024); no encryption at rest;
  no quota, no timestamps, no permission bits — authority is capabilities, not mode bits.

## Alternatives considered

* **Extent lists / indirect blocks for large objects.** Rejected for now: an object spanning more
  blocks than one transaction can carry needs multi-transaction mutation, which reintroduces exactly
  the intermediate states this ADR removes. Doing it properly means a transaction-chaining design
  (each chain link committed and the chain head switched atomically); that is real work and gets its
  own ADR rather than being smuggled in behind a `Vec`.
* **A conventional inode table + free map with independent metadata writes.** Rejected: it is the
  design whose crash windows require `fsck`. The journal already gives multi-block atomicity; declining
  to use it for metadata would be choosing the repair pass on purpose.
* **Capability checks inside the filesystem.** Rejected — see decision 5. Two authorization points is
  a security boundary that can disagree with itself.
* **A log-structured store (append-only, compacting).** Rejected as premature: it buys crash safety we
  already have from the journal, and it costs a garbage collector plus a live-set scan — neither of
  which can be proved by a boot-time invariant suite today.
