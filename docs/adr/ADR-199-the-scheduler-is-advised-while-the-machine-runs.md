# ADR-199 — The scheduler is advised while the machine runs

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-ML-008 (new)
**Builds on:** ADR-056 (the resident risk forest), ADR-081 (the memory boundary), ADR-186 (System 1 /
System 2 roles).

## Context

The scheduler's System 1 is the resident risk forest (`mlrisk`, `mlsched`): a frozen integer model,
verified at load, consulted for every admission on the `PriorityScheduler` path, answering Low,
Elevated or Abstain. A decisive Low displaces an Elevated leader of the same band; an abstention
leaves the deterministic policy exactly as it would be without a model, which is this scheduler's
System 2. That binds dispatch order.

But the only real user-mode tasks that ever reached it were the two the boot suite spawns. After
the prompt nothing asked it anything, and `mlstat` said so badly: commissioning (ADR-173) spreads its
arrivals over roughly eight hours of SIMULATED time, so the advisor's clock stood at ~28,665 s and
every live timestamp, being uptime, landed "before" it. `silence` read hours straight after boot.

## Decision

* `tasks` at the console (capability `system.schedule`, a new `ShellAction::Schedule`) runs each
  target's real user-mode tasks now: two tasks in their own address spaces, each described to the
  resident advisor with the pages it actually mapped, admitted through `resident::admit`, dispatched
  by `PriorityScheduler::schedule_next`, dispatches and exits fed back. It is the same
  `run_advised_scheduler` the boot suite proves, reached through `ShellHost::run_tasks`.
* The run is fenced for a live machine. The tasks' own frames already mask interrupts at the lower
  privilege (aarch64 SPSR 0x3C0, x86-64 RFLAGS 0x2); the kernel side is masked for the run's
  duration too, on aarch64 (DAIF.I), x86-64 (`without_interrupts`) and riscv64 (`sstatus.SIE` and every `sie` source, since an S-mode interrupt is taken from U-mode
  whatever SIE says); riscv64 also installs the user-mode trap vector for exactly the run, because
  the console runs on the kernel vector and an `ecall` taken there is stored through the task's own
  stack pointer.
* Every run gives its frames back. riscv64's advised run never tore its spaces down (~338 frames
  per run); aarch64's and x86-64's `cleanup_tasks` freed the user pages but kept the page tables
  (14 and 4 frames per run). `cleanup_tasks` now destroys each space, so the boot suites that share
  it free their tables too (aarch64 boots with 42 more frames free, x86-64 with 12).
* Commissioning ends with `resident::start_clock()`: the counts stay, the timestamps restart at
  zero, and the advisor is stamped with uptime from then on.
* The shipped host `console_ops` table classifies `tasks` Safe: it starts kernel-built stubs,
  changes no medium and sends nothing.

## Proof

* Host: `a_started_clock_measures_silence_in_uptime_not_simulated_time` (mlsched), and
  `tasks_reports_what_the_run_did_and_names_every_failure` / `tasks_needs_the_schedule_capability`
  (shell).
* Live, `scripts/console-e2e.sh`, all three CPUs under the running desktop: `tasks` twice, each run
  "every task ran in its own address space and exited" with the advisor's verdicts for that run
  (aarch64 2 abstain, riscv64 and x86-64 2 elevated); `mlstat` afterwards reports silence under
  60 s (measured 0-1 s); the free frame count before and after the two runs is identical on every
  CPU, asserted.

## Non-claims

* The desktop and console are still not scheduled tasks; the run is two kernel-built stubs, not a
  program loaded from disk. Loading and scheduling user programs is the next rung.
* There is no transformer in ring 0. The scheduler's System 1 is the forest; the console's System 1
  (the registry's `system1` role) stays host-side and ships as the v0.4.0 release asset.
* The shipped System-1 console checkpoint was trained before `tasks` existed, so a request for it
  routes to System 2 by design until the next fine-tune.
