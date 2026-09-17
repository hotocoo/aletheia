# ADR-144 — The TLS 1.3 handshake: a client that cannot be used insecurely

- **Status:** accepted
- **Date:** 2026-09-18
- **Requirement:** REQ-SEC-TLS-004, Lethe integration stage N2 (fourth rung)
- **Supersedes:** nothing. Extends ADR-141 (key schedule), ADR-142 (key exchange) and ADR-143
  (record layer).

## Context

Three rungs of stage N2 exist: a key schedule, a key exchange and a record layer. The handshake is
what drives them — ClientHello out, ServerHello in, keys derived from the transcript, the server's
flight checked, Finished exchanged.

It is also the rung where a partial implementation is dangerous rather than merely incomplete. **A
TLS handshake that completes without checking who it is talking to is worse than no TLS at all**,
because it looks encrypted. Certificate verification is the one step whose absence produces no
error, no warning, and no visible difference: the connection works, the bytes are ciphertext, and
the peer is whoever got there first.

This kernel has no X.509 parser, no signature verification and no trust root. So the question this
ADR answers is not "how do we handshake" but "how do we land a handshake that **cannot** be used
insecurely while those are missing".

## Decision

`kernel-core/src/tlshandshake.rs` is a client handshake whose security-critical step is a
**constructor argument**, not an optional call.

- `Handshake::new` takes a `PeerVerifier`.
- The only implementation this kernel ships is `RefuseAllPeers`, which refuses every certificate.
- Therefore every handshake runs to the server's `Certificate` and stops with
  `HandshakeRefusal::PeerUnverified`, and `application_keys()` returns `None` — always, on every
  path, for every message order.

That is a fail-closed default rather than a documented caution. The boot suite and a host test
both prove the negative directly: **no sequence of messages reaches application traffic keys**.
When a real verifier arrives it is an addition, not a removal of a guard someone has to remember.

The rest is the usual shape of this tree:

- **The offer is exactly what the client can honour**: one cipher suite
  (`TLS_CHACHA20_POLY1305_SHA256`), one group (x25519), one signature scheme (Ed25519), one key
  share. Advertising more invites a server to choose something the client must then refuse.
- **The downgrade sentinels of RFC 8446 §4.1.3 are checked.** A client that ignores them can be
  talked down to TLS 1.2 by anyone in the path, and everything above would still look like it
  worked.
- **Every parse refuses rather than reads**: a length that runs past the buffer, a version that is
  not TLS 1.3, a suite or group not offered, a malformed key share, a message out of order.
- **The transcript binds the keys to what was actually said**, and a message processed but not
  absorbed would be a message an attacker could change for free.
- **Finished is compared in constant time.** A byte-by-byte early exit lets a peer learn the
  expected value one byte at a time.
- **One allocation, at construction** (an 18 KB workspace), and `restart` reuses it — which is also
  what keeps the boot suite to a single handshake instead of ten. The first run of this wave
  exhausted the aarch64 kernel's heap with a 80 KB workspace built ten times, which is the same
  trap ADR-137's file-panel suite hit; the fix is the same one.

## The invariants (`tlshandshake=9`, on all three CPUs at boot)

The ClientHello's shape; a TLS 1.3 ServerHello deriving handshake keys that did not exist a moment
earlier; the downgrade sentinel ending the handshake by name; an unoffered cipher suite refused;
**the peer refused and no traffic keys derived with no verifier installed**; messages out of order
refused; a lying length refused; the transcript binding keys to every byte; and a deterministic,
constant-time Finished.

## Alternatives considered

**Ship the handshake with verification "to be added", defaulting to accept.** Rejected outright.
That is the exact shape of the vulnerability this ADR is about, and a comment does not stop it.

**Wait for X.509 and land both together.** Rejected: X.509 parsing, Ed25519 verification and a
trust store are each larger than this rung, and holding the handshake back would mean landing all
of it with one proof at the end.

**Accept a certificate when the caller passes a flag.** Rejected: a flag is a thing somebody sets
in a hurry. A trait with no permissive implementation in the tree cannot be set by accident.

## Consequences

Stage N2 now has four of its five parts. What remains is the verifier itself: X.509 parsing,
signature verification (Ed25519 first), a trust root, and name checking. Until that lands **this
kernel still cannot speak TLS** — and now it cannot *pretend* to either, which is the difference
this wave is for.
