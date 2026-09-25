# ADR-175 — A photograph behind the desktop

**Status:** Accepted (2026-09-25)
**Requirements:** REQ-GFX-012 (new)
**Builds on:** ADR-078 (the composition contract meets scanout), ADR-084 (managed windows),
ADR-063 (the boot heap never frees).

## Context

The operator asked for photorealistic graphics. The desktop is a 2D compositor over a virtio-gpu
framebuffer, 640x240, and every compositor surface is 1-bit: a pixel is ink or paper, and the
sink turns ink into white and paper into black. There is no 3D, no GPU acceleration and no alpha.
Photorealistic rendering is not reachable on this compositor. A real photograph shown as the
desktop's backdrop is.

## Decision

* **The photo.** NASA ISS007-E-17719, sunrise over the Earth's limb from the ISS (public domain).
  Cropped to 8:3, scaled to 640x240, packed BGR at `kernel-core/assets/wallpaper-640x240.bgr`
  (460,800 bytes), included with `include_bytes!`. It lives in the kernel image, not on the heap,
  so the 16 MiB boot heap and its 2 MB margin are untouched. A `const` assertion ties its length
  to the scanout.
* **Colour by origin, not by format.** `Raster` gains `put_from(surface, x, y, ink)`, whose
  default is the old `put`. `blit_region` calls it with the placed surface's id. Only the
  desktop's `ComposeSink::with_wallpaper(PANEL, WALLPAPER)` overrides it: a PAPER pixel read from
  the wallpaper panel takes the photo's colour. Ink pixels stay white, and every other surface
  (windows, taskbar, menus) stays 1-bit. Surfaces, the compositor's model, damage tracking and
  storms are unchanged.
* **Readback stays 1-bit.** `Surface::get` now means "is exactly `FG`" rather than "first byte is
  not zero", and the asset caps every channel at 0xFE. A photo pixel therefore reads back as
  paper, and every existing pixel assertion keeps its meaning.

## Proof

* Host: `a_wallpaper_surface_shows_its_photograph_only_in_its_own_paper_pixels`
  (`kernel-core/tests/compfb.rs`): photo in the wallpaper's paper, white in its ink, plain paper
  in a window above it, zero refusals, readback says paper.
* Live: `scripts/desktop-e2e-dt.sh` dumps the running scanout through QMP `screendump` and
  requires at least 25% of pixels to equal the photo's pixel at the same place. Measured:
  aarch64 45.2%, riscv64 43.8%. A 1-bit desktop matches almost none.
* vm-e2e on aarch64, riscv64 and x86-64, and the vinput, keyboard and console gates, all PASS.

## Consequences

**Good.** The desktop shows a real photograph on all targets that run it, with no heap cost and
no change to the compositor's contract.

**Costs.** The kernel image grows by 460,800 bytes (the x86-64 bootable payload measured
2,174,976 B before and 2,634,240 B after).

**Not claimed.** No photorealistic rendering, no colour windows, no alpha, no scaling: the
photo is fixed at the scanout's size. Colour surfaces would change every surface's storage by
32x and are a separate decision.
