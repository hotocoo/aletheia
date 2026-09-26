# ADR-211 — A program larger than one page

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-USER-007 (new)
**Builds on:** ADR-201 (the ELF judge), ADR-210 (writable memory).

## Context

A program's code had to fit in one 4 KiB page, so nothing beyond a demo could be written. This is
the same rung as ADR-210 in `docs/research/PRODUCTION-ROADMAP.md`: an operating system whose
programs cannot grow is not one anything can be built on.

## Decision

* **The judge counts pages.** `elf::Target` gains `code_pages` (16 today, 64 KiB); a code segment
  up to that many pages is accepted, everything else about it unchanged.
* **A program's own layout.** A console-started program no longer borrows the boot suites' stub
  addresses: `PROGRAM_STACK_VA` sits above the reserved code pages, its data page above that. Code,
  stack and data stay contiguous, so one range check still covers the whole address space, and the
  boot suites' own stubs keep the addresses they had.
* **The run maps every page the segment declares**, read+execute, and frees them all at teardown;
  a segment whose pages cannot all be mapped runs nothing.
* **Names come from a page the loop can see.** A read's name may lie in the program's FIRST code
  page, its stack page, or its data page (`progout::NamePage`), because the run loop serves reads
  through physical frames and views exactly one. `userland`'s `probe` keeps its name in `.data`
  for that reason; a `&[u8]` constant would sit in `.rodata`, which is code.

## Proof

* Host: the judge's page bound and `progout`'s name resolution (a name past the first code page is
  refused; one in the data page is served from it).
* Boot, every target (`usermode` 52/52/60): `big` — three pages of code, a 2,048-word table it
  sums — runs and exits with 0x5839_3C00, which only the whole table produces. The ADR-204
  refusals now name the program's real window.
* Live, `scripts/console-e2e.sh`, all three CPUs: unchanged behaviour for the seeded programs.

## Non-claims

* 16 code pages and one data page. No heap, no `mmap`, no dynamic loading, still one program at a
  time.
* A name in the second or later code page is refused rather than resolved; programs put names in
  `.data` or on the stack.
