# ADR-210 — A program has writable memory

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-USER-006 (new)
**Builds on:** ADR-201 (the ELF judge), ADR-205 (the Rust userland), ADR-207 (reads).

## Context

A program had one read+execute page and one stack page. `userland/`'s linker scripts discarded
`.data` and `.bss` because the judge refused a second segment, so no program could hold a mutable
global, and `docs/research/PRODUCTION-ROADMAP.md` names this as the first rung under everything
else: a program that cannot keep state cannot grow into an application.

## Decision

* **A second segment, writable, never executable.** `elf::judge` accepts at most one `PT_LOAD` with
  `PF_W` beside the executable one: readable, not executable, at `target.data_va`, at most a page,
  file bytes inside the file, offset and address congruent modulo its alignment. Two of either, or
  a writable-and-executable segment, is still refused by name. `Placement` gains `data` (the file
  bytes) and `data_memsz` (what the program declared, `.bss` included).
* **Where it goes.** `data_va` is the page above the stack, so a program's pages stay contiguous:
  code, stack, data. The target maps a third page, copies the file bytes in and leaves the rest
  zero, so `.bss` starts zeroed by construction rather than by a program's own effort.
* **The window follows the program.** `progout::Window` replaces the three loose addresses the
  syscall paths passed: `data_top` is the end of the data page for a program that declared one, and
  the stack top for one that did not, so a program without a data segment has no address there at
  all. A read's buffer may now lie in the stack page or the data page, never in the code page.
* **`userland/` has two segments.** The linker scripts place `.data`/`.bss` at the data address;
  `counter` is a new seeded program with a `.data` global, two `.bss` globals and a global buffer
  it reads an object into.

## Proof

* Host: `a_writable_data_segment_is_accepted_only_where_the_target_puts_one` (`tests/elf.rs`): the
  accepted shape, and refusals for writable+executable, the wrong address, both size bounds; a
  program without one reports no data. `progout`: a buffer in the data page is admitted with that
  page's offset, a program without a data page has no address there, and a served read lands in it.
* Boot, every target (`usermode` 51/51/59): `counter`'s `.data` arrives as the image declared it
  and its `.bss` arrives zeroed, both writable (exit 55); an object read into a global buffer on
  the writable page (exit 65, `counter read: just words`).
* Live, `scripts/console-e2e.sh`, all three CPUs: `run counter manifesto` prints what it read and
  exits with 55 plus the object's length.

## Non-claims

* One writable page, so a program has about 4 KiB of globals and no heap. A `brk`/`mmap`-shaped
  syscall and more than one page per segment are the next rung.
* No relocations, no PIE, no dynamic loading; the data page's address is fixed by the target.
* Still one program at a time.
