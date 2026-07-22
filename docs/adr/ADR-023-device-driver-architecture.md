# ADR-023: Device and driver architecture

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

A complete OS needs a real device model + driver architecture for storage, networking, graphics,
input, and accelerators (gap-register Issue 5). Aletheia's constraint is that drivers, like
everything, get **no ambient authority** — a driver operates only within its assigned capabilities.

## Decision

Drivers are **user-space services** reached through capability IPC, not privileged kernel blobs:

```text
Application → Capability IPC (ADR-020) → Device Service → Driver → Hardware
```

**Phase 1 — discovery + device capabilities.** A device manager enumerates hardware via ACPI
(x86-64) / device-tree (aarch64, RISC-V), abstracted behind the `Hal`. Each device becomes an entity
with a capability; a driver is granted a `device.<class>` capability scoped to exactly its device
(MMIO region + IRQ line). No driver can touch a device it was not granted.

**Phase 2 — bus + class abstractions.** PCIe/USB enumeration; a `BlockDevice` trait (the first
persistent-storage path, feeding ADR-024) and a `NetDevice` trait; input and audio device classes.
DMA is IOMMU-constrained so a driver cannot DMA outside its granted buffers (defends the DMA-attack
class in gap-register Issue 11).

**Phase 3 — isolation + recovery.** Drivers run isolated (a driver crash cannot corrupt the kernel or
other drivers); the supervisor (ADR-026) restarts a failed driver; hotplug + power management.

## Consequences

- Drivers require no ambient authority beyond their assigned capabilities — a compromised driver is
  bounded to its device.
- At least one persistent-storage path (`BlockDevice`) becomes available for ADR-024.
- Entirely hardware-bound; stays `deferred` (`REQ-DRV-001`) until a device service + one real driver
  are brought up and (where emulable) VM-gated. Driver failure is contained + diagnosable by design.
