# ADR-040 — The layout is a declaration you can check, and two addresses must never translate

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P1-006 / ALET-P1-012 · REQ-MM-007 (guard pages) / REQ-MM-008 (layout model) ·
contract in `docs/INVARIANT-CONTRACTS.md` §INV-LAYOUT

## Context

Each target knew its address-space layout as scattered literals: a RAM base in `frames`, a peripheral
window in `vm`, a user VA in `usermode`, a stack in `linker.ld`. Nothing stated what the layout *was*, so
nothing could check the properties a layout must have — and two of those properties were quietly false.

**Stacks had no guard.** Every kernel stack grows down into whatever the linker put below it. On the two
QEMU targets that is `.bss`; on x86-64 the ring-0 stack is a `static` with other statics around it. An
overflow did not fault — it silently corrupted state and surfaced later as impossible behavior.

**And VA 0 was mapped.** `kernel_core::vmaddr` has refused mapping the null page through the mapping APIs
since ADR-029. But the *boot identity maps* covered page 0 anyway: inside the peripheral device window on
aarch64 and RISC-V, and as ordinary RAM on x86-64. A kernel null dereference therefore read — or **wrote**
— a real MMIO register or real memory instead of faulting. This decision exists partly because writing the
layout check is what surfaced that.

## Decision

**1. A layout is a declaration — `kernel_core::layout`.** A target declares named regions (`device-mmio`,
`kernel-image`, `kernel-ram`, `user`) with their privilege, and `Layout::validate` refuses a declaration
that breaks any rule: no overlap, page-aligned, no region contains the null page, and **a user-reachable
region may never merely abut a kernel-only one** — something that grows would cross the boundary without
ever being unmapped, so a guard band is required. Every target's boot suite runs that validation: a layout
nobody validates is a layout that drifts.

**2. Every kernel stack gets a real guard page.** On aarch64 and RISC-V, `linker.ld` reserves
`__stack_guard` one page below `__stack_bottom`; on x86-64 the guard is the first page of the `KSTACK`
static, made page-aligned so exactly one leaf can be omitted, with `RSP0` moved above it. Each target's
identity map then **splits the containing block** — a 2 MiB block or a megapage cannot have a hole in it —
and leaves that one page with no descriptor at all.

**3. VA 0 never translates.** The same technique, applied to page 0: the first device block (aarch64), the
first megapage of the peripheral gigapage (RISC-V, which needs two levels of split) and the first 2 MiB
region (x86-64) are each built as 4 KiB pages with the page at 0 omitted.

**4. Both properties are proved on the live tree, on every target, and in the shared contract.** Not "the
builder intended to skip it" but "walking the active hierarchy finds no leaf": four `guard:` invariants
and three `layout:` invariants per target, with the guard-page and null-page behaviors added to
`scripts/conformance.sh` (62 → 64 core behaviors). The guard is asserted against the **kernel's own map**
(`kmap::root()` on x86-64) rather than whatever CR3 holds at that moment, because by the end of the suite
the active root may be a per-process space an earlier test built — a distinction the first version of this
invariant got wrong and the failure caught.

**5. KASLR: none, deliberately, and the reason is recorded rather than the absence being implied.** Every
target identity-maps, which is what keeps the DMA story auditable — a driver hands the device the very
address it writes through (ADR-036/037). Randomizing the kernel's virtual base is therefore a *different
memory model*, not a flag to set. And KASLR defends against an attacker who can read a pointer and use it,
whereas every effect here is gated on a capability, so a leaked kernel pointer is not itself authority.
What it would take is written down: a higher-half split, an offset-mapped physical window for DMA
translation, and PIE kernel images.

## Consequences

* A stack overflow and a null dereference are now **faults**, on all three targets, instead of silent
  corruption. Both are the kind of bug that otherwise costs days.
* The cost is three page-table splits and two pages of address space per target. No runtime cost.
* The layout model gives later work (a higher-half split, KASLR, a guarded heap) something to extend and a
  check that fails when the extension is inconsistent.
* **Not claimed:** per-process spaces built by copying the live tree get their own mappings for these
  regions — the guard and null-page properties are proved for the kernel's map, and re-asserting them
  inside every derived space is a named follow-on. There are no guard pages around the heap or around
  per-CPU stacks (the SMP secondaries), and no higher-half split.

## Alternatives considered

* **Leave the layout as literals and just add the guards.** Rejected: the guards were the *easy* half. The
  null-page hole existed precisely because no one had written down what the map should look like, and only
  the declaration check found it.
* **Detect stack overflow with a canary instead of a guard page.** Rejected: a canary is checked when
  someone remembers to check it, and the overflow has already happened. A missing translation is enforced
  by the MMU on the first byte.
* **Keep VA 0 mapped but make it read-only.** Rejected: a read of `*null` returning device state is still
  a bug that continues executing. The point is to stop.
* **Randomize now, with identity mapping, for "some" entropy.** Rejected as security theater: it would add
  entropy to the boot log and change nothing an attacker must defeat.
