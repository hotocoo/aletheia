# ADR-174 — DavidAU's LFM2.5 merge is the temporary default model

**Status:** Accepted (2026-09-25)
**Requirements:** REQ-AI-004 (advanced)
**Builds on:** ADR-052 (the model is a system property), ADR-017 (the AI subsystem).

## Context

The operator asked for DavidAU's LFM model to be the built-in model "temporarily", until
Aletheia-LM (`models/aletheia-lm.toml`, still `pretraining`) is ready. The operator already
serves `DavidAU/LFM2.5-2.6B-Qwen3.8-Turbo-Brilliance-Power-X12-NEO-MAX-GGUF:Q8_0` on the
workstation. It is a fine-tune of the same base the stock default used (LiquidAI/LFM2.5-2.6B,
Apache-2.0), so it runs on the same llama.cpp backend with no code change.

ADR-052's rule is that a manifest carries what was measured, not what was inherited. A merge
is a different model with the same architecture, so every runtime field was measured again.

## Decision

`models/davidau-neo-max.toml` is the one manifest marked `default = true`;
`models/lfm2.5.toml` stays, selectable with `aletheiad model use lfm2.5`. The compiled-in
constants (`DEFAULT_MODEL_REF`, `DEFAULT_MODEL_FILE`, `DEFAULT_MODEL_SHA256`) follow the
default manifest. The tests now find the default manifest by its flag, not by the id `lfm2.5`.

What was measured, on the file the operator serves (`…-NEO-MAX-Q8_0.gguf`):

* **Pin.** sha256 `524dbaa8…837e` and 3,120,573,088 bytes, measured on the blob in the local
  HF cache; they match the hub's LFS object id. `aletheiad model status` reports `verified`.
* **Identity.** `serve_id = "NEO-MAX"`. It appears in the id llama.cpp advertises both when
  started with `-hf` (`…X12-NEO-MAX-GGUF:Q8_0`) and with `-m` (`…TBrilliance-NEO-MAX-Q8_0.gguf`).
  `"LFM2.5"` would let a stock LFM2.5 holding the port pass the check.
* **Thinking.** The model is forced-thinking: asked for "ok", it spends about 100 reasoning
  tokens first. `thinking = true`, so requests send `enable_thinking = false` (2 tokens for the
  same answer).
* **Structured output.** `gbnf-grammar`, the opposite of stock LFM2.5. With thinking off,
  llama.cpp refuses the JSON-schema request (`Failed to initialize samplers`). With thinking on,
  the schema path returns broken JSON after about 500 tokens. The GBNF grammar with thinking
  off returns the object in 8 tokens.

## Results (llama.cpp, `-c 8192`, two consecutive runs each, identical)

| gate | DavidAU NEO-MAX Q8_0 | stock LFM2.5 Q4_K_M (recorded) |
|---|---|---|
| `model bench` (operations) | 5/6, median 583 ms, misses `capability.grant` | 6/6 |
| `console bench` | 7/8, misses `head manifesto 1` | 8/8 |
| `console-ai-e2e.sh` model arm | PASS | PASS |
| `console-agent-e2e.sh` model arm | x86-64 PASS; aarch64, riscv64 FAIL | - |

Before the thinking and grammar fields were measured, the same model scored 5/6 at a median
of 5,658 ms and 6/8 on the console. The two fields made it ten times faster.

The agent failure: after `append poem second line` and `cat poem`, the model asks for
`cat poem` again instead of answering, and the session refuses it as no progress. Letting the
agent path think (no `enable_thinking = false`) was tried and was worse: 0/3, with no tool call
in the response.

## Consequences

**Good.** The default is the model the operator already runs, pinned and verified. It answers
about ten times faster than the unmeasured configuration.

**Costs.** The merge is less accurate than stock LFM2.5 on Aletheia's own benchmarks: one
missed operation, one missed console line, and a multi-step agent loop that does not finish on
two of three CPUs. The Q8_0 file is 1.9x the size of the stock Q4_K_M.

**Not claimed.** CI never runs a model arm (no inference server on the runner), so these
numbers are one workstation's. Still no inference engine in kernel space. When Aletheia-LM is
`ready`, it replaces this default and this ADR is superseded.
