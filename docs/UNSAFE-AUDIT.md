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
| kernel-core | 180 | arch-independent contracts: volatile device-register access behind typed seams (incl. the virtio-blk reset renegotiation and the host-test device model's register/ring access), page-table entry bit manipulation on owned tables, the shared spine's raw-pointer task contexts; +63 for the shared virtio-pci transport seam (ADR-074): the volatile common-config register access behind PciTransport, the PciEnv call sites, and the BAR sizing/assignment probes - moved here from kernel-x86_64 when ARM's SMMUv3 rung became the second consumer; +10 for the real-pixel composition rung (ADR-078): the live-device control ops inside `virtiogpu::compose_suite` (resource create/attach/scanout bind, transfer, flush, detach, unref, error probe) over the DMA-gated backing frames; +14 for the input hardware rung (ADR-080): the virtio-input config-space select/subsel reads and the transports' first config WRITE seam (`ConfigWrite` — a read-modify-write of the aligned word on virtio-mmio in `virtioblk.rs`, a volatile store into the device-config capability on virtio-pci in `virtiopci.rs`), the `init` bring-up and the `next_event` used-ring harvest in `vinput.rs`, and the suite's armed-silence poll |
| kernel | 203 | aarch64 bring-up: EL1/EL0 trampolines, GIC programming, MMIO for virtio-mmio/PL011/PL031/per-CPU areas/frame-table bitmaps, plus the fw_cfg window (ADR-072) - two sites since ADR-073's fw_cfg refactor: one 16-bit BE selector store at +8 and the single-byte data load at +0 (the bulk read loop is now safe code calling that one load); +22 for the SMMUv3 rung (ADR-074): the volatile register seam over the DT-declared unit page and the owned-frame table-memory seam in src/smmu.rs, plus the ECAM configuration-space reads/writes and BAR programming in src/pci.rs; +1 for the input hardware rung (ADR-080): the virtio-mmio slot probe and transport construction inside `input_pair` in src/virtio.rs |
| kernel-x86_64 | 270 | the largest surface, because x86-64 has the most assembly-adjacent state: IDT/GDT/TSS construction, CR3/CR4/EFER control registers, port I/O, LAPIC, SMP trampolines, OVMF memory-map parsing; +2 for the fw_cfg ioport transport (outw selector at 0x510, inb data at 0x511, ADR-072); +14 for the VT-d rung (ADR-073): the volatile MMIO register seam over the DRHD page and the unaligned reads inside the checksum-validated DMAR walk in `src/vtd.rs`, plus the raw config-space enumeration the context programming needs in `src/pci.rs`; +2 for the per-device-window rung (ADR-075): the CR3 read inside the widened vt-d gate and the grant-table BDF probes; +12 for the input hardware rung (ADR-080): the PCI input-function probe in `src/pci.rs`, the transport bring-up inside `input_pair` in `src/virtio.rs`, the live desktop's single-writer static plus its device ops (resource create/attach/scanout bind, transfer + flush, the pump's `next_event` harvests) in `src/desktop.rs`, and the two PIT sites that keep the pump alive through the console — `idt::restore_timer` re-pointing IRQ0 at the plain tick handler after the ring-3 suite, `pic::unmask_timer` clearing the mask bit read-modify-write |
| kernel-riscv64 | 179 | S-mode bring-up: stvec/satp/sepc CSR blocks, PLIC/CLINT MMIO, hart context switch, sbi call sites; +2 for the fw_cfg window transport (ADR-072, same measured model as aarch64); +1 for the input hardware rung (ADR-080): the virtio-mmio slot probe and transport construction inside `input_pair` in src/virtio.rs |
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
