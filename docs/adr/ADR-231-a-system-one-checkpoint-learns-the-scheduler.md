# ADR-231 — A System-1 checkpoint learns the scheduler's dispatch decision

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-016 (new)
**Builds on:** ADR-229 (scheduler corpus), ADR-186 (System-1 decision wire), ADR-187/193 (console fine-tune), ADR-056 (advice reorders equals only).

## Context

ADR-229 left two open items: no checkpoint had been trained on the scheduler corpus, and nothing
measured one. A first fine-tune also showed the corpus itself was not yet sufficient: a model can
only learn a decision the rendered state lets it make cheaply.

## Decision

* **The corpus states the ready pool.** Each row's `state` now also lists the runnable tasks
  grouped by base priority, oldest first within a band, the running task last, with each task's
  advisory verdict. Donation is not applied there; the endpoint graph still says it. Each option
  states the endpoints the task holds and its place in line (`line 3 of 9`) instead of a raw
  enqueue counter. The self-check (winner recomputed from the rendering, else panic) is unchanged.
* **The benchmark speaks the wire, not a backend.** `scripts/system1/sched_bench.py` sends every
  row of a corpus to any sidecar's `POST /v1/decide` and compares the answer with the scheduler's
  own label: accuracy per kind, and coverage, accuracy and wrong-and-sure count at the escalation
  threshold.
* **Pass is measured on episodes nobody looked at.** The final verdict uses a seed that played no
  part in training or in corpus design (31337), and a second set with a different workload shape
  (`--steps 1000`). Seed 9001 was used to study run 1's errors, so it is reported but does not
  decide.

## Evidence (2026-10-08, Apple M4 Max, MPS)

Run 1: Laya base, ADR-229 corpus (24 000 rows, seed 56), 4 epochs, top 12 encoder layers.
Run 2: run 1 warm start, ready-pool corpus (30 000 rows, seed 56), 3 epochs, top 16 layers.

| set | run 1 | run 2 | run 2 at conf ≥ 0.9 | run 2 wrong and sure |
|---|---|---|---|---|
| held-out groups (trainer split) | 94.4 % | 98.6 % | 99.1 % (cov. 97.1 %) | 27 / 3185 |
| seed 9001 | 93.2 % | 98.9 % | 99.6 % (cov. 96.9 %) | 12 / 3000 |
| **seed 31337, untouched** | - | **99.5 %** | **99.9 %** (cov. 97.6 %) | **3 / 3000** |
| seed 4242, `--steps 1000` | - | 99.2 % | 99.8 % (cov. 97.3 %) | 6 / 3000 |

Per kind on seed 31337: advice 99.9 %, donation 98.7 %, FIFO 99.8 %. Unseen-seed accuracy is not
below the trainer's own held-out split, so run 2 is not overfitted to its episodes. Run 1's
FIFO and advice errors were mostly ties it could not order from scattered option text; the
ready-pool line removed most of them. Reports: `docs/evidence/system1/sched/`.

## What this is not

The checkpoint is not in the dispatch path and is not a release asset. ADR-056 stands: no model
decides what runs, and the resident forest (ADR-199) is still the scheduler's live System 1. This
measures that a general System-1 model can reproduce the dispatch decision from a text rendering.
Donation (transitive inheritance) is the weakest kind at 98.7 %. The workload shapes are those of
one generator; a trace from a real workload has not been tried.

## Consequences

* Any System-1 backend can be compared on the same corpus and benchmark without code changes.
* Shipping the checkpoint needs a manifest with a `[provision]` asset and a role that consumes it;
  each is its own decision.
