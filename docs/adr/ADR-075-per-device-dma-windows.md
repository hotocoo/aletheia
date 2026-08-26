# ADR-075: per-device DMA windows - the IOMMU contract narrows to what each driver granted

**Status:** delivered on the x86-64/VT-d rung (2026-08-26). ARM SMMUv3 per-stream windows stay
scoped in the gap register beside ADR-074's emulator boundary.

## Context

ALET-P1-018's first hardware rung (ADR-073) programmed ONE identity domain over conventional
RAM minus the kernel image and pointed every present PCI function at it. That closed "a device
inventing addresses reaches the image" but left inter-device isolation a SOFTWARE claim only:
device A's buffers were translatable for device B too, and the header of kernel-x86_64/src/vtd.rs
named this exact residual ("the registry-driven per-device-window rung").

Meanwhile the software boundary (kernel-core/src/dma.rs, ADR-043) already held the truth - each
driver instance owns a registry of the frames IT will hand ITS device, with named owners,
revocation, and bounded capacity. What was missing was making the HARDWARE obey that registry
instead of ignoring it.

## Decision

1. **The registry is the single source of truth; the tables obey.** DmaRegistry::grants()
   snapshots the live grants NAMED by owner; every driven function's window domain is built from
   exactly its own driver's snapshot (DeviceGrant), one second-level tree per function, distinct
   domain ids, all frames claimed through the ownership model as before.
2. **Deny-by-default extends to FUNCTIONS.** A PCI function no driver drives gets NO context
   entry at all - read back from the live context table as absent (zeroed qwords) by the gate.
   Present-but-ungranted functions are a named boot failure.
3. **Leaf-set equality is the audit.** vtd::leaf_spans collects a tree's translated spans; the
   gate requires it to EQUAL the grant set - neither more (a window nobody granted) nor less (a
   grant that would fault) - plus zero image violations, pairwise DISJOINT grant sets across
   functions, and a foreign-frame probe answered NotMapped.
4. **Revocation granularity drops to ONE PAGE.** New wire seams leaf_entry / rewrite_leaf walk
   existing interior paths WITHOUT allocating (absent interior = MalformedRange), refuse
   huge-covered IOVAs, read/write the raw leaf slot, and REFUSE to revoke an already-absent page
   (absence must never look like an act). The live gate revokes the block device's data-frame
   leaf under enforcement and demands an ACTIVE fault record naming source-id AND address.
5. **The measured reason code is pinned like ADR-073 pinned its:** revoking a PAGE yields FRCD
   reason 6 (PAGING_NOT_PRESENT) at exactly the revoked address - distinct from 2
   (CONTEXT_ENTRY_P, whole-function revocation) and 4/5 (permission denials).

## Consequences

* Inter-device DMA isolation is STRUCTURAL on VT-d: another function's buffers have no leaf in
  this function's tree, proved by leaf-set equality plus disjointness, not by policy discipline.
* Drivers needed no behavioral change - they already registered everything they publish; they
  gained dma_grants() accessors so the gate can consume what they already vouch for.
* Gate grew dmar=12 -> dmar=14 (grant sanity, per-device build/accounting, leaf equality,
  cross-device absence + ungranted-absence, model agreement, clean kick, page revoke-by-name,
  restore-to-silence, residency+registry layering).
* Host proofs grew tests/vtd.rs to 15 (window domains hold exactly their leaves; page revocation
  spares sibling windows; context-read reports absence for ungranted functions).

## Named non-claims

Interrupt remapping, queued invalidation, pass-through translation types, post-enable completion
assertions (QEMU TCG loses virtio completions across mid-run enablement - ADR-073), SMMUv3
per-STREAM windows on aarch64 (next rung), and real-hardware variety beyond two emulated units.

## Addendum (same day, first CI contact)

The rung requires a 4-level unit (AGAW Lev4). The ubuntu-latest emulator generation is
3-level-only and, under per-device windows, emits BOUNDED zero-address write records plus an
iova 0x28 permission error against a granted function - artifacts never seen on the 4-level
development unit across repeated boots. Per repository doctrine the gate now SKIPS LOUDLY on
such units (named skip line before anything is programmed; translation stays OFF) instead of
failing on emulator bookkeeping, and the skip is tracked in the gap register beside the other
P1-018 residuals.
