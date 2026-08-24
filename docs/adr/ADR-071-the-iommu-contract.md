# ADR-071: The IOMMU contract - modeled now, hardware-realized next

**Status:** Accepted · **Date:** 2026-08-23 · **Advances:** ALET-P1-018 (the IOMMU half; the
hardware rung stays scoped) · **Builds on:** ADR-043 (DMA boundary), ADR-063 (never-freeing heap
discipline), ADR-062 (fault-injection doctrine)

## Context

ALET-P1-018's software boundary (ADR-043) constrains what the KERNEL tells a device. What kept the
row open is the other half: nothing stopped a device that INVENTS its own addresses. Hardware
IOMMUs answer that, and QEMU 11.1 emulates both flavors this project targets (ARM SMMUv3 on virt,
Intel VT-d on q35) - so the capability is reachable. But programming real VT-d root/context/page
tables or SMMUv3 stream tables is a subsystem of its own with firmware interactions, and doing it
BEFORE defining what it must enforce would put the cart before the horse.

## Decision

### The contract is defined once, as software, in kernel-core

`kernel-core/src/iommu.rs` defines the full enforcement semantics as `SoftIommu`, a complete
software model:

* **Per-device address spaces**: each attached device translates through ITS OWN mappings;
  device A's windows do not exist for device B. Isolation between devices is structural rather
  than a policy anyone remembers to apply.
* **Deny by default, faults NAMED**: translating an unmapped page returns
  `NotMapped { device, iova }`; a mapped page without the permission returns
  `PermDenied { device, iova }` - exactly the shape a hardware IOMMU's fault queue reports.
* **The kernel image is not a DMA target** on either side: neither the IOVA nor the PA of a
  mapping may overlap the image span, for any device, ever (`KernelImage`). This closes the
  write-to-code path at the TRANSLATION layer too, complementing W^X which closes it at the MMU
  layer.
* **Mapped means translated**: a mapped window translates each page to exactly its own physical
  page - making this a real translation check (an offset IOVA lands on the offset PA), not a
  pass-through registry.
* **Revocation is unmap**: removing a mapping ends access immediately; later translations fault.
* **Double-map refused**: overlapping IOVA windows AND physical aliasing inside one device's
  space are named refusals (`DoubleMap`) - the DMA twin of a double free.
* **Null page never legal**: neither side may be zero (`NullPage`).
* **Bounded**: mappings live in a fixed-capacity table (MAX_MAPPINGS = 256) so the model cannot
  grow without bound on a never-freeing heap.

### Proof posture: host-exhaustive + boot-compact

Host proofs in `kernel-core/tests/iommu.rs`: state-machine fuzz against a mirror model, per-device
isolation in both directions, kernel-image refusals on BOTH sides of every mapping, revocation
mid-flight, and double-map detection. In-kernel: `iommu_suite`, 9 invariants on every boot of all
three targets (`[iommu] ALL 9 IOMMU-CONTRACT INVARIANTS HOLD`), seven pinned cross-CPU in the
conformance contract.

### Why NOT boot-gate the exhaustive sweeps

The first version wired the full suite into every kernel boot and PANICKED THE CONSOLE SUITE: the
boot heap never frees (ADR-063), and sweep churn starved later suites of exactly the allocations
they need. The fix was architectural, not cosmetic: the boot proves the core contract with small
allocations while the EXHAUSTIVE sweeps run on the host where memory is unconstrained. This
mirrors ADR-069's posture for the encryption-at-rest lifecycle exactly.

### Hardware realization path (scoped, not claimed)

When a target gains a real IOMMU hardware unit, the target implements the SAME trait surface with
VT-d/SMMUv3 register programming instead of SoftIommu's table walk. The contract does not change;
only the enforcement mechanism underneath it does. Until then, SoftIommu IS the reference
enforcement - and the proofs that hold against it are the ones a hardware implementation must
also satisfy.

## Named non-claims

* No REAL hardware IOMMU is programmed by any target yet. SoftIommu models the semantics; VT-d
  or SMMUv3 register-level enablement is future work scoped by this ADR.
* Distinct devices mapping the same physical page is ALLOWED (buffer sharing between devices is
  a kernel decision made explicitly at map time). What is forbidden is ALIASING within one
  device's space - two windows reaching one frame.
* The null page is never a legal translation source or target on either side.
* The suite runs on the boot heap, which never frees: the in-kernel suite is deliberately SMALL
  (~15 allocations) and the exhaustive sweeps live on the host.

## Consequences

Writing this module surfaced three things the hard way: a missing `Mapping` struct definition
(caught immediately by rustc), pattern-matching expressions inside struct patterns (rustc
rejects arithmetic in patterns - guards are the correct form), and clippy's doc-list-indentation
lints on the module docs. All fixed before landing. The gate-marker map changed DELIBERATELY
(`iommu=9` added to all four expected maps) per ADR-061 doctrine: new suites join the map, they
are never silently ignored.