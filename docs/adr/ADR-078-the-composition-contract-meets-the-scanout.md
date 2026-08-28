# ADR-078: The composition contract meets the scanout — real pixels over the virtio-gpu flush path

**Status:** Accepted · **Date:** 2026-08-28 · **Advances:** ALET-P2-021 (the real-pixel rung;
GPU isolation between surfaces stays scoped) · **Builds on:** ADR-077 (the composition
contract), ADR-057 (graphics is a device that parses us), ADR-058 (the framebuffer console's
scatter-gather backing), ADR-075 (per-device DMA windows — the backing pages are granted, not
ambient), ADR-064 (the machine measures itself)

## Context

ADR-077 defined the composition contract as a complete software model: owner tokens decide WHO
may draw, the scanout is a structural bound on WHERE. What it deliberately did not do was put a
composed frame where a human could see it — the gap register named the missing rung: "routing
the compose sink at the live fbcon backing pages + one virtio-gpu flush per frame". The risk of
leaving it modeled forever is the one ADR-071 named for the IOMMU: a contract with no device leg
can drift into describing a machine nobody boots.

## Decision

### The device carries the verdict; it does not make it

`ComposeSink` (`kernel-core/src/fbcon.rs`) implements the compositor's `Raster` over a
`fbcon::Surface` — the same scatter-gather page list a virtio-gpu 2D resource names as its
backing store. The model's structural bound and the raster's bounds must agree exactly, and
that agreement is MEASURED: the sink counts every put and every refusal, and every suite asserts
the refusal count is zero. A non-zero count would mean the model believed it was writing inside
the scanout while the real raster disagreed — precisely the defect class the adapter exists to
surface, and the reason refusals are counted rather than swallowed.

`compose_suite` (`kernel-core/src/virtiogpu.rs`, 8 invariants, boot fails 640+i) then drives the
real device end to end on all three targets:

1. the composed frame's resource exists — created, 150 scatter-gather backing pages attached
   (each registered with the DMA gate, the ADR-075 posture), scanout 0 bound;
2. the first compose lands the owned surfaces in REAL memory — the window's ink border reads
   back through the wallpaper interior it covers, and the sink's own counters agree with the
   model's `FrameStats` (every counted write is a real put, none refused);
3. TRANSFER plus FLUSH of the whole extent — the display device itself answers OK, exactly two
   commands of device traffic;
4. a QUIET frame writes zero pixels AND issues zero device commands (the idle desktop moves
   nothing — measured on the driver's command counter, not assumed);
5. a wrong token changes nothing on the real path — draw, move and raise all refused by name,
   the frame checksum identical, the command counter frozen;
6. a move is visible the same frame — the vacated area reverts to the wallpaper (no ghost), the
   new area shows the window, and the changed frame goes to the device as exactly two commands;
7. the exact clip holds at the real raster's edge — a window pushed 160 px past the right edge
   lands only its intersection, and the sink was never asked for a pixel the raster does not
   have;
8. the z-order flips are visible in real memory (raise the wallpaper and the window's border
   pixel disappears under it; lower it and the border reappears), detach reveals the wallpaper
   for good, the last frame is shown, and the teardown revokes every page's DMA registration —
   the DEVICE confirms the end with ERR_INVALID_RESOURCE_ID.

Geometry is the console's 640x240-over-150-pages shape because that is what the boot allocator
reliably hands out; the contract's promises are geometric, so the rung proves the same
enforcement any scanout size would.

### Host proofs

`kernel-core/tests/compfb.rs` (5 tests) runs `ComposeSink` against real host memory viewed as
4 KiB frames: composed frames read back pixel-exact, moves leave no ghost in real bytes, an
overhanging surface never asks outside the raster, a wrong token changes no real byte, and the
device suite's exact move/clip/raise/lower/detach sequence is replayed on the host with the
readback each flip must produce.

## Consequences

* **Named non-claims.** QEMU's virtio-gpu still enforces nothing about who composes — the
  enforcement lives entirely in the kernel-side contract, layered over the DMA registry; one
  device command pair per changed frame is not interrupt-driven completion; the 1-bit model
  depth is unchanged (no alpha); no cursors, no input routing, no per-surface GPU address-space
  isolation at the device — all scoped in the register.
* **The performance story is now a device-traffic number.** A quiet desktop costs zero writes
  AND zero commands; every changed frame costs exactly one transfer plus one flush. Both are
  asserted on every boot of all three targets.
* **Marker map changed deliberately** (`compose=8` on the three QEMU gates; VirtualBox lists the
  family SKIP with the rest of the graphics stack — no virtio-gpu, ADR-061); the conformance
  contract grew two compose behaviors on all three targets (152 → 154); the unsafe audit grew
  ten named sites for the live-device ops in the suite.
