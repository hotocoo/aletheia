# ADR-221 — The operator adds programs to the boot

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-CON-013 (new)
**Builds on:** ADR-213 (programs left running in the background), ADR-219 (the machine keeps its
settings).

## Context

What runs when the machine comes up was fixed in the kernel: the desktop and the console. A person
who writes a program - a clock, a monitor, a game server - had to type `start` after every boot.
Letting the operator extend what the machine does at boot, without rebuilding it, is the first
piece of a mod system.

## Decision

* **`autostart=NAME` lines in the `settings` object** name programs to start, in order, up to
  `jobs::MAX_JOBS` (4) - the job table's size, so every one listed can actually run. A duplicate,
  an empty name or a fifth line is a skipped line.
* **`autostart [add NAME | remove NAME]`** at the console lists, adds (refused when there is no
  such object, it is listed already, or the list is full) and removes, keeping the rest in order.
  The hosted planner classifies it as destructive: it changes what runs at every boot.
* **The console starts them before its first prompt**, after the other settings, through the same
  path as `start` - judged, admitted through the resident advisor, left as background jobs - so a
  program that is not one for this CPU is refused by name and the console starts regardless.

## Proof

* Host: order, duplicates, the cap and the render/parse round trip (`kernel-core/src/settings.rs`);
  `autostart add hello` is kept, a missing object refused, and the next console start has `hello`
  running as job 1 before the first prompt (`kernel-core/tests/shell.rs`).
* Live, every target (`scripts/console-e2e.sh`): session one adds `hello`; the second boot of the
  same disk prints `settings: start: hello is job N, running in the background` and later reports
  `hello` ended - on aarch64, riscv64 and x86-64.

## Non-claims

* No arguments for an autostart program, no ordering beyond the list, no restart when one fails.
