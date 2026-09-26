# ADR-187 — System 1, trained on the console

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-AI-012 (new)
**Builds on:** ADR-186 (two thinking systems as registry roles), ADR-053 (the console planner),
ADR-052 (model identity), release policy (a VMware package on every stable tag).

## Context

ADR-186 measured the base System-1 checkpoint on the console and found it unfit: 1/8 cases right,
4 of them wrong at confidence >= 0.94. The operator asked for System 1 to be trained on Aletheia's
own CLI and shipped with the release, without hard-coding the command set or any phrasing.

## Decision

1. **The questions are exported, not copied.** `aletheiad console system1-schema` prints the question
   wording and the command table; `aletheiad console system1-questions` turns `{request, context,
   command}` lines into the exact wire request the console would send System 1. `ai/dual.rs` builds
   every question through one function (`questions_for` / `arg_ask`) used by both serving and export.
2. **The corpus is generated from the command table.** `scripts/system1/corpus.py` asks a System-2
   model (any OpenAI-compatible endpoint) to paraphrase each command's usage and help text with
   concrete argument values, keeps a paraphrase only if it carries those values (so every gold
   label is an option the console really offers), adds the literal command form, and routes every
   example through `system1-questions`. A command added to the kernel is in the next corpus with no
   edit to the generator. The eight `console bench` requests are never training data (checked: 0 of
   8 appear). Committed: `docs/evidence/system1/console-corpus.jsonl` (4,751 examples, 47 commands).
3. **Training** (`scripts/system1/laya_finetune.py`): the backend's own sequence builder through the
   sidecar's own wire conversion, options reshuffled every epoch, top 8 encoder layers plus the
   decision head (124.6 M parameters) trained 5 epochs; whole paraphrase groups held out and split
   into a calibration half (per-option-count temperatures refitted by NLL) and a test half. Weights
   are saved in the base checkpoint's dtype (fp16, 804 MiB; an fp32 save was 1.6 GiB).
4. **Shipping.** The checkpoint is a release asset (`aletheia-console-s1.tar`, 807 MiB) on the tag,
   characterized by `models/aletheia-console-s1.toml` with an `url` and `archive_sha256`.
   `aletheiad model pull <id>` downloads it, refuses it unless the digest matches (and refuses an
   archive with no pinned digest), and unpacks it into the model cache layout under the manifest's
   `repo`, where discovery already looks. The release notes list every manifest whose archive url
   names the tag. `model pull <id>` works for either role.
5. **Words, not punctuation.** Object and text options are built from the request's words with
   wrapping punctuation removed (`manifesto.` offers `manifesto`), in the console and the corpus
   alike.

## Measurement (workstation, Apple silicon, MPS, 2026-09-26)

| | base checkpoint | head only, 6 epochs | top 8 layers, 5 epochs (shipped) |
|---|---|---|---|
| held-out test, all questions | - | 61.5% | **83.8%** |
| held-out: command / name / text | - | 67% / 64% / 37% | **88% / 98% / 74%** |
| held-out at 0.90: coverage / accuracy | - | 22% / 95.0% | **64% / 98.0%** (8 wrong-and-sure of 631) |
| `console bench`, 8 held-out requests | 1/8, 4 wrong-and-sure | - | **5/8, 0 wrong-and-sure**, 2 answered alone |

Training: 18 minutes on MPS. Serving: 57-217 ms per console request (one to three questions).

Together with System 2 (DavidAU LFM2.5 Q8_0 on llama.cpp, `llama-server --jinja --reasoning-budget 0
-c 8192 -ngl 99` on :8099, same workstation, the System-1 sidecar sharing the GPU): **7/8 right, the
same as System 2 alone**. No request System 1 answered alone was wrong. Time depends on System 2's
latency, so it is stated with its conditions: two warm runs gave 12.1 s and 12.2 s for the dual path
against 16.1 s for System 2 alone (System 2 at 1.4-3.0 s per request on that server; System 1
answered `ls` and `rm notes` itself at 100-220 ms each); a cold first run gave 37.2 s vs 48.8 s.
Against ADR-174's recorded System-2 median of 583 ms the same routing would save about 5%, because
the six escalated requests also pay System 1's ~100 ms. The dual path's value today is therefore
that it is **accuracy-neutral with zero wrong-and-sure answers**; its speed gain is real where
System 2 is slow and small where it is fast. The shipped manifest is therefore `status = "ready"` and is the System-1 default; the
base checkpoint stays listed as `unfit`.

**Live, on a booted machine** (`scripts/console-ai-e2e.sh`, new `dual` arm, aarch64): every planned
line typed at the guest's serial console and its output asserted, the governed-removal and
power-cycle legs included: deterministic PASS, model PASS, **dual PASS**, with System 1 answering 2
of the 8 requests itself (`ls` at 0.99, `rm notes` at 0.95) and the other 6 escalating with their
reasons printed. The arm fails if System 1 answers none (it would be the model arm again) and SKIPs
by name unless both systems serve. With the sidecar stopped, `console plan` says
`system1 ... not serving ... system2 only` and plans through System 2.

## Non-claims

* 8 wrong-and-sure answers in 631 held-out questions: System 1 still errs while sure at about 1.3%
  of the questions it answers; the approval gate for destructive commands still applies after it.
* `grep`, `write` with free text, and `arch` versus `ver` are still weak (the bench escalates or
  misses them). More corpus and a lower-level unfreeze are the named next steps.
* The corpus depends on the System-2 model used to paraphrase it; a different paraphraser gives a
  different corpus. The generator and seed are recorded; the paraphraser's outputs are not
  deterministic across llama.cpp versions.
* Nothing runs in the kernel: this is the console's System 1. The scheduler's System 1 is still the
  integer forests of `mlrisk` and `lethe`.
