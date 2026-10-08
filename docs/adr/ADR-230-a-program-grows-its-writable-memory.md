# ADR-230 — A program grows its writable memory at run time (`SYS_BRK`)

**Status:** Accepted (2026-10-08)
**Requirements:** REQ-USER-016 (new)
**Builds on:** ADR-210 (writable data page), ADR-214 (sixteen data pages), ADR-207/215 (requests
admitted by the handler, served by the run loop).

## Context

A program's writable memory was fixed when it was placed: the pages its image declared (`.data`
and `.bss`), at most sixteen. A program could not size a buffer from its input, and nothing larger
than 64 KiB of state could run. PRODUCTION-ROADMAP.md rung 1 lists run-time growth as open.

## Decision

* **`SYS_BRK = 16`, argument `top`.** The kernel maps zeroed, writable, non-executable pages
  contiguously above what the program holds, so that its writable memory reaches `top` rounded up
  to a page. `top == 0`, or any top at or below the current one, maps nothing and returns the
  current top. The answer is the top after the call, or `u64::MAX` when refused.
* **Memory never shrinks while a program runs.** All of it is given back when the program ends,
  through the same path as its image pages.
* **One ceiling for image and growth together:** `progout::DATA_CEILING_PAGES` = 64 pages
  (256 KiB) above the stack. The ELF judge still accepts at most sixteen declared pages.
* **No capability.** Growth touches only the program's own address space and is bounded by its
  ceiling, the way its stack is. With at most four programs at once, all programs together can
  hold at most 1 MiB of grown memory.
* **Same shape as the other services.** `progout::admit_brk` (shared, host-tested) decides; the
  trap handler answers a query itself and parks a growth request; the run loop maps the pages
  into the program's root once it is off the CPU, widens its `Window` (so `SYS_FS_READ`,
  `SYS_WRITE_CONSOLE` and `SYS_PRESENT` reach the new pages without change), and writes the
  answer into the saved return register. If frames run out part way, the pages just mapped are
  given back and the call is refused, so a failed call leaves nothing behind.
* The run loop's per-page views are sized by the ceiling (64 entries) on all three CPUs.

## Evidence (2026-10-08)

* `kernel-core` host test `brk_grows_by_whole_pages_up_to_the_ceiling_and_never_shrinks`.
* `userland/src/wide.rs` `grow` mode asks for a top 1 GiB away (must be refused), grows eight
  pages (must answer exactly the asked top), checks they are zero, fills them, reads `note` into
  the last, and asks for less (must not shrink). It exits with 442,368.
* Boot suites on aarch64, riscv64 and x86-64 run it: usermode 62/62/70 (`scripts/vm-e2e.sh`,
  `scripts/vm-e2e-riscv.sh`, `kernel-x86_64/scripts/smoke-test.sh`, all PASS).

## Not done

No shrinking, no `mmap`-style regions, no guard page between grown memory and the next mapping
(nothing is mapped above the ceiling). A general allocator in `userland/` is the next user of
this call.
