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
| total, first suite to summary | 3847 ms |
| sum of laps | 3821 ms |
| unattributed (between laps) | 26 ms |
| slowest suite | perf-report at 1313 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `perf-report` | phase | 1313 | 34% |
| 2 | `bench` | suite | 331 | 9% |
| 3 | `fsstorm` | suite | 307 | 8% |
| 4 | `mlrisk-stress` | suite | 273 | 7% |
| 5 | `smp` | suite | 224 | 6% |
| 6 | `reclaim` | suite | 214 | 6% |
| 7 | `schedstorm` | suite | 163 | 4% |
| 8 | `conring` | suite | 152 | 4% |
| 9 | `compose` | suite | 126 | 3% |
| 10 | `soak` | suite | 89 | 2% |
| 11 | `usermode` | suite | 82 | 2% |
| 12 | `tlsclient` | suite | 82 | 2% |

## riscv64 (QEMU virt, rv64, TCG)

| measure | value |
|---|---|
| laps timed | 56 (56 suites) |
| total, first suite to summary | 2265 ms |
| sum of laps | 2238 ms |
| unattributed (between laps) | 27 ms |
| slowest suite | bench at 387 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `bench` | suite | 387 | 17% |
| 2 | `mlrisk-stress` | suite | 234 | 10% |
| 3 | `schedstorm` | suite | 198 | 9% |
| 4 | `reclaim` | suite | 189 | 8% |
| 5 | `conring` | suite | 187 | 8% |
| 6 | `fsstorm` | suite | 172 | 8% |
| 7 | `compose` | suite | 153 | 7% |
| 8 | `tlsclient` | suite | 85 | 4% |
| 9 | `tlshandshake` | suite | 81 | 4% |
| 10 | `trust` | suite | 67 | 3% |
| 11 | `soak` | suite | 66 | 3% |
| 12 | `usermode` | suite | 60 | 3% |

## x86-64 (QEMU q35, OVMF, TCG)

| measure | value |
|---|---|
| laps timed | 58 (58 suites) |
| total, first suite to summary | 4499 ms |
| sum of laps | 4471 ms |
| unattributed (between laps) | 28 ms |
| slowest suite | dmar at 3510 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `dmar` | suite | 3510 | 78% |
| 2 | `mlrisk-stress` | suite | 149 | 3% |
| 3 | `reclaim` | suite | 101 | 2% |
| 4 | `fsstorm` | suite | 101 | 2% |
| 5 | `bench` | suite | 98 | 2% |
| 6 | `usermode` | suite | 59 | 1% |
| 7 | `schedstorm` | suite | 46 | 1% |
| 8 | `smp` | suite | 43 | 1% |
| 9 | `soak` | suite | 35 | 1% |
| 10 | `conring` | suite | 27 | 1% |
| 11 | `mlrisk` | suite | 25 | 1% |
| 12 | `compose` | suite | 25 | 1% |

## The interactive image (ADR-163: contracts before the prompt, storms deferred)

| target | gate image total | interactive image total | saved | interactive slowest | deferred line printed |
|---|---|---|---|---|---|
| aarch64 | 3847 ms | 1410 ms | 2437 ms (63%) | `smp` 323 ms | yes |
| riscv64 | 2265 ms | 1150 ms | 1115 ms (49%) | `reclaim` 194 ms | yes |
| x86-64 | 4499 ms | 436 ms | 4063 ms (90%) | `reclaim` 96 ms | yes |

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
