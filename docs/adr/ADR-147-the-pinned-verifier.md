# ADR-147 — The pinned verifier: the first peer this client can trust, and the only shape it trusts

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-SEC-TLS-007, Lethe integration stage N2 (sixth rung)
- **Supersedes:** nothing. Extends ADR-144 (the handshake that takes a verifier), ADR-145 (the
  signature check) and ADR-146 (the certificate reader).

## Context

ADR-144's handshake takes a `PeerVerifier` as a constructor argument and, until this wave, the only
implementation in the tree was `RefuseAllPeers`. ADR-145 and ADR-146 built the two mechanisms a
real verifier needs — checking a signature and reading a certificate — but not the decision:
*whom* to trust, *when*, and *for which name*.

Lethe's TLS client validates "against a pinned trust root" (`docs/LETHE-INTEGRATION.md`, N2). This
kernel also has no wall clock, and a validity window judged without one is either always open or
always shut.

## Decision

`kernel-core/src/trust.rs` — `PinnedRoot`, the first `PeerVerifier` that can say yes. It says yes
to exactly one shape: a leaf certificate signed **directly** by one pinned Ed25519 root, speaking
for the expected name, inside its validity window at a time the caller supplies.

Three decisions, each of which removes surface rather than adds it:

1. **A pin, not a store.** A root store is a list of parties allowed to speak for every name; a pin
   is one party allowed to speak for the names this client will dial. There is no chain to walk, so
   there is no path building, no name constraints, no intermediate-CA handling, and no chain-walking
   bug. Certificates the server sends after the leaf are *framed* (so a lying length is refused) and
   never read.
2. **The clock is an argument, and zero is a refusal.** `PinnedRoot::new(root, now)` refuses a time
   of zero or less by name (`NoClock`). Reading "no clock" as "time zero" would find every
   certificate not yet valid; reading it as "skip the check" would accept every expired one. When
   the platform grows a clock it will hand a real time here rather than change this code.
3. **The signature is checked first.** Nothing in an unsigned document is read as a fact: the
   validity window and the names are consulted only once the pinned root has been shown to have
   signed them. The order is observable in the refusals — a forged leaf is `NotSignedByRoot`, never
   `WrongName`.

The TLS 1.3 `Certificate` message (RFC 8446 §4.4.2) is framed to its last byte: a non-empty
request context (which a server SHALL NOT send), a list length that lies in either direction, a
zero-length certificate, an extensions length past the end, and more than four entries are each a
named refusal.

`check` hands back the leaf's public key. That key is what the handshake must check the server's
`CertificateVerify` against — the next rung.

## The proof

`trust=9` on all three CPUs at boot, against a **real root-issued Ed25519 leaf** produced by OpenSSL
(through Python's `cryptography`) — checking something this kernel generated would prove only
self-agreement. The invariants: the leaf is accepted and the key returned is the one OpenSSL put in
it; the same leaf under a pin one bit away is unsigned; ADR-146's self-signed fixture is refused
rather than believed; the wrong name is refused even though the root signed it; the window is
judged at the supplied time with both edges inclusive; no verifier exists without a clock; one
changed byte in the tbs or the signature is unsigned; the chain message's framing is checked to its
end, every truncation refused; certificates after the leaf change nothing and too many is refused.

`tlshandshake=10`: with a `PinnedRoot` and a chain it signed, the handshake passes the server's
`Certificate` and reaches `CertificateVerify`, where it **still stops by name** with no application
traffic keys. This is the one place the rung can be seen moving, and the negative from ADR-144 is
kept exactly where it now belongs.

Host (`kernel-core/tests/trust.rs`): every bit of the leaf's signature and every bit of everything
before it flipped one at a time, each refused and none accepted; every truncation of a two-entry
chain message refused; a trailing byte refused; the root presented as a leaf speaks for no host.

Conformance contract 313 -> 323 core behaviours.

## Alternatives considered

**A root store with path building.** Rejected: general-purpose PKI is most of the historical attack
surface, and a client that dials a small set of services does not need it.

**Defaulting the clock.** Rejected: both defaults are wrong, and a verifier that is right only when
the platform happens to have a clock is a verifier that cannot be trusted to be wrong loudly.

**Reading intermediates "just to validate them".** Rejected: a certificate the decision does not
rest on is a certificate the parser should not open.

## Consequences

The verifier exists. What remains before this kernel can speak TLS: checking `CertificateVerify`
over the transcript with the key this verifier returns, and a platform clock to hand `PinnedRoot` a
real time. Until both land, the handshake still ends at `CertificateVerify` by name.
