# ADR-223 — An NVMe driver

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-DRV-009 (new)
**Builds on:** ADR-023/ADR-037 (virtio-blk, the `BlockDevice` seam), ADR-043 (DMA registry),
ADR-074 (aarch64 BAR assignment), ADR-150 (time-bounded completion waits).

## Context

Every storage driver so far speaks virtio, a paravirtual device that exists only under a
hypervisor. A machine that is not a VM boots from an NVMe SSD or a SATA disk. The roadmap
(docs/research/PRODUCTION-ROADMAP.md) names real hardware as the gap between this OS and a
production one, and the operator asked for drivers. NVMe is one class-defined protocol for every
vendor's SSD, so one driver covers a whole device class.

## Decision

* **`kernel_core::nvme`**: one shared driver, generic over `NvmeRegs` (how BAR0 is reached) and
  the existing `VirtioHal` (frames, barrier, monotonic clock). It resets the controller (disable,
  wait `CSTS.RDY = 0`), programs one admin queue pair, enables, masks interrupt vector 0, runs
  Identify Controller and Identify Namespace 1, and creates one I/O queue pair (interrupts off).
  Each `BlockDevice` block is one command with PRP1 only (a 4 KiB block is exactly one page).
  Completions are polled by phase bit against a 20 s time budget.
* **Constants from the specification.** Every register offset, bit field and structure offset
  names its section of the NVM Express Base Specification and was checked against QEMU's
  `include/block/nvme.h` before use.
* **Fail closed.** Init refuses a controller with no NVM command set, a minimum page size above
  4 KiB, fewer than two queue entries, a ready bit that never changes within `CAP.TO`, fatal
  status, no namespace 1, an empty namespace, per-LBA metadata, or an LBA size outside
  512 B..4 KiB. A completion with the wrong command id, the wrong queue id or a non-zero status
  is an error, never data. A command whose buffer the DMA registry does not know is refused
  before the doorbell.
* **Found by class code.** `virtiopci::find_class_nth(CLASS_NVME = 01h/08h/02h)`, any vendor.
  x86-64 uses the BARs UEFI firmware assigned; aarch64 assigns them itself 16 MiB above the
  virtio-blk-pci window. RISC-V has no PCI host bridge in this kernel yet and does not probe.
* **Flush only when it means something.** `FLUSH` is sent only if Identify Controller reports a
  volatile write cache.

## Proof

* `kernel-core/tests/nvme.rs`: a simulated controller that runs the queue protocol from the
  specification and records protocol violations (admin registers written while enabled, doorbell
  off its stride, interrupts enabled on the I/O CQ). The suite holds over 512 B and 4 KiB LBA
  formats, a doorbell stride of 8 and a 4-entry queue. Eight unsafe controllers are refused by
  name. Lying completions (command id, queue id, status, none at all) are errors. Requests out
  of range or with the wrong size never reach the controller. Mutation check: removing the
  phase flip or the command id check makes the tests fail.
* VM gates (`scripts/vm-e2e.sh` aarch64, `kernel-x86_64/scripts/smoke-test.sh` x86-64) attach
  QEMU's `-device nvme` with a 1 MiB namespace and require `ALL 23 NVME INVARIANTS HOLD`:
  controller up, geometry, DMA gate, unregistered PRP refused, round-trip on an inner and the
  last block, out-of-range refusal, queue wrap over three queue lengths, journal commit and
  recover, and the 15 filesystem behaviors over the namespace. Measured: 17 ms on aarch64 TCG.
  Failure exits 1110 + invariant.

## What this does not claim

* Proven against QEMU's NVMe model only, not against SSD silicon. "Real hardware: none" in the
  roadmap stays until a physical machine boots.
* One queue pair, one command in flight, polled. MSI-X interrupts and multiple queues are the
  next rungs (roadmap rung 3); throughput is not measured or compared here.
* On x86-64 the controller is idle by the time VT-d enforcement turns on, so it gets no IOMMU
  window. Using it after that point needs its grants added to the DMAR table.

## Next drivers

Queued, not claimed: xHCI (USB keyboards, mice and storage), e1000e/igc (wired network), AHCI
(SATA), HD Audio. Each is a class or vendor family QEMU also models, so each can get the same
simulated-controller tests plus a VM gate before any physical machine.
