# ADR-214 — A program's writable memory is several pages

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-010 (new)
**Builds on:** ADR-210 (a program has writable memory), ADR-211 (a program larger than one page),
ADR-207 (a program reads the namespace).

## Context

ADR-210 gave a program one writable page: every global, every buffer and every `.bss` byte had
to fit in 4 KiB. Nothing a person would recognise as an application fits in that - a text buffer,
a decoded image, a frame to draw (a 640 x 240 one-bit surface alone is about five pages), a game's
state. ADR-211 widened code to 16 pages; this does the same for data, and it is the rung a program
that draws into a window needs first.

## Decision

* **The judge counts data pages.** `elf::Target` gains `data_pages` (16 on every target, 64 KiB).
  A writable, non-executable segment at the target's data address may declare up to that many
  pages of `.data` + `.bss`; one byte more is refused as too large. A target that reserves one
  page keeps ADR-210's bound exactly.
* **The target maps every page the segment declares**, contiguous above the stack, each zeroed and
  then filled with its share of the image's file bytes, so `.bss` starts zeroed wherever it falls.
  A segment whose pages cannot all be mapped runs nothing, and every page is given back at the end.
  The syscall window's `data_top` follows the pages the program actually has.
* **Reads land in one data page of several.** `progout` finds which data page a name or a buffer
  lies in, by offset from the first; a range that runs from one data page into the next is refused
  for the same reason a name straddling code and stack is: the run loop serves through physical
  frames, which are not adjacent. `serve_read` is handed one view per data page.
* **Static size, no `brk`.** A program's writable memory is sized when it is linked (the userland
  linker scripts assert it fits the 16 pages); a userland allocator can sit on top of it. Growing
  it at run time waits until a program needs that.

## Proof

* Host: the judge accepts a 16-page segment with file bytes past the first page and refuses one
  byte more, and a one-page target keeps the old bound (`kernel-core/tests/elf.rs`); a read names
  one data page of several, a buffer straddling two is refused, a request naming a page the loop
  was not handed is refused (`kernel-core/src/progout.rs`).
* Boot, every target (`usermode` 57/57/65): `wide`, built from `userland/src/wide.rs`, fills three
  pages of `.bss` with 1, 2, 3 and exits with their sum, 24,576 - only if every page arrived zeroed
  and distinct; given `note`, it reads the object into the middle of its second page, prints it,
  and exits with 25,601.

## Non-claims

* No run-time growth (`brk`/`mmap`); no shared memory between programs.
* A read's buffer must lie inside one data page; a program reading more than 4 KiB at once must
  split it.
