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

**Not delivered (REQ-MM-006 is `partial`; ALET-P1-007 stays `open`):** the *bootstrap* map.
aarch64 and RISC-V identity-map the kernel image in 2 MiB block / megapage descriptors that span
text, rodata, data, stack and heap together, so those blocks are necessarily writable **and**
kernel-executable — **64 such descriptors on each target, a number the gate now PINS**, so the
exception cannot grow unnoticed and shrinking it is a visible change. Closing it means splitting the
image at page granularity using linker symbols, which is its own wave. On x86-64 the inherited OVMF
tree contains ~524 795 W^X violations across ~524 799 leaves; that tree is the firmware's, is
reported informationally at every boot, and is not something this kernel created or can fix without
building its own kernel map.

## Consequences

* **Cost.** One decode + four boolean checks per mapping call. The audit is O(mappings) and runs in
  the gate, not on any hot path.
* **What it buys.** A memory-corruption bug in Aletheia no longer has a writable page to execute
  from among the mappings the kernel makes, and the property is checked against the live tree rather
  than trusted from the API.
* **What it does not buy.** It is not CFI, not ASLR (ALET-P1-006 covers layout hardening, still
  open), and it cannot help while the bootstrap blocks remain W+X — an attacker who can already
  write to kernel .data can write to kernel .text through those blocks. That is precisely why the
  count is pinned rather than described as "some".

## Alternatives considered

* **Enforce at the API only, no audit.** Rejected: the bootstrap map is built by code that predates
  the API, and an invariant nobody checks against reality drifts (the whole reason ALET-P2-011/013
  exist).
* **Fail the boot on any W^X violation.** Rejected as dishonest-by-panic: it would have to except
  the bootstrap blocks anyway, and an exception buried in a panic condition is less visible than a
  pinned number in a gate.
* **Claim W^X as delivered because the dynamic paths are clean.** Rejected outright. The register
  entry says "W^X as a complete global invariant"; it is not complete, so it stays open.
