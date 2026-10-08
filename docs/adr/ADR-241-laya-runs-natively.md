# ADR-241 — Laya runs natively: the System-1 backend without Python

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-AI-017 (ADR-237 decision 1, "the Laya backend becomes shipped and supervised")
**Builds on:** ADR-186 (the decision wire), ADR-187/193 (console checkpoint), ADR-231 (scheduler checkpoint), ADR-240 (supervision).

## Context

ADR-240 made `aletheiad` own the System-1 server's life but said plainly that the server was
still Python: `pip install laya` brings torch and transformers, roughly a gigabyte of framework,
and a cold start of about 22 s. Whether a native forward pass could replace it was left as a
measurement: label parity, side of the 0.9 escalation threshold, latency, memory, load time.

## Decision

* **`aletheia-laya`, a separate host crate and binary.** It loads a Laya checkpoint directory
  (`model.safetensors`, `rl_agent_config.json`, `encoder/`, `tokenizer/`) with candle: candle's
  own ModernBERT encoder, the two-layer `norm_first` transformer head written out (ReLU feed-
  forward, padding mask), the type embedding and the scorer. It answers the decision wire on
  127.0.0.1 with the same arguments as the Python sidecar. `aletheiad` stays free of the tensor
  stack.
* **Computed in f32 from the checkpoint's f16 weights**, as the reference runtime does on CPU and
  MPS. f16 compute on Metal was measured and rejected: 399/400 labels and a 0.03 confidence
  difference against the reference, for a 9 % latency gain (175 vs 192 ms p50). Exactness won.
* **The pure parts are separate and tested without a checkpoint:** sequence construction (the
  reference `build_sequence`: option budget, even cut, eight-token instruction floor, `[MASK]`
  scrubbing, state truncation), calibration (per-bucket then per-type temperature) and the
  confidence formula, question parsing and its refusals, and the HTTP front (identity, decide,
  bounded body, 404). A CI job builds the crate and runs them.
* **Discovery stays generic (ADR-186 holds).** `runtime::serve_command` looks for
  `aletheia-<backend>` beside `aletheiad` (how a release ships it), in the sidecar directory, in
  the tree it was built from, then on `PATH`; it prefers that binary and falls back to
  `<backend>_server.py`. `SYSTEM1_RUNTIME=python|native` forces one. No backend name enters
  `aletheiad`'s source.
* The runtime picks the GPU on macOS (Metal) and the CPU elsewhere, `--device cpu|metal` or
  `SYSTEM1_DEVICE` to choose.
* Dependencies: candle 0.9 (MIT/Apache-2.0), `tokenizers` 0.21 with only Oniguruma
  (BSD-2-Clause) and no progress bars or C++ suffix arrays. The license allow-list gains
  `Unicode-3.0` (OSI-approved, notice-only), which candle's ICU4X dependencies (`yoke`,
  `zerofrom`) carry. One `unsafe`: the read-only memory map of the weights.

## Evidence (2026-10-08, Apple M4 Max; `scripts/system1/native_parity.py`)

Each sampled row is asked one question per request; then rows go three at a time with an extra
yes/no question in one request, so the padded-batch path is compared as well.

| checkpoint, set | answers | label parity | same side of 0.9 | max \|Δconf\| | accuracy (ref / native) |
|---|---|---|---|---|---|
| console v2, console corpus (400 rows) | 928 | 928/928 | 926/926 | 0.0001 | 384/400 / 384/400 |
| scheduler run 2 (ADR-231), seed 31337 (300 rows) | 700 | 700/700 | 700/700 | 0.0001 | 298/300 / 298/300 |

| | Python sidecar (torch MPS) | native, Metal | native, CPU (Accelerate) | Python, CPU |
|---|---|---|---|---|
| load | 21.5 – 22.9 s | 1.1 – 3.6 s | 0.3 s | 21.2 s |
| decide p50 / p95 (console) | 98 / 208 ms | 179 / 244 ms | 261 / 290 ms | 112 / 120 ms |
| resident memory | 1.13 GB | 0.88 GB | 1.95 GB | 2.30 GB |
| install | Python + torch + transformers | one 6.2 MB binary | same | same as MPS |

Through `aletheiad`: `model serve aletheia-console-s1` started the native runtime under the
ADR-240 supervisor, ready in 1.5 s instead of 22.9 s; `console bench` gave the same result on both
runtimes (7/10 right, 5 answered by System 1 alone, 0 wrong-and-sure, identical confidences),
which also proves `aletheiad`'s own HTTP client against the native front.

## What this is not

**Not faster per decision.** The native forward pass is 1.8× slower than torch on Metal and 2.3×
slower than torch on CPU; it is faster to start, smaller to install, and lighter in memory where
the GPU holds the weights. The latency gap is an open row; where the time goes is measured in
`docs/evidence/system1/native-profile-2026-10-08.md` (matmul is not the gap, candle's CPU
elementwise path and scalar erf are), and any change must keep the parity above. On a CPU-only machine
the f32 weights cost 1.95 GB resident. The binary is not yet a release asset (that is the release
wave). The Python sidecar remains in the tree as the reference the parity script measures
against, and as the fallback when no native binary is present. Fine-tuning still uses the
Python backend (`laya_finetune.py`).

## Consequences

* ADR-237 row "Console decisions": the backend is native and supervised; only training needs
  Python.
* Any future System-1 checkpoint in the Laya format is served by the same binary with no code
  change; `native_parity.py` is the gate a new checkpoint passes before it ships.
