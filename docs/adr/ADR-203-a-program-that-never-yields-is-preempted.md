# ADR-203 — A program that never yields is preempted

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-SCHED-004 (new)
**Builds on:** ADR-199 (the live-run fence), ADR-201 (`run NAME`), ADR-202 (fault containment).

## Context

ADR-202 left one way for a program to take the machine: never yield. A console `run` dispatched
cooperative frames with interrupts masked at the lower privilege, so a branch-to-self held the CPU
forever and the console never answered again.

## Decision

* A `run` program's frame is preemptible: aarch64 SPSR 0x340 (IRQ unmasked at EL0), riscv64 the
  S-timer as the only `sie` source while it runs, x86-64 RFLAGS.IF in ring 3. The kernel side stays
  masked by the ADR-199 fence, so an interrupt is only ever taken from the program.
* The timer ends every slice the program does not end itself; at the 64-slice budget the run is
  abandoned (ADR-204 made the budget count only timer-ended slices, so syscalls do not spend it),
  its outcome fed back to the advisor as an eviction, its frames returned.
* The timer is shared with the live desktop's pump, and each target hands it back:
  * aarch64: the run brings the GIC and generic timer up only if the console has not (GICD_CTLR),
    and takes them down again only then; a fresh deadline is armed first so a pending tick cannot
    end the first slice before it starts.
  * riscv64: a fresh deadline and STIE for the run; the boot suite's run switches the deadline off
    afterwards, a live run leaves it for the pump's handler to re-arm.
  * x86-64: IRQ0 points at the ring-3 preemption entry for the run and back at the desktop's
    handler after; the PIT, which a console-only machine stops (`pit::quiesce`, ADR-168), is
    restarted for the run and stopped again, and IRQ0's PIC mask is restored.
* A new medium also holds `spin` (a branch to itself).

## Proof

* Boot, every target (`usermode` 38/38/46): `spin` is preempted every slice and abandoned at the
  budget; `hello` then still exits with 55. The boot suite gives the spinner a 4-slice budget
  (`BOOT_SPIN_SLICES`): the same proof, and the `usermode` lap stays at 130-144 ms instead of the
  719/504/220 ms that 64 real slices cost the first landing.
* Live, `scripts/console-e2e.sh`, all three CPUs (desktop live on aarch64/riscv64, console-only on
  x86-64): `run spin` is abandoned and the console answers the next command; `hello` exits with 55
  after it; the free frame count is unchanged across all runs.

## Non-claims

* The budget is slices, not time: 64 slices is about 0.5 s on aarch64 (8 ms slices), 0.3 s on
  riscv64 (5 ms), 0.6 s on x86-64 (the PIT's 10 ms). A long legitimate computation is abandoned
  too; per-program budgets and a real process lifetime are later work.
* Only the timer interrupt ends a slice by design; on aarch64 any other interrupt taken at EL0 is
  acknowledged by the preemption handler and ends the slice early (the device is serviced after
  the run). `run spin` on x86-64 with the desktop live is not exercised by a gate.
