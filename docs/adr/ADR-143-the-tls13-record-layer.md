# ADR-143 — The TLS 1.3 record layer: every way to get it wrong is silent

- **Status:** accepted
- **Date:** 2026-09-18
- **Requirement:** REQ-SEC-TLS-003, Lethe integration stage N2 (third rung)
- **Supersedes:** nothing. Extends ADR-069 (the proved AEAD), ADR-141 (the key schedule) and
  ADR-142 (the key exchange).

## Context

With a schedule and a key exchange in place, the next rung is the thing that actually carries
bytes. A record layer is small — a five-byte header, an AEAD, a counter — and it is the part of TLS
where the failures do not announce themselves:

- A sequence number that does not advance **reuses a nonce**, and anyone watching can XOR the two
  records together. Nothing looks wrong from either endpoint.
- A header that is not authenticated as associated data lets an attacker **rewrite the length in
  flight**.
- A content type read from the *outer* header rather than from the decrypted inner plaintext lets
  a peer claim a record is whatever it likes; TLS 1.3 puts the real type inside precisely so that
  an observer cannot see the handshake's stage, and the outer byte always says `application_data`.
- A failed tag that returns "nothing decrypted" instead of ending the connection gives an attacker
  **one guess per record for the life of the connection**.

Each of those is a one-line mistake with no visible symptom, which is what makes them invariants
rather than comments.

## Decision

`kernel-core/src/tlsrecord.rs` is a pair of sequence-numbered state machines — one per direction —
over the ChaCha20-Poly1305 this kernel already proves at boot.

- **The header is the associated data.** Rewriting a length in flight fails the tag.
- **The real content type is the last non-zero byte of the inner plaintext**, after the padding is
  stripped. An all-padding inner plaintext has no type at all and is a decode error
  (RFC 8446 §5.4), not an invented `0`.
- **Padding is inside the AEAD**, caller-chosen, invisible to the reader and visible on the wire.
- **A failed tag is `RecordRefusal::Fatal`**, not a soft error. In TLS 1.3 a decryption failure
  ends the connection.
- **The sequence never wraps**: exhaustion is a named refusal, because a layer that wrapped would
  reuse every nonce it had already used. A `rekey` resets the sequence — and that is safe only
  because the key changes with it, which is why the two are one operation.

Allocation: **one, at construction**, for a 33 KB workspace (the inner-plaintext buffer and the
AEAD's MAC scratch). Nothing allocates per record, because a peer drives the record rate and a
heap that never frees (ADR-063) would turn that into a remote leak. `reset` reuses the workspace
for a new connection, which is also what keeps the boot suite's own footprint to two layers rather
than twenty-six — the trap ADR-137's file-panel suite hit and this one avoided by construction.

This wave also gives `kernel-core/src/crypto.rs` allocation-free AEAD entry points
(`aead_seal_into`, `aead_open_into`, `aead_scratch_len`) over caller-owned buffers. The existing
`Vec`-returning functions remain for callers that are not on a hot path.

## The proof

- **`tlsrecord=9`, on all three CPUs at boot**: seal/open round trip with the inner type preserved;
  the outer type always `application_data`; padding inside the AEAD, changing the wire length and
  stripped exactly; a flipped bit in header, ciphertext or tag refused; the sequence advancing so
  identical content is never identical on the wire; a record opened out of order failing its tag;
  framing answerable without touching the AEAD; an all-padding inner plaintext refused as a decode
  error; and a rekey resetting the sequence while changing the bytes.
- **Interoperability, both directions, on the host** (`kernel-core/tests/tlsrecord.rs`): a record
  produced by **OpenSSL** (through Python's `cryptography`) over ADR-141's key schedule opens here
  byte for byte, and sealing the same content here produces **exactly the same record**. A layer
  that frames, pads or nonces differently from the specification is perfectly self-consistent and
  cannot talk to anything.
- Plus a full-size record at the 2^14 limit, two thousand records with no repeated ciphertext and
  no lost ordering, and a sweep of **every single bit** of a record requiring each one to be
  authenticated.

## Alternatives considered

**Return a soft error on a bad tag and let the caller decide.** Rejected: the caller that decides
wrongly is the one an attacker is talking to.

**Read the content type from the outer header.** Rejected: it is always `application_data` in TLS
1.3, so a layer that trusts it cannot tell handshake from data — and a layer that *writes* the real
type outside has told every observer what stage the handshake is at.

**Allocate per record.** Rejected: the record rate is the peer's to choose.

## Consequences

Stage N2 now has its schedule, its key exchange and its record layer. Still missing: the handshake
state machine (ClientHello through Finished, with the transcript hash), certificate parsing,
signature verification, and a trust root. **This kernel still cannot speak TLS**, and the
integration page continues to say exactly that.
