# ADR-073: The IOMMU contract is programmed into real silicon

**Status:** Accepted · **Date:** 2026-08-25 · **Advances:** ALET-P1-018 (first hardware rung of
the IOMMU half) · **Builds on:** ADR-071 (the IOMMU contract — modeled, not assumed), ADR-043
(DMA boundary), ADR-037 (virtio-pci transport), ADR-061 (the gate counts itself)

## Context

ADR-071 defined the full enforcement semantics of an IOMMU once — `SoftIommu` — and stated
plainly that a hardware implementation must satisfy the same contract. The hardware rung stayed
open: programming a real DMA-remapping unit behind the trait surface the model already proved.

On x86-64 that unit is Intel VT-d, which QEMU emulates on q35 (`-device intel-iommu`). This
wave delivers that first rung and names exactly what the emulator cannot prove.

## Decision

### Discovery, not poking

The unit answers at an address the firmware DECLARES: the ACPI DMAR table's DRHD structure names
the register base; the variable-position registers are found through the unit's OWN capability
fields (ECAP.IRO for IVA/IOTLB, CAP.FRO/NFR for the fault-record bank). Structures are walked by
DECLARED length; a malformed table ends the search instead of steering it. No DMAR declared —
VirtualBox — means no unit, and the suite skips green saying why.

### Identity domains over owned frames

One identity domain shared by every present bus-0 function: conventional RAM minus the kernel
image, translated 1:1, built entirely from frames the ownership model claims
(`Owner::PAGETABLE`), 2 MiB leaves where alignment allows and 4 KiB leaves elsewhere. Every
function gets a context entry naming that tree under its own domain id; everything else on the
wire — the image, MMIO holes, unmapped addresses — has NO leaf. A live audit re-walks the
programmed tree and counts image violations; zero is a gate condition.

### Live probes whose evidence comes from the fault bank

After SRTP adoption and the TE handshake, the suite kicks the LIVE block functions this boot
already drives and takes its evidence from the unit's fault-record bank:

* **Granted function walks clean** — repeated stimulus, the bank records nothing.
* **Revoked function denied BY NAME** — the context entry is withdrawn through the same seam
  that granted it, both caches are invalidated, and the next kick produces an ACTIVE record
  naming the probed source-id with reason `CONTEXT_ENTRY_P`.
* **Restored grant returns to silence** — the record is retired (see W1C below), the kick walks
  clean again, and enforcement stays latched until the machine halts.

Guest-visible request COMPLETIONS are deliberately NOT the assertion. QEMU's TCG loses virtio
completions across a mid-run enablement: its per-device ring caches resolve against the flatview
of the moment they were created, so once translation is on, buffer maps return NULL ("virtio:
bogus descriptor or out of resources") and the device is marked broken — while the unit's own
translation verdicts stay exact. The evidence trail for this artifact is in the gate history:
device-side queue state showing completions the guest never received, host RAM dumps proving the
completions landed nowhere, and traces showing every walk succeed while maps failed. Real VT-d
silicon translates dynamically per access and has no such window; when the emulator is fixed,
strengthening these invariants to require completions is a one-line change each.

### Wire facts pinned against the emulated unit

Several first drafts carried plausible-but-wrong constants; real traffic corrected them, and each
correction is recorded in the source where it lives:

* GCMD.SRTP is bit 30 (bit 24 is SIRTP; the draft's mistake latched nothing and set IRTPS while
  the poll read RTPS — two mirrored errors cancelling into a green).
* The context-entry domain id lives at high-qword bits [23:8]; AW at [2:0].
* FRCD entries are SIXTEEN bytes indexed by devfn (an eight-byte stride left every granted
  function reading back absent).
* Fault-record decode: low qword = address bits 63:12; high qword = F(63), T(62),
  FR[39:32], SID[15:0].

### Two register-interface facts the live unit forced

* **The fault bank is WRITE-ONE-TO-CLEAR.** The high qword accepts no ordinary writes; only a 1
  written at the F position retires the record (QEMU models FRCD_REG_0_2 with wmask 0 and a W1C
  mask of exactly the F bit). Clearing by writing zeros silently does nothing.
* **QEMU implements FSTS at offset 0x34; the VT-d specification puts it at 0x30.** A spec-exact
  driver reads silence forever while records pile up — which is WHY the proofs above take their
  evidence from the fault-record BANK (whose layout is exact everywhere) and treat FSTS as
  diagnostic only. On real silicon both offsets agree and either source works; on this emulator
  only the bank does.

### Device bring-up order is part of the contract

Every PCI device this boot drives comes up BEFORE enforcement turns on and stays live across it —
how real platforms meet an IOMMU, and the only ordering this emulator supports. Drivers
negotiate `VIRTIO_F_IOMMU_PLATFORM` whenever offered: behind the identity domain descriptor
addresses are unchanged, and a device that REQUIRES the feature keeps FEATURES_OK.

## Consequences

* ALET-P1-018 advances to "hardware first rung DELIVERED": the contract ADR-071 proved in
  software is enforced by the machine's own remapping unit from the vt-d gate to halt.
* Still open in the gap register: SMMUv3 delivery on the virt targets, per-device WINDOWS
  (inter-device isolation remains the software registry's job until then), interrupt remapping,
  queued invalidation, pass-through translation types, and post-enable completion assertions once
  the emulator honors them.
* The x86 boot order changes deliberately: all DMA-dependent suites run before the vt-d gate;
  the gate is last because what it turns on stays on.
