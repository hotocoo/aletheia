# ADR-034 — W^X and memory attributes: permissions are validated, not assumed

**Status:** Accepted (2026-08-02) — **delivered on ALL THREE targets (2026-08-03 addenda: the
bootstrap map is split at page granularity, ALET-P1-007; x86-64 builds and ACTIVATES its own kernel
map instead of inheriting the firmware's, ALET-P1-031; the image refusal moved into `kernel-core`,
ALET-P2-032) — see Scope**
**Context:** GAPS4 ALET-P1-007 (W^X as a global invariant + a checker) and ALET-P1-008 (per-arch
memory-attribute validation) · REQ-MM-006 · builds on ADR-029 (address admission) and ADR-030..033
(the frame lifetime model)

## Context

The memory model up to here answered *which* memory a mapping may name and *who* owns it. It said
nothing about what a mapping is allowed to *do*. Three permission mistakes were possible, and all
three existed in the tree:

1. **Writable and executable.** A W+X page turns any memory-corruption bug into code execution: an
   attacker who can write bytes has, by construction, written instructions the CPU will run.
   Aletheia's dynamic kernel page (`NORMAL_PAGE`) was writable and *kernel-executable* on aarch64;
   user code pages were writable *and* user-executable on aarch64 and RISC-V; on x86-64 every
   mapping was executable because NX was never enabled (`EFER.NXE` "is not guaranteed by firmware"
   was the stated reason — true, and the fix is to enable it, not to skip the invariant).
2. **Executable device memory.** MMIO registers are not instructions. An executable device mapping
   lets a speculative or mispredicted fetch hit registers with read side effects, and hands an
   attacker a jump target whose contents the *device* controls.
3. **A user page executable at kernel privilege.** The classic ret2usr: the user writes a payload
   and induces the kernel to jump to it, executing with full authority.

## Decision

**Decode every mapping's permissions into one arch-neutral model, validate at every mapping API, and
audit the live tree.**

1. **One rule set (`kernel-core/src/memattr.rs`).** `PageAttrs { kind, write, exec_user,
   exec_kernel, user }` and `validate()` implementing: no write+execute; no executable device
   memory; no user page executable at kernel privilege; and no descriptor claiming user-execute
   without user-access (a mis-encoding is how W^X quietly gets lost).

2. **Enforced where the flags enter.** Caller-supplied flags are untrusted input exactly like
   `va`/`pa` (ADR-029), so each target's mapping API decodes and validates before touching a table.

3. **Real mappings changed, not just checks added.** User code pages are now read-only + executable
   on all three targets (the stub is written through the frame's kernel identity address *before*
   the user mapping exists). aarch64's dynamic kernel page gained `PXN`. x86-64 marks writable pages
   `NO_EXECUTE` — which required enabling `EFER.NXE` (checked via CPUID) and `CR4.SMEP`, both
   reported at boot rather than assumed.

4. **An audit, because enforcement at the API is not proof about the tree.** `memattr::audit` walks
   a live hierarchy through the same `TableOps` seam reclamation uses and counts violations by
   class — mappings the kernel created versus block/huge/firmware descriptors it inherited. Each
   target's VM gate requires **zero** in the first class.

5. **Per-architecture honesty, written down rather than averaged away.**
   * aarch64 has separate `UXN`/`PXN`, so all three rules are directly expressible.
   * RISC-V Sv39 has **one** execute bit qualified by `PTE_U`, so "user-accessible AND
     kernel-executable" is *unrepresentable*; its gate proves the U-mode W+X analogue instead, and
     the shared conformance contract omits the rule rather than pretending it is checked.
   * x86-64 also has one NX bit: a USER page with NX clear *is* ring-0-fetchable by paging alone.
     `CR4.SMEP` is what forbids it, so `exec_kernel` is decoded as `exec && !user` and SMEP is
     enabled explicitly — the division of labour between paging and the chip is stated, not implied.

## Scope — what is delivered and what is not

**Delivered:** validation on every dynamic mapping path on all three targets; user code RX; kernel
dynamic pages NX; NX+SMEP enabled on x86-64; an audit that proves zero violations among the mappings
each kernel created; four shared conformance behaviors.

### Addendum (2026-08-03) — the bootstrap map, split at page granularity (ALET-P1-007 resolved)

