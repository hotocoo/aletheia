# ADR-177 — An answer in hand is not refused

**Status:** Accepted (2026-09-25)
**Requirements:** REQ-AI-009 (advanced)
**Builds on:** ADR-054 (the console is a session), ADR-055 (a refusal the model never sees is a
re-roll), ADR-174 (the temporary default model).

## Context

With ADR-174's default model, `console-agent-e2e.sh`'s model arm failed on aarch64 and riscv64.
The case: "add a line saying second line to the poem, then show me that line". The model typed
`cat poem`, `append poem second line`, `cat poem`, and the machine printed the answer. Then it asked
for `cat poem` again, three corrections later still did, and the session ended with `NoProgress`:
a refusal of a request whose answer was already on the transcript.

ADR-055's rule is that the bound exists to stop the LOOP, not the request.

## Decision

When the no-progress bound trips (`MAX_CORRECTIONS` spent) on a READING (a command that does not
change the machine) that was already typed and answered since the machine last changed, the
session ends with `Advance::Done` carrying the machine's own answer, labelled as such:

> the model kept asking for `cat poem` instead of answering; the machine's answer was: hello world! / second line

Nothing more is typed and no budget is spent. A repeated MUTATION still refuses with `NoProgress`:
repeating a write is damage, not an answer.

## Also fixed, found on the way

* `console-agent-e2e.sh`: an arm whose machine never reached a prompt printed FAIL but did not set
  the gate's verdict, so the gate said PASS. It now fails the gate and prints the machine's last
  bytes.
* riscv64 `u-mode` invariant 13 required the progress counter to rise in EVERY 5 ms slice. `rdtime`
  is wall-clock under TCG, and on a saturated host (a large clang build and a llama.cpp server beside
  the gate, load average about 110) the resume path outlasted the slice and one slice made no
  progress. That is the host's load, not a lost context. The invariant now checks what it claims:
  the counter never goes backwards (a lost or crossed context would), and every task advances over
  the run. Two riscv64 boot gates passed at load average about 110 after the change.

## Proof

Unit tests: the repeated-`ls` and repeated-`cat poem` cases now end with the machine's answer and
type nothing more; the repeated-`append` case still refuses. Live, with the DavidAU model on
llama.cpp: `console-agent-e2e.sh` model arm PASS on aarch64, riscv64 and x86-64 (it failed on the
first two before). Deterministic arms PASS on all three.

## Consequences

**Good.** A small model that forgets to summarize no longer turns a correct session into a refusal.

**Costs.** The final answer can be the machine's words rather than the model's sentence. It says so in prose only: the driver exits 10 ("answered") exactly as for a model answer, so a script cannot tell the two apart without reading the label.

**Not claimed.** The model's own answers are still sometimes its reasoning ("The user wants me
to: ..."). The gate checks what was typed and confirmed by the console, not the prose.
