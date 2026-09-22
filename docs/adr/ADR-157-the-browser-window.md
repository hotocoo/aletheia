# ADR-157 — The browser window: a fifth managed window with a URL line and the page the model rendered

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-003, Lethe integration stage N4 (second rung: the window)
- **Supersedes:** nothing. Completes ADR-156 (the model) into Lethe's stage N4.

## Context

ADR-156 delivered navigation from the console. Lethe's stage N4 asks for a managed window like
the terminal and the monitor, driven by the window manager and the input session the desktop
already has (ADR-084/085).

## Decision

The desktop (`kernel-core/src/desktop.rs`) gains a fifth managed window, `browser`, on the same
contract as the four before it: a `TextGrid` surface, a window-manager slot, a taskbar button
(`web`), an `Alt+5` launcher, a line in the shortcuts window, and a place in the start menu's
Alt+Tab order.

- **The desktop owns no network and no trust.** The window shows a URL line the person types into
  (`url> …`) and the page text the platform pushed. Keystrokes reach it the way they reach the
  terminal — through the compositor's per-window input queue, drained each tick — printable bytes
  extend the line, Backspace shortens it, Enter LATCHES it (`take_navigation`). Whoever holds the
  network and the trust table collects the latch; the desktop never dials.
- **The console session navigates for the window.** `run_loop_serviced` takes `BrowserHooks`
  (take the latched URL, show a page) and, on each idle turn, runs the session's own `Navigator`
  through the same `fetch_into` the console's `go` uses, renders the page into the window's grid
  shape, and pushes it back (`set_browser_page`). The window and the console share one navigator,
  one host table and one history: `trust` at the console is what the window is allowed to dial.
- **Five buttons fit the panel by arithmetic, not by hope.** Nine cells per button (was twelve):
  launcher, five buttons and four workspace buttons sum to 616 of 640 pixels; the persona layout
  suite and the chrome-parity test hold for every persona.

## Proof

The live-desktop gate on both device-tree targets now comes up with **5 managed windows**
(`scripts/vm-e2e.sh`, `scripts/vm-e2e-riscv.sh` require the marker); the desktop unit tests hold
for the launcher map, the chrome parity and every persona's panel; the console suite's storm and
transcript tests hold with the hooks present; the boot gates, conformance and the live HTTPS gate
are unchanged and green. Typing into the window and pressing Enter travels the exact path
`scripts/https-e2e.sh` proves live through the console (`go`).

## Alternatives considered

**A navigator inside the desktop.** Rejected: the desktop would then hold a trust table and a
network path, two things it has been kept from on purpose since ADR-085.

**A URL bar as part of the terminal.** Rejected: Lethe asks for a window, and a browser that
lives inside a terminal's line editor is a terminal.

## Consequences

Lethe's stage N4 is delivered. Stage N5 — a bounded, fail-closed content renderer into the
window's grid — is next; today the window shows the page's text as the model renders it.