The first landing of this ADR left the *bootstrap* identity map as a disclosed exception: aarch64 and
RISC-V mapped the kernel image in 2 MiB block / megapage descriptors spanning text, rodata, data,
stack and heap together, and one descriptor carries one permission set — so each such descriptor had
to be writable **and** kernel-executable. The gate pinned the count (64 per QEMU target, i.e. all of
RAM) rather than hiding it. That exception is now closed on both QEMU targets:

* **The linker states the boundaries; the kernel maps to them.** `linker.ld` exports
  `__text_start` / `__text_end` / `__rodata_end` on both targets. `build_identity` builds every RAM
  block overlapping `[__text_start, __rodata_end)` as a table of 512 4 KiB leaves and gives each page
  the permissions its *section* deserves: text is read-only + kernel-executable, `.rodata` is
  read-only + execute-never, and everything else (data, bss, stack, heap, and RAM merely sharing the
  block) is writable + execute-never. RAM outside the image keeps one block descriptor, now writable
  + execute-never at both privilege levels — so no descriptor anywhere in the tree is W+X.
* **Derived, not restated.** `image_split_blocks()` computes the affected block count from the linker
  symbols, so an image that grows past a block boundary changes the map instead of silently breaking
  a hard-coded assumption. `.rodata` and `.data` both carry `ALIGN(0x1000)`, so rounding a section end
  up to a page can never merge a text page with a rodata page.
* **Both violation classes are now required to be zero,** where before only the dynamic class was:
  the audit walks the live hierarchy through the same `TableOps` seam and each QEMU gate asserts
  `dynamic_violations == 0` **and** `bootstrap_violations == 0`. Four further invariants assert the
  split is real rather than assumed — the leaf covering `__text_start` is a 4 KiB *page* (not a block)
  and identity-maps, text is executable and read-only, `.rodata` is neither writable nor executable,
  and kernel data plus the running stack are writable and never executable. Virtual-memory invariants
  49 → **55** on aarch64 and RISC-V (1087 and 576 live leaves audited, 0/0 violations).
* **The split removed an implicit guard, so the guard became explicit.** Kernel-image VAs used to be
  unmappable-over only as a side effect: the level above them was a block/megapage leaf, and
  `map_page`/`unmap_page` refuse to descend into one. With real tables there, mapping a fresh WRITABLE
  page over kernel text succeeded — the exact write-to-code path this ADR exists to close — and neither
  the address plan (a legal VA, an owned PA) nor the attribute decode (a clean RW+NX page) refuses it.
  Both APIs now reject the block-aligned split span at their entry, next to the ADR-029 admission check,
  and two invariants per QEMU target assert the refusal *and* that text still maps to itself read-only.
* **Not a shared conformance behavior — until x86-64 could emit it too.** The first landing left this
  pair out of `conformance.sh` because x86-64's image is a PE loaded by UEFI, not by a map this kernel
  builds, so requiring the marker would have failed it for an architectural reason. The 2026-08-03
  addendum below removes that reason, and both behaviors are now part of the core contract, proved by
  all three targets in identical words.

## Addendum, 2026-08-03 — x86-64 builds its own map, and it is the LIVE one (ALET-P1-031)

The first landing could validate every mapping the x86-64 kernel *created* and still leave ~524 795
W^X violations across ~524 799 leaves live, because the tree the machine translated through was the
firmware's: long mode requires paging, so OVMF hands the kernel a running MMU and `ExitBootServices`
transfers *ownership of its hierarchy* rather than the chance to build one. No amount of admission
checking un-maps an inherited page. The fix is the one the other two targets already implement —
build the map — and the missing ingredient was the bounds.

* **The image describes itself.** `linker.ld` exports `__text_start`/`__text_end`/`__rodata_end` on
  aarch64 and RISC-V; a UEFI PE has no such symbols. `kernel-x86_64/src/kmap.rs` takes base + size
  from `LoadedImage` (captured before `ExitBootServices`) and reads the image's own PE section table
  for the rest: `IMAGE_SCN_MEM_EXECUTE` and `IMAGE_SCN_MEM_WRITE` carry exactly the facts the linker
  symbols carry. Unparsable headers make the build REFUSE rather than default kernel text to
  writable.
* **Same shape as the other targets.** Identity — so nothing moves — with 2 MiB RW+NX huge pages for
  RAM and low MMIO, and 4 KiB pages over every 2 MiB region the image touches: text RO+X, `.rodata`
  RO+NX, data/bss RW+NX, headers and padding RO+NX. Measured: 4 GiB covered, 2043 huge + 2560 page
  leaves, 5 split blocks, 11 table frames, all claimed through the ADR-030 ownership model.
