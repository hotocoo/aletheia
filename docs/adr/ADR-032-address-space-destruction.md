# ADR-032 — A dying address space gives everything back: destruction as a bounded operation

**Status:** Accepted (2026-08-02)
**Context:** GAPS4 ALET-P1-004 · REQ-MM-004 · builds on ADR-030 (frame ownership) and ADR-031
(page-table reclamation), extends ADR-019 (HAL / multi-target)

## Context

ADR-031 reclaims the tables an *unmap* empties. That serves a task that tidies up page by page. It
does nothing for a task that simply **dies** — faults, is killed, or exits without unmapping.
Everything such a space held stayed allocated forever: its user pages, every intermediate table, and
its root.

An OS in which process death leaks memory has no process lifetime worth the name: the pool only ever
shrinks, and a crash loop is a slow, unattributable exhaustion. Destruction had to become a bounded,
provable operation before anything above the kernel could treat tasks as disposable.

The difficulty is not walking the tree. It is that **a page-table tree is not a private forest**:

* **Shared kernel structure.** x86-64 builds a per-process PML4 by *copying* the live one, so almost
  every top-level slot points at firmware and kernel tables that other spaces — and the running
  kernel — depend on. A naive recursive free takes the machine down on the first teardown.
* **Block/huge leaves that are not pool frames.** aarch64 and RISC-V per-process roots carry an
  identity map built from 2 MiB block / megapage descriptors over RAM and MMIO. Those addresses were
  never handed out by the frame allocator.
* **Shared pages.** A frame passed to another endpoint through the grant table (REQ-IPC-008) is
  mapped in this space but not owned by it.

## Decision

**Destroy exactly what the space owns, behind two independent guards, and never the space you are
running in.**

1. **One arch-independent walk.** `kernel-core/src/teardown.rs` owns the traversal and the rules;
   each target implements `SpaceOps` (level count, leaf test, entry address, privacy predicate, leaf
   free) on top of the `TableOps` it already wrote for ADR-031.

2. **Guard one — privacy.** `is_private(level, index)` lets a target declare which slots are its
   space's own. Teardown neither descends into nor frees anything else. x86-64 scopes the walk to
   PML4 slot 0's privatized PDPT and, within it, the single 1 GiB user region. The QEMU `virt`
   targets, whose per-process trees are built whole by `build_identity`, declare every slot private.

3. **Guard two — ownership.** Every free goes through the ownership model (ADR-030): leaves are
   freed as `Owner::USER`, tables as `Owner::PAGETABLE`. A block descriptor over RAM, a device
   mapping, or a granted page is refused by the allocator and counted as **skipped**. The guards are
   independent on purpose: if the privacy predicate were wrong, ownership still refuses; if a frame
   is shared, ownership still refuses.

4. **Depth-first, entries cleared first, root last.** A table's children are freed before the table,
   and the root after everything it referenced, so no freed frame is ever still reachable. Each entry
   is zeroed before its target is freed. Unlike ADR-031 there is no restore-on-refusal: the space is
   being destroyed, so a cleared entry to a frame we could not free is the correct end state — the
   frame stays with whoever owns it and nothing dangles.

5. **Refusals are counted, not swallowed.** `Teardown { tables_freed, leaves_freed, leaves_skipped,
   tables_refused }`. A non-zero `tables_refused` means the tree contained a table this space did not
   own — surfaced as data rather than ignored.

6. **Destroying the active space is refused.** Each target's `destroy_space` returns `None` when the
   root is the one the CPU is currently translating through. That is a target-level check because
   only the target can read TTBR0/satp/CR3, and it is part of the conformance contract: no
   architecture may allow the running kernel to free the ground beneath it.

## Consequences

* **Cost.** One walk of the private region on death — 512 entry reads per table visited. No
  allocation, no new locking, nothing on the mapping fast path.
* **What it completes.** With ADR-029 (address admission), ADR-030 (ownership) and ADR-031
  (reclamation), physical memory is now conserved across the full lifetime of an address space:
  frames cannot be aliased, double-freed, leaked by unmapping, or leaked by dying.
* **What it does NOT do.** It does not zero reclaimed frames before reuse (ALET-P2-026, still
  **open** — a freed page's bytes are still readable by the next owner), does not enforce W^X
  (ALET-P1-007), and does not itself terminate a task: it is the memory half of process teardown,
  not the scheduler half.
* **Proof obligation.** Host tests against an in-memory model: full return of pages/tables/root in
  child-before-parent order, unowned memory skipped rather than freed, shared kernel slots neither
  walked nor freed nor modified, a refused table surfaced with no dangling reference, an empty space
  still returning its root, and an upper-level huge leaf freed without being walked as a table. Each
  target then proves it on live hierarchies in its VM gate (aarch64 and RISC-V 42 virtual-memory
  invariants, x86-64 33), including that the frame count returns EXACTLY to its pre-space value.

## Alternatives considered

* **Free the whole tree recursively without a privacy predicate.** Rejected: on x86-64 that frees
  the kernel's own tables on the first teardown. Ownership alone would refuse most of it, but relying
  on a second guard to cover a wrong first guard is not a design.
* **Track every allocation per space in a side list.** More memory, and it can drift from the tree
  that actually exists; the tree is the truth, and ownership tags already say who holds each frame.
* **Free lazily, from an idle sweep.** Rejected for the same reason as in ADR-031: it leaves the
  exhaustion window open and needs a reachability scan to be safe.
