# ADR-258 — An HD Audio driver plays a tone

**Status:** Accepted (2026-10-15)
**Requirements:** REQ-DRV-017 (new)
**Builds on:** ADR-223 (queued drivers: HD Audio), ADR-253 (xHCI: the house shape of a PCI driver
with a lying simulator), ADR-228 (drivers safe outside a gate), ADR-256 (the one-frame stream
table on aarch64).

## Context

Aletheia had no sound. Intel High Definition Audio is the controller interface real PCs, VirtualBox
and QEMU share; ADR-223 queued it.

## Decision

* **`kernel_core::hda`**, one shared driver, generic over a new `HdaRegs` seam (8/16/32-bit
  register access: HDA's map mixes widths, and a 32-bit store to a byte register would write its
  neighbours on silicon) and `VirtioHal`. Facts were taken from the HDA specification rev 1.0a and
  checked against QEMU's `intel-hda.c` / `hda-codec.c` before use.
* **Bring-up**: rings stopped, CRST low then high with bounded waits, a millisecond for codecs to
  announce in STATESTS, interrupts off, CORB (256 entries) and RIRB (256) in one DMA-gated frame,
  the CORBRP reset handshake, RIRBWP reset, RINTCNT 1 with RIRB status reporting on (QEMU holds
  CORB after RINTCNT responses until that status is cleared; INTCTL stays off, so nothing
  interrupts the CPU).
* **Every verb is answered or refused**: a response must be solicited and carry the codec address
  asked; otherwise `Unsolicited` / `WrongCodec`; none within the budget is `Timeout`.
* **The codec is walked, not assumed**: root node count, an audio function group, its output
  converters and output-capable pins, and a pin whose connection list reaches a converter directly
  or through one widget. The vendor id is printed, never pinned.
* **Playback**: D0 on the path, 48 kHz / 16-bit / stereo, stream tag 1, pin output enabled, amps
  unmuted, EAPD on; the first OUTPUT stream descriptor (after GCAP's input ones) runs a two-entry
  BDL, one frame each with IOC, looping a 750 Hz triangle tone (64 samples a period, a whole number
  of periods per buffer). Buffer completion is cleared by a byte write to the status register: QEMU's
  32-bit CTL write stores status bits as given, so clearing through CTL would set them (the first
  boot counted 451255 "buffers" in 250 ms that way; the simulator now models QEMU's behaviour and the
  test bounds the count).
* **ADR-228**: no controller, no codec, or no output path is a named skip; the suite ends with the
  stream stopped and the controller held in reset, and dropping a controller does the same.
* Polled.

## Evidence

* Boot: new family `hda` = 6 on aarch64 (00:07.0) and x86-64 (00:08.0): codec 0 vendor
  0x1af40012, AFG 1, DAC 2, pin 3; 13 buffers completed in 250 ms (21.3 ms each). VT-d 14/14 and
  SMMU 13/13 unchanged; riscv64 has no PCI and no family.
* **Audible**: `scripts/hda-e2e.sh` (new CI job) boots aarch64 with QEMU's `wav` backend and reads
  the capture: 0.243 s of signal, RMS 3931, 1496 zero crossings a second (750 Hz within 10 %).
* Host (`kernel-core/tests/hda.rs`, 7 tests): a simulated controller and three-widget codec;
  the suite plays and reports the path and a bounded buffer count; the format, stream tag and pin
  verbs are the ones sent; both BDL entries are IOC buffers holding the tone; no codec and an
  input-only codec are skips; a silent codec, an answer from codec 1 and an unsolicited answer are
  refused by name; a stream whose position never moves is refused and stopped; a controller with
  no output stream, or one whose registers leave the window, is refused.

## What this is not

* No mixer, no volume control, no input/capture, no sample-rate choice, no interrupts, no
  application can play anything yet (the tone is the boot suite's).
* Never run on a physical codec; QEMU's `hda-output` is the only one met. Real codecs often need
  EAPD/GPIO quirks this driver does not have.
* **aarch64 stream table is now full.** With HDA at device 7 the one-frame table of ADR-256 covers
  every slot; the next PCI device on the aarch64 gate (igc, say) needs a two-level stream table
  first, or the SMMU suite refuses it by name at invariant 7.
