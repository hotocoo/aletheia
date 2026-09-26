# ADR-186 — Two thinking systems, as registry roles

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-AI-011 (new)
**Builds on:** ADR-052 (the model is a system property; a port is not a model), ADR-053 (the
console planner and its control arm), ADR-174 (the temporary System-2 default).

## Context

Aletheia's language model (System 2) plans by writing: slow (hundreds of ms per request on the
workstation), general, and the only arm that can produce free text. Most console requests are not
open-ended: they are *which command, which object, which number*. A non-autoregressive decision
model (System 1) answers typed questions in one forward pass with a calibrated confidence, so it can
be asked first and can say when it does not know. The operator asked for both, with the occupant of
each role replaceable at any time and nothing hard-coded.

## Decision

1. **Roles live in the registry.** `ModelEntry.role` is `system1` or `system2` (absent = `system2`,
   what every earlier manifest meant); System 1 entries carry `confidence`, the escalation
   threshold. Defaults, selections and `model use` are per role: `selected-model` stays System 2's
   file, `selected-system1` is new, and `model use <id>` persists into the entry's own role, so a
   System-1 model can never be typed into System 2's place. A selection file naming a model of the
   other role reads as no selection. `model list` gains a role column; `model status` reports
   System 1 (serving / present / absent, threshold).
2. **Presence by the manifest's file, any format.** `runtime::cached_file` finds the file a manifest
   names in any snapshot of its repo's cache directory; `discover` still lists only GGUFs.
3. **An Aletheia-owned wire.** `ai/decision.rs`: `DecisionProvider`, `Question` (choice, yes/no),
   `Answer` with confidence; `HttpDecisionProvider` speaks `POST /v1/decide` and proves identity
   through `GET /v1/models` exactly as System 2 does. Answers are untrusted: a label that was not
   offered, a confidence outside [0, 1] or a wrong count is `InvalidOutput`. The sidecar for a
   backend is `scripts/system1/<backend>_server.py` (127.0.0.1 only, bounded bodies).
4. **Dual process.** `ai/dual.rs` `DualProcess` implements `ModelProvider`: System 1 picks the
   command from `console_ops` (the kernel's table), object arguments from the context brief (plus
   request words, for names being created), numbers from the request, free text as a choice over
   the request's own suffixes; addresses, pins and URLs are not typed decisions and escalate. Any
   answer under the threshold, or any required argument unresolved, hands the request unchanged to
   System 2, and the route and reason are reported (`route: system1 (confidence 1.00)` /
   `route: system2 (system1 unsure (0.78 < 0.90))`). The same validator, control-byte check and
   approval gate sit after both.
5. **Fitness is measured, not assumed.** `aletheiad console bench` gains a System-1 arm: every case
   with no threshold, then classified at the manifest threshold into answered / escalates /
   WRONG-AND-SURE. A System-1 model fronts the console unasked only when its manifest status is
   `ready`; `--interpreter dual` forces it.

The current occupant is `models/laya.toml` (`convaiinnovations/laya`, ModernBERT-large, 421 M
parameters, 804 MiB, sha256 pinned). It is temporary, and nothing outside that manifest names it.

## Measurement (workstation, Apple silicon, MPS, 2026-09-26)

Base checkpoint through the System-1 arm: **1/8** console cases right, 50-260 ms per request (one to
three questions each); at threshold 0.90, 1 answered alone, **4 wrong-and-sure** (`wc`, `grep`,
`write`, `rm` planned as other commands at confidence 0.94-1.00), 3 escalated. The manifest
therefore says `status = "unfit"`: the console runs System 2 alone until a checkpoint trained on
Aletheia's command table measures fit. Training that checkpoint is the next wave.

## Non-claims

* Nothing enters the kernel. The scheduler's System 1 already exists and is resident on every CPU:
  the integer forests of `mlrisk` (admission risk) and `lethe` (power). The console's System 1 is a
  host sidecar, as System 2's llama-server is.
* The sidecar is started by the operator; no lifecycle management yet.
* Which argument names are objects (`name`, `src`, `dst`) or numbers (`n`, `port`, `khz`,
  `domain`) is a classification in `ai/dual.rs`, not something the kernel's table states; a new
  argument name is NOT a typed decision until it is classified, so it escalates (fails closed).
* The threshold 0.90 is not a measured optimum; the bench exists so it can be moved on evidence.
