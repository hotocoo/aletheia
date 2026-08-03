# ADR-034 — W^X and memory attributes: permissions are validated, not assumed

**Status:** Accepted (2026-08-02) — **partially delivered by design; see Scope**
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
  49 → **53** on aarch64 and RISC-V (1087 and 576 live leaves audited, 0/0 violations).
* **Not a shared conformance behavior, deliberately.** `conformance.sh` requires identically-worded
  behaviors from *all* targets, and x86-64 cannot emit this one: its image is a PE loaded and mapped
  by UEFI, not by a map this kernel builds. Adding it would either fail x86-64 for an architectural
  reason or force a marker that proves nothing. The precedent is the RISC-V single-execute-bit
  omission above.

**Still not delivered (x86-64 only; tracked as ALET-P1-031):** the inherited OVMF tree contains
~524 795 W^X violations across ~524 799 leaves. That tree is the firmware's, is reported
informationally at every boot, and cannot be closed by validating mappings — it needs the x86-64
backend to build its own kernel map from its own PE image bounds instead of adopting the firmware's.
The x86-64 gate therefore still pins its bootstrap count rather than requiring zero.

## Consequences

* **Cost.** One decode + four boolean checks per mapping call. The audit is O(mappings) and runs in
  the gate, not on any hot path.
* **What it buys.** A memory-corruption bug in Aletheia no longer has a writable page to execute
  from among the mappings the kernel makes, and the property is checked against the live tree rather
  than trusted from the API.
* **What it does not buy.** It is not CFI and not ASLR (ALET-P1-006 covers layout hardening, still
  open). On the QEMU targets the write-to-.text-through-a-block path the first landing disclosed is
  now gone: kernel text is mapped read-only by 4 KiB pages, so a kernel write primitive has no
  writable alias of the code to aim at inside this map. On x86-64 that path remains open through the
  firmware's tree (ALET-P1-031), which is why its count stays pinned rather than described as "some".

## Alternatives considered

* **Enforce at the API only, no audit.** Rejected: the bootstrap map is built by code that predates
  the API, and an invariant nobody checks against reality drifts (the whole reason ALET-P2-011/013
  exist).
* **Fail the boot on any W^X violation.** Rejected as dishonest-by-panic: it would have to except
  the bootstrap blocks anyway, and an exception buried in a panic condition is less visible than a
  pinned number in a gate.
* **Claim W^X as delivered because the dynamic paths are clean.** Rejected outright. The register
  entry says "W^X as a complete global invariant"; it is not complete, so it stays open.
