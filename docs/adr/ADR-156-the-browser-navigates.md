# ADR-156 — The browser navigates: URLs, trusted hosts, history, and the page — from the console first

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-002, Lethe integration stage N4 (first rung: the model and console
  navigation; the managed window is the second rung)
- **Supersedes:** nothing. Sits on ADR-155 (the request) and ADR-151 (the conversation).

## Context

Lethe's stage N4 is "a browser window in the desktop: a managed window like the terminal and
monitor, owning a URL/navigation state model". The model is the part every later rung stands on,
and the part that decides what this browser will and will not speak to; it is proved first, alone,
and reached from the console the desktop already has, before a fifth window is cut into a
2,900-line desktop.

## Decision

`kernel-core/src/browser.rs` touches no wire. It parses URLs, keeps the hosts a person has chosen
to trust, keeps history, and holds the page a window shows; the PLATFORM dials (ADR-151) and
requests (ADR-155) and hands the answer back.

- **`https://` only.** `http://` is refused as `Plaintext` — its own refusal, so a person sees
  "plaintext refused", not "not a URL". Nothing is downgraded to or upgraded from; Lethe's
  HTTPS-first rule (stage N6) belongs in the model and is adopted here.
- **A host is dialed only if a person pinned it.** `trust NAME IP PIN` puts a name, its address
  and the Ed25519 root that vouches for it in a table of eight. There is no DNS in this kernel and
  there is no root store; the table is both, filled by the operator, nothing pre-installed. An
  unpinned host is refused before any address is resolved. The ninth host is refused, not evicted;
  trusting a name again replaces its pin — the person changed their mind.
- **History is a bounded ring of eight.** `back` and `forward` walk it; a new navigation drops
  the forward pages, as every browser does.
- **The page is bounded.** Two kilobytes of body, the status, the reason, and — when nothing came
  back — the named refusal with the URL that was asked for. `render` draws it into a `TextGrid`:
  URL, status, body wrapped to the width and cut at the last row, never past it.
- **From the console:** `go URL` navigates and prints the page the window would show; `back`
  re-fetches the previous page. Both are outward-facing writes and are approved as such.

## Proof

`browser=8` on all three CPUs, no network: URL defaults; plaintext refused as plaintext; a bad
host, port or path refused for the part that is bad; an unknown host refused before anything is
dialed and a trusted one resolved to its pin and address; the table bounded and a re-trusted name
replaced; history walked, forward pages dropped, the ring bounded; a page rendered and cut at the
grid; a failed page naming its reason and URL, an over-long body cut and said to be.

`console=49`: `go` refuses plaintext and an unpinned host by name before anything is dialed,
`trust` refuses a pin that is not a key, and a trusted host navigates.

**Live (`scripts/https-e2e.sh`):** at the aarch64 and RISC-V consoles, `go` to a host nobody
pinned is refused before a dial; `trust` pins the fixture host at the runner's address; `go` to
`http://` is refused as plaintext; `go` to `/plain.txt` and `/chunked.txt` render their URL, status
and body lines from the real server; `back` renders the first page again. Conformance contract
356 -> 365 core behaviours.

## Alternatives considered

**DNS.** Not this rung, and not a dependency: a pinned host has an address the person gave; a
resolver would be a second party to trust before the first.

**A root store with well-known CAs.** Rejected: Lethe pins; a store trusts strangers.

**Cutting the window first.** Rejected: a window over an unproved model proves the window.

## Consequences

Stage N4 is started: the model and console navigation are delivered. The managed window — URL
bar, keyboard focus, Alt+5 — is the next rung, on top of `render` and the desktop's existing
window manager.
