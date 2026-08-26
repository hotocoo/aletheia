# ADR-074: The IOMMU contract crosses the ARM fence

**Status:** Accepted · **Date:** 2026-08-26 · **Advances:** ALET-P1-018 (second hardware rung of
the IOMMU half) · **Builds on:** ADR-071 (the contract), ADR-073 (the VT-d rung and its method),
ADR-036 (bus facts are not CPU facts), ADR-061 (the gate counts itself)

## Context

ADR-073 delivered the contract's first hardware rung on Intel VT-d under q35 and named the
method: discovery over a declared channel, identity domains built from owned frames, live
probes whose evidence comes from the unit itself, wire facts pinned against the emulated unit
where first drafts were wrong. What remained open on the ARM side was stated just as plainly:
SMMUv3 delivery on the virt targets.

## Decision

### The second consumer generalized the transport

The virtio-pci transport lived in kernel-x86_64 because x86-64 was its only consumer. A second
consumer now exists - the SMMUv3 rung drives a virtio-blk-pci behind the unit - so the
capability walk, region resolution and modern register protocol moved ONCE into
kernel_core::virtiopci behind a PciEnv seam: configuration-space access plus each target's
region-mapping rule. x86-64 keeps ports + map_device_range; aarch64 supplies ECAM. Two copies
of a protocol that must agree is how drift ships (ADR-036's lesson, applied one layer up).

### Discovery through the tree the machine publishes

QEMU generates the device tree for the CONFIGURED machine but, on direct -kernel ELF boots,
delivers it through no register at all - x0 reads zero at the entry (measured). The gate
dumps the generated blob (-machine dumpdtb), trims it to its declared totalsize, and
republishes it as an opt/org.aletheia/dtb fw_cfg item: the same declared door the custody
anchor arrives through (ADR-072). The kernel parses it by declared lengths only, and takes
three facts from it, never from constants:

* the arm,smmu-v3 node names the register base (0x0905_0000 with highmem-ecam off - inside the
  Device-mapped GiB the identity map already covers; with ECAM left high the config window
  lands at 0x40_1000_0000, outside every address this kernel can name, which is why the gate
  pins highmem-ecam=off rather than widening the whole VA model for one window);
* the host bridge's iommu-map binds Requester IDs to the unit (identity here: sid = rid);
* platform devices carry NO iommus property - virtio-mmio stays OUTSIDE the unit, asserted
  rather than assumed, because it changes what enforcement covers.

Discovery runs FIRST in kmain: the blob lives in RAM the frame pool manages, so a late parse
could read frames long since handed out.

### Stage-2-only STEs are the VT-d twin

One identity domain, stage-2-only (CONFIG=0b010): input == output, no context descriptors,
one tree per domain - structurally the same object as VT-d's second-level tables. Geometry:
SL0=0b01, S2T0SZ=25 (39-bit input space, single level-1 table, natural page alignment, shifts
30/21/12 - the same shape as this target's own TTBR0 walk). Wire facts pinned against QEMU's
smmuv3 implementation, several correcting plausible first drafts:

* STE.S2R must be set or stage-2 walk faults are DENIED SILENTLY - correct enforcement, zero
  evidence. The encoder refuses to emit an STE without it.
* AF (bit 10) must be set per leaf or every access faults F_ACCESS; S2AP bits [6:7] are read
  as a permission BITMASK, so 0b11 grants read+write.
* Queue base registers carry address [51:6] AND log2size [4:0] in ONE register; splitting them
  publishes garbage geometry the unit honors until its first doorbell.
* S2TTB occupies word bits [51:4] directly - a draft that shifted right by four encoded a
  different table than it built, and every walk faulted.
* CR0's enable word is exactly bits 0|2|3; bits [15:5] are RESERVED and the unit mirrors only
  legal bits into CR0ACK.
* CFGI_ALL is refused CERROR_ILL by the emulator outright; invalidation uses the scoped forms
  plus SYNC (CFGI_STE, TLBI_S12_VMALL), the coarse-but-named posture of the VT-d flushes.

### BAR assignment: the kernel is its own PCI firmware

Bare-metal -kernel boots run no PCI firmware, so nothing programs BARs. The kernel sizes each
memory BAR (all-ones probe), assigns addresses from the PCIe MMIO window the DT ranges
declare, then enables memory decode + bus master. Assignments land inside the Device-mapped
GiB, so resolved regions need no second mapping path.

## The measured boundary of this rung

On QEMU 11.1, a virtio-blk-pci attached on the COMMAND LINE does not route its DMA through the
legacy iommu=smmuv3 unit. Measured canary: with EVERY function's STE programmed CONFIG=ABORT,
the device's completions still arrive intact; the unit's own trace points never fire for
device traffic. Everything UP TO device-side walks is delivered and gated - discovery,
identification, domain construction from owned frames, live-tree audit, grants under DECLARED
stream ids, stream-table adoption by readback, queue publication, the enablement handshake,
latched residency layered over the software registry - and the host suite proves the walker
semantics (grant/deny/restore, deny-by-default, image refusals both sides) against SMMUv3
SHAPES taken from the emulator's own decoder. What stays open in the gap register, with this
measurement as evidence: grant-serves-clean / revocation-faults / restore-silences probes,
which need a device whose DMA actually traverses the unit (hotplug after machine-done, a
fixed emulator, or real silicon).

## Consequences

* ALET-P1-018 advances to "second hardware rung delivered up to the emulator boundary": the
  contract is programmed into a second real unit family end to end except device-side walks,
  which are blocked by the measured attachment artifact, not by any decision here.
* The virtio-pci transport is shared; x86-64's copy was deleted, not forked.
* The aarch64 marker map deliberately DIVERGES from RISC-V (smmu=10 appears only where the
  unit exists); the shared suites still prove identical counts on both.
* Per-device WINDOWS, stage-1 translation, interrupt remapping and ATS/PRI remain open beside
  their VT-d twins.
