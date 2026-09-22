# Boot cost, measured (ADR-162)

Every target proves its contracts at boot, suite after suite, before the console or the desktop is
offered. This page is what that COSTS, read from the machine's own monotonic counter and printed on
every gate's boot log: one `[boot] FAMILY suite: N ms` line under each family's marker, and one
`[boot] suites: N timed, T ms total, slowest F at S ms` line when the suites are done.

**How to read it.** Numbers are QEMU TCG on one development machine (Apple silicon, 2026-09-23),
uncontended, one boot each; they are RELATIVE - which suite is heavy on which CPU - not a promise
about hardware. `total` runs from the first suite to the summary and includes everything between
laps: device bring-up, storms without a marker, the desktop's installation. `unattributed` is
`total` minus the sum of the laps: time no suite marker claims.

Regenerate: run `scripts/vm-e2e.sh`, `scripts/vm-e2e-riscv.sh`, `scripts/vm-e2e-x86.sh` and read the
`[boot]` lines; the harvest below is what those runs said.

## aarch64 (QEMU virt, cortex-a72, TCG)

| measure | value |
|---|---|
| suites timed | 56 |
| total, first suite to summary | 3800 ms |
| sum of suite laps | 2440 ms |
| unattributed (between laps) | 1360 ms |
| slowest suite | bench at 346 ms |

| rank | suite | ms | share of total |
|---|---|---|---|
| 1 | `bench` | 346 | 9% |
| 2 | `fsstorm` | 294 | 8% |
| 3 | `mlrisk-stress` | 275 | 7% |
| 4 | `reclaim` | 213 | 6% |
| 5 | `schedstorm` | 174 | 5% |
| 6 | `conring` | 152 | 4% |
| 7 | `smp` | 126 | 3% |
| 8 | `compose` | 123 | 3% |
| 9 | `soak` | 94 | 2% |
| 10 | `tlsclient` | 86 | 2% |
| 11 | `usermode` | 83 | 2% |
| 12 | `tlshandshake` | 81 | 2% |

## riscv64 (QEMU virt, rv64, TCG)

| measure | value |
|---|---|
| suites timed | 55 |
| total, first suite to summary | 2286 ms |
| sum of suite laps | 2256 ms |
| unattributed (between laps) | 30 ms |
| slowest suite | bench at 410 ms |

| rank | suite | ms | share of total |
|---|---|---|---|
| 1 | `bench` | 410 | 18% |
| 2 | `schedstorm` | 215 | 9% |
| 3 | `mlrisk-stress` | 203 | 9% |
| 4 | `conring` | 199 | 9% |
| 5 | `reclaim` | 180 | 8% |
| 6 | `fsstorm` | 178 | 8% |
| 7 | `compose` | 161 | 7% |
| 8 | `tlsclient` | 89 | 4% |
| 9 | `tlshandshake` | 84 | 4% |
| 10 | `trust` | 71 | 3% |
| 11 | `soak` | 67 | 3% |
| 12 | `usermode` | 60 | 3% |

## x86-64 (QEMU q35, OVMF, TCG)

| measure | value |
|---|---|
| suites timed | 56 |
| total, first suite to summary | 4514 ms |
| sum of suite laps | 983 ms |
| unattributed (between laps) | 3531 ms |
| slowest suite | mlrisk-stress at 150 ms |

| rank | suite | ms | share of total |
|---|---|---|---|
| 1 | `mlrisk-stress` | 150 | 3% |
| 2 | `fsstorm` | 104 | 2% |
| 3 | `reclaim` | 101 | 2% |
| 4 | `bench` | 97 | 2% |
| 5 | `usermode` | 60 | 1% |
| 6 | `schedstorm` | 46 | 1% |
| 7 | `smp` | 43 | 1% |
| 8 | `soak` | 35 | 1% |
| 9 | `tlshandshake` | 30 | 1% |
| 10 | `conring` | 27 | 1% |
| 11 | `mlrisk` | 25 | 1% |
| 12 | `tlsclient` | 22 | 0% |

## What the numbers say

* **The storms and the bench are the cost, not the contracts.** On every CPU the heavy laps are
  `bench`, `fsstorm`, `mlrisk-stress`, `reclaim`, `schedstorm`, `conring` and `compose`: suites that
  run thousands of iterations to prove a bound holds under load. The contract suites (a TLS
  handshake, the HTTP reader, the renderer, the policy) cost tens of milliseconds each.
* **x86-64's time is mostly between the laps.** Its suites sum to well under a second while the
  total is over four: device bring-up (PCI enumeration, virtio-gpu, the input devices), the
  desktop's installation and the un-marked storms live there. The next measurement must lap those
  phases too before anything is moved.
* **The interactive image pays all of this before its prompt.** `scripts/comparative-bench.sh`
  measures boot-to-prompt on x86-64 against Linux; this page is the part of that number the
  kernel controls. What to do about it (defer the storms to an opt-in `selftest` command, keep
  the contracts at boot) is a decision for its own ADR, with this page as its evidence.
