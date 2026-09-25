# ADR-182 — The desktop under random input, and heap forensics

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-GFX-013 (new), REQ-QUAL-007 (advanced)
**Builds on:** ADR-084/086 (managed windows, the desktop under storm), ADR-180 (the console under
hostile input), ADR-063 (the boot heap never frees).

## Context

The desktop gates send scripted, sensible device events; `wmstorm` drives the window manager inside
the kernel. Nothing threw random events at the running desktop through the real virtio keyboard
and tablet, and nothing measured what a long interactive session costs the heap.

## Decision

* **`scripts/desktop-fuzz-e2e.sh`** sends seeded random device events through QMP on aarch64,
  riscv64 and x86-64: keys with Ctrl/Alt/Shift/Meta chords, function keys, arrows, Enter/Tab/Esc;
  pointer moves anywhere including edges and corners, half of them aimed at title bands, window
  boxes and the taskbar; clicks, double clicks, wheel turns, and drags paced a desktop tick apart.
  After random batches the serial console must answer an `echo` sentinel and nothing may panic.
  Warm-up (maximize/restore of every window, then two thirds of the storm) pays every buffer's
  one-time growth; the last third must grow the heap by less than 16 KiB.
* **`heaptrace`** (aarch64 kernel feature, diagnostic only): once the console is up, every
  allocation prints its size and frame-pointer backtrace; `scripts/heaptrace-symbolize.py`
  attributes them by function with `llvm-nm`. It turned "the heap grows" into call sites.

## What it found

No panic, no hang, on any CPU. It found heap growth per interaction on the never-freeing heap
(~227 KB per 3,000 random events on real aarch64), and every cause is fixed:

* A resize drag reallocated per motion event: `Compositor::resize_surface` built a new bit buffer
  and `TextGrid::resize` a new cell buffer. Both now double-buffer (build into a kept spare, swap);
  surfaces are minted with capacity for a whole scanout, and the spare grows at most once.
* A closed window's surface and event queue were dropped and every reopen minted new ones. Detached
  surfaces and queues now go to a spare pool that `mint_surface` reuses.
* `render_menu` cloned the menu grid on every hover repaint; it renders in place.
* Every browser navigation (console `go`/`back`/`follow` and the window's) built a fresh `TextGrid`;
  the `Navigator` keeps one per surface and reuses it (`Navigator` is no longer `Copy`).
* History entries regrew when a recycled entry held a longer line; each is allocated at `MAX_LINE`.

Measured after the fixes, the last third of a 9,000-event storm grows the heap by 7.2 KB on aarch64,
0 on riscv64 and 3.2 KB on x86-64 (about 2 bytes per event of remaining first-use); host tests show
drag, cursor motion and close/reopen cost 0 bytes.

## Consequences

**Good.** A long interactive session no longer runs the desktop out of heap, and heap forensics now
exist for the next leak.

**Costs.** Each window pays a scanout-sized buffer once (~19 KB), a few hundred KB of the ~5 MB
margin; the gate takes about 75 s per CPU for 9,000 events.

**Not claimed.** The plateau is measured, not proved: a path the storm never reaches can still pay
its first use later. `heaptrace` exists on aarch64 only.
