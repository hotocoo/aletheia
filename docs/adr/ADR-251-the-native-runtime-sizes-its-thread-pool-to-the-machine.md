# ADR-251 — The native runtime sizes its thread pool to the machine

**Status:** Accepted (2026-10-09)
**Requirements:** REQ-AI-017
**Builds on:** ADR-249 (fused CPU kernels).

## Context

ADR-249's fused kernels split across rayon's default pool, one thread per logical CPU. The BLAS
library (Accelerate on macOS) runs its own threads beside them, and on a machine with efficiency
cores the slowest cores set the pace of every parallel pass.

## Measurement (Apple M4 Max, 12 performance + 4 efficiency cores; console v4, 150 rows, CPU)

| rayon threads | 4 | 6 | 8 | 10 | 12 | 16 (default) |
|---|---|---|---|---|---|---|
| native p50 | 118 | 114 | 112 | 112 | 115 | 129 ms |

Reference (torch CPU) on the same runs: 100-102 ms.

## Decision

With no `RAYON_NUM_THREADS` set, `aletheia-laya` uses half the logical CPUs. It is one line, needs
no platform query, and lands on the measured optimum here; the environment variable still decides
when an operator sets it.

## Evidence

400 rows: native p50 112 ms (p95 127) against the reference's 102 ms (p95 111): 1.1x, from 1.3x
after ADR-249 and 2.3x after ADR-241. Parity 932/932 answers, max |Δconf| 0.0001.
