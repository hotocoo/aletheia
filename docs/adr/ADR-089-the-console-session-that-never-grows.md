# ADR-089 — The console session that never grows

* Status: accepted
* Date: 2026-09-03
* Register: ALET-P2-040 (console), ALET-P2-009 (soak), REQ-CON-001, REQ-QUAL-007, REQ-CON-007
* Extends ADR-044 (the interactive console), ADR-050 (the line editor),
  ADR-086 / ADR-087 / ADR-088 (the storm discipline), ADR-063 (the boot heap never frees)

## Context

Three storms had already measured the desktop, the scheduler and the filesystem at volume and each
found a per-event allocation on a heap that never frees. The console is the fourth hot path — and
the only one a HUMAN drives. A session is exactly a stream of commands, so a console that spends
memory per command is a machine that dies of being used.

Measured before this wave: **~453 bytes per command**.

## Decision

* **`kernel-core/src/linebuf.rs`** — `LineBuf<N>`, a fixed-size stack buffer implementing
  `core::fmt::Write`, plus the `outf!` macro. Every one of the console's fifty
  `out(&format!(...))` sites became `outf!(out, ...)`: the same `write!` formatting, on the stack.
  Truncation is NAMED (`truncated()`), never silent, and never splits a character — a partial
  UTF-8 sequence is dropped whole, so what the console prints is always valid.
* **A reused history buffer.** The line editor rotated its bounded history by dropping the oldest
  `String` and allocating a new one per submitted line. It now reuses the oldest buffer.
* **Streamed listings.** `Filesystem::for_each` walks directory slots IN PLACE; `ls` and `find`
  use it instead of `list()`, which builds a `Vec<DirEntry>` of owned `String`s. `list` stays for
  callers that keep the answer.
* **An allocation-free capability check.** `CapEngine::allows` is the yes/no half of `evaluate`:
  same tokens, same order, no `Decision::Deny(String)` built for a caller that only asks "may I?".
  The console asks on every command.
* **In-place wildcard matching.** `capalg::action_covers` compared a `format!("{}.", prefix)`
  against the action — a `String` on EVERY wildcard capability test. It now compares in place.
  This one was invisible until the storm ran on the TARGET: the host suite passed at zero while
  the machine still spent eight bytes a command, because the host's stand-in host authorized
  without a capability engine.

`kernel-core/src/shellstorm.rs` holds the dispatcher to four claims on all three CPUs, measured on
the platform's own heap: reporting commands (`help`, `ver`, `mem`, `ls`, `history`) allocate
NOTHING; a thousand submitted lines keep a bounded history and cost only the line itself; a
command that returns DATA costs its data, not a multiple of it; and the same session twice prints
byte-for-byte the same answer.

Measured after: **0 bytes** for a reporting command.

## Consequences

* Two new boot-gate families on all three targets: `[linebuf] ALL 4 LINE-BUFFER INVARIANTS HOLD`
  (fails `800 + i`) and `[shellstorm] ALL 4 CONSOLE-STORM INVARIANTS HOLD` (fails `820 + i`).
* Five new cross-CPU conformance behaviours.
* `LineEditor::history_len` is observable so a suite can pin the bound without reaching inside.
* The line width is `LINE_MAX = 256`. A console line longer than that is cut and says so — the
  widest line this kernel prints is well under it.

## Named non-claims

* **`Edit::Line` still owns its bytes.** The finished line is handed to the caller as a `String`;
  the claim is that a submission costs the LINE, not the line plus a history copy kept forever.
* **Commands that return data still allocate that data** (`cat`, `wc`, `hexdump`): those are the
  caller's bytes, named rather than hidden.
* **`Decision::Deny(String)` is untouched** where a refusal is AUDITED — naming a refusal is worth
  an allocation; asking a yes/no question is not.
