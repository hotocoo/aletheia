# ADR-209 — System 1 v3, measured and not shipped

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-AI-012 (extended)
**Builds on:** ADR-187, ADR-193 (System 1 v1, v2).

## Context

ADR-199 and ADR-201 added `tasks` and `run`, and three ADRs' non-claims said the shipped System-1
checkpoint (v2) predates them, so requests for them go to System 2. This wave tried to teach them.

## What changed, whatever the checkpoint

* **`run NAME [TEXT]`** (was `[ARGS]`). The host planner treats only a trailing `text` argument as
  free-form (`console_ops::ConsoleOp::is_free_form`), so a multi-word `ARGS` was refused as
  `SpaceInWordArgument`: no model could plan `run hello to the world`. ADR-206's `ARGS` is the
  same argument; the kernel's behaviour is unchanged.
* **The console bench has 10 cases** (`tasks`, `run hello to the world` added), both also typed at
  a booted machine by `scripts/console-ai-e2e.sh`.
* **The corpus** gains 122 `tasks` and 328 `run` rows paraphrased by System 2 (7,097 rows).
  `scripts/system1/corpus.py` gains a hygiene rule (a row repeating the command's help text or a
  usage placeholder, containing ` -- `, or with an unbalanced quote) as `--scrub` and `--hygiene`,
  off by default: it found 2,608 of 7,097 rows (37 %, mostly v2's) to be paraphraser debris such
  as `help list commands` or `...?" - Polite question`, and, measured below, removing them made the
  model worse.

## Measured (workstation, MPS; System 2 = DavidAU LFM2.5 Q8_0 on :8099)

Same 10-case bench, same System 2, same run of the day:

| | v2 (shipped) | v3 (+450 rows) | v3b (v3 corpus scrubbed) |
|---|---|---|---|
| System 1 alone | 7/10, 6 answered | 7/10, 7 answered | 6/10, 5 answered |
| wrong-and-sure | **0** | 1 (`arch` -> `ver`, 0.96) | 1 (`grep front manifesto` -> `grep manifesto`, 0.94) |
| dual (System 1 then 2) | **9/10** | 8/10 | 8/10 |
| held-out at 0.90: coverage / accuracy | 78 % / 97.7 % | 83 % / 97.4 % | 67 % / 98.0 % |

The pass criteria were fixed before either result was read: wrong-and-sure 0 on the bench, dual at
least 9/10, held-out coverage and accuracy not below v2's. **Neither v3 nor v3b passes; v2 stays
the shipped checkpoint** (`models/aletheia-console-s1.toml` unchanged). The held-out figures are
on different test sets (v3b's excludes the debris) and do not compare across columns; the bench
does.

Live, `scripts/console-ai-e2e.sh` on aarch64 with v2: the deterministic arm passes all 10 cases,
including `tasks` and `run hello to the world` typed at the machine. The model and dual arms plan
`tasks` correctly and it runs; they miss `write notes hello from the model` (System 2 planned
`write notes hello`) and `run hello to the world` (System 2 planned `cat hello`).

## Non-claims and next step

* No retrained checkpoint ships and no release is cut here.
* The bottleneck this measures is the paraphraser: a 2.6 B System 2 writing the training requests
  produces a third of them as debris, and the new commands' rows are thin (328 for `run` after
  its filter). A better paraphrase source, or rows reviewed by a person, is the next attempt;
  tuning the temperatures to make the bench pass would not be (they are refit from held-out
  groups or they mean nothing).
* `display` and `resolution` have no corpus rows at all (they postdate v2's corpus).
