# Boot cost, measured (ADR-162)

Every target proves its contracts at boot, suite after suite, before the console or the desktop is
offered. This page is what that COSTS, read from the machine's own monotonic counter and printed on
every gate's boot log: one `[boot] FAMILY suite: N ms` line under each family's marker, a
`[boot] NAME phase: N ms` line after each timed phase that is not a suite, and one
`[boot] suites: N timed, T ms total, slowest F at S ms` line before the console.

**How to read it.** Numbers are QEMU TCG on one development machine (Apple silicon, 2026-09-26),
uncontended, one boot each; they are RELATIVE - which suite is heavy on which CPU - not a promise
about hardware. `total` runs from the first suite to the summary. `unattributed` is `total` minus the
sum of the laps: time no lap claims (device bring-up between suites, printing).

Regenerate: `python3 scripts/boot-cost-harvest.py A64.log RV.log X86.log > docs/BOOT-COST.md` from the
three boot gates' output.

## aarch64 (QEMU virt, cortex-a72, TCG)

| measure | value |
|---|---|
| laps timed | 61 (61 suites) |
| total, first suite to summary | 4667 ms |
| sum of laps | 4637 ms |
| unattributed (between laps) | 30 ms |
| slowest suite | perf-report at 1507 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `perf-report` | phase | 1507 | 32% |
| 2 | `bench` | suite | 410 | 9% |
| 3 | `fsstorm` | suite | 330 | 7% |
| 4 | `mlrisk-stress` | suite | 313 | 7% |
| 5 | `conring` | suite | 313 | 7% |
| 6 | `compose` | suite | 258 | 6% |
| 7 | `reclaim` | suite | 244 | 5% |
| 8 | `schedstorm` | suite | 191 | 4% |
| 9 | `smp` | suite | 144 | 3% |
| 10 | `usermode` | suite | 107 | 2% |
| 11 | `tlsclient` | suite | 104 | 2% |
| 12 | `soak` | suite | 102 | 2% |

## riscv64 (QEMU virt, rv64, TCG)

| measure | value |
|---|---|
| laps timed | 59 (59 suites) |
| total, first suite to summary | 3171 ms |
| sum of laps | 3143 ms |
| unattributed (between laps) | 28 ms |
| slowest suite | bench at 677 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `bench` | suite | 677 | 21% |
| 2 | `conring` | suite | 356 | 11% |
| 3 | `compose` | suite | 290 | 9% |
| 4 | `schedstorm` | suite | 253 | 8% |
| 5 | `mlrisk-stress` | suite | 235 | 7% |
| 6 | `reclaim` | suite | 209 | 7% |
| 7 | `fsstorm` | suite | 203 | 6% |
| 8 | `usermode` | suite | 100 | 3% |
| 9 | `tlsclient` | suite | 97 | 3% |
| 10 | `tlshandshake` | suite | 91 | 3% |
| 11 | `shellstorm` | suite | 90 | 3% |
| 12 | `soak` | suite | 80 | 3% |

## x86-64 (QEMU q35, OVMF, TCG)

| measure | value |
|---|---|
| laps timed | 61 (61 suites) |
| total, first suite to summary | 5434 ms |
| sum of laps | 5403 ms |
| unattributed (between laps) | 31 ms |
| slowest suite | dmar at 3521 ms |

| rank | lap | kind | ms | share of total |
|---|---|---|---|---|
| 1 | `dmar` | suite | 3521 | 65% |
| 2 | `mlrisk-stress` | suite | 254 | 5% |
| 3 | `reclaim` | suite | 175 | 3% |
| 4 | `bench` | suite | 172 | 3% |
| 5 | `fsstorm` | suite | 167 | 3% |
| 6 | `conring` | suite | 147 | 3% |
| 7 | `usermode` | suite | 115 | 2% |
| 8 | `compose` | suite | 98 | 2% |
| 9 | `schedstorm` | suite | 76 | 1% |
| 10 | `smp` | suite | 68 | 1% |
| 11 | `soak` | suite | 58 | 1% |
| 12 | `tlsclient` | suite | 46 | 1% |

## The interactive image (ADR-163: contracts before the prompt, storms deferred)

| target | gate image total | interactive image total | saved | interactive slowest | deferred line printed |
|---|---|---|---|---|---|
| aarch64 | 4667 ms | 1855 ms | 2812 ms (60%) | `conring` 310 ms | yes |
| riscv64 | 3171 ms | 1648 ms | 1523 ms (48%) | `conring` 352 ms | yes |
| x86-64 | 5434 ms | 973 ms | 4461 ms (82%) | `reclaim` 168 ms | yes |

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
