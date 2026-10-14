# ADR-259 — A two-level SMMU stream table

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-018 (new); advances ALET-P1-018
**Builds on:** ADR-256 (legal STEs, a linear table aligned to its size), ADR-257 (device-side
enforcement invariants), ADR-258 (HDA filled the last of the linear table's eight device slots).

## Context

ADR-256 made the aarch64 stream table one linear 4 KiB frame of 64 STEs, the only linear table a
single frame can hold aligned to its size. It covers bus-0 devices 0..7; ADR-258's HD Audio
controller took slot 7, so any further PCI device on the aarch64 gate would be refused by name at
SMMU invariant 7. A linear table over all 256 bus-0 requester ids needs 16 KiB aligned to 16 KiB,
which the frame allocator does not hand out.

## Decision

* **A two-level table** (STRTAB_BASE_CFG.FMT = 01b) when the unit offers it (IDR0.ST_LEVEL != 0):
  `build_strtab_2level` takes one level-1 frame and one level-2 frame per descriptor, each level-2
  table 2^6 STEs = 4 KiB (SPLIT 6), naturally aligned; the level-1 descriptors carry L2Ptr and
  SPAN = SPLIT + 1. LOG2SIZE 8 covers every bus-0 requester id with four level-2 frames.
  `strtab2_slot` finds an STE through its level-1 descriptor (refusing an id past the table or a
  descriptor that is not this layout); `rewrite_ste_2level` writes through it with the same
  never-torn VALID protocol; `Controller::set_strtab_2level` refuses a level-1 base not aligned
  to its size (at least 64 bytes).
* **A unit without two-level support keeps ADR-256's linear frame** and the by-name refusal of
  requester ids past 63.
* The aarch64 suite builds the table before the frame-balance sample, so invariant 5's "the walk
  claimed exactly the frames it built" still compares like with like.

## Evidence

* aarch64 gate: `[smmu] stream table: two-level over 256 requester ids`. A ninth PCI function
  (virtio-rng-pci, 00:08.0, rid 0x40) is granted where the linear table would have refused it,
  and the probe virtio-blk-pci now sits at 00:09.0 (rid 0x48, the SECOND level-2 frame):
  invariants 11-13 (DMA completes quiet, a revoked STE is C_BAD_STE at 0x48, restore returns the
  bytes) hold through it. `smmu` = 13, twice. `usbkbd-e2e` (aarch64 leg), `desktop-e2e-dt` and
  `hda-e2e` pass on the two-level table.
* Host (`kernel-core/tests/smmu.rs`, 19 tests): the CFG and level-1 encodings pinned to the
  architecture (FMT/SPLIT/LOG2SIZE, SPAN = 7, SPLIT 7 refused, a misaligned level-2 refused); a
  simulated device walker that follows level-1 descriptors translates streams 0x08, 0x48 and 0xF8
  in three different level-2 frames, denies an ungranted neighbour by name, revokes 0x48 without
  touching 0x08, and refuses an id past the table; the controller publishes only an aligned
  level-1 base.

## What this is not

* Not per-stream windows: every granted stream still shares the identity domain.
* Bus 0 only (LOG2SIZE 8): a PCI bridge with secondary buses would need more level-1 entries.
