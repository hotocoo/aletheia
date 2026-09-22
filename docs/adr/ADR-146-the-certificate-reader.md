# ADR-146 — The certificate reader: a parser that refuses rather than reads

- **Status:** accepted
- **Date:** 2026-09-18
- **Requirement:** REQ-SEC-TLS-006, Lethe integration stage N2 (fifth rung, second half)
- **Supersedes:** nothing. Extends ADR-144 (the handshake whose verifier this feeds) and ADR-145
  (signature verification).

## Context

ADR-145 gave this kernel a way to check an Ed25519 signature. A certificate verifier needs two more
things: a way to read a certificate, and something to trust. This wave takes the reader.

Certificate parsers are where TLS clients get compromised, and the historical failures share one
shape: **a parser that reads what a length claims instead of refusing what the buffer cannot
hold**. The input is attacker-chosen, the format is recursive, and every length is self-described.

## Decision

`kernel-core/src/x509.rs` is a DER reader with a bounded depth and a named refusal for every
malformed shape, plus exactly the X.509 fields a TLS client needs.

Read: the `tbsCertificate` bytes **as they appeared** (a re-encoded tbs is a different document, and
verifying over it would verify nothing), the validity window, the Ed25519 subject public key, the
DNS names from the subject-alternative-name extension, and the outer signature.

Deliberately **not** read, as a decision rather than an omission: RSA and ECDSA keys, the full
distinguished-name grammar, path-length constraints, CRL and OCSP pointers, and every extension
except SAN. Each is attacker-reachable surface, and none is needed to answer "is this the key that
signed, and is that name this host". A certificate carrying something else is refused **by name**
rather than half-understood.

Three rules that exist because leaving them out is invisible:

1. **DER's length encoding is enforced**: indefinite lengths (legal in BER) and non-minimal long
   forms are refused. Those are how two parsers are made to see different documents, which is the
   whole of a signature-bypass bug.
2. **A wildcard covers exactly one label.** `*.a.test` matches `b.a.test` and never `c.b.a.test` —
   the rule that keeps one compromised host from speaking for a subtree.
3. **A time this reader cannot read is a refusal, never a zero.** Zero would make a certificate
   valid from the epoch.

Nothing allocates: every accessor returns a slice of the caller's buffer.

## The proof (`x509=9`, on all three CPUs at boot)

The fixture is a real self-signed Ed25519 certificate produced by OpenSSL — parsing something this
kernel generated would prove only that it agrees with itself. The invariants: it parses to key, tbs
and signature; **its own signature verifies over the tbs bytes this reader returns**; the validity
window reads as the seconds it means; the certificate speaks for the name it carries and no other,
case-insensitively; a wildcard covers one label and never a subtree; **every truncation** of the
certificate is refused (each prefix, not a sample); indefinite, non-minimal and overlong lengths are
refused by name; a non-Ed25519 certificate is refused by name; and nesting deeper than the bound is
refused rather than recursed.

## Alternatives considered

**A general X.509 library.** Rejected: the general grammar is most of the attack surface, and this
client needs five fields.

**Accept any signature algorithm and check it later.** Rejected: a parser that half-understands a
certificate hands the next layer a decision it cannot make.

**Tolerate BER lengths for compatibility.** Rejected outright — that tolerance is the bug.

## Consequences

The verifier now has both halves of its machinery: it can read a certificate and check a signature.
What is still missing is what to **trust**: a root store, and a clock to check validity against.
Until those land `RefuseAllPeers` stays, and **this kernel still cannot speak TLS**.
