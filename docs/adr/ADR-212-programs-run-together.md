# ADR-212 — Programs run together

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-008 (new)
**Builds on:** ADR-201 (a program from the namespace), ADR-203 (preemption), ADR-204/207 (the
program's syscalls), ADR-211 (a program's own layout).

## Context

Until now the console ran exactly one program at a time: `run NAME` placed it, ran it to its end
and only then returned. The scheduler, the advisor and the preemption timer were all real, but
they only ever had one user task to choose from. An operating system that cannot keep two programs
on the machine at once cannot host anything that is more than one program - a shell and its job, a
game and its audio, a server and its client. This is the next rung of
`docs/research/PRODUCTION-ROADMAP.md` after ADR-211.

## Decision

* **One run loop, N programs.** Each target's `run_programs(set, services, budget)` places every
  program in its own address space (code pages, stack, data, arguments - unchanged from
  ADR-206..211), admits each to the resident advisor as its own task (`TaskId(i)`,
  `task_index: i`), and dispatches them from one `PriorityScheduler`. At equal priority the
  scheduler rotates, so a timer slice or a syscall ends a program's turn and the next one runs.
  `run NAME` is the same path with a set of one, so every existing `run` proof also proves it.
* **The trap handler does not change.** Each program owns a `Slot` holding its share of the
  "program on the CPU" state: its saved frame (aarch64, x86-64), its `console.output` grant, its
  output sink and its syscall window. The loop swaps the slot into those statics before the slice
  and back out after it, so `SYS_WRITE_CONSOLE` and `SYS_FS_READ` serve whichever program is
  running without knowing others exist. A pending read is served through THAT program's frames.
* **Each program ends on its own.** Exit, a fault the supervisor terminates (charged to the
  program's own supervisor id, compared per slice) or its spent budget each `finish` that task
  alone; the others keep their turns. Each report carries `ended_at`, the set's dispatch count when
  the program ended, which is the observable proof that they took turns.
* **`together NAME1 NAME2 [NAME3] [NAME4]`** at the console: two to four programs, no arguments,
  every object judged before anything runs (one refusal refuses the set, by name). Each is
  reported in `run`'s words, then `together: finished in order: ...`. The hosted planner classifies
  it as `run` is classified: destructive, a human approves it.

## Proof

* Host: `PriorityScheduler` rotates equal-priority tasks and a finished one never returns
  (`kernel-core/tests/priosched.rs`); `together` refuses a bad set whole and reports each program
  (`kernel-core/tests/shell.rs`).
* Boot, every target (`usermode` 54/54/62): `spin` (never yields) is admitted FIRST with `hello`
  and `trap`; `hello` exits with its status and `trap` is terminated, both ending before the
  spinner, which is abandoned at its budget; the set gives back every frame and heap byte.
* Live console, every target (`scripts/console-e2e.sh`): `together spin hello trap` under the
  desktop prints `together: finished in order: hello trap spin`.

## Non-claims

* One CPU runs the set; programs do not yet run on several cores at once (the SMP scheduler of
  ADR-021 runs kernel tasks, not these).
* The console waits for the whole set; a program cannot yet be started in the background and left
  running while the console takes the next command.
* The advisor is still told `memory_pages: 2` per program, as before this wave; it does not yet see
  a program's real page count.
* Programs cannot talk to each other; each sees only its own pages and the namespace.
