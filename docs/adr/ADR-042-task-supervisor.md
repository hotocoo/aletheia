# ADR-042 — Kill the task, keep the system

**Status:** Accepted (2026-08-03)
**Context:** REQ-REL-002 · GAPS4 (`docs/MATURITY.md`'s first production requirement) · consumes ADR-039's
classification; the narrow slice of REQ-REL-001 / ADR-026 that is provable today

## Context

ADR-039 gave every fault a classification and a verdict, and `FaultVerdict::KillTask` had **nowhere to go**.
Each target's handler ended the boot, because nothing could remove one task and let the rest continue. That
is why `docs/MATURITY.md` lists a task supervisor first among the things production would additionally
require: without it the kernel *detects* a bad access rather than *surviving* one, and every user bug is a
system outage.

The mechanism was closer than it looked. On x86-64, `isr_pf_entry` already abandons the faulting task and
returns to the scheduler — but only for an *armed* fault the isolation trial declared in advance; anything
else called `usermode_fatal`. So the missing piece was policy plus bookkeeping, not assembly.

## Decision

**1. Policy lives in `kernel_core::supervisor`, away from any target's trap path.** `on_fault` turns a
verdict into an action: a **user** fault terminates the task; a kernel fault, corrupt translation or unknown
report **escalates**, because the kernel cannot sensibly kill a task for its own bad access.

**2. A `KillTask` verdict with no task to blame escalates.** Something faulted at user privilege while the
kernel believed nothing was running — that is a kernel bug wearing a user fault's clothes, and treating it
as a task death would hide it.

**3. A terminated task is terminated forever, with a reason.** `may_run` is the question a scheduler asks
before dispatch and never answers yes again; `reason` distinguishes a fault from an exit from a policy kill,
so a log can say *why* a task died. Termination is idempotent and keeps the **first** reason — a later
policy sweep must not overwrite the fault that actually killed it.

**4. Contained and escalated faults are counted separately.** A system that quietly turned kernel bugs into
task deaths would look healthier than it is; `escalations()` is what makes that visible.

**5. The live proof is a fault taken on purpose.** The x86-64 suite runs a ring-3 task that reads a
supervisor-only page it never declared. Four invariants then require: exactly one task terminated, the dead
task may never run again and its recorded reason is the fault, **zero** escalations (a user fault was
contained, kernel bugs stay fatal), and — the point — **a later ring-3 task still runs and proves its own
invariant**. Ring-3 boundary invariants 22 → 26. Anything else would be a policy nobody had exercised.

## Consequences

* An undeclared ring-3 fault now prints its classification, terminates that task, and the boot continues:
  `task 7 TERMINATED (Fault(UserNotMapped)); system continues`.
* The verdict from ADR-039 is finally connected to an outcome; `KillTask` means something.
* **Not claimed.** The supervisor does not free the dead task's memory — that is
  `teardown::destroy_address_space` (ADR-032), which a caller invokes with the task's root, and this slice
  does not wire it into the fault path. It does not **restart** anything: a restart policy needs a
  supervision tree (REQ-REL-001), not a flag. The handler is wired on **all three** targets — an
  unexpected user fault routes through the supervisor everywhere, and every boot asserts the policy behaves
  (a user fault terminates that task, a kernel fault escalates) — but the **end-to-end** proof, taking an
  undeclared fault and then running another task, exists on **x86-64 only**. aarch64 and RISC-V need their
  own unarmed-fault excursion to make the same claim; until they have it, their invariant says exactly what
  it proves and no more. There is no quota,
  no rate limit on repeated faults, and one excursion runs at a time, so the task id is a counter rather
  than a TCB field. REQ-REL-001 stays deferred.

## Alternatives considered

* **Wire the policy into each target's assembly.** Rejected: three copies of a security-relevant decision,
  which ADR-036's reasoning already rules out. The policy is arch-independent; only the three lines that
  call it are not.
* **Terminate on ANY fault, including kernel ones, to keep booting.** Rejected — it is the inversion of
  fail-closed: the kernel would survive by ignoring evidence that its own memory model is broken.
* **Reclaim the task's address space inside the fault handler.** Rejected for now: teardown walks page
  tables and frees frames, which is real work to do on a trap stack with a task's state half-abandoned. It
  belongs in a scheduler reap step, and claiming it here without that step would be claiming a leak-free
  kill that leaks.
* **Prove the policy only in host tests.** Rejected: the interesting failure is a handler that says
  "terminated" and then wedges. Only taking a real fault in a real ring-3 task shows the boot continuing.
