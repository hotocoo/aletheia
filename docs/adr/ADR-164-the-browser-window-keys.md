# ADR-164 — The browser window's own keys: follow a link, back, forward

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-008
- **Supersedes:** nothing. Closes part of what ADR-160 left open ("the window's URL line has no
  `back`").

## Context

ADR-160 proved the browser window live through the GUI, and named what it still could not do:
the window only navigated to what was typed on its URL line. A rendered page numbers its links
(`[1]`, `[2]`, ... - ADR-158) and the console session walks history with `follow N`, `back` and
`forward` (ADR-156, ADR-158), but a person at the window had to leave it for the console to use
any of them. A window that shows links it cannot follow is half a browser.

## Decision

- **Three key shapes, only while the browser window holds focus, only on a press with Ctrl held
  and Alt not:** `Ctrl+1..9` follow link `[n]` of the page the window shows, `Ctrl+B` goes back,
  `Ctrl+F` goes forward. The policy is one pure function, `browser_shortcut`, so it is tested
  without a device. A bare digit stays URL text; `Ctrl+Alt+digit` stays a workspace; `Ctrl+W`
  stays the close gesture. A browser key is a desktop command and never reaches the URL line.
- **The desktop still never dials.** What it latches widens from "a typed URL" to one
  `BrowserRequest` (`Go`, `Follow(n)`, `Back`, `Forward`), one at a time, with the same
  `(fetching...)` note Enter shows. The console session collects it on its idle turn exactly as
  before (ADR-160).
- **Every request resolves through the navigator the console verbs use.** `Go` through
  `navigate`, `Follow` through the page's own hrefs (`link_target`) then `navigate`, `Back` and
  `Forward` through the history ring then `resolve`. So the window refuses exactly what the
  console refuses - an unpinned, blocked or plaintext target is refused by name - and a request
  with nothing to act on is refused by name too: `refused: the page offers no such link`,
  `refused: no previous page`, `refused: no next page`.
- The shortcuts overlay gains one line naming the keys (`HELP_ROWS` 19 -> 20).

## Proof

Host: `browser_keys_follow_links_and_walk_history_only_with_ctrl_on_a_press` in
`kernel-core/src/desktop.rs` (the four requests, and release / bare digit / Ctrl+Alt / Ctrl+W
refused as requests); the help-overlay row count test. LIVE: `scripts/browser-e2e.sh` step 6 on
aarch64 and riscv64 - an HTML index with one link is typed and fetched, `Ctrl+1` follows the link
(the window shows the plain page and the peer saw a second GET for it), `Ctrl+B` returns to the
index, `Ctrl+F` forward again, `Ctrl+9` on a page without links is refused by name. The readout's
`first` field is cut at 30 bytes, so the gate matches the refusal's first 30 bytes. No new boot
invariants: the assembly exists only on a live machine; conformance unchanged (383).

## Alternatives considered

**Plain digits or letters as link keys.** Rejected: the URL line takes text, and a digit that
sometimes typed and sometimes navigated would make the window's behaviour depend on whether the
line was empty.

**Alt+Left / Alt+Right for history, as other browsers do.** Rejected: Alt is the desktop's own
launcher modifier (`Alt+1..5`), and the desktop has no arrow-key vocabulary yet.

## Consequences

The window can read a site, not just a page. Still open, named: only links `[1]`..`[9]` are
reachable by key; the URL line does not show the address a followed link or history step went
to (it stays what was typed); no cursor movement, no mouse on links; the x86-64 live gate still
types at the console only.
