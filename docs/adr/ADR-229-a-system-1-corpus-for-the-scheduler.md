# ADR-229 — A System-1 corpus for the scheduler, labelled by the scheduler

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-015 (new)
**Builds on:** ADR-056 (advisory verdicts), ADR-186 (System-1 decision wire), ADR-187/209 (console corpus and trainer), ADR-199 (resident scheduler forest).

## Context

The System-1 role (ADR-186) is trained only on the console: `docs/evidence/system1/console-corpus.jsonl`
asks which command an operator's request names. The scheduler has its own System 1, the resident
integer forest (ADR-056, ADR-199), which advises every admission live. Nothing let a
general-purpose System-1 model learn the priority scheduler's dispatch decision, and nothing
measured whether a model could reproduce it. Hand-written scheduling questions would restate the
rules and drift from the code.

## Decision

* `kernel-core/examples/sched_corpus.rs` drives the real `PriorityScheduler` through seeded
  episodes built to be hard: two to seven priority bands (ties everywhere), endpoint
  acquire/wait/release chains with transitive priority donation, wait cycles, waiters stranded by
  a holder that finished, advisory verdicts that reorder equals, and admissions mid-run.
* Before each dispatch with 2 to 12 runnable tasks it renders the machine as text and asks one
  choice question on the existing wire shape (`group`, `state`, `question.options`, `answer`,
  `kind`). The label is whatever `schedule_next` returned. No rule is restated in the corpus.
* **Every row is checked.** The winner is recomputed from the rendered facts alone; if it differs
  from the scheduler's, the generator panics. A row the rendered state does not answer cannot be
  written.
* Rows a unique highest base priority answers alone are dropped by default (`--all` keeps them).
  The rest are balanced across three kinds: `schedule-donation`, `schedule-advice`,
  `schedule-fifo`. Rows longer than the trainer's 512-token window (a 1400-character budget)
  are skipped and counted.
* No model or backend is named. Any trainer that reads the console corpus reads this one;
  `scripts/system1/laya_finetune.py` was checked to read only `group`, `state`, `question` and
  `answer`, and reports per `kind`.
* CI (`property-campaign` job) generates 2000 rows with the run number as seed, so each run
  checks a new set of episodes.

## Evidence (2026-10-08, host)

* `--rows 20000 --seed 56`: 20 001 rows from 5678 episodes, 6667 per kind, 0 over budget.
* Seeds 1, 2, 3, 7, 99, 1234, 98765, 424242 at 2000 rows: no mismatch.

## What this is not

The corpus is training and evaluation data for the System-1 role on the dispatch decision. No
language model runs inside the kernel, and the dispatcher does not consult one. The scheduler's
live System 1 is still the resident forest. A checkpoint fine-tuned on this corpus has not been
trained or measured; there is no scheduler arm in `console bench` yet. Both are open items, and
no accuracy claim is made here.

## Consequences

* A scheduler change that alters dispatch changes the labels with it; the corpus cannot go stale.
* The same pattern (render state, ask, label with the subsystem's real answer, recompute from
  the rendering) applies to the power governor and the risk advisor. Each gets its own ADR.
