# ADR-059: The console asks before it destroys

**Status:** Accepted · **Date:** 2026-08-22 · **Closes:** ALET-P2-046 · **Builds on:** ADR-015, ADR-052, ADR-053, ADR-054

## Context

The console planner (ADR-053) validated every model-proposed line against the kernel's own command
table and refused a destructive one before it was rendered — the safe half of governance. But its
approval input was a CLI flag (`--approve`), which is not governance: it is a pre-given answer. A
destructive console command was never ASKED — the caller either passed the flag or the line died,
and in neither case did a human decision exist anywhere in the system's own records.

Meanwhile the Core already had the whole machinery (ADR-015): a policy engine independent of
authority, pending approvals bound to the exact intent, durable `ApprovalRequested`/
`ApprovalResolved` events replayed on open. It just had nothing to do with the console.

## Decision

A destructive console line becomes a question in the Core's approval store, bound to the EXACT
rendered line, answered by a human, spendable exactly once:

```text
plan → classify (console registry, the hosted mirror of the kernel table)
     → safe   ⇒ lines type as before
     → destructive ⇒ request_console_approval   — durable Pending, IDEMPOTENT per (subject, line)
     → human answers via `aletheiad approvals grant|deny <id>` (or denies; both are records)
     → re-plan ⇒ take_console_approval at TYPING TIME — spends the grant on that one line
     → Consumed is terminal; replaying the event log cannot resurrect it
```

The binding rules are the substance:

1. **Bound to the rendered line.** Not to the request, not to the plan — to the exact ASCII string
   the console would receive (`rm poem`, control-byte-checked, length-bounded). A yes for one line
   is silence about every other; the gate proves a granted `rm poem` does not let `rm manifesto`
   through.
2. **Spent once, at typing time.** `take_console_approval` runs in the driver at the moment of
   typing, atomically moves Granted→Consumed, and emits `ApprovalConsumed`. One grant buys one
   typed line; a second run asks again.
3. **Idempotent ask.** Re-requesting the same line while its approval is pending returns the SAME
   record. An ask that minted duplicates would let one line accumulate grants it did not earn.
4. **Denial is also a record.** `Denied` is terminal for that record; asking again opens a NEW
   question rather than re-opening the refused one.
5. **The verdict comes from THE policy engine.** `request_console_approval` calls
   `PolicyEngine::evaluate`, so a future change to what needs approval applies here without a
   second implementation drifting into existence.

### Authority boundary, stated plainly

These three operations are LOCAL-OPERATOR authority and say so. `aletheiad` already runs with the
operator's power over its data directory (it can rewrite the store file), so capability checks here
would be theater; what they buy instead is that **no destructive line is typed without a recorded
human yes bound to that exact line, in an audit log that survives restart**. For exactly that
reason they are deliberately NOT exposed through the service boundary (`service.rs`), where an
untrusted client could otherwise grant itself destructive lines; the IPC surface keeps the
capability-checked `ResolveApproval` for core intents only.

Console approvals bind to a first-class intent verb, `Verb::Console { line }`. The core never
executes one — execution IS typing, which no interpreter does — and the pipeline refuses such an
intent outright if one ever reaches it, because guessing at execution semantics for a verb whose
meaning is "a human said yes to this exact line" would be worse than refusing.

### Exit codes are the contract with drivers

`aletheiad console plan`: **0** stdout holds line(s) to type · **1** refused · **7** approval
required (the id is on stderr). Stdout still means "type this" and nothing else, so a driver that
pipes stdout at a serial port can never type an unanswered question.

## Consequences

- `--approve` remains, now said out loud: stderr reports consent granted inline and that NO record
  was made, so a benchmark arm and a governed operator can never look identical again.
- The e2e gate (`console-ai-e2e.sh`) replaced its refuse-only check with the full dance per arm:
  ask (7) → listed Pending → deny → fresh ask → grant binds exactly its line → typed once →
  Consumed recorded → spent grant types nothing. All records live in a per-arm scratch data dir,
  because grading governance in the operator's real `~/.aletheia` would inherit last week's
  answers — a gate that reads old answers is not a gate.
- Host proofs live beside the machinery: `policy.rs` (spend-once state machine),
  `tests/console_approvals.rs` (durability across reopen, idempotence, binding, denial,
  consumption-survives-restart, and the two approval worlds refusing each other's records).

## Non-claims

- ~~The AGENT loop still carries `approved` as a session flag~~ - CLOSED in the same wave:
  `advance` now consults a `Governor` seam. An unapproved session's destructive proposal ASKS
  (exit 7, id on stderr, question carried IN the transcript so it survives the process); a grant
  is spent at typing time exactly once; a denial becomes a Correction to the model while a second
  insistence on an already-refused line stays terminal overreach (`denied_lines`); an unreachable
  store refuses by name rather than typing ungoverned. `--approve` remains inline consent that
  says it recorded nothing. The gate (`console-agent-e2e.sh`) replaced its refuse-only bound with
  the full dance against a live machine.
- No expiry test by time travel: TTL logic is inherited unchanged from ADR-015 and exercised at
  the boundaries the tests can reach deterministically.
- No kernel changes: this is entirely the hosted side, consistent with ADR-053 — no inference and
  no governance engine entered kernel space.
