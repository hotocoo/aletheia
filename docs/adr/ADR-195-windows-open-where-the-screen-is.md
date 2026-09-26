# ADR-195 — Windows open where the screen is

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-016 (extended)
**Builds on:** ADR-192/194 (the desktop at the display's size, scaled).

## Context

At 1280x720 logical (a 1440p screen at 2x), every window still opened at its 640x240-era
coordinate: five windows in the top-left third of the screen, the rest empty.

## Decision

* `desktop::place(x, y, w, h)` maps a historic position to the same FRACTION of a `w x h` desktop;
  `place_window` then keeps the window inside the work area (on screen, above the taskbar). At
  640x240 both return the historic coordinate untouched, so every recorded layout and every gate
  that clicks it keep their meaning.
* Used when the desktop opens its five windows and when a closed window is reopened from the taskbar.

## Proof

Host: `window_placement_is_historic_at_640x240_and_proportional_beyond`,
`a_placed_window_stays_above_the_taskbar_and_on_screen`. Live: `docs/evidence/desktop-2560x1440-scale2.png`
(windows spread across the screen, the browser window above the taskbar). All nine boot and desktop
gates and `scripts/quality-gate.sh` pass.

## Non-claims

No tiling, snapping or remembered positions; window SIZES are still the historic cell counts.
