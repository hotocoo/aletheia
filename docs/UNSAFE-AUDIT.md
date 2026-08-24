# The unsafe audit (ALET-P3-002)

`unsafe` is where Rust stops checking and this project starts claiming. This page is the
CENTRALIZED inventory of the unsafe surface — one row per crate, what the count covers, why the
surface has the shape it has, who owns reviewing it, and the rules a change must follow.
`scripts/check-boundary-docs.sh` recomputes every count from the tree and fails when this page
drifts.

**Counting rule (stated because it is enforced):** occurrences of the token `unsafe` on CODE
lines — any line whose first non-whitespace character is not `//` — across all `.rs` files
under `<crate>/src`. The count includes `unsafe fn`, `unsafe impl`, `unsafe extern`,
`unsafe {}` blocks and the word inside code-level attributes; purely-prose comment lines are
excluded. It is deliberately a TOKEN count, not a judgement: the judgement lives in the rules
below, and the number exists so neither growth nor deletion can happen unnoticed.

## Inventory

| Crate | Count | What the surface actually is |
|-------|-------|------------------------------|
| aletheia | 3 | the Windows transport FFI (`BCryptGenRandom` extern + call) — the only unsafe in the hosted Core; everything else, including all crypto and storage, is safe Rust over audited libraries |
| kernel-core | 93 | arch-independent contracts: volatile device-register access behind typed seams (incl. the virtio-blk reset renegotiation and the host-test device model's register/ring access), page-table entry bit manipulation on owned tables, the shared spine's raw-pointer task contexts |
| kernel | 178 | aarch64 bring-up: EL1/EL0 trampolines, GIC programming, MMIO for virtio-mmio/PL011/PL031, per-CPU areas, frame-table bitmaps |
| kernel-x86_64 | 253 | the largest surface, because x86-64 has the most assembly-adjacent state: IDT/GDT/TSS construction, CR3/CR4/EFER control registers, port I/O, LAPIC, SMP trampolines, OVMF memory-map parsing |
| kernel-riscv64 | 176 | S-mode bring-up: stvec/satp/sepc CSR blocks, PLIC/CLINT MMIO, hart context switch, sbi call sites |
| component-sdk | 4 | guest-export glue in the macro that stamps the ABI custom section |

## Ownership

* **Owner:** the kernel maintainers as a set — recorded here rather than left implicit. A change
  to any unsafe surface is reviewed by someone OTHER than its author; that reviewer reads the
  SAFETY justification against the invariant it invokes, not against intent.
* **The rule every block must satisfy:** a `// SAFETY:` comment naming the specific invariant
  that makes the block sound ("the pointer is valid because...", "this CPU owns the table
  because..."). There are 445 such annotations in the tree today. A block whose comment argues
  from convenience instead of an invariant does not merge; a block with NO comment does not
  compile under review even if it compiles under rustc.
* **Where soundness is PROVED rather than argued**, prefer that: the trap-frame layouts,
  re-entry guard, fault classification and ownership models are host-proved property suites
  precisely so their adjacent unsafe blocks rest on tested claims (see
  docs/INVARIANT-CONTRACTS.md). New unsafe should ask first whether a model test could hold the
  same invariant.

## Rules of the audit

1. Growth must be DELIBERATE and visible: the checker holds these counts exactly, so any change
   to the tree's unsafe token count requires updating this page in the same commit — which puts
   the delta in front of review by construction.
2. Unsafe is confined to LEAF modules at hardware/FFI boundaries. Pulling unsafe into policy,
   protocol or data-model code is refused in review regardless of correctness.
3. Every unsafe block's soundness argument must name its invariant; where the invariant has a
   contract section, cite it.
