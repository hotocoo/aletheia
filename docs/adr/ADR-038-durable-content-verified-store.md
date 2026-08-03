# ADR-038 — The OS remembers: a durable store whose every byte is verified

**Status:** Accepted (2026-08-03)
**Context:** REQ-STOR-003 · builds on ADR-035 (namespace) and ADR-036/037 (real devices on all three
targets); the storage half of what ALET-P2-018 opened. Requires the atomic `replace` introduced here.

## Context

Everything below this decision was per-boot. The journal made a multi-block write atomic (ADR-024), the
namespace gave durable objects names (ADR-035), and the drivers put both on real hardware on every
target (ADR-036/037) — but the capability-secure **spine itself** was rebuilt in RAM at every boot. Not
one entity, not one recorded event, nothing Aletheia knew survived a power cycle. An operating system
that forgets everything at reset is a demo of an operating system.

Two things had to be true before this could be built honestly, and one of them did not exist yet.
Saving a store means *updating* an object, and the namespace could only create or remove. "Remove then
create" is **two** transactions, so a crash between them leaves the name **gone** — data loss where the
object was merely being updated. That is a worse failure than the torn write the journal was built to
prevent.

## Decision

**1. `Filesystem::replace` — an update is ONE transaction.** The new data blocks, the old blocks zeroed
(erase on delete, ADR-033), the bitmap and the directory commit together. A crash leaves the old
contents or the new ones; the name is *continuously present*. The transaction bound is correspondingly
tighter — old blocks + new blocks + 2 must fit `MAX_ENTRIES` — and is refused (`TooLarge`), never split.
Proved by a host sweep over **every** crash prefix for five size transitions (same-size, grow, shrink,
from empty, to empty), each asserting the name never disappears and the contents are never a mixture.

**2. The store is one object, saved atomically.** `persist::save` encodes the whole store and writes it
through `replace`. Rewriting everything is a deliberate simplicity: an incremental log would need its
own compaction and crash story, and the store is bounded by one transaction anyway.

**3. A load VERIFIES; it does not merely parse.** Two independent checks, in this order:

* **Per entity, the content address.** Each entity carries the `content_hash` the spine computed. On
  load the hash is recomputed from the bytes actually read. This is what a content-addressed store is
  *for*, finally applied to the medium.
* **Per record, a trailing checksum** over every preceding byte.

The second check exists because writing the first one's test found a real hole. A byte-flip sweep over
the encoded record — flip one bit in each byte in turn — showed that flipping an entity's **id**
produced a store that loaded *successfully with different data*. The content hash covers content; it
says nothing about id, version, chain, provenance, type or the deleted flag. Silent corruption of
metadata was possible, and the sweep is what caught it. Now every byte is load-bearing, and the sweep
asserts exactly that: any flip is either refused or yields identical data, and most flips are refused.

The order is deliberate: the content check runs first, so damage to *content* reports the precise
`ContentHashMismatch` rather than the coarser record failure.

**4. A corrupt store is a refusal, not a reset.** `open_and_witness` never replaces a store it cannot
verify with a fresh one. "Your data is damaged" must not silently become "your data is gone".

**5. Ids never repeat across a reboot.** `next_id` is part of the record, and `Store::restore` continues
the sequence past the highest id it read.

**6. Capabilities are deliberately NOT persisted.** What a capability's lifetime means across a reboot
is an open question (ALET-P1-026) — a token minted before a power cycle, referring to an engine that no
longer exists, is not authority, and *making* it authority by writing it to disk would be inventing
durable privilege by accident. Entities persist; authority is re-established by whoever holds it.

**7. The cross-reboot contract is one shared function.** `open_and_witness` mounts (formatting a blank
medium), loads, records a witness entity for this boot, and saves — so boot 1 creates the store and
boot 2 on the same medium must *find and verify* boot 1's entities and report boot number 2. All three
targets run the same nine-behavior suite, and `conformance.sh` requires every one of them: whether your
data is intact must not depend on the CPU.

## Consequences

* **State survives.** The spine can carry entities across a power cycle, verified on the way back in.
* **Every write path in the system is now atomic at the same granularity** — block (journal), name
  (create/remove), update (replace), store (save) — and each is crash-swept at every prefix on the host.
* **Cost.** A save rewrites the whole store and is bounded by one transaction (~248 KiB of record).
  Growth beyond that needs the transaction-chaining design ADR-035 already named, not a bigger buffer.
* **Not claimed:** no encryption at rest at this layer (ALET-P1-028/029), no incremental/append save, no
  event-log persistence, no schema migration beyond refusing an unknown version, and FNV-1a is an
  integrity check against rot and bugs — **not** a defence against deliberate forgery, which needs the
  signing hierarchy of REQ-BOOT-002.
* **An interplay worth knowing:** while a committed transaction is still pending, a mount *replays* it
  and thereby repairs a damaged home block. The corruption tests must therefore quiesce the journal
  first to exercise the verification path — the repair is correct behavior, and it masks the check.

## Alternatives considered

* **Keep "remove then create" and accept the window.** Rejected: it converts an update into possible
  data loss, at exactly the moment (a crash) when the user most needs the old value.
* **Trust the journal alone and skip the content re-verification.** Rejected: the journal protects a
  write from being *torn*; it says nothing about a byte that changed afterwards. Content addressing is
  already in the design — declining to check it on read would be carrying the cost without the benefit.
* **Checksum only, drop the per-entity hash.** Rejected: the per-entity check localizes the damage and
  keeps the store's own identity model (content addressing) meaningful rather than decorative.
* **Persist capabilities too, so authority survives.** Rejected — see decision 6. That is a security
  model change; it needs ALET-P1-026 answered first, not a serializer.
* **Reformat when the store fails to verify, so the system always boots.** Rejected: it turns a
  detectable, reportable fault into silent, total data loss. Refusing is the honest failure.
