# ADR-204 — A program writes to the console

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-USER-002 (new)
**Builds on:** ADR-201..203 (programs from the namespace, contained and preempted).

## Context

A `run` program could only report an exit status. `SYS_FS_*` stay reserved "until user-memory
copying lands"; nothing copied from a program's memory into the kernel at all.

## Decision

* **`SYS_WRITE_CONSOLE` (12)**, capability `console.output`: `(address, length)`, returning the
  bytes kept or `u64::MAX` when refused. aarch64 reads the length from the saved x1 and writes the
  result into the saved x0 (the EL0 entry path returned nothing before); riscv64 takes a0/a1; x86-64
  gains a third argument from the saved rsi.
* **Authority.** `run` mints the program one `console.output` capability for the run
  (`progout::Grant`) and drops it after: the operator's `run`, authorized as `system.schedule`, is
  the root. The syscall is evaluated through `CapEngine::evaluate` like every other effect; a task
  started any other way holds no grant and is refused.
* **The range.** `usermem::UserSlice::validate` against the program's window, which on every target
  is exactly its two mapped pages (code, stack; asserted at compile time as `PROGRAM_WINDOW`). That
  equality is what makes the arithmetic check sufficient: a copy that faulted inside the trap would
  be fatal. riscv64 sets `sstatus.SUM` for exactly the copy and clears it after.
* **The sink.** `progout::OutputSink` keeps the first 256 bytes a run writes and counts the rest,
  allocating nothing on the trap path. The console prints what was kept through the file panel's
  byte filter (`filepanel::print_safely`, now shared: unprintable bytes shown as dots, never
  executed) and says how many bytes it did not show.
* The seeded `hello` now writes `hello from user mode` before summing and exiting with 55.

## Proof

* Host: `progout` (the bound and the count; a write with no grant, outside, straddling,
  overflowing or past the copy budget refused with nothing kept), `run` rendering an escape byte as
  a dot in `tests/shell.rs`, `syscall` table round trip.
* Boot, every target (`usermode` 41/41/49): `hello`'s line arrives; writes outside the pages,
  straddling their end, and past the copy budget return `u64::MAX` and keep nothing; a 400-byte
  flood keeps 256 and counts 144.
* Live, `scripts/console-e2e.sh`, all three CPUs: `run hello` prints `hello from user mode`.

## Non-claims

* PAN (aarch64) and SMAP (x86-64) are not present on the qualified CPUs (cortex-a72, `qemu64`), so
  the kernel reads user pages without toggling them; a CPU that enforces either needs
  `PSTATE.PAN`/`stac`-`clac` handling around the copy, which this wave does not add.
* The window check relies on every program having exactly those two pages; programs with more
  mappings need a page-table walk before any copy.
* Output is shown after the run, not streamed; there is no input, no file access, no argv.
