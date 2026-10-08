# ADR-239 — Background programs share turns by weight

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017 (row 2 of ADR-237's wave order)
**Builds on:** ADR-056 (advice reorders equals only), ADR-213 (background programs), ADR-238 (jobs keep their admission).

## Context

ADR-237 found background programs taking turns in blind round-robin: `Jobs::next_turn` walked
the table in place order, so every running program got the same share of the CPU whatever the
operator wanted, and the risk advisor's verdict at admission played no part in which one ran.
Strict priority would fix the first and starve: a background spinner at a higher priority would
never let a lower one run again.

## Decision

* **Stride scheduling.** Each job carries a weight (1..=16, default 4) and a pass. The job with the
  lowest pass runs; running adds `720 720 / weight` to its pass (720 720 is divisible by every
  weight, so passes are exact integers and a schedule is reproducible). Over any window a job's
  turns are proportional to its weight, and every job with a weight keeps running.
* **A newcomer starts at the present.** A job added later starts at the lowest pass already
  running, so it neither waits behind the old jobs' history nor gets a burst of catch-up turns.
* **Advice orders equals, as ADR-056 does.** Equal passes are broken by the verdict the resident
  advisor gave at admission (a decisive Low first, no opinion next, Elevated last), then by id.
  With equal weights and no advice the order is the old round-robin, exactly.
* **The operator sets the weight.** Console `weight ID|NAME [N]` reads or sets it; `jobs` shows it.
  Out-of-range values are refused with the job unchanged. `aletheiad` classifies `weight` as
  destructive (a human answers for it), like `start` and `kill`.
* Taking a turn still allocates nothing: the choice is a scan of four fixed slots.

## Evidence

* Host: weights 1/2/4 over 700 turns give exactly 100/200/400 turns; a late job starts at the
  present; equal passes go Low, then no opinion, then Elevated; refusals. Console transcript test
  covers set, read by name, out of range, no such job, not a number, usage. kernel-core release
  suite 935 passed; `aletheia` hosted suite passes (the known release-mode timing failure in
  `component_resources` passes in debug, as CI runs it).
* Live, console-e2e on all three CPUs: two spinners, the older at weight 1 and the newer at 4,
  across two idle seconds. Measured new turns (weight 1 / weight 4): aarch64 49 / 195 (3.98×),
  riscv64 80 / 320 (4.00×), x86-64 268 / 1069 (3.99×). The gate requires ≥ 2× and that the
  weight-1 job still runs.
* Boot gates on all three CPUs pass unchanged.

## What this is not

Weights apply to background programs only; `run` and `together` keep their own dispatch through
the priority scheduler. A weight is not kept across a reboot (`autostart` starts at the default).
The console System-1 checkpoint was trained on the command table before `weight` existed, so it
does not know the command; System 2 answers for it until the corpus is regenerated.

## Consequences

* ADR-237 row "Background programs" is live: turns follow an operator-set share, and the
  advisor's verdict orders ties.
