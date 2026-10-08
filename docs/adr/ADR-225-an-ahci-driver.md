# ADR-225 — An AHCI (SATA) driver that never writes the boot disk

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-DRV-011 (new)
**Builds on:** ADR-223 (NVMe), ADR-224 (e1000, the VirtualBox device rung), ADR-043 (DMA registry).

## Context

SATA through an AHCI controller is the disk interface of most PCs, of QEMU's q35 machine
(ich9-ahci at 00:1f.2) and of VirtualBox's default VM. On both hypervisors the boot disk sits on
that same controller, so a storage driver for it must be unable to damage the disk it booted from.

## Decision

* **`kernel_core::ahci`**: enables AHCI mode (`GHC.AE`), lists implemented ports with an
  established link (`PxSSTS.DET = 3`) and an ATA signature (`PxSIG = 0x101`). Per port: stop the
  command and FIS engines and wait for `CR`/`FR` to clear, publish the command list (+0), received
  FIS area (+0x400) and command table in owned DMA-gated frames, clear `SERR`/`IS`, mask port
  interrupts, start `FRE` then `ST` once the disk is not busy. One command slot, one PRD entry,
  completion polled on `PxCI` with task-file errors (`IS.TFES`, `TFD.ERR`) treated as failure,
  and the byte count the HBA reports must equal the PRD length. Commands: IDENTIFY DEVICE, READ
  DMA EXT, WRITE DMA EXT, FLUSH CACHE EXT. 48-bit LBA is required; logical sectors of 512 B or
  4 KiB. Offsets are from the AHCI 1.3.1 specification and were checked against QEMU's
  `hw/ide/ahci-internal.h`.
* **Writes only to scratch.** A disk is writable only when its IDENTIFY serial number is exactly
  `ALETHEIA-SCRATCH`. `write_block` and `flush` on any other disk return an error before a
  command is built, and the suite proves no command reached the disk.
* **Read proof on data nobody here wrote.** The suite reads sector 0 of every disk and requires
  the 0x55AA boot signature the image builder's GPT protective MBR carries.
* x86-64 only (ABAR = BAR5, firmware-assigned). ich9-ahci on aarch64 also has an I/O BAR, which
  the aarch64 assigner refuses, same as the e1000 in ADR-224.
* **NVMe leaves the VirtualBox gate.** ADR-224 attached VirtualBox's NVMe controller; CI showed
  it is part of the Oracle Extension Pack (`VERR_PDM_DEVICE_NOT_FOUND` without it). The
  VirtualBox gate now attaches a second SATA disk with serial `ALETHEIA-SCRATCH` (set through
  `VBoxInternal/Devices/ahci/0/Config/Port1/SerialNumber`) and requires the AHCI and e1000
  families. NVMe stays proved on QEMU only.

## Proof

* `kernel-core/tests/ahci.rs`: a simulated HBA with a boot disk on port 0 and a scratch disk on
  port 2. It asserts the FIS type, `CFL = 5`, the W bit matching the direction and the PRD
  covering the sector count. The suite holds for 512 B and 4 KiB scratch sectors, and the boot
  disk receives zero writes. An ATAPI signature is not listed. No 48-bit LBA or a disk that stays
  busy is refused at open. Task-file errors and short transfers are errors.
* `kernel-x86_64/scripts/smoke-test.sh` attaches `-device ide-hd,bus=ide.1,serial=ALETHEIA-SCRATCH`
  next to the boot disk and requires `ahci=25`. Measured on QEMU 11.1: both disks identified,
  boot signature read, 18 ms suite.
* `scripts/vm-e2e-vbox.sh` requires `ALL 25 AHCI INVARIANTS HOLD` on VirtualBox's controller (CI).

## What this does not claim

* Not proved on silicon; one command at a time, no NCQ, no hot-plug, no port multipliers.
* The boot disk is identified and read, never mounted or written.
