# ADR-039 — A fault handler must know what happened, and must not re-enter itself

**Status:** Accepted (2026-08-03)
**Context:** GAPS4 ALET-P1-009 / P1-010 / P1-011 / P1-013 · REQ-FAULT-001 / REQ-FAULT-002 · contracts in
`docs/INVARIANT-CONTRACTS.md` §INV-FAULT and §INV-REENTRY

## Context

Three separate gaps sat on the trap-entry path, and they share a shape: the code worked, and there was
no *model* behind it.

1. **No classification.** Each target's `#PF` / abort handler printed the raw architectural code and
   exited. That is honest, and it is also the whole story — there was nowhere to state that a fault
   reporting a **reserved bit** in a translation structure must never be resumed, because there was no
   vocabulary that distinguished it from a task touching an unmapped page.
2. **No re-entrancy statement.** A handler runs on top of whatever it interrupted. If both touch one
   structure — the console, a saved register context — the handler can observe it half-updated. Nothing
   said so, and nothing detected it.
3. **A manual ABI checked by comments.** The x86-64 trap assembly addresses `TrapFrame` with literal
   byte offsets (`[rdi + 152]`). Five offsets were asserted at compile time; the register slots were not.

## Decision

**1. Classification is a shared, total model — `kernel_core::faultclass`.** A normalized `Fault`
(present / write / user / exec / reserved_bit / from_kernel) that each architecture decodes into, a
`FaultKind` saying what it *means*, and a `FaultVerdict` saying what the kernel may do. Normalizing is
what lets one contract cover three CPUs; the decoders are `from_x86_error_code`, `from_aarch64_esr`
(EC + DFSC class + WnR) and `from_riscv_scause`.

**2. The policy is fail-closed, in every direction.** Only user faults are ever survivable
(`KillTask`); kernel faults, corrupt translations and unknown reports are `Panic`. A reserved bit
dominates every other reading — if a translation structure is malformed, what the other bits "mean" is
not knowable. And an architectural bit the model does not interpret (protection key, shadow stack, SGX)
makes the fault `Unknown` rather than being classified from the bits that happen to be understood: that
is what makes the model safe to extend, because a new bit degrades to fatal rather than to "routine".
Even `Fault::none()` is a kernel fault, so a decoder that forgets a field cannot make a fault look
routine.

**3. RISC-V's asymmetry is stated, not papered over.** `scause` reports neither present-vs-absent nor the
faulting privilege, so `from_riscv_scause` takes them as parameters from the caller (`sstatus.SPP`, and
whether the walk found a leaf). Inventing a `present` bit the ISA does not report would make the
classification a guess that reads like a fact.

**4. Re-entrancy becomes detectable and fatal — `kernel_core::reentry::ReentryGuard`.** The fault-report
path is wrapped: entering while already entered returns `None`, and the handler then prints one line and
exits with a distinct code (106) instead of recursing until the stack runs out and the machine
triple-faults. The counter is a compare-exchange, so the same guard also catches a *second CPU* entering
a section with no lock — a different bug with the same consequence. Refusals are counted, so a caller
that swallows one still leaves evidence.

**5. The manual ABI is checked exhaustively.** The `TrapFrame` const-assert block now pins size,
alignment, the register array's offset and width, the named register indices, every `iretq`-frame offset
the assembly uses, and that nothing hides past the last field. A partial assert set is what makes the
remaining literals *look* verified.

**6. The model must be proved where it runs, not only where it is convenient.** Exhaustive host proofs
(all x86 error codes including unknown-bit combinations, every EC/DFSC pair, every `scause`) plus three
x86-64 boot invariants (56–58) that the classification and the guard behave inside the kernel. A
classification that only holds in `cargo test` protects nothing.

## Consequences

* A `#PF` now reports *what it was* and *what the kernel is allowed to do*: the log line carries the
  kind, the decoded facts and the verdict.
* A fault inside fault reporting exits 106 with one line, instead of a triple fault.
* Extending the model is safe by construction: an unrecognized report is already fatal.
* **Wiring scope, stated:** the classifier and the guard are live on **x86-64**. The aarch64 and RISC-V
  decoders are host-proved but not yet wired — those handlers still print the raw `ESR` / `scause`.
* **What stays open.** ALET-P1-009 keeps its `fuzz` half: the register-file round-trip through the real
  trap assembly (build a frame of 15 distinct sentinels, take a trap, assert every register returned) is
  not written. ALET-P1-011 keeps adversarial *entry* testing: real ring-3 `#UD` and `#GP` trials,
  contained the way the isolation trials are, rather than only the decode surface being adversarial.
  Both rows carry that scope in the register rather than being flipped on a partial claim.

## Alternatives considered

* **Classify per target, in each handler.** Rejected: three implementations of one security-relevant
  decision, differing silently — the divergence Issue 1 exists to prevent.
* **Interpret the bits the model understands and ignore the rest.** Rejected: it makes an unknown fault
  look routine, which is the exact inversion of fail-closed.
* **A recursion depth counter that allows N levels of nested faults.** Rejected: there is no sound value
  for N. One nested fault already means the diagnostic path is broken; the second one cannot fix it.
* **`debug_assert!`-style layout checks.** Rejected: the manual ABI must fail the *build*, not a
  debug run — release builds are what boot.
