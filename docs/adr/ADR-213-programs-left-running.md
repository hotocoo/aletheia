# ADR-213 — Programs left running in the background

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-009 (new)
**Builds on:** ADR-212 (programs run together), ADR-180 (the console's idle loop), ADR-085 (the
live desktop's timer pump).

## Context

After ADR-212 several programs could share the CPU, but only while the console waited for all of
them: `run` and `together` return when the last program ends. Nothing could keep running while the
operator did something else, so no program could ever be a service, a game loop, or anything
that lives longer than one command. ADR-212 listed this as its second non-claim. It is the rung
every windowed or interactive program needs first: a program cannot draw or take input unless it
keeps running while the desktop and the console carry on.

## Decision

* **One table, every target.** `kernel_core::jobs::Jobs<S>` keeps up to `MAX_JOBS` (4) programs
  in fixed slots under a name and an id that is never reused while the machine runs. Each target
  stores its own placed program (`Slot`: address space, saved registers, grant, output) in it. A
  turn is taken on every idle pass of the console, so nothing in the table allocates.
* **The console's idle loop gives the turns.** When nobody is typing and a job is live,
  `run_loop_serviced` asks the target for one turn (`ShellHost::tick_jobs`) instead of sleeping.
  A turn is one slice, ended by the timer or a syscall exactly as in ADR-212, so a key pressed
  during a turn is read as soon as it ends. With no job live the console sleeps as before.
* **One slice, one code path.** Each target's `run_slice` is the body ADR-212's run loop already
  had (state onto the CPU, resume, state off, fault/exit/read handling); `run_programs` and the
  background tick both call it, so every `run` and `together` proof exercises it too.
* **Ids stay unique across foreground and background.** The supervisor's task counter is a
  high-water mark: a turn sets it to the running program's id and puts the high-water value back
  after, so an id handed out later never collides with a live job.
* **The desktop keeps drawing.** A program's slice ends on the timer the desktop is pumped from,
  but at user level, where nothing pumps it; a background turn pumps the desktop itself after the
  slice, so a program that never yields cannot freeze the screen.
* **x86-64: a latched tick is drained before each slice.** At the console IRQ0 is masked at the
  PIC while the 8254 keeps counting (or fires its last one-shot after `pit::quiesce`), so a tick
  sits latched in the PIC. Unmasked for a slice, it fired the moment the program entered ring 3
  and ended the slice before one instruction ran - every turn, so a background program never
  moved (57,000 slices and `hello` still running, found by the live console gate). Each turn now
  briefly enables interrupts on the plain handler so the latched tick lands there, then restarts
  the PIT period with `pit::init()` and installs the preemption entry. A whole blocking run only
  ever lost its first slice to this, which is why ADR-203..212 never saw it.
* **Console:** `start NAME [TEXT]` (judged as `run` judges, admitted through the resident advisor,
  console returned at once), `jobs` (id, name, slices so far) and `kill ID|NAME`. A job that ends on its
  own is reported in `run`'s words under `job ID (NAME) ended:`, and the prompt comes back. The
  hosted planner classifies `start` and `kill` as destructive and `jobs` as safe.
* **No budget in the background.** A background program runs until it exits, faults or is
  killed; the 64-slice budget stays a property of the blocking `run`/`together`.

## Proof

* Host: the table's rotation, its full-table refusal, id non-reuse and name cut
  (`kernel-core/src/jobs.rs`); the console's `start`/`jobs`/`kill` and the idle loop's end report
  (`kernel-core/tests/shell.rs`).
* Boot, every target (`usermode` 56/56/64): `spin` and `hello` started in the background; turns
  are given until `hello` ends on its own with its status, a foreground `run hello` works while
  the spinner is still live, `jobs` lists the spinner with slices, `kill` ends it, and every frame
  and heap byte comes back.
* Live console, every target (`scripts/console-e2e.sh`): `start spin`, `start hello`, `mem` twice
  two seconds apart, `jobs`, `kill spin`, `jobs` under the desktop - the spinner keeps running
  while the operator types, `hello` is reported as ended, the two heap readings taken while the
  console busy-polls for the spinner are equal (nothing allocated per pass), and the free frame
  count after the jobs end equals the count before any program ran.

## Non-claims

* All jobs share the console's CPU; they are not spread over the other cores.
* A job's output is shown when it ends, not as it is written.
* Jobs do not survive a reboot, and nothing restarts one that faulted.
* There is no priority between jobs or between jobs and the console beyond taking turns.
* A blocking `run` or `together` holds the console until it returns, and background jobs get no
  turns meanwhile.
