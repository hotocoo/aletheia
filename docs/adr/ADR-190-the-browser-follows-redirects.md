# ADR-190 — The browser follows redirects, under its own policy

**Status:** Accepted (2026-09-26)
**Requirements:** REQ-WEB-010 (new)
**Builds on:** ADR-156 (navigation), ADR-158 (content), ADR-159 (Lethe's policy contract),
ADR-178 (trust by name).

## Context

Real sites answer a first request with a redirect (`301`, `302`, `303`, `307`, `308` and a
`Location`). Aletheia's browser rendered the redirect's own empty page. A browser that cannot follow
a redirect cannot reach most pages, and one that follows them carelessly can be walked off https,
onto a host the operator blocked, or around in a loop.

## Decision

* `browser::redirect_target(from, location)`: an absolute `https://` URL, or an absolute path on the
  SAME host and port. `http://` is refused as a downgrade; scheme-relative (`//host`), path-relative
  and unreadable references are refused by name, never guessed at.
* `shell::fetch_into` loops over `fetch_once`: a redirect's target goes through `Navigator::resolve`
  — the same check `go` applies — so a blocked host is refused (`the redirect names a blocked
  host`) and an untrusted one is refused (`... a host this machine does not trust`) before anything
  is dialed. At most `MAX_REDIRECTS` (5) hops (`too many redirects`); a redirect with no `Location`
  is refused by name.
* The page says it was redirected (`HTTP 200 OK (after N redirect(s))`), and the history entry the
  navigation made names where the chain ENDED, so `back` returns to a real page. The console's `go`
  and the browser window share this path.

## Proof

* Host (`kernel-core/tests/redirect.rs`): a two-hop chain across two trusted hosts arrives; loop,
  downgrade, blocked host, untrusted host and missing `Location` are each refused by their name;
  same-host paths keep host and port.
* Boot: `browser=9` on all three CPUs (the redirect policy is invariant 9).
* Live (`scripts/https-e2e.sh`, aarch64, riscv64, x86-64): the real HTTPS peer redirects `/moved` to
  `/plain.txt` (followed: `HTTP 200 OK (after 1 redirect(s))`) and `/away` to a blocked host
  (refused by name, nothing dialed).

## Non-claims

No cookies, no method rewriting (a `GET` stays a `GET`), no redirect to an address that is not
already a trusted host, no `Refresh` header or `<meta refresh>`.
