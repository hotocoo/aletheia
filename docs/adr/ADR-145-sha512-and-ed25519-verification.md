# ADR-145 — SHA-512 and Ed25519 verification: the signature half of a certificate verifier

- **Status:** accepted
- **Date:** 2026-09-18
- **Requirement:** REQ-SEC-TLS-005, Lethe integration stage N2 (fifth rung, first half)
- **Supersedes:** nothing. Extends ADR-069 (the proved crypto), ADR-142 (the field this shares) and
  ADR-144 (the handshake whose verifier this is for).

## Context

ADR-144 landed a TLS handshake that refuses every peer, because `PeerVerifier` has no permissive
implementation in this tree. Building a real one needs three things: a way to check a signature, a
way to parse a certificate, and a trust root. This wave takes the first — the part with published
test vectors and an independent implementation to check against.

TLS 1.3 certificates are signed; this stack advertises exactly one signature scheme (Ed25519), and
Ed25519 is specified over SHA-512, which this kernel did not have.

## Decision

Two modules, both plain:

- `kernel-core/src/sha512.rs` — FIPS 180-4, written as the specification reads. No streaming API:
  a certificate and a signed transcript both fit in a buffer, and an incremental interface would be
  surface with no caller.
- `kernel-core/src/ed25519.rs` — **verification only**. A TLS client checks signatures; it never
  makes them. Shipping a signer would mean shipping a private-key path this kernel has no use for
  and every reason not to have, so `verify` is the whole public surface.

The curve arithmetic reuses X25519's field (`Fe` became crate-visible for it). Two copies of a
carry chain are two places for a carry bug, and only one of them would have published vectors
pointed at it.

### The cofactored equation

RFC 8032 §5.1.7 lets a verifier check either `[S]B = R + [k]A` or the cofactored
`[8S]B = [8]R + [8k]A`. This implementation uses the cofactored form, for two reasons:

1. It accepts exactly what a batch verifier accepts, so this kernel cannot end up rejecting a chain
   every other implementation takes.
2. It lets `k` be the full 512-bit hash rather than reduced modulo the group order, because `[8k]A`
   depends only on `k mod L` once torsion is annihilated. The reduction it removes is a hundred
   lines of 21-bit-limb arithmetic **with no published vectors of its own** — the kind of code that
   is wrong quietly.

What is **not** relaxed: `S` must be strictly below the group order. Accepting `S + L` would accept
a second, different signature for the same message, which is the malleability that breaks anything
using a signature as an identifier.

## The proof

- **`sha512=5` and `ed25519=8`, on all three CPUs at boot**: FIPS 180-4's published digests
  including the padding boundary and a thousand-byte message; RFC 8032's published signature; a
  signature refused over a message it was not made for; a flipped bit in either half refused; a
  scalar at or above the group order refused **by name**; an off-curve key or `R` refused by name;
  short inputs refused before any byte is read; and the base point round-tripping through
  compression with `[0]B` the identity.
- **Independent agreement on the host** (`kernel-core/tests/ed25519.rs`): three signatures made by
  OpenSSL over keys and messages this kernel never saw, all verifying; every cross-pairing of key,
  message and signature refused; **every single bit** of a signature and of a message swept, each
  one required to break verification; and the malleability cases checked directly.

A vector chosen carefully rather than conveniently: the "not a point" encoding in the suite is
`y = 2`, verified in advance to be a non-residue. Most random 32-byte encodings **are** valid
points, so a test that fills bytes with `0xff` and expects a refusal proves nothing.

## Alternatives considered

**Adopt `ed25519-dalek`.** Rejected on the same ground as ADR-142: the arithmetic has published
vectors and an independent implementation to check against, and the crate brings an allocation and
feature model this kernel would then own.

**Implement signing too, for symmetry.** Rejected: a client has no use for it, and a private-key
path that exists is a private-key path that can leak.

**Do the full (non-cofactored) check with a scalar reduction.** Deferred: the reduction is the
least-testable hundred lines in the whole scheme, and the cofactored equation is both permitted and
strictly more permissive in exactly the way batch verifiers already are.

## Consequences

The signature half of a verifier exists and is proved. Still missing before `RefuseAllPeers` can be
replaced: X.509 parsing (a DER reader that refuses rather than reads), the certificate's validity
window against a clock this kernel does not yet have, name checking against the server name, and a
trust root. **This kernel still cannot speak TLS.**
