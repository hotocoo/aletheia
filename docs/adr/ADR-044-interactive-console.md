# ADR-044 — An OS you can sit in front of

**Status:** Accepted (2026-08-07)
**Context:** REQ-CON-001 · builds on ADR-035 (filesystem namespace), ADR-038 (durable store), ADR-010 (contract-honest)

## Context

Every target in this repo boots, proves its invariants, and **exits with a verdict**. That discipline is why
the claims here are checkable — and it is also why the most ordinary question anyone can ask about an
operating system had no answer:

> Can I run it?

You could run a *proof*. You could not run the *system*. Nothing kept the machine up, nothing read input, and
every one of the three targets' UART drivers was transmit-only: `putc`, `puts`, and no receive path at all.
That is not a small omission dressed up as a design choice. An OS that cannot be used is an OS whose
usability has never been tested, and "the invariants hold" is a weaker claim than it sounds when the only
program that ever ran was the invariant suite.

The competing pressure is real: an interactive session has **no exit code**. Every gate here asserts a process
exit status and a `[e2e] PASS` marker, and a boot that waited for input would hang CI instead of failing it.
Whatever we build must not be able to break that.

## Decision

**A polled, in-kernel interactive console — arch-independent, gated by a cargo feature, and proved by a
scripted operator rather than by a human noticing it works.**

1. **The console lives in `kernel_core::shell`, once.** A serial port differs per target; a line does not.
   Each target supplies exactly two things — a non-blocking `getc` and a way to print — and inherits the
   editor, the command grammar, every refusal, and the loop. The alternative (a console per target) would
   have produced three input boundaries with three sets of bugs, which is precisely the divergence
   `scripts/conformance.sh` exists to prevent.

2. **The editor is a filter, not a buffer.** It is the first thing a human byte touches, so admission is
   explicit and everything else is discarded:
   - only printable ASCII (`0x20..=0x7e`) may **enter** a line;
   - only CR/LF **ends** one; Ctrl-C discards it; backspace/DEL and Ctrl-U edit it;
   - **every other byte is dropped without an echo** — no escape sequences, no bytes >= 0x80, no stray
     control codes;
   - a line stops growing at `MAX_LINE` (256): further input is dropped, so a terminal that pastes a
     megabyte cannot make the kernel allocate one.

   Because only ASCII is admitted, the buffer is valid UTF-8 *by construction* rather than by a check that
   could be forgotten. What the user sees is what the kernel holds: a refused byte is never echoed.

3. **Commands drive only subsystems that are already proved** — the named-object namespace over the journal
   (ADR-035), the frame allocator, the HAL clock. `write` uses `Filesystem::replace`, so one keystroke
   sequence is one transaction and a crash mid-write leaves the old contents or the new ones, never a
   vanished name. The console therefore adds *reach*, not unproved surface: everything it can do, something
   already gates.

4. **Interactivity is a cargo feature (`interactive`), off by default.** Without it the boot ends exactly as
   before, so every gate keeps its exit-code contract; with it, the boot hands the machine to the serial line
   after the suites pass. This is the decision that lets the console exist without weakening anything.

5. **A session still has an exit-code contract**, because `halt` is a command: the guest stops through the
   same path the gates use (semihosting / SiFive test / `isa-debug-exit`). So a scripted session can be a
   **gate**: `scripts/console-e2e.sh` boots each target with the feature on, types at it, and asserts the
   transcript — and a wedged console fails as a timeout rather than hanging CI forever.

6. **The operator waits for the prompt; it does not sleep.** A byte typed before the console exists is not
   merely early — on x86-64 the boot's `serial::init` clears the receive FIFO, so those keystrokes are
   *destroyed*. A fixed delay would have made the gate a race against however long the suites take on that
   host. The gate watches for `aletheia> `, then types one line at a time, waiting for the next prompt.

7. **A device that mounts is never reformatted.** The console prefers the persistent disk when one is
   attached and formats only a device that carries no namespace. An interactive session must not be the thing
   that eats the disk.

## Consequences

**What this buys.** The reboot demonstration is now first-person: write an object at the prompt, halt, boot
again, and read it back. The cross-reboot durability ADR-038 proved is something a person can now *do*,
on all three CPU targets, rather than something a log asserts.

**What it costs.**

* The dispatcher runs in **kernel space** over the kernel's own objects. It is *not* a user-mode shell
  process over a syscall ABI, and this ADR does not claim one. That is the honest next slice: the syscall
  surface each target exposes today is narrower than a shell needs.
* The console is **polled**, like every driver here. It is provable and it burns a core while idle.
* `getc` is the first *input* path in the kernel. It reads a hardware register a device controls, so it is
  the one part of this that no host test can prove; the byte-space sweeps prove what happens to a byte
  **after** it arrives, on every target, and the scripted-operator gate proves that real bytes do arrive.

**Proof obligations discharged.** 15 live invariants per target (scripted sessions against a real namespace,
inside the boot gate — so the gate covers the code an interactive boot runs, not a parallel path only humans
see); 20 host tests that attack the editor and dispatcher, including a sweep of all 256 byte values; 7
behaviors added to the cross-architecture conformance contract, because "what may become a command" and
"is a console write committed" must not vary by CPU; and a three-target end-to-end gate in which an operator
types, writes, halts, and reboots.
