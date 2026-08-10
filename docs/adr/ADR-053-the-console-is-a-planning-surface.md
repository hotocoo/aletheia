# ADR-053 — The kernel console is a planning surface, and the model reaches it by calling tools

**Status:** Accepted
**Date:** 2026-08-10
**Requirements:** REQ-AI-006 (the console command surface as a planning target, with a live gate)
**Extends:** ADR-017 (AI subsystem), ADR-018 (Context Engine), ADR-051 (a command set you can work
in), ADR-052 (the model is a system property)

---

## Context

ADR-052 ended by measuring something real and then saying, in the benchmark's own output, what it
had *not* measured:

> this measures the hosted Core's operation surface, NOT the kernel console
> (`kernel-core/src/shell.rs` has no inference engine under it)

That sentence was true and it described a hole. Aletheia had two command surfaces and intelligence
attached to exactly one of them. The hosted Core's six entity operations could be planned by the
resident model, validated, authorized and executed. The twenty-seven commands a person actually
types when sitting in front of the machine — `ls`, `grep`, `write`, `rm` — could not be planned at
all. An AI-native OS in which the AI cannot reach the console is an AI-native OS with a very large
asterisk.

Three things had to be settled before that hole could be closed.

**Are these one surface or two?** They are two. `entity.derive` and `grep` share no vocabulary, no
arguments, no executor and no store. Forcing them into one registry would have produced a menu in
which half the entries are meaningless for any given request. So: a second operation family, not a
widening of the first.

**Where does the list of commands live?** `tools.rs` already says why this matters — arguments are
declared next to the operation because *"a second list is a list that drifts, silently, into the one
place where drift looks like the model being wrong."* The console's list already exists:
`kernel_core::shell::COMMANDS`, the table the dispatcher and `help` are both generated from.
Retyping it in the hosted crate would have been shorter and would have been that exact defect.

**Does the model go into the kernel?** No. It does not, and this ADR is not a step toward it.

---

## Decision

**1. `aletheia/src/console_ops.rs` derives the console operation family from
`kernel_core::shell::COMMANDS`.** The hosted crate takes a path dependency on `kernel-core` for that
one table. `kernel-core` is `no_std`, has zero dependencies, and builds for the host in under a
second; the cost is negligible and the property bought is that a command cannot be added to the
kernel without appearing in the model's menu.

What the hosted side adds is what the kernel table cannot know:

* **Risk.** Conservative and stated once: anything that writes to the medium or stops the machine is
  `Destructive`. Whether a particular write is additive (`append`, `touch`) is not knowable before
  it runs. The classification is exhaustive and a command with no explicit arm fails a test.
* **A rendering contract.** A validated step becomes EXACTLY ONE console line. A control byte in any
  argument is a refused plan rather than a keystroke, so `"beta\rrm notes"` cannot become a second
  command with the authority of the first. A value with a space in a non-final argument is refused
  too, because the dispatcher reads those with `split_first` and would otherwise search for `no` in
  an object called `space` and report a wrong answer as a right one.

**2. The model reaches the console through native tool calling, not through constrained JSON.** The
commands are sent as OpenAI-shaped tool definitions generated from the registry, `tool_choice` is
`required`, and the returned `tool_calls` entry becomes the plan. `llama-server` must run with
`--jinja`, or it never parses the model's tool call and a correct answer reads as no answer at all.
`runtime::spawn_llama_server` now passes it — but that helper has no callers, so on any machine the
server is started by an operator, and the exact invocation is written into the header of
`scripts/console-ai-e2e.sh` rather than left in somebody's shell history. A measurement nobody else
can reproduce is a measurement with the same standing as an assertion.

**3. The console gets a context brief, as the Core already has (ADR-018).** Before planning, the
driver types `ls` at the live machine and hands back what it printed, framed as data. The brief is
re-read after any command that changes the namespace.

