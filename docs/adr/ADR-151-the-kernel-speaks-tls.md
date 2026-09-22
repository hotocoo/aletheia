# ADR-151 — The kernel speaks TLS: handshake, records and TCP joined, proved against OpenSSL

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-SEC-TLS-010, Lethe integration stage N2 (the last rung)
- **Supersedes:** nothing. Joins ADR-139/140 (TCP over the device), ADR-143 (records), ADR-144
  through ADR-149 (the handshake and everything it verifies) and ADR-148 (the clock).

## Context

Every part of a TLS 1.3 client existed and was proved alone: a TCP pump over a real device, a
record layer that interoperates with OpenSSL byte for byte, a handshake that completes only under a
signature over its own transcript, a verifier built from the platform's clock. None of them had
been joined, and "this kernel cannot speak TLS" stayed true through eight ADRs on purpose: a
client that is three proved pieces and an unproved join is a client that protects nothing.

## Decision

`kernel-core/src/tlsclient.rs` is the join: one bounded pump that opens a connection, sends the
ClientHello as a plaintext record, routes every record the peer sends to the layer it belongs to,
splits coalesced handshake messages and reassembles split ones, completes the handshake, sends this
client's Finished under the handshake keys, switches both directions to the application keys, and
only then sends the request. Nothing in it decides anything cryptographic. It refuses by name: an
encrypted record before there are keys, application data before the peer's Finished, a fatal alert
(by its description), a post-handshake message it does not process, a record larger than one
record can be. The compatibility `ChangeCipherSpec` is dropped unread, as RFC 8446 §5 says.

**Allocation, once.** `TlsPump` holds the pump's workspace and, from the first handshake on, the
record layer; both are built once and re-keyed per conversation. `Handshake::rebind` lets one
handshake serve every conversation a console opens. The suite's first version built these per
conversation and the desktop that boots after it found the heap gone
(`memory allocation of 2600 bytes failed`); on a heap that never frees (ADR-063) this is the shape
every long-lived path in this tree has to take.

**The operator states whom to trust.** The console command is
`tls ADDR PORT NAME PIN TEXT`: the DNS name the peer must speak for and the Ed25519 root's public
key, as 64 hex digits, typed by the person. This console ships no root of its own. A pin that is
not exactly a 32-byte key is refused by usage before a byte reaches a wire. The time is the
platform's own clock (ADR-148) through `verifier_at`; a machine whose clock refuses opens no
conversation.

**Entropy, named as a gap.** The ephemeral X25519 key and the client random are derived, through
SHA-256, from the platform's timer readings. This kernel has no entropy device. That seed is
unpredictable to a casual observer and not to a determined one: adequate for a gate on a private
network and for nothing else. A real entropy source (virtio-rng, `RNDR`, `RDRAND`) is the next rung,
and until it lands this console's `tls` is a demonstration, not a product.

## The proof

**Boot, on all three CPUs (`tlsclient=8`)** over a link that is a test double with a stand-in TLS
server behind it. The stand-in encrypts with this tree's own primitives — that part proves only
agreement with itself — but its `CertificateVerify` is ADR-149's fixture, signed by an independent
implementation with a key this kernel does not have. What the suite proves is the JOIN: the whole
conversation completes and the answer decrypts; a peer that cannot prove its key ends the
conversation by name and receives nothing protected; a failed tag is fatal and nothing is sent
after it; application data before Finished is refused and never copied out; a deaf peer costs the
budget; a fatal alert names itself; the compatibility CCS is skipped whether or not sent; an
oversized answer is truncated and said so.

**Live, on the runner (`scripts/tls-e2e.sh`).** A scripted operator types `tls` at the console of
the aarch64 and RISC-V guests. The peer is Python's `ssl` — OpenSSL — serving the fixture leaf on
the runner's loopback, reached through QEMU's user network. Under the right pin the guest reports
`peer verified as aletheia.test under the pin`, the peer's log shows `TLSv1.3` and the request
received in the clear only on ITS side, and the protected answer comes back on the serial line.
Under a pin one digit different the same server is refused by name — `the peer's certificate is
not one the pinned root signed for that name` — and the peer's log shows a handshake the client
refused, never a request. A port nobody listens on is refused by name. This gate is in CI as
"A real TLS 1.3 conversation".

**Console (`console=47`).** `tls` refuses a short or malformed pin by usage and a missing network
by name. Conformance contract 332 -> 341 core behaviours.

## What the live gate found

The first run against OpenSSL was refused by this client: `the peer's Finished does not match the
conversation this client saw`. The certificate had verified under the pin and the server's
`CertificateVerify` — OpenSSL's real signature over the real transcript — had verified too; only
the `Finished` disagreed. RFC 8446 §4.4.4 derives the finished key as
`HKDF-Expand-Label(BaseKey, "finished", "", Hash.length)` with an EMPTY context. Since ADR-144 this
tree derived it through `Derive-Secret` with no messages, whose context is `Transcript-Hash("")` —
thirty-two bytes, not zero. Every suite agreed with itself about it: the client computed both
Finished values, and this ADR's own stand-in server used the same function. OpenSSL did not agree,
which is the whole reason the stand-in was never going to be enough. `finished_mac` now derives the
key by the letter of the RFC, and one vector computed outside this tree (Python's `hmac`, by hand
from the RFC) pins it, with the old formula's output recorded beside it as the value that must
never come back.

Second finding, smaller: the pump's receive buffer was sized to this side's 536-byte MSS. A real
peer sends what the path allows (1460 bytes over user-mode networking), and every server flight was
refused as a datagram that did not fit. The buffer is an MTU now.

## Alternatives considered

**A root store, or a pin baked into the kernel.** Rejected: the fixture root's private key is in
`scripts/tls-fixtures.py`; a kernel that trusted it would trust anyone who read the repository. The
person at the console decides whom to trust, per conversation.

**Proving the join only against the stand-in.** Rejected: a stand-in can only prove agreement with
this tree. OpenSSL on the runner is what makes "speaks TLS" a fact about the world.

**Deriving the ephemeral key from the certificate fixture or a constant.** Rejected without
discussion; a fixed key is a published key.

## Consequences

Lethe's stage N2 — a TLS 1.3 client validating against a pinned trust root, with no downgrade
path — is delivered, with the entropy gap named above. Stage N3 (an HTTP/1.1 client over it) can
begin. `docs/LETHE-INTEGRATION.md` says so, and says what the entropy caveat means.
