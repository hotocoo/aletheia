# ADR-155 — The HTTP/1.1 client, bounded by construction

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-WEB-001, Lethe integration stage N3
- **Supersedes:** nothing. Sits on ADR-151 (the TLS client) exactly as Lethe's table says N3 does.

## Context

Lethe's stage N3 is "an HTTP/1.1 client over N1+N2, bounded by construction (no unbounded response
buffering on a heap that never frees — ADR-063)". The TLS client returns the bytes a peer sent
before it closed; HTTP is the grammar of those bytes, and the grammar is where clients get hurt:
lengths the peer names, boundaries two parsers can disagree about, bodies that do not end.

## Decision

`kernel-core/src/http.rs`, and nothing in it allocates.

- **The request is a `GET` with `Connection: close`.** One conversation, one answer, then the peer
  closes and the pump returns. The path is checked before it is sent: absolute, ASCII, visible, no
  whitespace, bounded — a path that would never be sent is refused at the console by usage, before
  a connection is opened for it.
- **Every length the peer names is checked against the bytes that arrived** before anything is
  read past them: the status line's shape and version, each header's colon and bound, the header
  count, each chunk's hexadecimal size and its CRLF, a `Content-Length` the body must honour
  (shorter is `Incomplete`, not a shorter body).
- **Two body boundaries are an ambiguity this client refuses.** `Content-Length` together with
  `Transfer-Encoding: chunked`, or two different `Content-Length`s: RFC 7230 §3.3.3 says which wins,
  and two parsers disagreeing about where a body ends is the whole of a request-smuggling bug. A
  client that resolves the ambiguity is a client that can be made to see a different response than
  the server sent. Chunk extensions are refused for the same reason: nothing here reads them, and a
  parser that skips what it does not read is one two peers can disagree about.
- **A body longer than the caller's buffer is truncated and said to be**, never overflowed and
  never grown into: on a heap that never frees there is no "grow the buffer", and a client that
  buffered whatever the peer sent would be a client the peer could fill. The transport's own cut
  is told to the reader: when the TLS pump filled its buffer and dropped the rest, a body shorter
  than its `Content-Length` or a chunk that runs out is a truncated body, not an incomplete
  response — the head must still be whole.
- **The console's `https ADDR PORT NAME PIN PATH`** builds the request, runs ADR-151's conversation
  under the operator's pin, reads the answer, and prints `HTTP status reason; N header(s); N byte(s)
  of body`, with `(chunked)` and `(truncated)` said when true, then the body if it is text.

## Proof

`http=8` on all three CPUs, pure: the request's exact bytes; refused paths; a `Content-Length`
response to status, headers and exact body; a chunked body reassembled with its trailer skipped; a
body larger than the buffer truncated and said so with the guard bytes untouched; a bad status
line, version, header, fold, unfinished head, too many or too long headers each refused by name; a
chunk that is not hex, carries an extension, runs past the bytes or does not end in CRLF refused;
two body boundaries refused as ambiguous and a short `Content-Length` body incomplete. Host: every
truncation of a response is incomplete or a refusal, never a wrong answer.

**Live (`scripts/https-e2e.sh`, a CI gate):** a scripted operator types `https` at the aarch64 and
RISC-V consoles. The peer is Python's `http.server` behind `ssl` (OpenSSL) serving the fixture
leaf on the runner. `/plain.txt` answers with `Content-Length`; `/chunked.txt` answers in three
chunks; `/big.txt` answers with 4000 bytes and the console reports `2048 byte(s) of body
(truncated)`; `/missing.txt` reports `HTTP 404`; a wrong pin is refused by name before the request
leaves the guest; a dead port is refused by name. The peer's log shows each GET with its `Host`.

`console=48`. Conformance contract 347 -> 356 core behaviours.

## What the live gate found

The first run of `https-e2e.sh` answered every request with `the peer did not finish inside this
machine's budget`, and the peer's log showed a handshake that ended in EOF for every connection —
including the right-pin ones. The gate's first server did its TLS handshake on the accept thread;
the guest's wrong-pin attempt received the certificate, refused it, and RETURNED — without an alert
and without a FIN. The server sat in that handshake until the guest halted, and every later
connection was accepted by the kernel's backlog and answered by nobody. Two faults, both fixed:

- **The client now winds down.** A refused conversation sends a fatal alert naming the refusal
  (`bad_certificate` for a pin that does not match, `decrypt_error` for a signature or Finished
  that does not verify, `bad_record_mac` for a failed tag, `unexpected_message` for data out of
  turn — RFC 8446 §6), protected when there are keys and plain when there are not, then closes and
  pumps a bounded number of turns so the FIN goes out. The boot suite proves it: the stand-in that
  cannot prove its key receives alert 51 and sees the client close.
- **The gate's server handshakes on the connection's thread**, so a client that walks away can
  never starve the next one — and its log now says, per connection, what it saw.

The `tls` gate never showed this because its server handshook on a per-connection thread from the
start. A server written differently is what found the client's omission.

## Alternatives considered

**Resolving `Content-Length` vs `chunked` per RFC 7230.** Rejected, as above: correct for a
well-behaved server, and exactly the disagreement an attacker arranges.

**Growing the body buffer.** Rejected by ADR-063.

**Keep-alive.** Not this rung: one request, one answer, one close is what a page needs today, and
a connection that stays open is a resource a peer controls.

## Consequences

Lethe's stage N3 is delivered. Stage N4 — a browser window in the desktop, owning a URL and its
navigation — can begin on top of `https`.
