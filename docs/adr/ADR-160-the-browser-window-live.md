# ADR-160 — The browser window, LIVE: a URL typed with a real keyboard, a page shown in the window

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-006
- **Supersedes:** nothing. Closes the proof gap ADR-157 named.

## Context

ADR-157 cut the browser window into the desktop and said, honestly, that typing a URL into it
"travels the path `scripts/https-e2e.sh` proves live through `go`". That is a claim about a path,
not a proof of it: the live gates typed at the console, and the window's own path - compositor
queue, URL line, Enter latch, the console session's idle turn, the page pushed back - had only
unit tests and a `5 managed windows` marker behind it.

A GUI with a browser "built in" must be proved through the GUI.

## Decision

Two things, both small:

- **The window answers for itself.** The console's `input` readout gains a `browser:` line read
  from the window's own state: the URL line as typed, `fetching` while a latched navigation is in
  flight, and otherwise how many bytes of page the window holds and its first line. A live gate
  can now ask the machine what the window shows instead of inferring it from the serial line.
- **A live gate that types into the window.** `scripts/browser-e2e.sh` boots the interactive
  kernel on both device-tree targets with the desktop's devices AND the network's on one machine,
  pins the peer with `trust` on the serial line, then drives the real virtio keyboard through QMP:
  `Alt+5` focuses the browser window (`focus: surface 9`), the URL is typed one key event at a
  time (shift for the colon), the readout shows it in the URL line, Enter latches it, and the
  readout shows the page - its first line the URL the window fetched - while the peer's log shows
  the GET with the fixed user agent. Then Backspace empties the line, a plaintext URL is typed and
  the window shows `refused: plaintext`, and the peer saw exactly one request.

## Proof

Host: the readout renders all three shapes (page, fetching, no page) from the facts. LIVE:
`scripts/browser-e2e.sh` on aarch64 and riscv64, in CI as its own job. No new boot invariants:
this rung is a proof of the assembly, and the assembly only exists on a live machine.

## Alternatives considered

**Read the compositor's framebuffer.** Rejected for now: a glyph-level check needs a font
oracle and would prove painting, which the compositor gates already prove; the question here is
whether the PAGE reached the WINDOW's state.

**Drive the window from the x86-64 desktop gate too.** Not this wave: `vinput-e2e.sh` boots
without a network, and adding one there is a separate decision about that gate's scope. The
window code is shared; the DT targets prove it.

## Consequences

The browser is proved end to end through the GUI. Still open, named: the window's URL line has
no `back`, no cursor movement and no mouse; the x86-64 live gate types at the console only.
