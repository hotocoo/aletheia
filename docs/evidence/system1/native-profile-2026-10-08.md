# Where the native System-1 runtime spends its time (2026-10-08)

ADR-241 measured the native runtime (`aletheia-laya`) 2.3x slower per decision than torch on CPU
(261 vs 112 ms p50) and named "fused attention and a cached local mask" as the first candidates.
This note measures instead of guessing, and the measurement says otherwise.

## Method

```bash
aletheia-laya ~/.aletheia/system1/console-v4/run1 --serve-id p --port 8097 --device cpu &
# a client posting console-corpus questions in a loop, one question per request
sample <pid> 8 -file sample.txt          # macOS sampling profiler, 1 ms interval
```

Top-of-stack samples, excluding idle waits (`__psynch_cvwait`, `__workq_kernreturn`, ...), grouped:

| category | samples | share |
|---|---|---|
| BLAS matmul (Accelerate) | 3620 | 46.1 % |
| candle CPU elementwise ops (iterator `from_iter`, unary/binary maps, allocation) | 1791 | 22.8 % |
| scalar `libm::erff` (exact GELU in the encoder's GeGLU and the scorer) | 1233 | 15.7 % |
| vector exp (softmax) | 301 | 3.8 % |
| strided copies | 264 | 3.4 % |
| rayon/crossbeam scheduling | 223 | 2.8 % |
| other | 420 | 5.3 % |

A microbenchmark of candle's `Linear` on a (1, 300, 1024) x (1024, 3072) product: 0.78 ms per call
rank-3, 0.76 ms rank-2 (about 1.2 TFLOP/s), so the weights are not copied per call.

## Conclusion

Matrix multiplication is not the gap: it runs at the speed the hardware offers. About 40 % of active
time is candle's CPU elementwise path (single-threaded iterator code that allocates per op) and its
scalar exact `erf`, which torch vectorizes and parallelizes. Attention and the local mask are not
visible in the profile. Closing the gap means changing the elementwise path (a vectorized GELU,
fewer intermediate tensors) inside the encoder, which candle's ModernBERT does not expose; that is a
vendoring decision under ADR-237 decision 3, with `native_parity.py` as its gate. Expected ceiling
from the shares above: roughly 1.3-1.6x, still behind torch. Not done in v0.7.x.

## Follow-up (ADR-249, 2026-10-09)

Acted on: the encoder is vendored and its CPU elementwise path fused (GeGLU in one parallel pass with
a vectorizable f32 erf; attention scale, mask and softmax in one pass; the sliding-window mask built
once per forward pass; LayerNorm on candle's fused kernel). Same machine, v4 console checkpoint,
400 corpus rows: native CPU p50 261 ms -> 129 ms against the reference's 100-115 ms (2.3x -> 1.3x),
parity 932/932 answers unchanged.
