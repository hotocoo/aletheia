# ADR-029 — Raw addresses are untrusted input: a mapping-API admission check

**Status:** Accepted (2026-07-28)
**Context:** GAPS4 ALET-P1-001 · REQ-MM-001 · extends ADR-019 (HAL / multi-target), ADR-012 (validation pyramid)

## Context

Every Aletheia target exposes a dynamic mapping API — `map_page`/`unmap_page` on aarch64 and
RISC-V, `map_user_frame`/`map_kernel_frame`/`map_supervisor`/`unmap_user` on x86-64 — that took a
raw `va` and `pa` and walked straight into the page tables. A page-table walker decodes a fixed
number of virtual-address bits (39 for aarch64 TTBR0 with T0SZ=25, 39 for RISC-V Sv39, 48 for
x86-64 4-level paging). Bits above that width are not part of the walk.

That produces four failure modes, none of which fail loudly:

1. **Aliasing.** Two virtual addresses differing only above the decoded width resolve to the *same*
   page-table entry. A second map silently overwrites the first; an unmap of one address tears down
   the other's mapping.
2. **Silent truncation.** A misaligned address has its low bits dropped (`Page::containing_address`
   on x86-64 truncates to the page base), so the caller believes it mapped what it named while the
   hardware maps something else.
3. **Mapping memory the kernel does not own.** A `pa` outside the frame allocator's window maps
   firmware tables, MMIO, or another address space's frames.
4. **A crash instead of a refusal.** On x86-64 a non-canonical address is a `#GP`, and the
   `VirtAddr::new` constructor *panics* on one — a caller-supplied value turning into a kernel abort.

Aletheia's whole security argument is that authority is checked at a boundary. An unvalidated
address is untrusted input crossing that boundary with no check at all.

## Decision

A single arch-independent admission check — `kernel_core::vmaddr` — runs at the entry of every
mapping API on every target, fail-closed.

* Each target declares an `AddrPlan` **once**: decoded VA width, whether the ISA requires canonical
  sign-extension, and the physical window the frame allocator actually owns. The window is read
  from the allocator at call time (`frames::base()`, `frames::total_count()`), so the check cannot
  drift from the pool it protects.
* The rules are pure arithmetic — no allocation, no architecture registers, no `unsafe` — so they
  are proved on the host under `cargo test`, not discovered in a VM.
* `canonical` is a real architectural distinction, not a style flag. aarch64 TTBR0 covers a flat
  `[0, 2^39)` with TTBR1 disabled, so every higher bit must be zero. x86-64 sign-extends from bit 47.
  **RISC-V Sv39 sign-extends from bit 38** — its 39 bits *include* the sign bit, so its low half is
  `[0, 2^38)`, and treating it like the aarch64 case would wrongly accept `[2^38, 2^39)`.
* Rejections are typed (`MapFault`), so a target reports which rule it broke rather than a bare
  `false`.

## Consequences

* **Two-layer proof, matching ADR-012.** `kernel-core/tests/vmaddr.rs` proves the *properties* over
  every target's plan — no two accepted virtual addresses may alias, and every accepted physical
  address is a frame the allocator owns — by enumeration, not by example. Each target's VM gate then
  proves its own plan is really wired in, on live page tables, against a still-allocated frame (so a
  refusal is attributable to the address, never to allocator exhaustion).
* **The refusals are part of the cross-architecture contract.** `scripts/conformance.sh` requires
  the identically-worded refusals from all three targets: a target that accepts an address the
  others refuse is a security boundary that varies by CPU.
* **A filter, not a blanket denial.** Every target's gate ends by proving a legal map/translate/unmap
  still succeeds after the refusals, so "everything is refused" cannot masquerade as a pass.
* `unmap_user` on x86-64 now returns `bool` so a refusal is observable; existing teardown callers
  ignore the value.
* **Scope.** This ADR covers address *admission*. Frame ownership and double-free defense
  (ALET-P1-003), intermediate page-table reclamation (ALET-P1-002), address-space destruction
  (ALET-P1-004) and a global W^X invariant (ALET-P1-007) are separate findings and remain open in
  `docs/gap/ARCHITECTURE-GAPS4-REGISTER.md` — this check does not imply them.
