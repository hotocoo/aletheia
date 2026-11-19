# ADR-270: SMMUv3 per-stream windows and ordered publication

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-SEC-DMA-008 (new)
**Builds on:** ADR-075 (per-device windows on VT-d, the x86-64 twin), ADR-074, ADR-256, ADR-257 and
ADR-259 (the SMMUv3 rung: identity domain, legal STEs, post-enable assertions, two-level stream
table), ADR-043 (the DMA registry). Closes "SMMUv3 per-stream windows on aarch64" from
ALET-P1-018 and "the SMMU register seam carries no memory barriers" from ADR-263.

## Context

On aarch64 every PCI function behind the SMMUv3 shared ONE stage-2 identity domain over all of RAM
minus the kernel image. The image was safe from every device, but any device could DMA into any
other device's buffers: inter-device isolation was the software registry's claim only. x86-64 closed
the same gap with per-device windows in ADR-075.

Separately, `kernel_core::smmu` wrote stream table entries, page-table leaves and command-queue
entries to normal memory and then wrote the unit's registers (device memory) with no barrier. The
ARM architecture does not order those two: a CMDQ_PROD doorbell can reach the unit before the command
it announces, and an STE can be read before it is complete. QEMU never reorders, so no boot gate
could show the bug.

## Decision

* **The registry is the truth, the tables obey.** After the shared domain's own invariants (1..13,
  unchanged), the block function behind the bridge gets its own stage-2 window built from exactly
  `VirtioBlk::dma_grants()` (merged spans), under its own VMID (2; the shared domain is 1). The live
  audit requires the window's translated spans (`smmu::leaf_spans`) to EQUAL the grant set and to
  hold no image byte; the shared domain's command-queue frame, RAM the identity domain maps, must have
  no leaf in it. Its STE replaces the identity one; DMA through it completes and the unit stays quiet.
  The window stays resident to halt.
* **Page-granular revocation, named.** `smmu::rewrite_page_leaf` replaces one page leaf without
  allocating, refuses a page a huge block covers, and refuses to "revoke" an absent leaf. Revoking
  the block driver's data page makes the unit record `F_TRANSLATION` at the function's own stream id
  and an address inside that page.
* **Ordered publication.** `smmu::Regs` gains `publish_barrier` (no default: every implementer
  decides), and the controller calls it before every register write that makes the unit read memory:
  STRTAB_BASE (linear and two-level), each queue base, CR0 enable, and every CMDQ_PROD doorbell
  (which also publishes the STE or leaf a CFGI/TLBI covers). In-kernel it is `dsb sy`.

## Evidence

* `[smmu] ALL 16 SMMUV3 INVARIANTS HOLD` on aarch64 (13 before): `[smmu] window for rid 0x48: 1
  granted span(s), vmid 2`; invariant 15 saw 64 `F_TRANSLATION` records at stream 0x48 inside the
  revoked data page; invariant 16 restored the leaf, reset the device and read the pattern back with
  the unit quiet. The later suites (NVMe, AHCI, xHCI, HD Audio, e1000, e1000e) still DMA through the
  shared domain, and the USB keyboard through it on every keystroke (`usbkbd-e2e.sh`).
* **A measured QEMU behaviour:** the denied request still COMPLETES with status OK. QEMU's
  virtio-blk drops the faulted writes and reports success, so the frame kept whatever it held; the
  first attempt passed `named` and failed `refused` because the frame still held the earlier read's
  pattern. The invariant therefore fills the revoked page with a sentinel and requires that not one
  byte of the requested block arrived. Silicon would abort the transfer; the evidence holds either
  way.
* `kernel-core/tests/smmu.rs` (19 -> 24): a window holds exactly its driver's grants and nothing of
  another device's (walked by the simulated unit, faults named at the exact address); a page revoke
  faults exactly that address, spares the rest, refuses a second revoke, and restores; a page rewrite
  never allocates and refuses a huge leaf; merged spans; and the ordering proof: the simulated unit
  records every doorbell that arrives with table or queue writes not yet published.
  `controller_end_to_end_against_the_simulated_unit` asserts no unordered doorbell. Removing the
  barrier from the CMDQ_PROD doorbell fails both ordering tests.
* Gates: `vm-e2e.sh` (smmu 13 -> 16), `usbkbd-e2e.sh`, `console-e2e.sh`, `conformance.sh`, quality
  gate, every `check-*.sh`.

## What this is not

* **One function gets a window.** The block function is the one whose driver runs during the SMMU
  suite; the others are opened by later suites and stay on the shared domain (which still refuses
  the kernel image to all of them). Giving each its own window needs the SMMU gate to run after every
  driver, or the windows to grow as drivers register, which is the next rung.
* **The barrier is proved ordered on the host only.** QEMU does not reorder, so no live gate can
  fail without it; the `dsb sy` is architecturally sufficient and cheap.
* **No MSI through the SMMU**: the suite's block device is polled. Interrupt remapping and
  per-stream MSI doorbell mappings are not done.
* QEMU's model, not silicon.
