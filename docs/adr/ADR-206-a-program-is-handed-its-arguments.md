# ADR-206 — A program is handed its arguments

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-USER-004 (new)
**Builds on:** ADR-205 (a program written in Rust).

## Decision

* `run NAME ARGS...`: everything after the name, as typed (the line editor already refuses
  unprintable bytes), up to `elf::MAX_ARGS` = 256 bytes. The shell refuses more by name; the target
  refuses it again before anything is mapped.
* The target copies the bytes to the top of the program's own stack page, 16-aligned, starts the
  stack below them, and enters the program with their address and length in the first two
  argument registers of the CPU's C calling convention (x0/x1, a0/a1, rdi/rsi). A Rust program
  therefore receives them as `_start(args: *const u8, len: usize)` with no shim.
* The Rust `hello` greets its arguments: `hello from user mode: <ARGS>`; with none, its line is
  unchanged.

## Proof

* Host: `tests/shell.rs` — the host is handed no bytes for `run hello` and exactly
  `to the  world` for `run hello to the  world`.
* Boot, every target (`usermode` 45/45/53): `hello` handed `World` prints
  `hello from user mode: World`; 257 bytes are refused before the program starts.
* Live, `scripts/console-e2e.sh`, all three CPUs: `run hello to the world` prints
  `hello from user mode: to the world`.

## Non-claims

* One flat string, not a vector: no quoting, no splitting, no environment. A program splits it
  itself.
* The argument bytes share the program's single stack page (at most 256 of its 4096 bytes).
