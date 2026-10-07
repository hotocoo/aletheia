# ADR-215 — A program draws into its own window

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-011 (new)
**Builds on:** ADR-084 (the window manager), ADR-213 (programs left running), ADR-214 (writable
memory of several pages).

## Context

Every pixel on the desktop so far was drawn by the kernel: the terminal, the monitor, the file
panel, the browser. A program could print to the console and nothing else, so no application with
a picture - a viewer, a tool, a game - could exist. With programs that keep running (ADR-213) and
have room for a frame buffer (ADR-214), the missing piece is a way for a program's pixels to reach
a window.

## Decision

* **`SYS_PRESENT(buf, width, height)` (13).** The program hands a packed one-bit bitmap - the
  compositor's own format, LSB first, row-major - that lies wholly in its data pages. The handler
  admits it (`progout::admit_present`: the run's grant must allow `window.present`, the size must
  be within 320 x 200, the bitmap inside the data pages) and the run loop serves it once the
  program is off the CPU, gathering the bitmap across its data frames (`gather_present`) straight
  into the window's buffer. The result is 0, or `u64::MAX` when refused.
* **One program window, owned.** `kernel_core::appwin::AppWindow` is the policy, over the window
  manager and the compositor and free of devices: the first program that presents gets a managed
  window (surface `APP`), raised; it is that program's alone - another program's present is
  refused - and it closes when the program ends (exit, fault, budget or `kill`). A size change
  reopens it in place. Its packed buffer is allocated when it opens, never per frame.
* **The operator wins.** A window the operator closes stays closed: its owner's next presents are
  refused, so the program can notice and end rather than have the window spring back.
* **A gate image has no live desktop**, so there a present is admitted and refused, and the
  program ends cleanly; the pixels are proved on the live desktop instead.
* **`draw`** (seeded, from `userland/src/draw.rs`) draws a framed 160 x 96 scene with a bar that
  moves a column per frame, in a two-page `.bss` frame, until it is killed or its window closed.

## Proof

* Host: `appwin` (ownership, refusal of a second program, resize, close on end, bad sizes and
  buffers, the operator's close sticking until the owner ends) and `progout` (grant, size bounds,
  bitmap inside the data pages, gathered byte-exact across a page boundary).
* Boot, every target (`usermode` 58/58/66): `draw once` in a gate image has its frame refused and
  exits 0 with nothing terminated.
* Live, every target (`scripts/console-e2e.sh`): `start draw` opens one more managed window
  (`windows: N+1 open`) and `kill draw` closes it (`N open, closed+1`).
* Live pixels, aarch64 and riscv64 (`scripts/desktop-e2e-dt.sh`): the scanout, dumped through
  QEMU, changes by thousands of pixels in the window's region when `draw` starts, keeps changing
  while the bar moves, and goes back when it is killed.

## Non-claims

* One program window at a time; one bit per pixel; no colour, no input routed to the program yet.
* Frames are copied, not shared: a program's buffer is gathered on every present.
* No frame pacing: a program presents as often as its turns allow.
