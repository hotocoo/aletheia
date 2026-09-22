# ADR-152 — A VMDK normalizer that keeps its length

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-QUAL-008 (release reproducibility), REQ-QUAL-006 (the packaged disks boot)
- **Supersedes:** the whole-file splice in `scripts/normalize-vmdk.py`.

## Context

The VMware packager converts each raw disk image to a VMDK with `qemu-img`, which stamps every VMDK
with a RANDOM 32-bit content identifier, printed in the embedded text descriptor as an UNPADDED
hexadecimal integer — so one time in sixteen the CID has seven digits instead of eight.
`normalize-vmdk.py` replaced it with eight digits derived from the payload so two builds of one
commit produce one package. It did so by splicing the WHOLE FILE around the match.

When the random CID had seven digits, the file grew by one byte. A monolithic-sparse VMDK locates
its grain directory, grain tables and grains by sector offsets from its header; every one of them
was now one byte past where the header said. QEMU read the disk as garbage, OVMF found nothing to
boot and fell through to PXE, and the packaged selftest disk hung until the 240-second watchdog —
intermittently, on the runner, with a serial log the packager did not print. Two builds of one
commit disagreed about a VMDK's digest for the same reason, at the same one-in-sixteen rate. The
diagnostics that would have named this ended under `set -o pipefail` at their first `cmp`
(fixed alongside ADR-150). Three intermittent CI failures, one byte.

## Decision

- **The replacement happens inside the descriptor region the sparse header declares**, and the
  file length never changes: the descriptor is text followed by zero padding, and a longer CID
  consumes one byte of padding. The header is parsed (`descriptorOffset`, `descriptorSize`), the
  region's text is found up to its first NUL, the CID is replaced there, and the region is
  re-padded to exactly its size.
- **The normalizer proves its own invariant.** `--self-test FILE` takes a real VMDK, forges the
  seven-digit shape qemu-img emits, and requires both shapes to normalize to identical bytes of
  identical length. The packager runs it on every build.
- **The packager verifies each VMDK against the raw image it was converted from** with
  `qemu-img compare` before it packages anything: a VMDK that no longer holds its image is refused
  by name, never zipped, never booted, never published.
- **The packager verifies its manifest** against the staged files before zipping (ADR-150's
  companion fix), so a digest that does not describe what ships fails there.

## Alternatives considered

**Padding the CID to eight digits in the regex and accepting the rare drift.** Rejected: the drift
was never rare in effect — it was a disk that did not boot.

**Dropping the CID normalization.** Rejected: the CID is the one random byte-string in an otherwise
deterministic package, and reproducibility is a gate, not a preference.

## Consequences

The reproducibility gate and the packaged-boot gate are deterministic again. The runner's other
intermittent failure this day, the live-desktop gate not reaching its prompt in time, is not
explained by this ADR and is still open.
