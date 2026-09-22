# ADR-159 — Lethe's policy contract, adopted natively as boot invariants

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-WEB-005, Lethe integration stage N6
- **Supersedes:** nothing. Completes the Track 2 ladder of `docs/LETHE-INTEGRATION.md`.

## Context

Lethe is pinned in this tree (Track 1) as a browser whose README specifies and proves a handful
of BEHAVIOURS rather than an engine: HTTPS-first with plaintext refused rather than silently
downgraded, tracker hosts refused as third-party requests, a fixed low-entropy user agent, an
ephemeral-by-default site-data store. Stage N6 asked for those behaviours to be Aletheia's own,
re-expressed as invariants in the style of the rest of this tree, not as a port of the C++.

Stages N1..N5 built the browser those invariants can be stated against: TCP, TLS 1.3, HTTP/1.1,
the navigation model and window, the content renderer.

## Decision

`kernel-core/src/policy.rs` proves the contract at boot, on every CPU, against the browser as it
is. Two small additions to the model make the contract complete:

- **A block list** (`Navigator::blocked`, eight names, operator-filled by `block HOST`) checked
  BEFORE the trust table, so a pin never overrides a block, and checked on `back` as well as on a
  typed URL. The ninth entry is refused, not evicted.
- **`forget`** empties history, the page and its links. The trust and block lists stay: they are
  what the person typed, not what a site left.
- **`follow` names a third-party link** when the target host is not the page's, and dials it only
  if that host is pinned and not blocked - through `navigate`, like every URL.
- **`http::USER_AGENT`** is the one constant the client ever sends.

The eight invariants (`policy=8`):

1. Plaintext is refused as plaintext, typed or followed, never rewritten to https, and leaves no
   history.
2. A blocked host is refused before lookup, pinned or not, forward or back; the list is bounded.
3. The renderer makes no requests: `img`, `script`, `iframe` and stylesheet sources become
   neither links nor text. Only anchors a person can choose survive, numbered.
4. A link to another host is third-party by name and dials only a host the operator pinned and
   did not block, to THAT host's pin, never the page's.
5. The user agent is one fixed string: requests to different hosts differ only in host and path,
   carry exactly the same headers, and name no platform, language or build.
6. No site data is kept: a `Set-Cookie` in the answer is a header and nothing more; the next
   request is byte-identical to the first.
7. `forget` empties history, page and links and keeps trust and blocks.
8. A fresh navigator holds nothing: no hosts, no blocks, no history, no page; every host is
   unknown until a person pins it.

## Proof

`policy=8` on all three CPUs (boot fails 1050+i); `console=51` (`block` refuses `go` by name
before lookup, `forget` leaves `back` nowhere to go); conformance contract 374 -> 383. **Live
(`scripts/https-e2e.sh`):** the real server's host, pinned and just fetched from, is blocked by
name and the next `go` is refused with nothing dialed; `forget` then `back` has nowhere to go.

## Alternatives considered

**A shipped tracker list.** Rejected: this kernel ships no root store and no DNS for the same
reason - a list nobody here typed is trust nobody here gave. The mechanism is the contract; the
names are the operator's.

**Porting Lethe's code.** Rejected in `docs/LETHE-INTEGRATION.md` from the start: a C++
application has nowhere to run here, and the contract is what was worth having.

## Consequences

Track 2 of the Lethe integration is complete for the ladder as written: every stage N1..N6 is
delivered and proved. What the browser still lacks is named where it lives: no DOM, CSS, images,
forms or subresources (ADR-158); one connection at a time; no DNS; the window's URL line is
typed-only. `docs/LETHE-INTEGRATION.md` says what would change this page.
