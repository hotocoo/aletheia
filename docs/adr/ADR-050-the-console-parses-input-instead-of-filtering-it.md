# ADR-050 — The console parses its input instead of filtering it

**Status:** Accepted
**Date:** 2026-08-09
**Requirements:** REQ-CON-004 (a line editor an operator can work in)
**Closes:** GAPS4 `ALET-P2-040`
**Extends:** ADR-044 (interactive console), ADR-045 (interrupt-driven input), ADR-049 (keyboard)

---

## Context

ADR-044 built the console as a *filter*: printable ASCII enters the line, everything else is
discarded without an echo. That rule was written to be fail-closed, and as a rule about single bytes
it is correct. It is wrong about sequences, and a terminal sends sequences.

An arrow key is not a byte. On a serial line it is `ESC [ A`, three bytes arriving one at a time. The
filter dropped the `ESC` — a byte it had no rule for — and then admitted `[` and `A`, because those
*are* printable ASCII. So every press of every arrow key typed `[A` into the middle of whatever the
operator was writing. Home, End, Delete, Page Up and a bracketed paste did the same thing with
different letters. Backspace worked, so the console looked like it had a line editor; it had a line
*buffer* that a cursor key could corrupt without any way to see why.

This is what the report "the input is messed up" meant, and it is a failure the whole existing gate
structure was blind to for a specific reason: `console-e2e.sh` and `keyboard-e2e.sh` both type
*characters*. A gate that only presses letters can never find a bug in what happens when you press
an arrow.

There was a second half to the complaint — the command set was too small to be an operating
system's. That is ADR-051; this ADR is about the input path, because a bigger command set typed
through a broken editor is a bigger surface for the same bug.

---

## Decision

### 1. The editor parses escape sequences; it does not drop the introducer and keep the tail

`LineEditor` now runs a three-state machine — Ground, Escape, CSI — over its input. `ESC` opens a
sequence, parameters are collected up to a fixed bound, and the final byte (`0x40..=0x7e`) closes it.
**No byte inside a sequence can reach the line**, whether or not the editor understands the sequence.

The states are all named because each has a failure mode that is worse than the bug being fixed:

* **An unrecognized final byte closes the sequence anyway.** A parser that stayed armed waiting for a
  final byte it knew would eat the operator's next real keystroke — a console that swallows one key
  in ten is harder to diagnose than one that types garbage.
* **Parameters are counted, not buffered.** `ESC [` followed by four thousand digits is a legal thing
  for a hostile peer on a serial line to send. Past `CSI_PARAM_MAX` the bytes are consumed and
  forgotten, so the attack costs a fixed eight bytes rather than an allocation.
* **A control byte inside a sequence abandons the sequence and means what it says.** Otherwise a
  stray `ESC [` on a noisy wire would make the console ignore every line the operator typed until a
  letter happened to arrive, and the machine would look hung while working perfectly.
* **`ESC` followed by a non-introducer swallows that byte.** This is the one deliberate loss in the
  design: it is the fail-closed answer, and it is why the keyboard driver does *not* deliver the
  Escape key (§3).

### 2. Redrawing never re-emits the prompt

Editing in the middle of a line means repainting part of it. The editor repaints **from the cursor
rightwards only**, using the tail of the buffer, spaces to cover deleted characters, and backspaces
to return — the three things every terminal understands, including one that understands no escape
sequences at all.

