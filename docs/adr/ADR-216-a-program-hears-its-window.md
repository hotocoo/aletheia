# ADR-216 — A program hears what is typed at its window

**Status:** Accepted (2026-10-07)
**Requirements:** REQ-USER-012 (new)
**Builds on:** ADR-215 (a program draws into its own window), ADR-079 (focus and the input queue).

## Context

After ADR-215 a program could show frames but not react: everything typed on the keyboard went
to the terminal or a kernel window. An interactive program - an editor, a tool, a game - needs the
operator's keys.

## Decision

* **The window takes the keyboard when it opens**, as on every desktop. Keys the operator types
  while it holds focus queue on its surface through the compositor's existing per-surface queue
  (ADR-079), in the console's decoded alphabet, with a `FocusLost` event when focus moves away.
* **`SYS_POLL_INPUT` (14)** takes one event: 0 when none is waiting, `appwin::KEY | byte` for a
  key, `appwin::FOCUS_LOST`, or `u64::MAX` when the program holds no open window. It is authorized
  by the same `window.present` grant as drawing: the input belongs to the window. The handler
  admits it and the run loop answers from `AppWindow::poll`, which refuses any program but the
  window's owner, and refuses the owner once the operator has closed its window.
* **`draw` listens:** `a`/`d` push its bar back and forward, `q` ends it with status 113.

## Proof

* Host: keys posted while the window holds focus reach its owner and nobody else; an empty queue
  is 0; an operator close refuses the owner (`kernel-core/src/appwin.rs`).
* Boot, every target (`usermode` 59/59/67): `draw poll` with no window is refused (exits 7).
* Live, aarch64 and riscv64 (`scripts/desktop-e2e-dt.sh`): with `draw` running, `q` typed on the
  virtio keyboard reaches it; it exits with status 113 and its window leaves the dumped scanout.

## Non-claims

* No pointer events for a program yet, and keys arrive as the console's decoded bytes (no key-up,
  no raw scancodes) - enough for a menu or a simple game, not for one that needs held keys.
* Polling only; a program waiting for a key spends its turns asking.
