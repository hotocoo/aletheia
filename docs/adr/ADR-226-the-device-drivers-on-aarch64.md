# ADR-226 — e1000 and AHCI on aarch64, and two bugs a second CPU found

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-DRV-010, REQ-DRV-011 (widened to aarch64)
**Builds on:** ADR-074 (aarch64 assigns its own BARs), ADR-224 (e1000), ADR-225 (AHCI).

## Context

ADR-224 and ADR-225 ran the e1000 and AHCI drivers on x86-64 only, where UEFI firmware assigns
BARs and starts the SATA ports before the kernel runs. On aarch64 the kernel boots with no PCI
firmware, so it meets these devices exactly as they come out of reset.

## Decision

* **I/O-port BARs are skipped, not refused.** The 82540EM (BAR1) and ich9-ahci (BAR4) carry
  legacy I/O-port BARs. `pci::assign_bars` used to fail the whole device on one; it now leaves
  them unassigned. They never decode, because `enable_bus_master` sets memory space and bus
  master but never I/O space.
* **Bug 1: a BAR that reads 0 is not unimplemented.** `assign_bars` skipped any BAR whose raw
  value was 0. A 32-bit non-prefetchable memory BAR at base 0 also reads 0 after reset, so the
  e1000's BAR0 was never assigned (`virtio-pci BAR is unassigned`). Every BAR is now size-probed
  (an unimplemented BAR probes to size 0), and the upper dword of a 64-bit BAR is skipped
  explicitly, not by reading it as 0.
* **Bug 2: the AHCI signature only exists after FIS receive starts.** `ahci::disk_ports` filtered
  on `PxSIG == 0x101`. PxSIG holds the signature from the device's first D2H FIS (AHCI §3.3.9) and
  reads all-ones before one arrived. On x86-64 the firmware had already started the ports, so
  this went unseen. `disk_ports` now filters on link state only, and `AhciDisk::open` checks the
  signature after enabling FRE, returning `ahci::NOT_ATA` for a non-disk port, which callers
  skip. The simulated HBA in the hosted tests now withholds the signature until FRE, like QEMU.
* aarch64 places e1000 BARs 32 MiB and AHCI BARs 48 MiB above the virtio-blk-pci window.

## Proof

* `scripts/vm-e2e.sh` (aarch64) attaches `-device e1000` on its own user-mode network and
  `-device ich9-ahci` with a disk carrying a 0x55AA signature on port 0 and the
  `ALETHEIA-SCRATCH` disk on port 1, and requires `e1000=6` and `ahci=25`. Both pass, and the
  x86-64 gate still passes with the new signature path.
* `kernel-core/tests/ahci.rs`: an ATAPI port has a link (it is listed) and is refused by name
  at open.

## What this does not claim

* RISC-V still has no PCI host bridge in this kernel, so none of the three drivers runs there.
