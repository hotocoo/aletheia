# ADR-194 — The UI scales with the display

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-016 (new)
**Builds on:** ADR-192 (the desktop at the display's best mode).

## Context

ADR-192 ran the desktop at 2560x1440, and exposed the next defect: an 8x8 font and 640x240-era
windows are unreadably small on a high-resolution screen. Every modern OS scales its UI with the
display.

## Decision

* `desktop::ui_scale(w, h)`: one integer step per 540 lines (the font is 8x8, so text lands at
  16-24 device pixels on common displays): 1080p and 1440p are 2x, 2160p is 4x, anything under
  1080 lines is 1x; never so large that the logical desktop falls below the 640x240 layout.
* The desktop is LOGICAL: layout, compositor, surfaces, pointer decoding, chrome and hit-testing work
  at `w/scale x h/scale` (1280x720 on a 1440p screen), and `ComposeSink::with_scale` writes each
  logical pixel as a `scale x scale` block into the device-sized framebuffer; the wallpaper is
  sampled at logical resolution.
* The taskbar spans the logical width (it was a fixed 100-cell grid).

## Proof

* Host: `the_ui_scale_keeps_text_legible_and_the_layout_whole` (the scale table, the layout floor,
  and `choose_geometry`'s fallbacks — too little memory, too small a display, no EDID, past 4 Mpx).
* Live screenshots (QEMU aarch64, QMP `screendump`): `docs/evidence/desktop-1920x1080-scale2.png`
  and `docs/evidence/desktop-2560x1440-scale2.png` — legible text, doubled windows, the taskbar
  across the full bottom edge.
* Every boot and desktop gate passes at its pinned 640x240 (scale 1, the unchanged path).

## Non-claims

* Integer scaling of an 8x8 bitmap font: crisp, blocky, not anti-aliased. A vector font, fractional
  scales and per-monitor DPI are not done.
* Window positions are still the historic logical coordinates (top-left cluster); there is no
  placement policy that uses the extra space yet.
* At 3840x2160 the desktop falls back to 640x240 today: the backing needs 8,100 pages, over the
  4 Mpx resource bound.
