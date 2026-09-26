# ADR-191 — The display says what it can show

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-014 (new)
**Builds on:** ADR-077/078 (virtio-gpu 2D, real-pixel composition).

## Context

Every operating system asks the monitor, not a constant, which resolutions and refresh rates it
supports. Aletheia's GPU driver declined `VIRTIO_GPU_F_EDID`, and its boot suite PINNED the
scanout at 1280x800 — one emulator's default — so a machine with any other monitor failed boot
(`[gpu] FAILED at gpu invariant 4`, reproduced with `-device virtio-gpu-device,xres=2560,yres=1440`).

## Decision

* `kernel-core/src/edid.rs`: a bounded, allocation-free EDID 1.3/1.4 base-block reader. Header,
  checksum and version are checked first (each refusal named); modes come from the four detailed
  timing descriptors (refresh COMPUTED from pixel clock and totals, in millihertz: 59.94 Hz is
  59,939 mHz, not "60"), the eight standard timings (aspect ratio by EDID revision) and the
  established-timing bitmap, deduplicated. The first detailed timing is the PREFERRED mode.
  `best_fit(max_w, max_h, max_area)` picks the preferred mode when it fits, else the largest area,
  then the highest refresh, progressive before interlaced.
* The driver accepts `VIRTIO_GPU_F_EDID` when offered and gains `get_edid` (`CMD_GET_EDID`
  0x010a, answer `OK_EDID` 0x1104, the device-writable window widened to 2 KiB for the 1,056-byte
  answer) and `read_display_facts`, which every target calls at boot; the result is kept in
  `edid::resident` for the console.
* `display` at the console: the scanout geometry, the monitor's name, manufacturer and EDID version,
  every mode with its refresh rate (largest first), the preferred mode and the best fit.
* GPU invariant 4 no longer pins 1280x800: scanout 0 must be enabled and, when the device offers
  EDID, its geometry must BE the monitor's preferred mode.

## Proof

* Host: the boot suite, a sweep that flips every one of the 1,024 bits of a valid block (each is
  refused: nothing corrupted is read), and an exact 59.94 Hz timing.
* Boot: `edid=5` on aarch64, riscv64 and x86-64; `console=58` (`display` answers).
* Live, QEMU `-device virtio-gpu-device,xres=2560,yres=1440`: `[edid] 12 modes, preferred
  (2560, 1440, 75), best fit 2560x1440 @ 74.99 Hz`, and the GPU suite now passes on that machine.

## Non-claims

* The desktop still RUNS at its fixed 640x240 surface: this wave reads the modes; making the
  desktop run at the best one is ADR-192.
* Extension blocks (CTA-861 with more detailed timings, DisplayID) are counted, not read.
* Under QEMU the "refresh rate" is the EDID's statement; there is no vertical-blank interrupt to
  pace against.
