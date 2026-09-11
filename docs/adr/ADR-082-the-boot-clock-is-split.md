# ADR-082 — The boot clock is split: firmware is not the kernel

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PERF-001 (advanced)
**Builds on:** ADR-081 (a parked machine costs nothing), ADR-056 (the honesty rule).

## Context

The comparative benchmark had one column Aletheia lost, and it had carried a caveat in prose for as
long as it had existed: Aletheia boots through OVMF, a full UEFI firmware implementation, while the
Linux leg is loaded directly by QEMU's `-kernel` and skips firmware entirely. Comparing their
totals compares two different boot paths.

ADR-081 said that out loud and then refused to spend it: *"splitting Aletheia's total into a
firmware share and a kernel share is the obvious next piece of work and has not been done, so no
part of that gap is currently excused."*

A caveat that excuses a loss without measuring it is worth nothing. Either the firmware share is
real and can be shown, or the caveat should be deleted.

## Decision

`boot_and_measure` takes an optional `SPLIT_MARKER`: the line a guest prints the moment it owns the
machine. For Aletheia that is `calling ExitBootServices`. The harness timestamps that line as well
as the prompt, so one boot yields two numbers — the share spent in firmware and the share spent in
the kernel this project wrote — medianed over the same `BOOT_SAMPLES` runs as everything else.

The marker is cleared before the Linux leg, because Linux has no firmware share here: `-kernel`
loading is the start of its own work, so its total *is* its kernel share.

## Result

Three independent runs (`docs/evidence/perf001`):

| | Aletheia | Linux 6.12-lts |
|---|---|---|
| boot to a prompt (total) | 2507 / 2516 / 2509 ms | 1790 / 1780 / 1786 ms |
| of which firmware (OVMF) | 1431 / 1429 / 1427 ms | none (`-kernel`) |
| **of which this kernel** | **1076 / 1087 / 1082 ms** | **1790 / 1780 / 1786 ms** |

The caveat was real, and it was most of the gap. On the part each project actually wrote, Aletheia
reaches an interactive prompt in ~1082 ms against Linux's ~1786 ms — **about 1.65x faster** — while
still losing the total by ~0.72 s because it pays for UEFI and the other leg does not.

Both statements are true and neither replaces the other. The table now prints all three rows so a
reader cannot take one without seeing the others.

## Consequences

* **The total-boot column is still LOST, and is still reported as lost.** Splitting a number does
  not win it. A machine that boots through firmware takes longer to reach a prompt than one that is
  handed the CPU, and that is the honest end-to-end experience.
* **Named non-claim on the split itself.** The two kernel shares are much closer to like-for-like
  than the totals were, but they are not identical work: Linux's 1786 ms includes QEMU loading and
  decompressing a 14.16 MB kernel-plus-initramfs payload, while Aletheia's 1082 ms starts from a
  1.44 MB image the firmware has already placed in memory. Part of the difference is the payload
  difference, which the `bootable payload` row prices separately. Anyone quoting the 1.65x owes the
  reader that sentence.
* **This did not change the kernel.** No boot path was optimized here; the same binary was measured
  more carefully. The work of actually making Aletheia's own boot faster is untouched and
  unclaimed.
* **The idle and round-trip columns are unaffected.** They never involved firmware.
