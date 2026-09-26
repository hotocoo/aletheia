# ADR-196 — The resolution changes while the machine runs

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-017 (new)
**Builds on:** ADR-191 (EDID modes), ADR-192 (the desktop at a chosen mode), ADR-063 (the kernel
heap never frees).

## Context

Every desktop OS lets its operator pick a resolution from the modes the display lists. Aletheia
chose one at boot and kept it.

## Decision

* `resolution WxH` at the console (capability `system.display`, a new `ShellAction::Display`; the
  hosted planner classifies it `Destructive`): allowed only for a mode the display's EDID lists (or
  the fixed 640x240), within the same bounds the boot applies (`desktop::mode_allowed`), and
  validated BEFORE anything is touched.
* The switch takes the desktop apart (`Desktop::into_devices`: scanout off, backing detached,
  resource destroyed), returns its frames to the pool, and installs it again at the new mode through
  the same `Desktop::install` the boot uses; if the new mode cannot be installed the old one is.
  The console session, its files and the browser's navigator live outside the desktop and survive;
  the windows' own contents reset.
* **The heap bounds it.** A switch rebuilds the compositor, surfaces and grids on a heap that never
  frees: measured 0.6-1.1 MiB per switch. A switch that would leave less than 4 MiB free is refused
  by name (`the kernel heap (which never frees) cannot afford another switch`).
* aarch64 and riscv64 switch. x86-64 refuses by name: its GPU is behind a VT-d window built from the
  boot's backing pages, and fresh pages would be outside it.

## Proof

Live, aarch64, a 2560x1440 display: `display` lists 12 modes; `resolution 1920x1080` -> `the desktop
now runs at 1920x1080` and a 1920x1080 screendump with the desktop live
(`docs/evidence/desktop-switched-to-1920x1080.png`); `resolution 1234x567` -> refused (not listed);
alternating switches: four succeed (heap free 8.3 -> 5.5 MB), the fifth is refused by the heap
floor, and the machine still answers. Boot: `console=59` on all three CPUs (`resolution` refuses by
name and teaches its syntax); the heap storm includes a refused `resolution` at zero bytes.

## Non-claims

* The number of switches per boot is bounded by the heap (about four from a fresh boot) until the
  kernel heap can free.
* No refresh-rate selection (virtio-gpu has no vblank); no persisted choice across reboots.
