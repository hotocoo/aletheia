# ADR-207 — A program reads the namespace

**Status:** Accepted (2026-09-27)
**Requirements:** REQ-USER-005 (new)
**Builds on:** ADR-204 (the write path and the per-run grant), ADR-206 (arguments).

## Context

`SYS_FS_READ` (8, `fs.read`) was reserved "until user-memory copying lands". A trap handler cannot
serve it: the namespace is the console's, mounted one call up from the run loop, not reachable from
an interrupt-time path.

## Decision

* **Admit in the handler, serve in the loop.** `(name, name_len, buf, buf_len)` arrive in the first
  four argument registers (x0..x3, a0..a3, rdi/rsi/rdx/r10 - x86-64's syscall entry now passes
  saved rdx and r10 too). The handler evaluates the run's grant (`progout::Grant` now mints
  `fs.read` beside `console.output`) and `progout::admit_read` checks, touching no memory: the name
  lies inside ONE of the program's two pages and is at most `fs::MAX_NAME` bytes; the buffer lies
  inside the STACK page only. It records the request and returns. Every trap already returns to the
  run loop; there, in the console's address space, `progout::serve_read` reads the name and writes
  the object through the program's physical frames, and the result goes into the saved return
  register. A request is never served after the program ends.
* **Why those two range rules.** Through physical frames the code page and the stack page are not
  adjacent, so a name straddling them would have to be stitched: refused. And a "buffer" in the
  read+execute code page, written through its frame, would rewrite the program's own text: refused.
* **No allocation per read.** `Filesystem::read_into` looks the slot up in place (no `DirEntry`
  and its `String`) and streams blocks into the buffer. It returns the object's full length; when
  that exceeds the buffer, only the buffer was filled, so a program always learns it was cut.
* **Services.** `ShellHost::run_program` takes `&mut dyn ProgramServices`; the console hands
  `FsServices` over its own mounted namespace, the boot suite one over a scratch namespace it
  formats itself, and a run handed `NoServices` is refused.
* **Programs.** `userland/` gains `show NAME` (seeded: reads an object and prints it) and `probe`
  (boot-only: calls the syscall the way its argument names). x86-64 programs get `memset`,
  `memcpy`, `memmove`, `memcmp` and `bcmp` (`userland/src/mem.rs`): the prebuilt `core` for
  `x86_64-unknown-none` calls them without providing them, so `show`'s zeroed buffer was a call
  through address 0 until this wave. The linker scripts keep `.got` in the text segment.
* **x86-64 entry alignment.** `_start` is now entered with `rsp + 8` 16-aligned, as SysV requires at
  a function's entry; it was entered 16-aligned since ADR-201, harmless only because the target has
  SSE off.

## Proof

* Host: `read_into` against `read` and truncation (`tests/fs.rs`); `admit_read` every refusal and
  `serve_read` name-in-either-page (`progout`); the shell hands the run a namespace it can read.
* Boot, every target (`usermode` 49/49/57): `probe read` returns 10 and prints `just words`; `show
  note` prints it; with no namespace the read is refused; a code-page buffer, a straddling name, a
  name outside, an empty name and a buffer past the stack are refused; 256 reads move the gross
  heap exactly as much as 16 (3,520-5,568 B, all per-run).
* Live, `scripts/console-e2e.sh`, all three CPUs: `run show manifesto` prints what the operator
  wrote.

## Non-claims

* Read-only, one object per call, into the stack page; no listing, no write, no handles.
* ADR-204's PAN/SMAP non-claim still holds for `SYS_WRITE_CONSOLE`, which reads the task's own
  translation; this path never touches a user address from ring 0/EL1/S-mode, so it needs neither.