**4. Two gates, and the second one closes the loop.** `aletheiad console bench` measures
interpretation. `scripts/console-ai-e2e.sh` boots a machine, reads the brief off it, asks in plain
English, types what the model chose, and asserts what the console printed — then power-cycles and
checks that what was written survived and what was removed stayed removed.

---

## What the measurements decided, and what they cost

Every part of the decision above replaced something that was tried first and measured worse. The
numbers are one workstation, LFM2.5-2.6B-Q4_K_M, llama.cpp, `-c 8192`, eight cases.

| Attempt | Score | What actually happened |
|---|---|---|
| Permissive JSON schema, argument names listed in prose | 4/8 | `find` chosen for "count the lines"; `{"args":{"args":{…}}}` for `write` |
| Usage line shown, args written out against it | 3/8 | `{"name":"cat"}` — the command's own name as its argument |
| Per-command exact schema (`oneOf`, `additionalProperties:false`) | 4/8 | Structural garbage gone; three cases still burned the full budget and returned empty |
| Neutral few-shot examples | 3/8 | Anchoring: the example's command was chosen for everything |
| **Native tool calling**, no brief | 5–6/8 | `ls` for anything needing a file — the model's own reasoning: *"Let me first look at what files are available"* |
| **Native tool calling + context brief** | **8/8**, twice | median ~800 ms |

Four of those results are worth stating as findings rather than as history.

**The model was asking for the channel the whole time.** Caught in the raw output of the schema
attempt: `{"…","text":"hello from the model}}}}]}<|tool_call_start|>[write(name='notes',
text='hello from the model')]`. It had produced the right call and was trying to escape a decode
that would not let it emit the format it was trained on. The 21-second empty completions were not a
model that could not plan; they were a model fighting the constraint.

**A prohibition that names a tool still names the tool.** Adding *"only call `ls` when the request
is literally to list the objects"* took the score from 6/8 to 3/8 — the model then called `ls` for
everything. The negation is not what survives contact with a 2.6B model; the token is. The system
prompt now names no command at all, and a test enforces that.

**The `ls` sink was not a defect in the model.** Given a request it could not answer from what it
could see, it went looking, which is correct behavior for an agent and wrong for an interpreter
whose output is typed once. The fix was not to argue with it but to let it see. That is ADR-018's
lesson arriving on a second surface.

**A benchmark whose stated context contradicts its own case measures the contradiction.** With one
brief for the whole run, the brief was stale by the last case: told `notes` did not exist and asked
to remove it, the model planned `find notes`. Each case now carries the namespace it runs against,
and the live gate re-reads `ls` for the same reason.

---

## Consequences

**Good.**

* The console is reachable by intelligence, and the path is the same one everything else in Aletheia
  uses: propose → validate → authorize → execute → verify. The model still executes nothing.
* The menu cannot drift from the kernel's dispatcher.
* Command injection through a planned argument is structurally impossible, and tested.
* The whole pipe is gated without a model at all, via the deterministic arm.
* `--jinja` is no longer an invisible prerequisite: a response with no tool call is an error that
  names the flag, rather than a silent fallback.

**Costs and limits, stated plainly.**

* The hosted crate now depends on `kernel-core`. That is a real coupling, taken deliberately for one
  table.
* 8/8 is eight cases on one machine with one quant of one model, held over two consecutive runs. It
  is a floor for reproducing the setup, not a benchmark of the model, and not a claim that arbitrary
  English works. `docs/MATURITY.md` governs.
* Approval is a flag on a CLI (`--approve`), not a human-in-the-loop surface. The Core has a real
  pending-approval mechanism (ADR-015); the console planner does not use it yet.
* The model arm is operator-started. Nothing here launches an inference server, so a fresh checkout
  runs the deterministic arm and SKIPs the model arm — loudly, but it does mean `8/8` needs a person
  to set the machine up before it can be reproduced.
* The gate runs the aarch64 target only. The other two boot the same dispatcher and are covered by
  `console-e2e.sh`, but the model arm has not been run against them.
* The model plans ONE command at a time. Multi-step console plans parse and render, and no case
  exercises them.
