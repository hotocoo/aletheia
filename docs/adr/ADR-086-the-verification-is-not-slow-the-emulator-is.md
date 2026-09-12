# ADR-086 — The verification is not slow; the emulator is

**Status:** Accepted (2026-09-12)
**Requirements:** REQ-PERF-001 (advanced)
**Builds on:** ADR-085 (the boot gap is self-verification), ADR-084 (a second operating system),
ADR-056 (the honesty rule).

## Context

ADR-085 attributed Aletheia's ~1047 ms kernel share and found that ~735.6 ms of it — about 70% — is
the kernel running its own invariant suites. It named the obvious next move and its one constraint:

> If a real optimization target exists it is now visible — the two ML advisor suites cost ~291 ms
> between them — but shaving them means making verification **faster**, not making it **optional**.

This wave went to shave them, and found there is nothing there to shave.

## What was measured

The largest single gap in the boot profile is ~147 ms, and it does not sit inside an invariant
check at all. It follows the line `[mlsched] ALL 12 LIVE-ADVISORY INVARIANTS HOLD`, which means the
work happens *after* the suite finishes — in `mlsched::commission(4_096, 7)`, the commissioning run
that admits 4,096 real tasks through both an advised and a model-free scheduler and proves the
advised drain is a permutation of the model-free one.

So the first correction is to ADR-085's own wording: a large part of what it called
"self-verification" is more precisely a **commissioning workload**, not an invariant check. The
distinction matters, because a workload's cost scales with how much work you asked for.

The second question was whether that workload is quadratic. It is not. Timed natively, in release,
on the host:

| tasks | time |
|---|---|
| 512 | 0.37 ms |
| 1,024 | 0.69 ms |
| 2,048 | 1.39 ms |
| 4,096 | **2.64 ms** |

Linear, cleanly. And **2.64 ms** — against ~147 ms for the same call during boot.

## Decision

Record the finding and change nothing in the kernel.

The benchmarked image is built `--release` (`opt-level = 3`, `lto = true`), so the gap is not an
optimization-level artefact. The same linear, 2.64 ms computation takes ~147 ms inside the boot,
which is roughly **56x** — squarely in the normal range for QEMU TCG on compute-bound code.

There is no algorithmic win available here. The verification is not slow. The emulator is.

## Consequences

* **The boot comparison is distorted in a third way, and this one favours Linux.** Aletheia's
  self-verification is *compute-bound*: tight integer loops through two schedulers and two decision
  forests, which is exactly what TCG emulates worst. A Linux boot is dominated by device probing,
  firmware tables and I/O, which TCG handles comparatively well. So the ~735 ms that ADR-085
  attributed to self-verification is a **TCG number**, and on real silicon it would shrink by far
  more than Linux's boot would.
* **That is a reason to be careful, not a reason to claim a win.** One component was measured
  natively — `mlsched::commission`, at 56x. The other ~588 ms of self-verification was **not**
  measured natively, and no native boot of either system exists to compare. Extrapolating 56x
  across the whole share would be inventing a number, which is the thing this repository does not
  do. The honest statement is: the absolute boot figures in ADR-082 and ADR-085 are properties of
  the measurement environment as much as of either kernel, and closing that requires hardware.
* **ADR-085's phrasing is corrected, not withdrawn.** Its ~70% figure stands; what changes is that
  a large part of it is a commissioning *workload* rather than invariant *checks*, and that the
  workload is linear rather than quadratic.
* **No code changed.** The suites keep their sizes, the schedulers keep their algorithms, and the
  kernel is byte-for-byte what it was. This wave produced a measurement and a correction.
* **The obvious real optimization is now known to be absent.** Anyone returning to "make the boot
  faster" should start from hardware, not from the suites.
