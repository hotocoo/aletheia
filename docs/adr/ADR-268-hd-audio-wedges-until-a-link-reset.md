# ADR-268: HD Audio wedges until a link reset

**Status:** Accepted (2026-11-19)
**Requirements:** REQ-DRV-022 (new)
**Builds on:** ADR-258 (the HD Audio driver), ADR-266 and ADR-267 (the same rule for NVMe, AHCI,
virtio-net and virtio-rng).

## Context

`Hda::verb` writes one verb into the CORB and waits for the RIRB write pointer to reach the entry
after the last one it read. When that wait ran out, or the entry that arrived was unsolicited or
from another codec, the driver returned an error and went on, but the codec still owed the answer.
It would arrive in the very RIRB entry the next verb waits on, and be read as that verb's answer: a
widget's parameter, a connection list, or a pin's capabilities that belong to a different question.

## Decision

* **Wedged until a link reset.** A verb that times out, or reads an unsolicited or another codec's
  entry in its place, sets `wedged`; every later verb is refused with `HdaError::Wedged` before
  anything is written to the CORB.
* **`Hda::reset`**: stop the output stream, then the same bring-up `init` uses (extracted as
  `start`): rings stopped, CRST low then high (which ends every verb a codec owed and restarts both
  ring pointers, so the driver's restart at 0), codecs rediscovered, CORB and RIRB programmed on the
  SAME ring frame and running. Nothing is allocated. The codecs' widget state is reset too, so an
  output path is configured again after it.
* **A boot invariant, deterministic** (`hda=6 -> 7`, x86-64 and aarch64): the suite sets the wedge
  directly, requires a parameter read refused with no verb written, resets, and requires the codec to
  answer the vendor-id read with the vendor it gave before. The path is configured and the tone played
  AFTER the reset, so `hda-e2e.sh`'s capture is also the proof that the controller works after one.

## Evidence

* `[hda] ALL 7 HDA INVARIANTS HOLD` on aarch64 and x86-64; `HDA-E2E: PASS` (the WAV the x86-64 boot
  wrote is the 750 Hz tone, played after the link reset).
* `kernel-core/tests/hda.rs`: `codec_answers_that_lie_are_refused_by_name` now also requires the
  wedge and that a wedged controller writes no verb (silent, wrong codec, unsolicited);
  `a_link_reset_brings_a_wedged_controller_back` times a verb out, then finds the output path
  after the reset. Removing the wedge check fails 3 of the 8 tests.
* Gates: `vm-e2e.sh`, `vm-e2e-x86.sh`, `hda-e2e.sh`, quality gate, every `check-*.sh`.

## What this is not

* **virtio-gpu and xHCI still return a timeout and go on.** The GPU's reset drops its resources and
  scanout; xHCI's command and event rings are their own rung. Named.
* **Nothing outside the boot suite calls `reset`**; the console does not play audio.
* **A stream that stalls** (`StreamStalled`) is stopped, not wedged: stopping the stream ends its DMA,
  and the verbs that follow do not depend on it.
