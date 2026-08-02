# ADR-033 — A freed frame carries nothing: erase on release, not on allocation

**Status:** Accepted (2026-08-02)
**Context:** GAPS4 ALET-P2-026 · REQ-MM-005 · completes ADR-030 (frame ownership) alongside ADR-031
(reclamation) and ADR-032 (destruction)

## Context

ADR-030 made frame ownership explicit, so two owners can never hold one frame *at the same time*.
That is a temporal guarantee, and it is silent about the thing an attacker actually wants: what the
**next** owner can read.

Until this decision, a frame returned to the pool kept its bytes verbatim. A task that held keys,
plaintext message bodies, decrypted store content, or another process's IPC payload released a page
whose contents survived intact until something happened to overwrite them — and the pool is LIFO, so
the very next `alloc`, in any address space, for any task, was usually that exact frame. Every path
that returns memory fed it: an explicit free, page-table reclamation (ADR-031), and address-space
destruction (ADR-032), which is precisely the path a *crashing* task takes.

`alloc_zeroed` existed, but it is the wrong place for a guarantee: it protects only callers who
remember to ask, and the caller who reads stale data is by definition the one who did not.

## Decision

**Erase a frame when it is released, not when it is allocated.**

1. **At release, after the ownership check.** Each target's `free_as` zeroes the whole 4 KiB frame
   once `FrameOwnerTable::release` has confirmed the caller really held it, and before the free-list
   link word is written. A refused free erases nothing — it is still a total no-op.

2. **Unconditional by construction.** Because the erase sits in the single choke point every return
   path goes through, reclamation and teardown inherit it without knowing it exists, and no caller
   can opt out by using plain `alloc`.

3. **The guarantee is stated precisely: no frame ever carries a previous OWNER's bytes.** It is not
   "every allocation returns zeros". A frame that has never been owned still holds whatever the
   firmware or the boot loader left there, because the allocator has never had a reason to touch it.
   That is pre-boot memory, not another task's data, and `alloc_zeroed` remains available for callers
   that need a guaranteed-blank page (page tables demand it) — it is kept deliberately, not by
   oversight.

4. **The proof is the reuse case, not the API.** Each target's VM gate writes a recognizable pattern
   across a frame, frees it, allocates again, asserts the returned frame is the same one (LIFO), and
   requires every word past the free-list link to be zero. Asserting that `alloc_zeroed` returns
   zeros would prove nothing about what a plain `alloc` hands the next task.

5. **It is part of the cross-architecture contract.** `scripts/conformance.sh` requires the erase
   behavior from all three targets: a CPU on which a reused frame still holds the last owner's bytes
   is a cross-task information leak, whatever its instruction set.

## Consequences

* **Cost.** One 4 KiB store per frame freed — on the free path only, never on map, unmap, or IPC.
  Bulk paths (teardown of a large space) pay it per frame, which is the point: that is exactly when
  a dying task's pages re-enter circulation.
* **What it closes.** The information-disclosure half of the memory model. With ADR-029..033, a
  frame cannot be aliased, double-freed, leaked by unmapping, leaked by dying, **or read by its next
  owner**.
* **What it does NOT do.** It does not zero *at boot*, so a never-owned frame may hold firmware
  bytes (stated above, deliberately). It does not clear caches or CPU registers, does not defend
  against a physical attacker reading DRAM, and it is not a mitigation for a live shared mapping —
  sharing is governed by the grant table (REQ-IPC-008).

## Alternatives considered

* **Zero on allocation instead.** Rejected: it only protects the caller who asks, leaves the stale
  bytes sitting in the pool in the meantime, and pays the same cost in the common case anyway.
* **Zero lazily in an idle sweep.** Rejected: it leaves an unbounded window in which the data is
  both free and readable, and the LIFO pool makes that window the *most* likely one to be reused.
* **Zero only frames tagged `USER`.** Rejected as a false economy — kernel and page-table frames hold
  capability state and translation structure, which is precisely what should not leak into a user
  page. The uniform rule is also the one that survives a future owner tag nobody has invented yet.
