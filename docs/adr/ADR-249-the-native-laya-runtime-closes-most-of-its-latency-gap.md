# ADR-249 — The native Laya runtime closes most of its latency gap

**Status:** Accepted (2026-10-09)
**Requirements:** REQ-AI-017
**Builds on:** ADR-237 (decision 3: external code pinned and recorded), ADR-241 (native runtime), ADR-245 (checkpoint v4).

## Context

ADR-241 shipped a native Laya runtime that matched the Python reference answer for answer but was
2.3x slower per decision on CPU. The profile (`docs/evidence/system1/native-profile-2026-10-08.md`)
put the gap in candle's CPU elementwise path and its scalar `erf`, not in matrix multiplication.
The production-readiness audit listed the gap as open.

## Decision

* **Vendor the encoder.** candle-transformers 0.9.2's ModernBERT becomes
  `aletheia-laya/src/encoder.rs`, trimmed to the encoder, pinned with its upstream digest and
  license in `third_party/candle-modernbert.pin`.
* **Fuse the hot elementwise path on the CPU** (`aletheia-laya/src/fused.rs`), each a candle
  custom op split across cores:
  * GeGLU: `gelu_erf(a) * b` over the 5 248-wide projection in one pass, with an f32
    Abramowitz-Stegun `erf` (error under 1e-6) the compiler vectorizes; was three passes, two
    intermediate tensors and a scalar libm `erf`.
  * Attention: scale, additive mask and softmax in one pass; was a pass over q, a broadcast add and
    a softmax.
  * The sliding-window mask is combined with the padding mask once per forward pass, not once per
    local layer (19 of 28).
  * Bias-free LayerNorms carry a zero bias so candle's fused kernel runs instead of its seven-op
    fallback.
  The GPU path keeps candle's own operations.
* **Link-time optimization** for the release binary (4.97 MB, was 6.2 MB).

## Evidence (Apple M4 Max, `scripts/system1/native_parity.py`)

| set | runtime | reference p50 | native p50 before -> after | parity |
|---|---|---|---|---|
| console v4, 400 rows, CPU | torch CPU | 100-115 ms | 261 -> 129 ms | 932/932, max \|Δconf\| 0.0001 |
| scheduler run 2, seed 31337, 300 rows, CPU | torch CPU | 125 ms | 225 -> 159 ms | 700/700 |
| console v4, 300 rows, GPU | torch MPS | 27 ms | 38 ms (Metal) | 696/696 |

Resident memory on Metal: native 0.90 GB, reference 1.10 GB. Host tests: the fused GeGLU and
masked softmax against candle's own operations, `erf` against a series across [-6, 6].

## What this is not

The native runtime is still 1.3x (CPU) to 1.4x (Metal) slower per decision than torch. What is
left is mostly matrix multiplication and strided copies around the attention permutes; closing it
means a fused attention kernel, which this ADR does not attempt.
