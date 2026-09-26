# ADR-197 — Only what changed reaches the display

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-018 (new)
**Builds on:** ADR-078 (damage-bounded composition), ADR-192/194 (large, scaled desktops).

## Context

The operator reported the desktop "laggy". The compositor already composes only damaged regions,
but every changed frame was then handed to the device WHOLE: `TRANSFER_TO_HOST_2D` and
`RESOURCE_FLUSH` of the full framebuffer, 8 MB at 1920x1080, 14 MB at 2560x1440 — for a cursor move.

## Decision

* `ComposeSink` records the device-pixel rects it writes (up to `MAX_DIRTY` = 8 separate rects,
  collapsed into one bounding box past that) and the desktop transfers and flushes exactly those.
* `kernel-core/src/kheap.rs`: a freeing kernel heap (power-of-two size classes up to 4 KiB with
  intrusive free lists, page-granular first-fit runs that split and coalesce, bump for fresh memory;
  gross bytes still counted so every "allocates nothing" storm keeps its meaning), proved on the host
  (a 200,000-operation seeded workload: no overlap, alignment honoured, everything freed is available
  again) and by a 5-invariant suite. It is NOT yet the global allocator on any target — named below.

## Measured (host, `kernel-core/tests/frame_cost.rs`, 1920x1080 at 2x)

A full-screen frame composes in 12.0 ms; a cursor move composes in 1.7 us and sends 512 pixels
(2 KiB) to the device instead of 2,073,600 pixels (8 MB). All nine boot and desktop gates and the
quality gate pass.

## Non-claims

* Wiring `kheap` in as each target's `#[global_allocator]` (behind a spin lock taken with interrupts
  masked) is the next step; until then the ADR-196 switch limit stands.
* Under QEMU TCG the guest itself is emulated, so the desktop is slower than the same code on
  hardware; this wave removes the avoidable device traffic, it does not make the emulator fast.
