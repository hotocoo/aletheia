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
| total, first suite to summary | 4697 ms |
| sum of laps | 4661 ms |
| unattributed (between laps) | 36 ms |
| slowest suite | perf-report at 1490 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `perf-report` | phase | 1490 | 32% |
| 2 | `bench` | suite | 395 | 8% |
| 3 | `fsstorm` | suite | 319 | 7% |
| 4 | `mlrisk-stress` | suite | 302 | 6% |
| 5 | `conring` | suite | 299 | 6% |
| 6 | `smp` | suite | 261 | 6% |
| 7 | `compose` | suite | 247 | 5% |
| 8 | `reclaim` | suite | 231 | 5% |
| 9 | `usermode` | suite | 195 | 4% |
| 10 | `schedstorm` | suite | 180 | 4% |
| 11 | `soak` | suite | 96 | 2% |
| 12 | `tlsclient` | suite | 90 | 2% |

## riscv64 (QEMU virt, rv64, TCG)

| measure | value |
|---|---|
| laps timed | 59 (59 suites) |
| total, first suite to summary | 3205 ms |
| sum of laps | 3177 ms |
| unattributed (between laps) | 28 ms |
| slowest suite | bench at 491 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `bench` | suite | 491 | 15% |
| 2 | `conring` | suite | 399 | 12% |
| 3 | `compose` | suite | 369 | 12% |
| 4 | `schedstorm` | suite | 247 | 8% |
| 5 | `mlrisk-stress` | suite | 225 | 7% |
| 6 | `reclaim` | suite | 211 | 7% |
| 7 | `fsstorm` | suite | 196 | 6% |
| 8 | `usermode` | suite | 177 | 6% |
| 9 | `tlsclient` | suite | 106 | 3% |
| 10 | `tlshandshake` | suite | 102 | 3% |
| 11 | `shellstorm` | suite | 101 | 3% |
| 12 | `trust` | suite | 86 | 3% |

## x86-64 (QEMU q35, OVMF, TCG)

| measure | value |
|---|---|
| laps timed | 61 (61 suites) |
| total, first suite to summary | 5348 ms |
| sum of laps | 5319 ms |
| unattributed (between laps) | 29 ms |
| slowest suite | dmar at 3515 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `dmar` | suite | 3515 | 66% |
| 2 | `mlrisk-stress` | suite | 239 | 4% |
| 3 | `reclaim` | suite | 166 | 3% |
| 4 | `bench` | suite | 162 | 3% |
| 5 | `fsstorm` | suite | 160 | 3% |
| 6 | `usermode` | suite | 151 | 3% |
| 7 | `conring` | suite | 150 | 3% |
| 8 | `compose` | suite | 99 | 2% |
| 9 | `schedstorm` | suite | 77 | 1% |
| 10 | `smp` | suite | 65 | 1% |
| 11 | `soak` | suite | 56 | 1% |
| 12 | `mlrisk` | suite | 42 | 1% |

## The interactive image (ADR-163: contracts before the prompt, storms deferred)

| target | gate image total | interactive image total | saved | interactive slowest | deferred line printed |
|---|---|---|---|---|---|
| aarch64 | 4697 ms | 2112 ms | 2585 ms (55%) | `smp` 585 ms | yes |
| riscv64 | 3205 ms | 1619 ms | 1586 ms (49%) | `conring` 346 ms | yes |
| x86-64 | 5348 ms | 1032 ms | 4316 ms (81%) | `reclaim` 210 ms | yes |

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
