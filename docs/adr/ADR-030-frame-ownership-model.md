# ADR-030 — A frame has an owner: physical-memory ownership as a kernel invariant

**Status:** Accepted (2026-08-02)
**Context:** GAPS4 ALET-P1-003 · REQ-MM-002 · extends ADR-029 (mapping-API admission check), ADR-019
(HAL / multi-target), ADR-012 (validation pyramid)

## Context

All three Aletheia targets run the same physical frame allocator: an intrusive LIFO free-list where
each free frame stores the next free frame's address in its own first word. It is fast, needs no
side table, and works before the MMU is on. It also cannot answer the only question that matters
when a frame comes back: *who was holding this?*

Before this decision, `free(f)` checked exactly two things — is `f` 4 KiB-aligned, and is it inside
the managed window. Both are true of a frame that is **already on the free list**, and of a frame
that is **currently live in another address space**. That admits three defects, none of which fault:

1. **Double free → aliasing.** Freeing the same frame twice pushes it onto the list twice. The list
   now reaches that frame by two paths, and two later `alloc` calls hand ONE physical page to two
   different owners — typically a page table and a user page over one another.
2. **Free of a frame that was never allocated.** Any aligned in-window address is accepted, so a
   caller can donate a live page belonging to someone else to the next allocation.
3. **Use after free.** A frame handle is `Copy`. Freeing one copy leaves every other copy looking
   like a valid handle, and the allocator has no way to distinguish them.

ADR-029 closed the *address admission* half of this problem: a `pa` outside the allocator's window
is refused at every mapping API. It deliberately stopped there — being inside the window proves the
kernel owns the memory, not that *this caller* may use or return *this frame*. Ownership is the
missing half, and it is also the precondition for the two reclamation findings that follow it:
page-table reclamation (ALET-P1-002) and address-space destruction (ALET-P1-004) both free frames in
bulk, and neither is safe to build until there is an owner to check against.

## Decision

**Every physical frame has exactly one owner, and every allocator transition is checked against it.**

1. **The model is arch-independent and lives once.** `kernel-core/src/frameown.rs` holds
   `FrameOwnerTable`: one byte of state per frame — `0` free, `1..=254` an owner tag, `255`
   permanently reserved (firmware/MMIO the pool must never hand out). No allocation, no architecture
   registers, no `unsafe`. Each target supplies only a `static` state array sized for its own RAM.

2. **Owners are tags, not pointers.** `Owner::KERNEL`, `Owner::PAGETABLE`, `Owner::USER`, and
   `Owner::address_space(id)` for per-address-space identities from tag 4 up. Tag `0` is not
   constructible, so "owned by nobody" can never be expressed as an owner. Page tables are tagged
   `PAGETABLE` and EL0/U-mode/ring-3 pages `USER` on all three targets, so the tags describe the
   kernel's real structure rather than being a placeholder for one.

3. **Illegal transitions are named refusals, not `false`.** `AlreadyOwned` (claiming a live frame),
   `NotOwned` (the double free), `WrongOwner` (freeing someone else's frame), `Reserved`,
   `Unaligned`, `OutOfWindow`, `CapacityExceeded`. The name is what a target logs, so a failure says
   which rule broke.

4. **Ownership is claimed before the frame leaves the list, and released before it goes back.** A
   refused claim leaves the frame on the list and returns `None`; a refused release leaves the list
   untouched. Fail-closed in both directions, and a refusal is a total no-op — proved as a property.

5. **`transfer` is atomic.** Ownership legitimately moves (a kernel frame becomes a user page).
   Doing that as release + claim would leave the frame momentarily free, i.e. claimable by a third
   party. One step, same reasoning as `CapEngine`'s atomic authorize-and-execute (REQ-CAP-006).

6. **A pool without a covering table does not run.** If the state array cannot cover the window, the
   table is refused and the target treats it as fatal (aarch64 exit 39, RISC-V exit 39, x86-64 exit
   29) rather than managing a tail with no ownership state. x86-64 learns its window from the UEFI
   map at runtime, so it CLAMPS the managed window to what the array covers and reports the clamp —
   a machine with more RAM manages less of it, and never silently.

7. **The refusals are part of the cross-architecture contract.** `scripts/conformance.sh` requires
   the same five ownership behaviors, in the same words, from all three targets. A CPU on which a
   double free is accepted is a different memory-safety boundary, not an implementation detail.

## Consequences

* **Cost.** One byte per 4 KiB frame: 32 KiB of `.bss` on each QEMU `virt` target (128 MiB windows),
  256 KiB on x86-64 (a 1 GiB ceiling, clamped and reported). One array index and one branch per
  alloc/free — no allocation, no locking beyond what the allocator already holds.
* **What it enables.** `release_all(owner)` frees exactly the frames of one address space with no
  address list to lose track of — the primitive ALET-P1-004 (address-space teardown) needs, and the
  ownership check ALET-P1-002 (page-table reclamation) needs before it can free intermediate tables.
* **What it does NOT do.** It does not reclaim page tables, tear down address spaces, zero freed
  memory (ALET-P2-026), or enforce W^X (ALET-P1-007). Those rows stay `open` in the GAPS4 register.
* **Proof obligation.** Properties on the host (`kernel-core/tests/frameown.rs`): no frame is ever
  held by two owners, the counters always balance, and every refusal changes nothing — asserted after
  each step of a 20 000-operation deterministic sequence. Each target then proves its own allocator
  is wired to the model in its VM gate (17 memory invariants per target, count-pinned).

## Alternatives considered

* **Keep the bounds check, document the hazard.** Rejected: the whole point of an OS memory model is
  that the kernel cannot be talked into aliasing two owners onto one page by a caller mistake.
* **A per-frame refcount instead of an owner.** Rejected for now: a count catches the double free but
  not the free-someone-else's-frame case, and it cannot answer "which frames belong to this address
  space" — which is exactly what teardown needs. Sharing is already modelled at a higher level by the
  grant table (REQ-IPC-008), which is capability-gated rather than count-based.
* **A bitmap allocator replacing the free list.** Rejected as a larger change with worse constant
  factors, and it would still need an owner tag to distinguish holders.
