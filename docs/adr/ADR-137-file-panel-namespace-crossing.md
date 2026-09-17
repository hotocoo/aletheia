# ADR-137 — The file panel is a view, and the console is what feeds it

- **Status:** accepted
- **Date:** 2026-09-17
- **Requirement:** ALET-P2-021 (the GUI rung), REQ-GUI-FILES-001
- **Supersedes:** nothing. Extends ADR-083 (the terminal window), ADR-084 (managed windows),
  ADR-085 (one desktop, three CPUs) and ADR-136 (the shell persona).

## Context

The desktop had a terminal, a monitor, a shortcut sheet and a window manager, and no way to see
what was on the disk. A desktop that cannot show you your own files is a demonstration, not a
system someone can use.

The obvious way to add one is wrong for this kernel. It would make `Desktop<H, T>` generic over
`BlockDevice` as well, thread a fourth type parameter through three kernels, and let the pump read
the disk. The pump runs on the display's cadence (ADR-132), so every directory read would land on
the frame path: a slow or retrying device would show up as a frozen cursor, and the allocation-free
repaint hot path (ADR-125) would acquire a device queue inside it.

There is also a plainer fact about where the filesystem already is. On a live machine exactly one
component holds both a mounted `Filesystem` and the device to read it with, and it is the console:
`shellio::session_on` mounts the namespace and lends it to `shell::run_loop` for the rest of the
machine's life. Nothing else can list a directory without being handed those two things.

## Decision

Split the file manager along the line the rest of the desktop is already split along: the panel is
a **model with no device in it**, and the platform feeds it from wherever it already holds the
filesystem.

`kernel-core/src/filepanel.rs` holds `FilePanel`: a bounded list of `FileRow`, a selection, and a
scroll window. It allocates once, at construction, and reuses that row storage for every listing —
on a heap that never frees (ADR-063) a per-refresh allocation is a leak, and a file panel refreshes
every time anything on the disk changes. It is total: defined on an empty listing, on a listing
longer than `ROW_CAP`, and on a name longer than `NAME_CAP`, with every truncation counted rather
than silent. Every gesture is **reported**, not performed: the panel decides which row, the desktop
decides what selecting a row means, and neither may act as the other.

`Desktop` owns the window (`FILES`, `Alt+3`, the fourth taskbar button and a `files` menu item),
turns presses into row indices, and latches an activation as a **name** rather than an index — the
listing can change between the click and the read, and reopening whatever now sits at row four is
how a user loses the wrong file.

The crossing itself is `filepanel::service_panel`, called from the console's loop through the new
`shell::run_loop_serviced` hook. Two phases, because they cost different amounts:

- `ServicePhase::Settled` — a command line just ended, so the namespace may have changed. One
  directory read here is something the operator already paid for by pressing return.
- `ServicePhase::Idle` — nothing was typed. This runs on every idle turn of the console loop, so it
  does no device work at all unless a latched activation says the operator clicked something. The
  check is a take of an `Option<[u8; NAME_CAP]>` and allocates nothing.

Opening a row prints the object the way `cat` would, into the console's surfaces, bounded to
`PREVIEW_BYTES` and with every unprintable byte shown as a dot. A file panel must be safe to point
at a binary: bytes that would move the cursor, change the colour or reprogram the terminal are
shown, never executed. When the hook prints, the loop re-issues the prompt, so output arriving from
outside the typist's line never leaves a session without one.

Three consequences are deliberate:

1. **The desktop never learns that a filesystem exists.** It takes rows and hands back a name.
   That is what keeps disk latency off the compositor's tick without a fourth type parameter.
2. **A device that cannot be listed leaves the previous listing standing.** "I cannot read the
   disk right now" and "you have no files" are different statements, and the panel must not turn
   the first into the second.
3. **The settle is driven by the byte, not by the editor.** `ends_a_command` is true for CR and LF
   and nothing else. A cancelled, empty or refused line still settles: one directory read too many
   costs a read, while a missed settle shows a stale listing until the next command.

## The invariants (`filepanel=13`, `console=44`, proved on all three CPUs at boot)

The panel's ten model invariants (empty, clamped selection, the visible window containing the
selection, counted truncation of both listings and names, storage reuse across refreshes, selection
preserved by name, a press below the last row refused by name, byte-identical re-render, and the
rendered rows being exactly the visible window) plus three for the crossing:

- an idle turn with nothing activated neither reads the disk nor prints;
- a settle publishes every live name with the device's own free-space accounting;
- opening a row prints its bytes safely, and a name that left the namespace between the click and
  the read is refused by name.

The console suite gains two: a command line ends on return and on no other byte, and the serviced
loop settles once before the first prompt and once per completed line — never mid-line, and never
for the line that halts the machine.

## Alternatives considered

**Make `Desktop` generic over `BlockDevice`.** Rejected: it puts I/O on the frame path and a fourth
type parameter into three kernels, to buy a listing that changes a few times a minute.

**A second filesystem mount for the desktop.** Rejected: two mounts of one device is two journals'
worth of belief about the same disk, and the panel would show a namespace the console does not
have.

**Refresh the panel on the pump's tick.** Rejected: a directory read at the display's cadence is
the frozen-cursor failure this ADR exists to avoid, and ADR-084's quiet-desktop property (nothing
written when nothing changed) would be lost.

**Open a file into a new viewer window.** Deferred, not rejected. The console is already the
surface that prints bytes, it already has scrollback, and reusing it keeps this wave to a view plus
a crossing.

## Consequences

The desktop now has a fourth managed window and the taskbar carries four application buttons, which
is what narrowed the button to twelve cells and the workspace button to six: 64 + 4×96 + 4×48 comes
to exactly 640 px, so ADR-136's reachability invariant still holds at the edge rather than by luck.

The panel is fed only while the console session is running. A machine that never reaches the
interactive console shows an empty panel that says so, rather than an inaccurate one.
