# ADR-043 — What a device is allowed to touch

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P1-018 · REQ-DRV-006 · sharpened by ADR-037, which enabled PCI bus-master

## Context

Every driver in this kernel hands a device a **raw physical address** and trusts it to write only there.
Nothing checked the address. ADR-037 made that gap concrete rather than theoretical by enabling bus-master
on the virtio-pci function: a descriptor with a wrong address is now a device writing wherever the number
points — kernel text, another task's frame, a page table — and the memory model (ownership, W^X, guard pages)
sees none of it, because none of those checks are on the path where a number becomes a descriptor.

The eventual answer is an IOMMU/SMMU. That is real hardware work and ALET-P1-018 exists for it. The question
this decision answers is what is *honest to build without one*.

## Decision

**A device-visible memory boundary, enforced by the kernel at the choke point where addresses become
descriptors** (`kernel_core::dma`):

1. **A driver registers what it intends a device to reach**, naming itself owner. Registration applies
   admission rules: page-aligned, non-null, and **never overlapping the kernel image**. A device writing into
   kernel text is the write-to-code path W^X closes, arriving from the other side.
2. **Deny by default.** `visible()` is false for anything nobody registered, and false for a range that
   extends past its registration — partial visibility is not visibility.
3. **One frame, one owner.** A second owner registering a frame someone else holds is refused: two drivers
   pointing one device at one frame is a bug in the same way a double free is.
4. **Revocation ends visibility**, and revoking twice is refused. A frame returning to the allocator stops
   being something a device may be told about — the DMA twin of erase-on-free (ADR-033).
5. **An undeclared image span is visibly unenforceable, not silently permissive.** `image_declared()` is
   false until a target declares its span, and the boot invariant *checks that*, so a target that forgets
   fails a check rather than losing the rule quietly.
6. **Every refusal is counted**, so a boot can report that the boundary did work rather than being silent.

Nine invariants, run identically on all three targets (`ALL 9 DMA-BOUNDARY INVARIANTS HOLD`, boot failing
`240 + i`), with three of them in the `conformance.sh` core contract (69 → 72): what the kernel may tell a
device is policy, not a hardware property, so it must not vary by CPU.

## Consequences

* The rule "a device is only ever told about memory a driver registered for it" now exists, is checked, and
  is auditable.
* **Explicitly NOT claimed, and this is the important part.** This is a *software* boundary: it constrains
  what the **kernel** tells a device, which is where every wrong address in this codebase would come from. It
  cannot constrain a device that invents its own addresses — a malicious or broken device still needs an
  IOMMU, and **ALET-P1-018 stays open** for that. **The gate is live in `virtq`** (addendum below); `virtioblk`'s own
  single-queue path still publishes descriptors directly and is the remaining wiring.

## Addendum (2026-08-03) — the boundary became a gate

`Virtqueue` now owns a `DmaRegistry`: its ring frame is registered at setup, a caller must
`register_buffer` before naming an address, and **`add` refuses an address that is not visible** — so the
check sits exactly where a number becomes a descriptor, which is the only place a wrong one could escape. The
virtio-net driver registers its receive and transmit buffers before posting them, and a new invariant on all
three targets proves the gate **denies by default**: an address far from any buffer is refused, and so is one
that overruns a registered buffer (network invariants 4 → 5, conformance 72 → 73).

`virtioblk` now carries its own registry too — it predates `virtq` and keeps a fixed ring, so it would
otherwise have been the one path in the kernel still naming unregistered addresses. Its ring and data frames
are registered at init, and `request` checks the header, status byte and data buffer **before** any of them
becomes a descriptor. Its suite asserts the gate denies by default, and — like the geometry check — takes the
answer from the DRIVER rather than assuming it, so a device whose gate stopped working fails instead of the
suite passing on a default (virtio-blk invariants 20 → 21, conformance 73 → 74).

Still not claimed: nothing constrains a device that **invents its own addresses** — that needs an
IOMMU/SMMU, and **ALET-P1-018 remains open** for it. A blanket `impl BlockDevice for &mut D` was added so a
suite can keep using a device after lending it to something that takes ownership; without it the choice
between the two would have been made by the type system rather than by what the test needs to prove.

## Alternatives considered

* **Wait for an IOMMU and do nothing.** Rejected: the bug this prevents is a kernel-side arithmetic error,
  which an IOMMU also catches but which we can catch now, in the place the error is made.
* **Claim DMA isolation and close ALET-P1-018.** Rejected — that is exactly the overclaim `docs/MATURITY.md`
  exists to prevent. A software boundary is not device containment.
* **Check addresses inside each driver.** Rejected: three copies of one security-relevant rule, which is the
  duplication ADR-036's reasoning already rules out.
* **Reuse `vmaddr`'s admission check.** Rejected: that check answers "may the KERNEL map this?" — its
  physical rule is about the frame allocator's window. A DMA target is a different question with a different
  owner model, and ADR-037 already showed what happens when one check is asked to mean two things.
