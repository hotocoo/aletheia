# ADR-180 — The console under hostile input

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-CON-008 (new), REQ-CON-002 (advanced), REQ-QUAL-007 (advanced)
**Builds on:** ADR-045 (the console's input ring), ADR-089 (the console session never grows),
ADR-063 (the boot heap never frees).

## Context

`console-e2e.sh` types what a person types. `shellstorm` (ADR-089) drives the dispatcher inside
the kernel. Nothing typed HOSTILE bytes at the running machine through its real serial path.

## Decision: `scripts/console-fuzz-e2e.sh`

A seeded generator types `FUZZ_LINES` lines into the booted OS on aarch64, riscv64 and x86-64:
every console command (read from the kernel's own table) with random, empty, huge, negative and
non-ASCII arguments; raw control bytes and escape sequences; bytes above 0x7f; lines of 257 to
2000 bytes; writes and appends until the scratch filesystem is full. After each line the driver
types `echo FZ-<n>` and waits for `FZ-<n>` to come back as output. PASS: every line answered within
`FUZZ_LINE_TIMEOUT`, nothing panicked or faulted, `ver` still answers, `halt` still gives the clean
exit code. A failure prints the seed, the line number and the bytes.

## What it found, and the fixes

Six defects, each reproduced, fixed at its root, and pinned by a test.

1. **A pasted long line hung the console** (all CPUs). The input ring held exactly `MAX_LINE` bytes,
   so a 256-byte line plus its CR overflowed, and drop-newest discarded the CR. The ring is now
   `MAX_LINE + 1` with the last slot kept for a terminator. A line that lost bytes gets its
   terminator delivered as Ctrl-C, so the damaged line is CANCELLED, never run truncated; the next
   line is clean. `conring` invariant 10; host tests over every overflow length.
2. **A lone ESC ate the Enter after it** (aarch64, x86-64). `ESC` then any byte swallowed that
   byte, CR included. A control byte after a lone ESC now means what it says, as it already did
   inside a CSI sequence. Host test `an_unfinished_escape_never_eats_the_enter`.
3. **The console could sleep on input that had already arrived** (all CPUs). The idle ran
   `wfi` / `sti; hlt` after `pop` had re-enabled interrupts, so a byte landing in between waited
   for the NEXT interrupt. Each target's `conirq::wait_for_input` checks the ring with interrupts
   masked and sleeps only if it is empty (a pending interrupt still ends the sleep).
4. **`Session::feed` cloned the whole history on every line** (~3.6 KB per command). It borrows it.
   The finished line is swapped out of the editor instead of taken (`Edit::Submitted`), so the line
   buffers keep their capacity.
5. **Tab completion allocated ~3.8 KB per press** (a `Vec<String>` of candidates plus a directory
   `list()`), and the history walk cloned entries. Both are allocation-free now: candidates are
   streamed twice through `Filesystem::for_each` with a fixed-size common prefix.
6. **The file panel re-listed the namespace after every command** through `fs.list()` and a
   `Vec<FileRow>`: ~227 bytes per command on the real targets, which killed every CPU after about
   1,900 commands. It streams into a fixed `[FileRow; MAX_FILES]`.

Before 4-6, a session died of heap exhaustion after ~1,100 to ~1,900 commands. Measured on real
aarch64 after them: an empty line, `ls`, `mem`, `history`, `echo`, `df`, `faults`, `tcp`/`resolve`/
`go` refusals cost 0 bytes; commands that carry data (`append`, `cat`) allocate that data.
`shellstorm` gains invariant 5: the whole session path (typing, Tab at a command and a file name,
a history walk, reporting commands) allocates NOTHING. `mem` now prints the heap watermark.

Also fixed in the harness: `-nographic` multiplexes QEMU's monitor behind Ctrl-A, so a fuzzed 0x01
was eaten by QEMU; the gate uses `-display none -serial stdio -monitor none`. And prompt counting
was replaced by the sentinel, because an ambiguous Tab legitimately reprints the prompt.

## Proof

`console-fuzz-e2e.sh` passes 1,500 lines on each CPU on two seeds, and a 5,000-line soak on a
third (10,000 commands per CPU counting the sentinels; 5-8 ms per line under TCG). vm-e2e on three CPUs (`conring=10`, `shellstorm=5`), conformance, console-e2e and the
kernel-core tests pass.

## Consequences

**Good.** The console survives input no person types, and a long session no longer runs out of
heap.

**Costs.** A damaged pasted line is cancelled; the operator retypes it (`input` reports the
dropped bytes).

**Not claimed.** The fuzz types through the serial line only, not the virtio keyboard. Data
commands still allocate their data on a heap that never frees: an unbounded `append` loop will
still end the session, by design of ADR-063, not by a leak.
