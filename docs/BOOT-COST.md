# Boot cost, measured (ADR-162)

Every target proves its contracts at boot, suite after suite, before the console or the desktop is
offered. This page is what that COSTS, read from the machine's own monotonic counter and printed on
every gate's boot log: one `[boot] FAMILY suite: N ms` line under each family's marker, a
`[boot] NAME phase: N ms` line after each timed phase that is not a suite, and one
`[boot] suites: N timed, T ms total, slowest F at S ms` line before the console.

**How to read it.** Numbers are QEMU TCG on one development machine (Apple silicon, 2026-09-23),
uncontended, one boot each; they are RELATIVE - which suite is heavy on which CPU - not a promise
about hardware. `total` runs from the first suite to the summary. `unattributed` is `total` minus the
sum of the laps: time no lap claims (device bring-up between suites, printing).

Regenerate: `python3 scripts/boot-cost-harvest.py A64.log RV.log X86.log > docs/BOOT-COST.md` from the
three boot gates' output.

## aarch64 (QEMU virt, cortex-a72, TCG)

| measure | value |
|---|---|
| laps timed | 58 (58 suites) |
| total, first suite to summary | 3723 ms |
| sum of laps | 3696 ms |
| unattributed (between laps) | 27 ms |
| slowest suite | perf-report at 1316 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `perf-report` | phase | 1316 | 35% |
| 2 | `bench` | suite | 334 | 9% |
| 3 | `fsstorm` | suite | 294 | 8% |
| 4 | `mlrisk-stress` | suite | 276 | 7% |
| 5 | `reclaim` | suite | 213 | 6% |
| 6 | `schedstorm` | suite | 164 | 4% |
| 7 | `conring` | suite | 152 | 4% |
| 8 | `compose` | suite | 126 | 3% |
| 9 | `smp` | suite | 93 | 2% |
| 10 | `soak` | suite | 87 | 2% |
| 11 | `usermode` | suite | 84 | 2% |
| 12 | `tlsclient` | suite | 83 | 2% |

## riscv64 (QEMU virt, rv64, TCG)

| measure | value |
|---|---|
| laps timed | 56 (56 suites) |
| total, first suite to summary | 2348 ms |
| sum of laps | 2316 ms |
| unattributed (between laps) | 32 ms |
| slowest suite | bench at 403 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `bench` | suite | 403 | 17% |
| 2 | `mlrisk-stress` | suite | 235 | 10% |
| 3 | `schedstorm` | suite | 214 | 9% |
| 4 | `conring` | suite | 205 | 9% |
| 5 | `reclaim` | suite | 191 | 8% |
| 6 | `fsstorm` | suite | 179 | 8% |
| 7 | `compose` | suite | 166 | 7% |
| 8 | `tlsclient` | suite | 91 | 4% |
| 9 | `tlshandshake` | suite | 85 | 4% |
| 10 | `trust` | suite | 73 | 3% |
| 11 | `soak` | suite | 68 | 3% |
| 12 | `usermode` | suite | 60 | 3% |

## x86-64 (QEMU q35, OVMF, TCG)

| measure | value |
|---|---|
| laps timed | 58 (58 suites) |
| total, first suite to summary | 4493 ms |
| sum of laps | 4465 ms |
| unattributed (between laps) | 28 ms |
| slowest suite | dmar at 3511 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `dmar` | suite | 3511 | 78% |
| 2 | `mlrisk-stress` | suite | 151 | 3% |
| 3 | `fsstorm` | suite | 103 | 2% |
| 4 | `reclaim` | suite | 102 | 2% |
| 5 | `bench` | suite | 98 | 2% |
| 6 | `usermode` | suite | 59 | 1% |
| 7 | `schedstorm` | suite | 47 | 1% |
| 8 | `smp` | suite | 44 | 1% |
| 9 | `soak` | suite | 36 | 1% |
| 10 | `conring` | suite | 27 | 1% |
| 11 | `mlrisk` | suite | 25 | 1% |
| 12 | `tlsclient` | suite | 23 | 1% |

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
