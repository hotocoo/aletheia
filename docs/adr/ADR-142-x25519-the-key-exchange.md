# ADR-142 — X25519: the key exchange, and the one output a caller must never use

- **Status:** accepted
- **Date:** 2026-09-18
- **Requirement:** REQ-SEC-TLS-002, Lethe integration stage N2 (second rung)
- **Supersedes:** nothing. Extends ADR-069 (the proved crypto) and ADR-141 (the key schedule this
  feeds).

## Context

ADR-141 built the schedule that turns a shared secret into traffic keys, and left the shared secret
itself undefined. TLS 1.3 without a pre-shared key has exactly one mandatory-to-implement group in
practice, and it is X25519.

Two things make this rung worth its own ADR rather than a quiet addition. First, it is arithmetic
that is either exactly right or silently useless: a ladder that agrees with itself and with nobody
else produces shared secrets no peer can reach, and every test written about it passes. Second, it
has a **security-critical refusal** that an implementation can easily omit, because omitting it
looks like success.

## Decision

`kernel-core/src/x25519.rs` implements RFC 7748's Montgomery ladder over GF(2^255 - 19), with field
elements as five 51-bit limbs so every product fits a `u128` and every sum fits a `u64`.

The properties are stated rather than implied, because the ones that are easy to lose are the ones
a reader cannot see:

- **Fixed work.** The ladder runs 255 iterations whatever the scalar is.
- **No secret-dependent branches or addresses.** The conditional swap is a mask, not an `if`; the
  inversion is a fixed addition chain, not a loop over exponent bits.
- **No allocation**, on a heap that never frees (ADR-063).
- **Clamping is inside the function.** A caller who forgets is not a caller who gets a different
  answer.
- **The peer's high bit is masked**, as RFC 7748 requires. A peer that sets it is not signalling
  anything, and honouring it would decode a different point than the peer computed with.

And the refusal: **[`x25519`] returns a `Result`, and an all-zero shared secret is
`X25519Refusal::SmallOrder`.** RFC 8446 §7.4.2 requires a TLS client to abort here. The all-zero
output happens when the peer sends a point of small order — a cheap, remote way to make both sides
agree on a key the attacker already knows. An implementation that returns those 32 zero bytes has
handed its caller a key that works perfectly and protects nothing. The zero test itself is
constant-time: branching on the secret's value would leak which peers are hostile, and the answer
is a refusal either way.

## The proof

- **`x25519=7`, on all three CPUs at boot**: RFC 7748 §6.1's published key pairs and their shared
  secret in both directions, §5.2's scalar-multiplication vector, the small-order refusal, clamping
  inside the function, the masked high bit, and sensitivity (a different scalar reaches a different
  secret, and both sides still agree).
- **Cross-implementation agreement, on the host** (`kernel-core/tests/x25519.rs`): public keys and
  shared secrets for four independent key pairs, taken from Python's `cryptography` (OpenSSL) and
  not from this code; **all seven** published small-order points refused by name; and RFC 7748's
  iterated ladder run **a thousand rounds**, matching the RFC's value at round 1 and round 1000. An
  error anywhere in the field arithmetic diverges under iteration and never comes back, which makes
  that one test worth more than any number of single-shot vectors.

A defect this wave found on its first run: the ladder constant was written as 121666 rather than
RFC 7748's a24 = (A - 2)/4 = 121665. Every vector failed at once — which is precisely why the
published vectors are in the boot suite rather than in a comment.

## Alternatives considered

**Adopt `curve25519-dalek` or similar.** Rejected for this rung: the crate is excellent and brings
an allocation and feature model this kernel would then own anyway, and the ladder is 300 lines over
arithmetic that has published vectors. Revisitable for the layers above, where the volume is larger
and the vectors are thinner.

**Return the all-zero secret and let the caller check.** Rejected. That is the failure mode this
ADR exists to close: it is the same shape as an API that returns `-1` for an error nobody checks,
except that here nobody can see the difference by looking at the output.

**Skip clamping and require callers to clamp.** Rejected: two implementations of one protocol then
disagree depending on who remembered.

## Consequences

Stage N2 now has its schedule and its key exchange. Still missing: the record layer over the AEAD
this kernel already proves, the handshake state machine, certificate parsing, signature
verification, and a trust root. **This kernel still cannot speak TLS**, and the integration page
continues to say so.
