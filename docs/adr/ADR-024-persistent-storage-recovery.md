# ADR-024: Persistent storage, object store, and recovery

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

The content-addressed semantic store needs a real OS storage substrate underneath it: a bootable
image + a hosted durable store are not general-purpose persistent OS storage (gap-register Issue 6).
The semantic store (ADR-005) is already content-addressed, versioned, and encrypted at rest — a
natural fit for a copy-on-write object store.

## Decision

Build the storage stack bottom-up, keeping the semantic store as the top layer:

```text
Physical Disk → Block Device (ADR-023) → Storage Driver → Partitioning
→ Object Store (content-addressed, CoW) → Encrypted Layer (ADR-005) → Semantic Store → World Model
```

**Phase 1 — object store design.** Content-addressed CoW: an object is named by its hash (matches the
semantic store's addressing), never overwritten in place; a new root commit atomically flips after all
children are durable. Crash consistency by construction — an interrupted write leaves the previous
committed root intact (no torn state). Integrity = the address IS the checksum; corruption is
detectable on read (hash mismatch).

**Phase 2 — durability + recovery.** Journaling/CoW commit; snapshots (a root is a snapshot); rollback
to any prior root; anti-downgrade tie-in with ADR-025. Encryption-key lifecycle bound to secure key
storage (TPM/enclave where available). Recovery tooling: mount last-known-good root; a fsck-equivalent
that verifies reachable objects' hashes.

**Phase 3 — atomic transactions.** Multi-object atomic commit so a semantic-store version bump + its
event-log append land together or not at all — the durable analogue of the in-memory pipeline's
all-or-nothing record step.

## Consequences

- State persists across reboots; interrupted writes never corrupt committed state (CoW roots).
- Corruption is detectable (content addressing) and recoverable (last-known-good root).
- Hardware-bound (needs ADR-023's `BlockDevice`); stays `deferred` (`REQ-STOR-001`). The CoW object
  model can be prototyped + tested on the hosted store first, over a file-backed block device, before
  any real driver — the honest first slice when this is picked up.
