# ADR-193 — System 1, v2

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-AI-012 (extended)
**Builds on:** ADR-187 (System 1 trained on the console), ADR-188 (storm).

## Change

The corpus grows by 1,896 paraphrase examples aimed at the commands v1 got wrong or escalated
(`grep`, `write`, `arch`/`ver`, `cat`, `wc`, `head`, `ls`, `rm`, `append`, `stat`, `find`), to 6,643
usable examples (one-option questions, which the console never asks since ADR-188, are dropped by the
trainer). Training unfreezes the top 12 encoder layers (173.6 M parameters) for 5 epochs.

## Measured (workstation, MPS)

| | v1 (ADR-187) | **v2** |
|---|---|---|
| held-out test, all questions | 83.8% | **90.4%** |
| held-out at 0.90: coverage / accuracy | 64% / 98.0% | **78% / 97.7%** |
| console bench alone | 5/8, 2 answered alone | **6/8, 6 answered alone** |
| wrong-and-sure on the bench | 0 | **0** |
| with System 2 (dual) | 7/8 | **8/8** |

The dual path with v2 is 8/8, better than System 2 alone (7/8: System 2 misplans `head manifesto
1`, System 1 answers it at 0.99), in 1.9 s against 7.1 s for System 2 alone. Conditions: System 2
is DavidAU LFM2.5 Q8_0 on `llama-server --jinja --reasoning-budget 0 -c 8192` on :8099, measured
while QEMU gates ran on the same machine; at ADR-174's quiet-machine median (583 ms) System 2 alone
would take about 4.7 s, and the dual path about 1.9 s.

v2 ships as `aletheia-console-s1-v2.tar` on release v0.4.0; `models/aletheia-console-s1.toml` pins
its archive and weights. The registry now prefers the snapshot named by the manifest's archive
digest, so an older unpacked version in the same cache cannot be served in the new one's name.

## Non-claims

Still weak: `arch` versus `ver` (0.78, escalates) and free-text `write` (0.19, escalates) —
escalation, not error, is what they cost.
