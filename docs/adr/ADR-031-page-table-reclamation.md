# ADR-031 — An unmap gives the page tables back: reclaiming emptied translation structure

**Status:** Accepted (2026-08-02)
**Context:** GAPS4 ALET-P1-002 · REQ-MM-003 · builds on ADR-030 (frame ownership), extends ADR-029
(mapping-API admission check), ADR-019 (HAL / multi-target)

## Context

Mapping one page in a fresh region allocates a chain of translation tables: on the 3-level aarch64
TTBR0 and RISC-V Sv39 walks an L2 and an L3; on x86-64's 4-level walk a PDPT, a PD and a PT.
Unmapping that page cleared the leaf entry and stopped. The intermediate tables stayed allocated
**and stayed referenced**.

At boot-test scale that was a bounded leak, documented as such. As a property of a running OS it is
not acceptable:

* A task that maps and unmaps across a wide virtual range permanently consumes one frame per
  512-page span it has ever touched. The pool drains in proportion to addresses **visited**, not
  pages **held** — a denial of service any unprivileged task can drive with a loop.
* An address space can never be fully torn down, because nothing knows which tables belong to it.
  ALET-P1-004 (address-space destruction) is blocked on this.
* The leaked tables remain *reachable* through their parent entries. They are not merely unused
  memory; they are live translation structure for a region with no mappings, which a later bug or a
  stale walk-cache entry can still follow.

## Decision

**An unmap that empties a table frees that table, and the rule is written once.**

1. **Shared policy, per-target seam.** `kernel-core/src/ptreclaim.rs` owns the rules; each target
   implements a small `TableOps` (read an entry, write an entry, test the present bit, hand a frame
   back). The only architectural knowledge in a target's implementation is the present bit and the
   entry width — the same split by which `kernel-core::sched` owns scheduling policy while each
   target owns the context switch.

2. **Five rules, each preventing a specific corruption.**
   * A table is freed only when **every** entry is absent — a sibling mapping keeps it.
   * The parent reference is cleared **before** the frame is freed. Free-then-clear leaves a window
     where a live entry points at a frame the allocator may already have re-handed-out.
   * The **root is never freed**: it is the address space's identity, owned by its creator.
   * Reclamation stops at the first table still in use; every ancestor holds the entry pointing at
     it, so no ancestor can be empty.
   * A refused free (the ownership model saying "not a page-table frame you hold") **restores** the
     parent entry and reports failure — a refusal must never leave a table unreachable-but-allocated.

3. **Reclamation is ownership-checked.** Tables are freed as `Owner::PAGETABLE` through the
   allocator from ADR-030, so an attempt to reclaim a user page, a frame belonging to another
   address space, or an already-free frame is refused rather than obeyed. This ADR is only safe
   because ADR-030 landed first.

4. **Invalidation stays with the target.** Clearing a parent entry can leave stale
   paging-structure (walk) cache entries, and the flushing instruction is architectural
   (`tlbi vae1`, `sfence.vma`, `invlpg`). Each target calls reclamation BEFORE the invalidation it
   already performed for the unmapped VA, so one flush covers the leaf and its detached ancestors.
   On x86-64 the walk path is captured *before* `Mapper::unmap` runs, while the chain is intact.

5. **The behavior — not the level count — is the cross-architecture contract.** A 3-level walk
   reclaims two tables where a 4-level walk reclaims three; that is an honest architectural
   difference. `scripts/conformance.sh` therefore requires the five *behaviors* from all three
   targets (a sibling protects the tables; nothing is returned while the leaf table is in use; the
   emptied chain comes back to the allocator; neither VA resolves afterwards; the address space
   rebuilds the chain with its root intact) and lets the counts differ.

## Consequences

* **Cost.** One scan of the emptied table (512 entries) per level actually freed, on the unmap path
  only. No allocation, no new locking. A table that is still in use costs one scan and stops.
* **What it enables.** Address-space teardown (ALET-P1-004) now has both halves it needs: ownership
  tags say which frames belong to a space, and reclamation is the operation that returns tables.
* **What it does NOT do.** It does not free the tables of a *dying* address space (that is P1-004:
  reclamation only triggers on an unmap that empties a table), does not zero reclaimed frames
  (ALET-P2-026), and does not enforce W^X (ALET-P1-007). Those stay `open` in the GAPS4 register.
* **Proof obligation.** Host tests against an in-memory page-table model
  (`kernel-core/src/ptreclaim.rs` unit tests): whole-chain reclaim, root never freed, sibling in the
  leaf table, sibling leaf table under a shared parent, a refused free restoring the reference, a
  refusal partway up leaving what was already freed consistent, and a too-short path. Each target
  then proves its own allocator and tables are wired to the model in its VM gate (aarch64 and
  RISC-V 33 virtual-memory invariants, x86-64 25).

## Alternatives considered

* **Reclaim lazily, in a background sweep.** Rejected: it needs a reachability scan of the whole
  hierarchy, and it leaves the DoS window open for as long as the sweep interval.
* **Reference-count each table.** A count per table is smaller than a scan, but it is a second
  source of truth that can drift from the entries themselves; the entries ARE the truth, and 512
  loads on an unmap that empties a table is not a hot path.
* **Free the table and then clear the parent.** Rejected outright — that is the window in rule 2,
  and it is exactly the aliasing failure ADR-030 exists to prevent.
