# ADR-055 — A refusal the model never sees is a re-roll, and a bound that assumes a static machine is a bug

**Status:** Accepted
**Date:** 2026-08-10
**Requirements:** REQ-AI-008 (correctable proposals), REQ-AI-009 (staleness-aware no-progress, and a
gate that asserts a claim rather than a preference)
**Amends:** ADR-054 (a request is a bounded session at the console)
**Extends:** ADR-052 (the model is a system property), ADR-053 (the console is a planning surface)

---

## Context

ADR-054's loop passed its deterministic arm on the first attempt and then met a real model
(LFM2.5-2.6B-Q4_K_M, llama.cpp `--jinja`, `-c 8192`). It failed **all three** cases.

None of the three was a model failure. This is the third consecutive wave in which that sentence is
the finding — ADR-052 and ADR-053 each recorded a set of defects that "presented as the model being
incapable" — and it is now frequent enough to be a design rule rather than an anecdote.

## Decision

### 1. A proposal Aletheia can name the fault in is CORRECTED, not refused

Asked to show one line of an object, the model proposed `cat` with a two-word name. `console_ops`
refused it, correctly, and `advance` turned that refusal into the end of the session. Aletheia knew
exactly what was wrong — *"cat: name must be one word"* — and said it to nobody. A mistake the model
would have fixed in one sentence killed a session instead.

ADR-054's own enum comment claimed *"every reason here is a reason the next attempt would be
identical."* That was false for most of them. `Refusal::is_recoverable` had already been written, and
documented as *"the whole safety content of the agent loop's retry"* — and was never called.

A recoverable refusal now becomes a `Correction`: the proposal and the reason enter the transcript
the model reads, and the model is asked again, up to `MAX_CORRECTIONS = 3`.

**Corrections do not cost a console step.** The budget counts lines typed at a live machine, and a
refused proposal types nothing; charging one would make the number mean two things at once.
Termination is guaranteed by the separate correction bound, and by the fact that `advance` either
returns or refuses.

**The split is MALFORMING versus OVERREACHING**, and it is the whole safety content of the change:

| Class | Treatment | Why |
|---|---|---|
| Unknown command, wrong arity, empty argument, space in a one-word argument, control byte, line too long | Corrected | Aletheia can say what is wrong, and a second attempt is a different attempt. |
| `Refusal::Approval` — a destructive command without authority | Refused | The step was well-formed and the authority was absent. Asking again changes nothing except how many times the model was invited to try. |
| `halt` / `reboot` | Refused | ADR-054's reason is unchanged and is not about the model. |
| Budget exhausted, backend unreachable, driver protocol error | Refused | A further attempt cannot change any of these. |

### 2. No-progress must not assume the machine never moves

The model ran `cat poem`, then `append poem second line`, then `cat poem` — and the third step was
refused as *"already in the transcript"*. It was, and it said `hello world!`, because it ran **before**
the append. Aletheia refused the one command that would have answered the request.

That is precisely the defect ADR-054 accuses the previous gate's shell `case` list of: assuming a
picture of the machine stays true. The bound had the same blind spot, in Rust.

**Repetition is judged only over the turns since the machine last changed**, and what counts as
changing it is the `Destructive` classification already derived from `kernel_core::shell::COMMANDS`
(ADR-053). No second list of verbs. The rule is deliberately **conservative about staleness rather
than precise about dependency**: any mutation invalidates every earlier reading, including readings
of objects the mutation did not touch. Tracking per-object dependency would be a second model of the
filesystem living outside the kernel, which is the thing this whole line of work exists to avoid.

**The window applies to readings only**, and the first run of the fixed rule is why. With a window
that reset on every mutation, the same model ran `cat`, `append`, `cat`, `append`, `append` — each
`append` resetting the window that would have caught the next one — while `poem` grew from 25 bytes
to 49. Repeating a command that changes the machine is never progress: it teaches the model nothing
it did not already know, and unlike a repeated read it leaves damage behind. A mutation is therefore
refused if it has **ever** been run in the session.

### 3. A gate asserts a claim, not a preference

Asked how big a copy was, the model ran `cp manifesto backup` and then `stat backup`. That is
correct, and it is reached through exactly the dependency the case exists to assert — `backup` does
not exist until `cp` has run. The gate failed it for not being `wc backup`.

The kernel's table offers two commands that report a size. Insisting on one of them was the gate
asserting a preference and calling it a claim.

`AgentCase::must_type` is now a list of `Reached { line, console_says }` pairs. The driver requires
that **one** alternative was both typed at the live console and confirmed by what the console
printed — index-aligned across the `|`-separated TSV columns, because checking the two independently
would prove only that *some* asserted line ran and *some* asserted text appeared, which is a much
weaker claim and an invisibly weaker one in a passing log.

Every alternative must be independently sufficient, and a test enforces that each is a real console
command which renders exactly as written.

## The gate, on three targets

`scripts/console-agent-e2e.sh` is restructured into three legs — aarch64, riscv64, and x86-64 under
OVMF — closing the half of **ALET-P2-047** that observed a planning path driven on one target out of
three. The same dispatcher running on all three was never a reason to believe the loop had been
driven on more than one. x86-64 SKIPs, loudly, on a host without OVMF or mtools.

Corrections are reported on **stderr** — never stdout, which is the line the driver types — and the
gate surfaces them, because a turn that quietly cost three model calls is a turn nobody can debug
from a log that only records the attempt that worked.

## Consequences

**Good.** The loop recovers from the mistakes small models actually make. The bound that exists to
stop a loop stops it without also stopping the request. The gate stops failing correct answers.

**Costs.** One turn may now cost up to four model calls, so a slow backend multiplies further. The
transcript carries corrections, which grows the prompt — bounded by `MAX_CORRECTIONS`.

**Not claimed.** Unchanged from ADR-054: there is still no inference engine in kernel space, the
model runs on the host, and what crosses into the guest is one validated line of printable ASCII.
Approval remains a CLI flag rather than the Core's human-in-the-loop surface (**ALET-P2-046**, still
open). `docs/MATURITY.md` governs every claim here.
