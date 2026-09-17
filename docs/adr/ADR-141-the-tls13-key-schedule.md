# ADR-141 — The TLS 1.3 key schedule: an order, not a bag of functions

- **Status:** accepted
- **Date:** 2026-09-18
- **Requirement:** REQ-SEC-TLS-001, Lethe integration stage N2 (first rung)
- **Supersedes:** nothing. Extends ADR-069 (the crypto this kernel already proves) and ADR-140 (the
  live transport TLS will eventually run over).

## Context

With stage N1 delivered, `docs/LETHE-INTEGRATION.md` names TLS 1.3 as the next blocker. A TLS 1.3
client is a large thing, and it decomposes: a key schedule, a key exchange, a record layer, a
handshake state machine, and certificate verification. Building it as one wave would mean landing
several thousand lines with one proof at the end, which is not how anything else in this tree
arrived.

The key schedule comes first because everything above it derives from it, and because it is the
part where a mistake is **invisible**. A wrong length is a failed handshake; a wrong ORDER is a key
derived from zeros that looks exactly like a correct key to every test written about it. Only the
peer can tell, and only by failing.

This kernel already proves SHA-256, HMAC-SHA-256, ChaCha20 and the Poly1305 AEAD at boot (ADR-069),
so nothing new is invented at the bottom.

## Decision

`kernel-core/src/hkdf.rs` holds HKDF (RFC 5869) and the TLS 1.3 schedule (RFC 8446 §7.1) as a
**state machine with named refusals**.

- `hkdf_extract` / `hkdf_expand` — the primitive, with RFC 5869's 255-block bound enforced by a
  refusal rather than a wrapping counter. A counter that wraps repeats key material.
- `write_hkdf_label` — the `HkdfLabel` structure, **exposed rather than hidden**. This encoding is
  the domain separation between TLS and everything else that expands a secret; the only way to
  know which bytes this kernel writes is to be able to look at them.
- `hkdf_expand_label`, `derive_secret` — the labelled derivations, with labels and contexts bounded
  by the wire format that carries them. A label too long is **refused, not truncated**: a truncated
  label is a different label, and it derives a different key in silence.
- `KeySchedule` — Early → Handshake → Master. Each stage's secrets exist only at that stage;
  asking early or late is `OutOfOrder`, counted like every other refusal in this tree.
- `traffic_keys`, `record_nonce` — the record-protection key and IV, and the per-record nonce. The
  nonce mixing is a function rather than a comment because a nonce reused across two records under
  one key destroys the AEAD's guarantees entirely.

Nothing allocates: every derivation writes into the caller's buffer or returns a fixed array.

## The proof

Two layers, deliberately:

1. **`hkdf=9`, on all three CPUs at boot.** RFC 5869's published vectors (including the
   empty-salt case TLS starts from, where an implementation that special-cases the empty salt fails
   at the beginning of every connection), the exact RFC 8446 label bytes, the 255-block bound, the
   refusal of an over-long label, the schedule's ordering, the distinctness of every derived
   secret, the traffic keys and the record nonce, and determinism with one-bit sensitivity.
2. **Cross-implementation agreement, on the host** (`kernel-core/tests/hkdf.rs`). The expected
   traffic keys and the full client/server handshake and application secrets were produced by an
   **independent** HKDF written against the RFCs in Python, not by this code. A key schedule that
   agrees only with itself derives keys no peer can reproduce, and that failure is invisible to
   any test this module could write about itself.

## Alternatives considered

**Free functions with no state.** Rejected: the ordering is the security argument, and a bag of
functions puts the argument in the caller, where it cannot be proved once.

**Adopt an existing `no_std` TLS crate.** Still open as a decision for the layers above, and
rejected here: the key schedule is 200 lines over primitives this kernel already proves, and
adopting a crate for it would mean adopting its allocation model for the one part that is cheapest
to own.

**Wait and land the whole TLS client at once.** Rejected: several thousand lines with one proof at
the end is exactly the shape this tree avoids.

## Consequences

The first rung of stage N2 exists and is proved. What does not exist yet: the key exchange (X25519),
the record layer over the AEAD, the handshake state machine, certificate parsing and signature
verification, and a trust root. Until those arrive **this kernel cannot speak TLS**, and the
integration page says so rather than counting a key schedule as a client.
