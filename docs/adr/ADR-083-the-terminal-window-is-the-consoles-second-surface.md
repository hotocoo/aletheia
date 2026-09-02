# ADR-083: the terminal window is the console's second surface — text in the window, the window under the pointer

**Status:** Accepted · **Date:** 2026-09-02 · **Advances:** ALET-P2-021 (the text rung: text rendering inside a composed surface, the window as a terminal, drag by title band; live pump still x86-64's, alpha and IME stay scoped) · **Builds on:** ADR-080 (real devices through the input session, the live desktop), ADR-079 (the input session, focus as routing), ADR-077 (the composition contract: owner tokens, `fill_packed` size-honest), ADR-049/050 (the console's alphabet and line editor), ADR-044/045 (the interactive console, interrupt-driven), ADR-063 (the boot heap never frees)

## Context

ADR-080 left the live desktop with a window nobody could read: keystrokes reached its queue,
the queue was drained by nobody, and the window showed a border. The console — the one
session a human can drive — lived on the serial line alone; the register said so ("no text
rendering inside composed surfaces"). A desktop whose window shows nothing and whose
keystrokes go nowhere is a picture of a GUI. This rung makes the window a TERMINAL: the
console's own bytes painted inside it, the virtio keyboard's keystrokes reaching the same shell
the serial line drives, and the window movable under the pointer — without inventing a second
shell, a second session, or a second alphabet.

## Decision

### One console, two surfaces

The interactive console (`shell::run_loop`, main thread) keeps ONE session. It gains a second
surface: every byte it emits also lands in the terminal window's grid (`desktop::term_write`,
called from the console's `emit`), and its `getc` asks the serial/PS-2 ring first and then the
terminal window (`desktop::term_getc`) — the keystrokes the input session routed into the
focused window's queue, drained by their OWNER (the window's token) into a bounded line the
console pops one byte per turn. A virtio keyboard therefore types at the same shell a serial
line does, and the shell's answer is painted where the keystroke landed. Nothing about the
shell changed: same editor, same commands, same authorization, same audit.

### The grid is the console's alphabet, exactly

`kernel-core/src/textgrid.rs` is a `cols x rows` grid of the console's output bytes: printable
ASCII lands in a cell, `\n` ends the line (scrolling exactly one row on the last row), `\r`
returns to the column, a wrap at the right edge is a newline, backspace erases exactly one
cell, and the editor's `ESC [ ... <final>` sequences are consumed unpainted (the grid is a
teletype for the console's stream, not a terminal emulator with cursor addressing — that is a
named non-claim). Anything else is refused and COUNTED; nothing unknown is drawn.
`render_packed` is a pure function of the cells into the row-major 1-bpp buffer
`Compositor::fill_packed` consumes (bit `i` is pixel `i`, LSB first; one 8x8 glyph of
`font8x8` per cell), into a caller-owned buffer allocated once and reused — the desktop paints
a changed grid with one `fill_packed` and no allocation, on a heap that never frees. Above the
text sits a `TITLE_H` band: solid ink with the window's name knocked out — the strip the
pointer drags by. The grid is deterministic and arch-neutral; its 6-invariant boot suite runs on
every CPU, and only the CPU with a desktop paints it.

### The window under the pointer

`vinput::route_pointer_batch` hands the committed batch back after doing everything
`route_pointer` did (cursor move, click as a focus decision), so the desktop decodes each
record once. A LEFT press whose point falls in the window's title band starts a DRAG (the
pointer's offset from the window's origin) and RAISES the window; motion while dragging moves
the window by exactly the pointer's delta through `Compositor::move_surface` (a fully-off
placement is refused by the compositor and the window simply stays); a release ends the drag
and counts it. `Compositor::placement` reports where a placed surface sits, so `input` can say
`window: at (x, y)`.

### Two contexts, one interrupt flag, no lock

The pump (IRQ0, IF=0 by construction) and the main thread (only through `with_desktop`, inside
`without_interrupts`) are the two writers, and they never overlap — serialized by the CPU's
interrupt flag, not by a lock, because the main thread also holds console locks (`RESIDENT`,
the input ring) that an IRQ path must never spin on. The pump drains the window's queue into
the terminal line so `getc` finds keystrokes on its next turn; the main thread repaints the
grid it just wrote and shows the frame. The idle tick is unchanged: two used-ring reads and one
allocation-free damage check.

### Observability

`InputFacts` gains the window's placement, the terminal's completed-line count and its
current line; `input` prints `window: at (x, y)` and `terminal: N lines, last "..."` from the
same grid the compositor paints — not a second copy.

### Host proofs

`kernel-core/tests/textgrid.rs` (7 tests): the boot suite host-run; exact scrolling over many
rows; editor sequences never paint and a bare escape swallows one byte; backspace at column
zero changes nothing; the rendered title knocks the name out of solid ink (and a byte the font
cannot serve is reported); pixel size and packed length agree with `fill_packed` against a real
compositor; rendering is deterministic.

### Boot gate

`textgrid_suite` (6 invariants, all three targets, `textgrid=6`): printable bytes land in
cells, `\r` returns, backspace erases one; a newline on the last row scrolls exactly one row; a
wrap at the right edge is a newline; control sequences are consumed unpainted and unknown bytes
refused, counted; rendering is pixel-exact (solid title band, glyph bits at the cell, blank
elsewhere); the render buffer is reused and dirty is a one-shot. Boot fails 700+i. VirtualBox
REQUIRES the marker (arch-neutral).

### Conformance

Three behaviors join the cross-CPU contract (175 → 178): the cell/return/backspace rule, the
exact scroll, the pixel-exact render.

### The live gate

`scripts/vinput-e2e.sh` grows from 17 to 22 checks: a virtio keystroke is now ECHOED by the
console (the byte appears on the serial log) and its repairing backspace is routed too — two
bytes posted, `queued` back to zero because the window's owner drained them; `help` typed on
the virtio keyboard is answered by the same shell (`commands:` on the serial log); the `input`
readout shows the window at (300, 60) and the terminal's last non-blank line being the prompt
line with the command just typed (`aletheia> input`), with the line count growing across readouts;
a press on the title band at (400, 64), a move to (420, 90) and a release move the window by
EXACTLY the mapped pointer delta — the expected position is computed by the harness from the
same axis formula the kernel uses, never a hardcoded pixel — with focus kept. Clicks still
post no keystroke; the FocusLost the window was told is read by its owner.

### The unsafe audit

kernel-x86_64 +1 (270 → 271): `with_desktop`, the main thread's single door to the desktop,
takes the static's reference under `without_interrupts`; every other site is ADR-080's.

### Named non-claims, in the register

The grid is a teletype, not a terminal emulator: no cursor addressing, no colors, no
attributes — the editor's sequences are consumed, not interpreted. The window is one; there is
no window manager beyond raise-on-drag, no resize, no close, no second application. The live
pump and the terminal are x86-64's (aarch64/RISC-V prove the grid in the boot suite, install no
desktop). The i8042 keyboard still reaches the console directly (both wires feed one shell; the
PS/2 wire does not pass through the session). Input is polled from the tick. Alpha, IME,
device-level GPU isolation stay open in the row.
