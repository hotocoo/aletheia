# ADR-162 — Boot cost, measured: a stopwatch under every suite marker

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-PERF-003
- **Supersedes:** nothing. Evidence for the next performance decision; corrects an ADR-154 print.

## Context

This tree's boot proves fifty-six families of invariants on every CPU before it offers a
console or a desktop. That is the design. What it COSTS had never been measured on the machine
itself: `scripts/comparative-bench.sh` reports boot-to-prompt against Linux (Aletheia around
6 s on the runner, Linux around 3 s), but nothing said which of those milliseconds were the
firmware's, the drivers' or the suites', or which suite. "Fastest" is an adjective until it is a
number with a name next to it.

## Decision

`kernel-core/src/boottime.rs` is a stopwatch with one hand, a handful of atomics over the
platform's monotonic counter through `Hal`: `start` when the suites begin, `lap(family)` as each
family reports, `summary` when they are done. Every target prints one `[boot] FAMILY suite: N ms`
line under each family's marker and one `[boot] suites: N timed, T ms total, slowest F at S ms`
line before the console; the marker lines every gate greps are untouched. `docs/BOOT-COST.md`
is the harvest, regenerated from the three boot gates.

On x86-64 the ADR-154 print "heap ... after every suite" sat inside the user-mode suite's arm,
fourteen suites in; it now says "after the user-mode suite", and a true after-every-suite heap
line and the summary print before the console starts.

## What was measured (2026-09-23, QEMU TCG, uncontended, one boot each)

| target | suites | total | sum of laps | unattributed | slowest |
|---|---|---|---|---|---|
| aarch64 | 56 | 3800 ms | 2440 ms | 1360 ms | `bench` 346 ms |
| riscv64 | 55 | 2286 ms | 2256 ms | 30 ms | `bench` 410 ms |
| x86-64 | 56 | 4514 ms | 983 ms | 3531 ms | `mlrisk-stress` 150 ms |

Two findings, both in `docs/BOOT-COST.md`: the heavy laps on every CPU are the STORMS and the
bench (`bench`, `fsstorm`, `mlrisk-stress`, `reclaim`, `schedstorm`, `conring`, `compose`), not
the contract suites, which cost tens of milliseconds each; and on x86-64 the suites sum to well
under a second of a four-second total, so the cost lives between the laps - device bring-up, the
desktop's installation, the un-marked storms - and must be lapped before anything is moved.

## Proof

Host: the stopwatch's laps, total and slowest name against a fake counter, and a counter that
runs backwards never underflows. Boot: the `[boot] suites:` line on all three gates, fifty-six
(fifty-five) laps counted, every existing marker unchanged; the three boot gates, conformance
(383), quality gate and doc gates green.

## Alternatives considered

**Changing the marker lines** to carry the time. Rejected: every gate greps them; a marker is a
contract.

**Deferring the storms now.** Rejected for this wave: a decision about what runs at boot needs
the x86-64 gap explained first, and it deserves its own ADR with this page as evidence.

## Consequences

The next performance wave has a number and a name for every suite on every CPU, and a stated
gap to close on x86-64. Nothing about what boot proves has changed.
