# ADR-205 — A program written in Rust

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-USER-003 (new)
**Builds on:** ADR-201 (the ELF judge), ADR-204 (`SYS_WRITE_CONSOLE`).

## Context

Every program the machine had run was machine code written out by hand and checked against LLVM's
assembler one instruction at a time. That does not scale past a handful of instructions, and it is
exactly the kind of artefact where a wrong byte hides. `kernel_core::elf` named the upgrade path: a
userland crate built by a cross toolchain.

## Decision

* `userland/` is a `no_std`, `no_main` Rust crate on the pinned toolchain
  (`nightly-2026-08-09`), built for `aarch64-unknown-none-softfloat`,
  `riscv64gc-unknown-none-elf` and `x86_64-unknown-none`. `src/sys.rs` is the program side of the
  syscall ABI (one trap per CPU); `src/hello.rs` writes its line, sums 1..=10 and exits with it.
* A linker script per CPU emits exactly the shape the ADR-201 judge accepts, unchanged: one
  read+execute `PT_LOAD` at the user code address (0x5000_0000; 0x4000_0000 on x86-64), file
  offset 0, headers included; `.data`/`.bss` discarded (a program with writable globals would need
  a second segment, which the judge refuses).
* The built ELFs are checked in under `userland/bin/<cpu>/`, the way the model blobs are, and each
  target embeds its own (`usermode::USERLAND_HELLO`) and seeds a new namespace's `hello` with it.
  `scripts/check-userland.sh` rebuilds them and requires byte-identity with what is checked in; a
  new CI job runs it with the three targets installed.

## Proof

* `check-userland.sh`: all three rebuild byte-identically (241, 269 and 225 bytes of segment).
* Boot, every target (`usermode` 43/43/51): the Rust-built `hello` is judged and runs, prints
  `hello from user mode` and exits with 55.
* Live, `scripts/console-e2e.sh`, all three CPUs: the seeded `hello` is now the Rust one, and the
  same assertions hold.

## Non-claims

* Byte-identity is proved on the development host (macOS); CI's Linux runner runs the same gate on
  every push, so a host or a future toolchain that stops reproducing it fails the gate rather than
  drifting.
* The judge's limits stand: one page, no writable segment, no relocations. The boot suite's
  adversarial programs (trap, spin, the writers) stay hand-assembled on purpose: they must be the
  exact instructions they claim to be.
