# ADR-188 — System 1 under storm

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-AI-013 (new)
**Builds on:** ADR-186/187 (System 1, trained and shipped), ADR-180/181 (hostile-input gates).

## Context

ADR-187 made a fine-tuned System 1 the console's default front line. The operator's bar is that
everything is stress- and load-tested; the only load System 1 had seen was eight bench requests.

## Decision

* `aletheiad console system1-storm [--requests N] [--seed S]`: a seeded stream (splitmix64 of the
  seed) of the command table's own help text, shuffled vocabulary, injection text, control bytes,
  4 KiB lines, random non-ASCII and empty input, planned through the dual path with the control arm
  as System 2 and nothing approved. It counts System-1 answers, escalations by reason class,
  validator refusals, rendered lines and HAZARDS (a rendered line with a control byte or past the
  console bound),
  keeps every distinct System-1 error with its count, and reports latency percentiles.
* `scripts/system1-storm-e2e.sh`: that storm (default 1,000), four storms at once against the same
  sidecar, and wire abuse straight at the sidecar (oversized body, malformed JSON, unknown question
  type, a choice with zero and with one option, an unknown path). Fails on any unsafe line, any
  System-1 error, any wrong status code, or a sidecar that stops serving; SKIPs by name without one.

## What it found (and fixed)

1. **A one-option choice crashed the backend's forward pass** (`RuntimeError: selected index k out of
   range`, a top-2 over one candidate): a two-word request such as `echo hello` offers one text
   suffix. The console now never asks a choice with fewer than two candidates (it escalates:
   `fewer than two candidates is not a choice`), and the sidecar refuses one with 400. Any other
   backend exception is now a named 500 and logged, instead of a dropped connection.
2. **Neighbouring seeds were the same storm** (`seed | 1` makes 190 and 191 equal): now splitmix64.
3. The sidecar logs every request it refuses, with the reason.

## Result (workstation, MPS, fine-tuned System 1, 2026-09-26)

| | requests | System 1 alone | escalated (unsure / untyped) | errors | unsafe lines | p50 / p99 |
|---|---|---|---|---|---|---|
| sequential, seed 188 | 1,000 | 260 | 701 / 39 | 0 | 0 | 27.6 / 56.9 ms |
| 4 concurrent, 250 each | 1,000 | 274 | 694 / 32 | 0 | 0 | ~112 / ~311 ms |

Wire abuse: 413, 400, 400, 400, 400, 404 as expected; serving afterwards. 790 of the sequential
storm's plans were refused by the validator (destructive without approval, or not a valid line):
refused, never typed.

## Non-claims

* The sidecar is single-threaded: concurrent clients serialize, and latency grows with them
  (about 4x at four clients). No request timed out at four; the timeout is 5 s.
* The storm's System 2 is the control arm, so this measures System 1 and the validator, not a
  language model under load.
* The gate is operator-run (it needs the sidecar and its model); it is not in CI.
