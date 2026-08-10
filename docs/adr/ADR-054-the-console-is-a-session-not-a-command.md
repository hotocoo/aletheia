# ADR-054 — A request is a bounded session at the console, not a single command

**Status:** Accepted
**Date:** 2026-08-10
**Requirements:** REQ-AI-007 (a multi-step console session, gated live)
**Extends:** ADR-017 (AI subsystem), ADR-018 (Context Engine), ADR-051 (a command set you can work
in), ADR-052 (the model is a system property), ADR-053 (the console is a planning surface)
**Amended by:** ADR-055 (what a live model did with this loop)

---

## Context

ADR-053 gave the model the kernel console and measured it: eight requests, one command each, typed
at a booted machine and verified by what the machine printed back. The contract was deliberately
narrow — *call one tool and stop* — and for an interpreter that is the right contract. Its output is
typed once and verified once, and nothing about it can be wrong in a way that survives to a second
step.

It also put a ceiling on the surface, and the ceiling is not a matter of degree. A request whose
answer is not visible in the namespace listing is not a slightly harder one-command request; it is
not a one-command request at all. *"Make a copy of manifesto and tell me how big the copy is"* cannot
be planned in one line, because the object being measured does not exist until the first line has
run. No context brief closes that gap: a brief describes a machine that has not moved yet.

The previous wave already knew this, and closed it in the wrong language. `scripts/console-ai-e2e.sh`
re-read its brief after any case that changed the machine, deciding which cases those were with:

```sh
case "$planned" in write*|rm*|mv*|cp*|touch*|append*) ... ;; esac
```

That is a second list of the kernel's commands — the exact thing ADR-053 was written to abolish —
living in the one language in this repository that nothing tests. It was also *incomplete*, which a
list in a shell script always eventually is, and nothing would have reported that.

## Decision

**One request becomes a bounded sequence of console commands, driven by Aletheia, and the model sees
what the machine said after each one.**

`aletheia/src/ai/agent.rs` holds the loop: propose, validate, render, type, observe, propose again.
It ends when the model says the transcript answers the request, or when a bound stops it. The
`ConsoleAgent` trait is the seam, deliberately separate from `ModelRuntime`, which is single-shot by
contract — widening `ModelRuntime` to carry a transcript would have made every existing implementer
carry a parameter it must ignore, which is how a seam stops meaning anything.

### Nothing about the safety argument is new, and that is the point

Everything ADR-053 established is re-applied **per step**, because a loop is only as safe as its
weakest iteration:

* every proposed command is validated against `console_ops`, the registry derived from
  `kernel_core::shell::COMMANDS`. There is still exactly one list of commands in the system, and the
  shell `case` statement above is deleted rather than replaced;
* a control byte in any argument is still a refused step, so an observation full of escape sequences
  cannot become a second console line;
* a destructive command still requires approval — now at every step rather than once.

### What the loop introduces, and the bound for each

**A new untrusted input.** Everything the model reads after the first turn is output from the
machine, which contains whatever an operator ever wrote into an object. `admit_observation` treats it
as data: control bytes other than newline are removed, carriage returns are removed, and it is
truncated **visibly** at 2048 bytes or 40 lines. Bounded in Rust, next to the tests, rather than by
whatever the driver's shell happened to capture — a model shown 40 lines of a 400-line listing and
not told is a model that will answer "there are 40 objects" and be believed.

**Three bounds that exist only because this is a loop:**

| Bound | Why it exists |
|---|---|
| A step budget (default 6) | An agent that cannot terminate is a denial of service against the operator sitting at the console. |
| No-progress detection | The cheapest way for a small model to burn a budget is to propose the same command twice. *(Amended by ADR-055: only when the machine has not moved in between.)* |
| A refusal to end its own session | `halt` and `reboot` are refused **even with approval**. An agent that stops the machine cannot read the result of stopping it, so the loop would report a step it has no evidence for. Stopping the machine is an operator's command, and the operator has a console. |

### The session is a file

State lives in a transcript on disk between invocations. The loop is driven by whatever can type at
the console — a shell script in the gate, an operator by hand — and a session that lived in a
long-running process would need that process to also own the serial port. Keeping it inspectable also
means a refused session can be read afterwards to see which step refused, which is the question
anyone actually asks.

## The gate

`scripts/console-agent-e2e.sh` asserts the claim against a live guest, per case:

1. the guest boots with `--features interactive` and is given a fixture through the console;
2. the driver types `ls` once and captures it — the opening brief, read off the live machine;
3. `aletheiad console agent` is asked for the NEXT command; it validates, authorizes and renders
   exactly one line, and charges one step to the session's budget;
4. the driver types that line and hands the console's reply straight back through
   `--observation-file`;
5. repeat until the model answers, a bound refuses, or the driver's own cap trips — which would
   itself be a failure, because the budget is supposed to be the thing that stops this.

The asserted claim is the **last** command of the sequence: `wc backup` cannot be planned before `cp
manifesto backup` has happened, because `backup` does not exist. A session that typed it is a session
in which the model saw the machine move and acted on it.

Two arms, as with every AI gate here. The deterministic control arm needs no model, gates the whole
pipe in CI, and is an oracle for the loop, the rendering and the typing path. The model arm runs only
when a backend really is serving the selected model, and SKIPs — never silently passes — when it is
not.

The bounds are **always** driven by the deterministic arm, whichever arm the cases run under, and
that is not a convenience. They are properties of `agent::advance`, and proving them through a
language model proves them about one model's mood. The first live run said so out loud: asked to
`rm poem`, the model proposed something harmless instead, a line was correctly rendered, and the gate
reported *"a destructive step was rendered without approval"* — an alarm about Aletheia raised by the
model declining to be destructive.

## Consequences

**Good.** The console surface stops being bounded by what one line can express. The shell `case` list
is gone, and with it the second command table. A refused session leaves a transcript naming the step
that refused.

**Costs.** A request may now cost several model calls, and a slow backend multiplies. Sessions are
files, so a driver that abandons one leaves it on disk.

**Not claimed.** There is still **no inference engine in kernel space**. `kernel-core` remains
`no_std` with no network and no model. Every model call happens on the host, and what crosses into
the guest is one validated line of printable ASCII, indistinguishable from one a person typed.
`docs/MATURITY.md` governs every claim here.