* **Built and proved BEFORE activated.** Nine invariants assert not "nothing was flagged" but that
  each class of address is mapped as the right thing — text at 4 KiB granularity and RO+X, data
  RW+NX, `.rodata` neither, bulk RAM a 2 MiB RW+NX block, low memory (the SMP trampoline's home)
  present. Only then does CR3 move. A wrong map fails an invariant instead of triple-faulting.
* **CR4.PGE is cycled across the CR3 write.** A global TLB entry survives a CR3 load by definition
  and OVMF marks its mappings global. Without clearing PGE, the firmware's permissions would remain
  live in the TLB for pages this map deliberately narrowed — a half-switch that makes the claim
  unprovable rather than false.
* **Everything after activation runs on it.** The spine invariants, SMP bring-up across four cores,
  and the whole ring-3 suite (syscalls, per-process spaces, IPC, grants, preemption, priority
  donation) execute on the kernel's own tree. Live audit: 4603 leaves, **0** violations; the boot
  fails closed (exit 28) if that ever changes, and the gate requires the marker rather than pinning a
  number. Virtual-memory invariants 40 → 52.
* **The ceiling is an allowlist, not the map's maximum.** Firmware describes an aperture reaching
  1 TiB; taking the raw maximum built a 1 TiB tree (1032 table frames) to reach registers the kernel
  never touches. Only memory types that describe real storage raise the ceiling, with a 4 GiB floor
  for the platform's devices.
* **The image refusal is now one implementation (ALET-P2-032).** Splitting an image removes the
  block/huge descriptor that had made its addresses undescendable, so all three targets need the
  refusal — and it was written twice, with x86-64 about to need a third copy.
  `AddrPlan::with_protected` + `MapFault::ProtectedVirt` in `kernel_core::vmaddr` hold the rule;
  each target declares only where its image is. Host-proved page by page across the span, both APIs,
  half-open boundaries, and malformed-address precedence.

**Not claimed:** memory-mapped devices above 4 GiB (64-bit PCI BARs) are unmapped by this tree —
nothing this kernel drives lives there, and a driver that needs one must map it explicitly. The
framebuffer is mapped as Normal write-back memory like the rest of the sub-4 GiB span; x86-64
expresses cacheability through PAT/MTRRs rather than a leaf field, which the decode models
explicitly (see above) rather than assuming.

## Consequences

* **Cost.** One decode + four boolean checks per mapping call. The audit is O(mappings) and runs in
  the gate, not on any hot path.
* **What it buys.** A memory-corruption bug in Aletheia no longer has a writable page to execute
  from among the mappings the kernel makes, and the property is checked against the live tree rather
  than trusted from the API.
* **What it does not buy.** It is not CFI and not ASLR (ALET-P1-006 covers layout hardening, still
  open). The write-to-.text-through-a-block path the first landing disclosed is now gone on all three
  targets: kernel text is mapped read-only at 4 KiB granularity, so a kernel write primitive has no
  writable alias of the code to aim at, and on x86-64 the firmware's tree is no longer translating at
  all rather than merely being reported.

## Alternatives considered

* **Enforce at the API only, no audit.** Rejected: the bootstrap map is built by code that predates
  the API, and an invariant nobody checks against reality drifts (the whole reason ALET-P2-011/013
  exist).
* **Fail the boot on any W^X violation.** Rejected as dishonest-by-panic: it would have to except
  the bootstrap blocks anyway, and an exception buried in a panic condition is less visible than a
  pinned number in a gate.
* **Claim W^X as delivered because the dynamic paths are clean.** Rejected outright when the
  bootstrap map still held violations — the register entry says "W^X as a complete global invariant",
  and it stayed open until the map actually was one, on every target.
* **On x86-64: keep the firmware's tree and repair its entries in place.** Rejected. Walking half a
  million inherited leaves to narrow each one leaves the kernel dependent on the shape of whatever
  firmware booted it, and there is no honest audit of "we fixed what we found". Building the map from
  the image's own bounds means the tree's correctness follows from how it was constructed.
* **On x86-64: activate the new map before proving it.** Rejected. An incomplete identity map faults
  the instruction after `mov cr3` and surfaces as a QEMU triple fault with no invariant to read. Build,
  audit, assert each address class, *then* switch — so a mistake is a failed check, not a dead machine.
