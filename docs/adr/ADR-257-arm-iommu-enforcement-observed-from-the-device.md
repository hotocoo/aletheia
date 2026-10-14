# ADR-257 — ARM IOMMU enforcement, observed from the device side

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-016 (new); advances ALET-P1-018
**Builds on:** ADR-074 (SMMUv3 identity domain), ADR-256 (legal STEs, aligned stream table),
ADR-075 (x86-64: grant serves, revocation faults by name, restore to silence).

## Context

ADR-074's SMMU suite proved the unit's configuration but never a translated completion, and the
gap register blamed the emulator. ADR-256 found two kernel defects behind that and fixed them; the
USB keyboard became the first PCI device to DMA under enforcement. The suite itself still asserted
nothing from the device side.

A second reason the probe could never have worked: QEMU's virtio-pci devices bypass the IOMMU
unless `iommu_platform=on` is set (VIRTIO_F_IOMMU_PLATFORM), so the suite's virtio-blk-pci probe
used physical addresses whatever the stream table said. "A CLI-attached virtio-blk-pci does not
route DMA through the unit" was true, and it was the gate's device line, not the unit.

## Decision

* **Every aarch64 gate attaches its virtio-blk-pci with `iommu_platform=on`** (`vm-e2e.sh`,
  `usbkbd-e2e.sh`, `browser-e2e.sh`, `desktop-e2e-dt.sh`, `desktop-fuzz-e2e.sh`). The driver
  already accepts VIRTIO_F_IOMMU_PLATFORM when offered (ADR-073).
* **Three invariants after enforcement latches** (`smmu` 10 to 13):
  11. a write then read of the device's last block completes byte for byte and the event queue
      stays empty;
  12. with the probe's STE revoked (V=0, then CFGI_STE, TLBI_S12_VMALL, SYNC) the same read fails
      and the unit records `C_BAD_STE` at the probe's own stream id;
  13. with the STE restored and invalidated, and the device reset (the denied DMA broke its
      queue), the read returns the same bytes and the event queue stays empty.

## Evidence

* aarch64 `vm-e2e.sh`: `ALL 13 SMMUV3 INVARIANTS HOLD`, twice. Before `iommu_platform=on`,
  invariant 12 failed with the read succeeding and zero events, which is what exposed the bypass.
* The other aarch64 e2e scripts pass with the new device line (listed in STATUS for this date).

## What this is not

* Still one identity domain for every function, not per-stream windows: invariant 12 revokes a
  whole stream, not one page (x86-64's ADR-075 does pages).
* Not interrupt remapping, not two-level stream tables (functions past device 7 are refused, ADR-256).
