# Boot cost, measured (ADR-162)

Every target proves its contracts at boot, suite after suite, before the console or the desktop is
offered. This page is what that COSTS, read from the machine's own monotonic counter and printed on
every gate's boot log: one `[boot] FAMILY suite: N ms` line under each family's marker, a
`[boot] NAME phase: N ms` line after each timed phase that is not a suite, and one
`[boot] suites: N timed, T ms total, slowest F at S ms` line before the console.

**How to read it.** Numbers are QEMU TCG on one development machine (Apple silicon, 2026-09-27),
uncontended, one boot each; they are RELATIVE - which suite is heavy on which CPU - not a promise
about hardware. `total` runs from the first suite to the summary. `unattributed` is `total` minus the
sum of the laps: time no lap claims (device bring-up between suites, printing).

Regenerate: `python3 scripts/boot-cost-harvest.py A64.log RV.log X86.log > docs/BOOT-COST.md` from the
three boot gates' output.

## aarch64 (QEMU virt, cortex-a72, TCG)

| measure | value |
|---|---|
| laps timed | 61 (61 suites) |
| total, first suite to summary | 4845 ms |
| sum of laps | 4813 ms |
| unattributed (between laps) | 32 ms |
| slowest suite | perf-report at 1527 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `perf-report` | phase | 1527 | 32% |
| 2 | `bench` | suite | 403 | 8% |
| 3 | `fsstorm` | suite | 334 | 7% |
| 4 | `conring` | suite | 317 | 7% |
| 5 | `mlrisk-stress` | suite | 312 | 6% |
| 6 | `smp` | suite | 292 | 6% |
| 7 | `compose` | suite | 259 | 5% |
| 8 | `reclaim` | suite | 247 | 5% |
| 9 | `schedstorm` | suite | 188 | 4% |
| 10 | `usermode` | suite | 144 | 3% |
| 11 | `soak` | suite | 102 | 2% |
| 12 | `tlsclient` | suite | 98 | 2% |

## riscv64 (QEMU virt, rv64, TCG)

| measure | value |
|---|---|
| laps timed | 59 (59 suites) |
| total, first suite to summary | 3074 ms |
| sum of laps | 3048 ms |
| unattributed (between laps) | 26 ms |
| slowest suite | bench at 505 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `bench` | suite | 505 | 16% |
| 2 | `conring` | suite | 350 | 11% |
| 3 | `compose` | suite | 295 | 10% |
| 4 | `schedstorm` | suite | 259 | 8% |
| 5 | `mlrisk-stress` | suite | 238 | 8% |
| 6 | `reclaim` | suite | 210 | 7% |
| 7 | `fsstorm` | suite | 203 | 7% |
| 8 | `usermode` | suite | 132 | 4% |
| 9 | `tlsclient` | suite | 105 | 3% |
| 10 | `tlshandshake` | suite | 99 | 3% |
| 11 | `shellstorm` | suite | 99 | 3% |
| 12 | `trust` | suite | 83 | 3% |

## x86-64 (QEMU q35, OVMF, TCG)

| measure | value |
|---|---|
| laps timed | 61 (61 suites) |
| total, first suite to summary | 5445 ms |
| sum of laps | 5414 ms |
| unattributed (between laps) | 31 ms |
| slowest suite | dmar at 3518 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `dmar` | suite | 3518 | 65% |
| 2 | `mlrisk-stress` | suite | 248 | 5% |
| 3 | `bench` | suite | 196 | 4% |
| 4 | `reclaim` | suite | 171 | 3% |
| 5 | `fsstorm` | suite | 167 | 3% |
| 6 | `conring` | suite | 163 | 3% |
| 7 | `usermode` | suite | 130 | 2% |
| 8 | `compose` | suite | 110 | 2% |
| 9 | `schedstorm` | suite | 75 | 1% |
| 10 | `smp` | suite | 70 | 1% |
| 11 | `soak` | suite | 58 | 1% |
| 12 | `mlrisk` | suite | 44 | 1% |

## The interactive image (ADR-163: contracts before the prompt, storms deferred)

| target | gate image total | interactive image total | saved | interactive slowest | deferred line printed |
|---|---|---|---|---|---|
| aarch64 | 4845 ms | 1763 ms | 3082 ms (64%) | `conring` 320 ms | yes |
| riscv64 | 3074 ms | 1671 ms | 1403 ms (46%) | `conring` 358 ms | yes |
| x86-64 | 5445 ms | 1021 ms | 4424 ms (81%) | `usermode` 182 ms | yes |

The interactive logs come from the live gates (`scripts/browser-e2e.sh` on the device-tree targets,
`scripts/vinput-e2e.sh` on x86-64). Those machines are not the gate image's machine: the x86-64
input gate carries no remapping unit, so its interactive boot skips the VT-d suite as absent and its
saving is NOT the storms alone - compare against the gate image's total minus `dmar` (about a second)
for the like-for-like figure.

## What the numbers say

* **The storms and the bench are the cost, not the contracts.** On every CPU the heavy laps are
  `bench`, `fsstorm`, `mlrisk-stress`, `reclaim`, `schedstorm`, `conring` and `compose`: suites that
  run thousands of iterations to prove a bound holds under load. The contract suites (a TLS
  handshake, the HTTP reader, the renderer, the policy) cost tens of milliseconds each.
* **What the first harvest left unattributed is now named.** On x86-64 it was the VT-d suite
  (`dmar`, a real remapping unit programmed and probed under TCG); on aarch64 the
  performance-validation pass after the suites (`perf-report`); on all three the compositor's
  marker had a shape the stopwatch missed. What remains unattributed is bring-up and printing.
* **The interactive image pays all of this before its prompt.** `scripts/comparative-bench.sh`
  measures boot-to-prompt on x86-64 against Linux; this page is the part of that number the
  kernel controls. What to do about it (defer the storms to an opt-in `selftest` command, keep
  the contracts at boot) is a decision for its own ADR, with this page as its evidence.
