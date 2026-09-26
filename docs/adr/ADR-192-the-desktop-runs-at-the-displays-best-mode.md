# ADR-192 — The desktop runs at the display's best mode

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-015 (new)
**Builds on:** ADR-191 (EDID), ADR-077/078 (the desktop's resource and composition), ADR-175
(the wallpaper photograph).

## Context

ADR-191 made the display say what it can show; the desktop still ran at a compiled-in 640x240 on
every monitor.

## Decision

* `desktop::choose_geometry(best, free_frames)`: the EDID best fit when it is at least 640x240 (the
  layout's minimum), within the GPU resource bounds (4096 px per side, 4 Mpx) and the compositor's
  surface ceiling, and backed by at most a quarter of the machine's free frames; otherwise 640x240.
  Each target logs the choice (`[desktop] 2560x1440 (3600 backing pages; display best ...)`).
* `Desktop::install` takes the geometry; the resource, the full-screen panel surface, the
  compositor, the pointer decoder (tablet coordinates scale to the real size), the cursor's start,
  the Alt+Tab switcher and the chrome metrics (the taskbar sits on the real bottom edge) all use it.
* The wallpaper photograph is sampled nearest-neighbour to any size.
* The compositor's per-surface ceiling rises from 1 Mpx to 4 Mpx (1 bit per pixel: 512 KiB),
  matching the GPU driver's resource bound.
* ATTACH_BACKING coalesces adjacent frames into `(address, length)` runs and registers one DMA
  region per run (a 2560x1440 desktop is 3,600 pages, which fit neither 160 entries nor the
  192-region DMA registry as pages); the targets sort the pages they allocate (the surface and the
  device read them in the same order), and the 2560x1440 backing becomes ONE run.
* The live gates pin their monitor at 640x240 (`-device virtio-gpu-*,xres=640,yres=240`), so the
  display itself says 640x240 and their recorded pointer coordinates keep their meaning; nothing
  in the kernel knows it is being tested.

## Proof

QEMU aarch64, `-device virtio-gpu-device,xres=2560,yres=1440`: `[desktop] 2560x1440`, `backing in
1 contiguous run(s)`, `LIVE ... 5 managed windows`; a screendump is a 2560x1440 frame with the
photograph filling the screen and the panel on the bottom edge. The boot gates on all three CPUs and
the desktop gates (vinput, console, keyboard, desktop-e2e-dt, browser, desktop fuzz) pass at their
pinned 640x240.

## Non-claims

* The UI is not yet SCALED: text is still 8x8 cells and windows keep their 640x240-era sizes, so at
  2560x1440 they are small in one corner. UI scaling is the next wave.
* The mode is chosen once at boot; there is no mode switch while running and no hotplug.
* No refresh-rate control: virtio-gpu has no vblank, so the EDID refresh is reported, not paced.
