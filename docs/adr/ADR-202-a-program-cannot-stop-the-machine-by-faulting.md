# ADR-202 — A program cannot stop the machine by faulting

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-FAULT-004 (new)
**Builds on:** ADR-042 (the supervisor), ADR-201 (a program from the namespace runs).

## Context

ADR-201 named its gap: a program that takes an exception other than a page fault stopped the
machine. aarch64 sent every lower-EL synchronous exception except `svc` and a data abort to the
fatal catch-all; riscv64 exited on every U-mode cause except `ecall` and the three page faults;
x86-64 contained `#UD`, `#GP` and `#PF` only while the boot suite ran, restored the fatal handlers
afterwards, and left `#DE`, `#DB`, `#NM`, `#TS`, `#NP`, `#SS`, `#MF`, `#AC` and `#XM` with no handler
at all (a double fault). The run loop also could not tell a terminated program from one still
running, and charged its faults to a stale supervisor id.

## Decision

* **Every exception a program raises costs that program.** aarch64: the lower-EL vector routes an
  instruction abort (EC 0x20) with data aborts, and every other class (UDF, BRK, alignment,
  illegal state, trapped system-register access) to `el0_contained_exception`. riscv64: every U-mode
  cause that is not `ecall` or a page fault goes to `supervise_user_exception`. x86-64: `run`
  installs ring-3 entries for `#UD`, `#GP` and the nine vectors above for exactly the run and
  restores the fatal handlers after; entries with an error code test the pushed CS and treat a
  ring-0 arrival as fatal. Each builds its fault the way x86-64's `#UD` always did (a user fault,
  present, exec) and asks the supervisor; only an escalation, which a kernel inconsistency
  produces, stops the machine.
* The kernel's own `#DE` ... `#XM` now report and exit (107) instead of double-faulting.
* **The run loop sees the termination.** Each `run` takes a fresh supervisor id; a change in the
  supervisor's terminated count ends the run as `TERMINATED (<kind>)`, the outcome fed back to the
  advisor as a failure, and the death record reaped (`Supervisor::reap`, still counted) so repeated
  faulting runs hold no record each.
* A new medium is also seeded with `trap`, whose first instruction is undefined on its CPU.

## Proof

* Boot, every target (`usermode` 33 -> 36 on aarch64/riscv64, 40 -> 44 on x86-64): `hello` exits
  with 55; `trap` is terminated and the machine continues; sixteen more faulting runs are all
  contained, hold no death record and give back every frame and live heap byte; on x86-64 also a
  ring-3 divide by zero.
* Live, `scripts/console-e2e.sh`, all three CPUs under the running desktop: `run trap` twice, each
  "TERMINATED (user-permission) after 1 slice(s); the machine continues", then `run hello` still
  exits with 55. Host: `Supervisor::reap` unit test.

## Non-claims

* A program that never yields still holds the CPU: the slice is cooperative under ADR-199's fence
  (interrupts masked). A timer-enforced slice budget needs the generic timer / S-timer / PIT shared
  with the desktop's pump and its IRQ routing at the lower privilege, and is ADR-203's.
* x86-64 `#DB` from `icebp` and `#MF`/`#XM` depend on CPU configuration this wave does not change;
  they are contained if raised, not provoked by a gate. Only `#UD`-class and `#DE` programs are
  exercised.
