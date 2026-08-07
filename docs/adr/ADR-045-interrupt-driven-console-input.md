# ADR-045 — The console stops spinning

**Status:** Accepted (2026-08-07)
**Context:** REQ-CON-002 · extends ADR-044 (the console) · closes the console's half of MATURITY item 3 (interrupt-driven I/O)

## Context

Every driver in this kernel polls. `docs/MATURITY.md` lists that as item 3 of what production would
additionally require, and the console (ADR-044) made it concrete rather than theoretical: `run_loop`
spun on `getc`, reading a register that was empty almost every time, and burned a core to do it. A
polled console is also a *lossy* one under load — bytes that arrive while a journal transaction is
committing sit in a 16-byte UART FIFO with nobody watching it.

Taking an interrupt for the console is not the same problem the timer path already solved. The timer
PPI is delivered while a task runs at **EL0** (vector 0x480). The console runs in kernel space, so its
interrupt arrives at *the current EL with SP_ELx* — **vector 0x280, which was a fatal catch-all**.
Making it live means giving up a safety net, and doing that carelessly converts "unexpected interrupt
in kernel space" from a loud failure into silence.

## Decision

**An interrupt hands bytes to a bounded ring; the console reads the ring.**

1. **The ring is arch-independent** (`kernel_core::conring`), because its policy is not a CPU
   property. Capacity is `shell::MAX_LINE`, so one whole line always fits: an overflow means the
   operator got ahead of a running command, never that a line was too long for the buffer carrying it.

2. **The overflow policy is DROP-NEWEST, and it is the substance of this decision.** A ring that
   overwrites its oldest byte is the conventional choice and it silently changes meaning: `rm notes`
   with its head overwritten reads as `notes` — a *different command*, which the editor would accept
   without complaint. Dropping the newest byte truncates a burst instead: the operator sees a short
   line and retypes it, and nothing they already typed was rewritten underneath them. **Every dropped
   byte is counted**, and `mem` reports the count, because input loss the operator cannot see is
   input loss they will blame on the command they typed.

3. **Vector 0x280 becomes a handler that is still fatal for every INTID except the console's.**
   Turning a fatal vector into a live one must not quietly swallow interrupts nobody expected: an
   unrecognized source is acknowledged (so the machine does not storm) and then exits 102, exactly as
   the catch-all did.

4. **Acknowledge before draining.** The UART's receive condition is level-triggered. Clearing it
   *after* draining loses a byte that lands mid-drain — its condition is cleared while the byte still
   sits in the FIFO, so no further interrupt is raised and the console goes deaf. This is not
   hypothetical: it is how the first working build failed, answering six commands and ignoring the
   seventh. Clearing first means a byte arriving mid-drain re-asserts and fires again.

5. **Both RXIM and RTIM.** With the receive interrupt alone, a burst shorter than the FIFO trigger
   level never raises anything — which is precisely what a human typing one character at a time
   produces. The receive-*timeout* interrupt is what makes single keystrokes work.

6. **No lock.** The ring has exactly two parties: the handler (producer) and the loop (consumer). A
   spinlock taken in a handler and in the code it interrupts is the classic self-deadlock. The
   consumer masks IRQs around its `pop`; the handler cannot be re-entered, because the CPU masks IRQs
   on entry and nothing unmasks before `eret`.

7. **All of it stays behind the `interactive` feature.** The non-interactive kernel every gate builds
   does not arm the GIC, does not enable the UART's interrupt, and never unmasks. Only the *handler*
   is compiled unconditionally — the vector must resolve its symbol in both builds — and it is inert
   because nothing ever routes an interrupt to it.

## Consequences

**What this buys.** The console is no longer a spin loop, and input that arrives while a command is
running is captured by the device rather than dropped on the floor. One item on the production list
now has one subsystem that does not belong to it.

**What it costs, stated plainly.**

* **Only aarch64 takes the interrupt.** x86-64 has a PIC with COM1 on IRQ4 and RISC-V needs a PLIC
  driver that does not exist yet; both still poll. The *ring* is proved on all three (8 live
  invariants each) so the policy cannot diverge, but "interrupt-driven" is an aarch64 claim today,
  and REQ-CON-002 is `partial` for that reason rather than `delivered`.
* The handler's own path — GIC acknowledge, FIFO drain, EOI — is hardware, so no host test covers it.
  What proves it is the scripted-operator gate: a session that answers command after command is a
  session whose interrupts kept arriving, and the ordering bug above shows that gate has teeth.
* The console still spins in `run_loop` when the ring is empty. A `wfi` there is the obvious next
  step and is not claimed here.