The prompt stays the session's business. This is not tidiness: `console: the prompt returns after
every command` counts prompts in the transcript, and an editor that reprinted the prompt on every
mid-line edit would make that invariant depend on how the operator edited their line. A gate whose
result depends on typing style is not a gate.

### 3. One alphabet, defined once, shared by both producers

`shell::editor_accepts` is the console's input alphabet, and `keymap::Keymap::is_console_byte`
delegates to it. Two producers feed this editor — a UART and a scancode decoder — and the decoder's
security property is *"every byte I can emit is one the editor has a rule for"*. That property can
only be proved against a single definition; a second copy of the list in the decoder would be a
second list that drifts, and the drift would be invisible until a device someone else is holding
sent the byte that fell through the gap.

Widening the editor's alphabet is therefore the **only** thing that may widen what the keyboard can
send: the `Ctrl` chord decoder maps a letter to its control byte and delivers it only if
`editor_accepts` says yes. `Ctrl-G` and `Ctrl-Z` produce nothing, and adding a rule for them to the
editor is what would change that.

The Escape key remains undelivered by the keyboard, and now for a sharper reason than before:
delivering a lone `ESC` would arm the parser waiting for a final byte a human pressing Escape is
never going to send, and the next key they pressed would be eaten as the sequence's body. The key
that means "cancel" to a person is `Ctrl-C`, and that one is delivered.

### 4. The navigation keys speak the same grammar on both wires

The keyboard decoder emits `ESC [ D` for the left arrow rather than inventing a private byte. One
editor, one grammar, whichever wire the keystroke came in on — so a fix or a bug in cursor movement
is a fix or a bug for both input sources, and cannot be right in one and wrong in the other.

`ConsoleRing::push_seq` makes the **sequence** the ring's unit of admission for a decoded key: it
fits and is accepted, or it does not fit and every byte is counted as dropped. Pushing the bytes
individually would let a full ring keep `ESC [` and drop the `D`, and a truncated sequence is worse
than a dropped one for the reason in §1 — the parser would be waiting when the next real keystroke
arrived.

### 5. What a line editor is, since this one now claims to be one

Cursor movement (arrows, `Home`/`End`, `Ctrl-A`/`E`/`B`/`F`), insertion and deletion at the cursor
(including `Delete`, which is `ESC [ 3 ~`), kill-word (`Ctrl-W`), kill-to-end (`Ctrl-K`), kill-line
(`Ctrl-U`), a bounded history walked with the up/down arrows or `Ctrl-P`/`Ctrl-N`, and `Tab`
completion.

Two of those need justifying rather than listing:

* **History is bounded at 32 entries and records neither blank lines nor an immediate repeat.** A
  console is a long-lived session on a machine with no swap; "remember every line" is a slow leak
  with a human-shaped fuse. Walking *down* past the newest entry restores the half-typed line the
  walk interrupted, because losing it is the classic history bug and it loses the operator's work.
* **Completion is resolved by the session, not the editor.** The editor owns the line; the session
  owns the namespace. `Tab` returns `Edit::Complete` and the session completes the first word against
  the command table and later words against the objects that **actually exist on the device**. An
  ambiguous `Tab` prints the candidates and redraws the line rather than beeping or guessing.

### 6. The gates press keys, not letters

`keyboard-e2e.sh` now presses `left`, `up`, `tab`, `home` and `delete` by name at the emulated i8042
and asserts on **Aletheia's own filesystem output**, not on the echo: a name typed wrongly in the
middle is repaired with two left arrows and runs; the up arrow re-runs the previous command; `Tab`
completes an object name that exists on the device; `Home`+`Delete` repair a line typed wrongly at
its start. Fifteen new invariants in `shell::console_suite` cover the parser itself on all three CPU
targets, including the two that are pure regression tests: an arrow types nothing into the line, and
an over-long sequence is bounded and still terminates.

---

## Consequences

**Measured.** `[console] ALL 30 CONSOLE INVARIANTS HOLD` (was 15) and `[conring] ALL 9 INPUT-RING
INVARIANTS HOLD` (was 8) on aarch64, RISC-V and x86-64. `[keys] ALL 12 KEYBOARD-DECODE INVARIANTS
HOLD` (was 10). `KEYBOARD-E2E: PASS` with 11 checks (was 7), four of which press keys that used to
corrupt the line.

**The alphabet grew, and that is a widened attack surface stated plainly.** The editor now has rules
for `ESC` and eleven control bytes it previously refused. The compensating property is that the
decoder's exhaustive sweep — every scancode against every reachable modifier state, prefixed and not
— is now run against `shell::editor_accepts` itself, so the two can no longer disagree.

**Not claimed — a terminal emulator.** The editor writes backspaces and spaces, never cursor-address
sequences, and it does not know the terminal's width. A line longer than the window wraps and its
redraw will look wrong on the wrapped portion. Bounded, ugly, and honest: fixing it needs a terminal
size, and nothing on this wire reports one.

**Not claimed — multi-line editing, reverse search, or completion of anything but names.** `Ctrl-R`
does nothing. Completion knows commands and object names because those are the two namespaces that
exist.

**Not claimed — UTF-8.** The line is ASCII, by construction rather than by check. A multi-byte
character cannot be typed, and this is unchanged from ADR-044.

## Alternatives considered

**Keep filtering, and drop `[` and `A` too.** Rejected: it cannot be done without state. `[` after
`ESC` is a sequence introducer and `[` on its own is a character an operator may legitimately type;
telling them apart *is* the state machine.

**Have the keyboard emit private control bytes for the arrows (e.g. 0x1c..0x1f).** Rejected. It
would work for the keyboard and do nothing for the serial line, which is where the bug was first
visible — and it would leave the console with two grammars for one editor, which is the exact shape
ADR-049 §1 rejected for input paths.

**Let the editor own the prompt so it can do full-line redraws.** Rejected — §2. The gate counts
prompts.

**Use `ESC [ K` (erase to end of line) instead of spaces-and-backspaces.** Rejected: it assumes the
terminal parses sequences, and the console must remain usable on one that does not. The cost is a
few more bytes per edit on a line the operator is typing at human speed.
