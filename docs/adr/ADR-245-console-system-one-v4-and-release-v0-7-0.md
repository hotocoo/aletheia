# ADR-245 — Console System 1 v4, and release v0.7.0

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017, REQ-REL-003
**Builds on:** ADR-187/193 (console checkpoint v1/v2), ADR-209 (v3 measured and not shipped), ADR-241 (native runtime), ADR-237..244 (this release's waves).

## Context

The shipped console checkpoint (v2, release v0.6.0) was trained on the command table before five
commands existed: `weight` (ADR-239), `passwd`, `login`, `logout`, `whoami` (ADR-244). Laya being
"shipped as a first-class capability" with a checkpoint that does not know five shipped verbs
contradicts the requirement, so the release carries a retrained one or says why not.

## Decision: ship v4

* **Corpus:** `scripts/system1/corpus.py --only weight,passwd,login,logout,whoami --append` with
  the registry's System 2 as paraphraser: 465 new rows from 376 requests (the committed
  `docs/evidence/system1/console-corpus.jsonl` now has 7 562 rows).
* **Training:** v2 warm-started on the new rows plus 250 replayed paraphrase groups (3 113 old
  rows, so the old commands are not forgotten), top 4 encoder layers, 3 epochs, lr 5e-5, on CPU
  (21 minutes; the GPU was another session's). Report: `docs/evidence/system1/console-v4-finetune.json`.
* **Measured before shipping** (same native runtime, CPU, current command table):

| | v2 | v4 |
|---|---|---|
| `console bench`, 10 hand cases | 6/10 right, 6 answered alone, 0 wrong-and-sure | **7/10 right, 5 answered alone, 0 wrong-and-sure** |
| held-out new-verb rows (18, whole groups held out by the trainer) | 17/18, 14 answered alone, 0 wrong-and-sure | 17/18, **17 answered alone**, 0 wrong-and-sure |
| trainer's held-out test (415 rows) | - | 99.0 %, coverage 95.7 % at 0.9, 0 wrong-and-sure |
| native vs Python parity (200 rows + batches) | 928/928 (ADR-241) | 464/464, max \|Δconf\| 0.0001 |

  v2's bench score under the current table is 6/10, not the 7/10 recorded in ADR-241: the five new
  commands are options in every question now, and v2 had never seen them. Per case, v4 fixed
  `tasks` (now right, escalating at 0.61) and answers `wc` alone; it now escalates `ls` (0.86, was
  0.98) and still gets `arch` wrong without being sure of it (0.74). The trainer's 99.0 % includes
  replayed groups v2 had trained on, so the bench and the new-verb rows are the numbers that decide.
* The manifest `models/aletheia-console-s1.toml` names the v4 archive under the v0.7.0 tag.
  `model pull` now treats only the pinned snapshot as pulled: before this, a cached v2 snapshot of
  the same repo made the pull a no-op and the machine kept serving v2 under a v4 manifest (found
  while preparing this release; test `an_older_snapshot_does_not_stop_a_new_version_from_being_pulled`).

## Release v0.7.0

* Crate versions 0.7.0; `ver` now prints the crate version (it printed 0.1.0 since the start).
* The release workflow builds `aletheia-laya` for Linux x86-64 and attaches it beside the VMware
  package; the release notes say how to place it. The console v4 archive is uploaded from this
  workstation after the tag and verified with `aletheiad model pull` from GitHub.
* `docs/PRODUCTION-READINESS.md` is the audit for this release; `docs/BENCHMARKS.md` section 000
  re-measures the comparison with Linux.

## What this is not

Ten bench cases and eighteen held-out rows are small samples; `arch`, `write` and `run` remain
System-2 work. The checkpoint still needs fine-tuning in Python; only serving is native.
