# ADR-149 — The handshake completes: CertificateVerify under the key the verifier named

- **Status:** accepted
- **Date:** 2026-09-22
- **Requirement:** REQ-SEC-TLS-009, Lethe integration stage N2 (eighth rung)
- **Supersedes:** nothing. Extends ADR-144 (the handshake), ADR-147 (the verifier) and ADR-148
  (the clock the verifier is built from).

## Context

ADR-144's handshake stopped at the server's `CertificateVerify` by name: "a signature this kernel
cannot check is a signature this kernel must not accept". ADR-145 gave it the signature check,
ADR-146 the reader, ADR-147 the decision and ADR-148 the time. What was missing was the last
binding: that the party whose certificate the verifier accepted is the party speaking in *this*
conversation. That is what `CertificateVerify` proves, and without it a verified certificate is a
public document anyone can replay.

## Decision

- **The verifier names the key.** `PeerVerifier::verify` now returns `Option<[u8; 32]>` — the
  peer's Ed25519 public key, or the refusal. A verifier that cannot name the key has not verified
  anything; `RefuseAllPeers` returns `None` and stays the fail-closed default the suites keep.
- **CertificateVerify is checked under that key, over this transcript.** RFC 8446 §4.4.3: the
  server signs 64 spaces, the context string, one zero byte and the transcript hash through the
  `Certificate`. The scheme must be the one this client offered (`UnsupportedChoice` otherwise), the
  signature must be exactly Ed25519's 64 bytes (`BadLength`), and it must verify under the key the
  VERIFIER returned — never one the message carries — or the handshake ends with `BadSignature`.
  The hash covers everything this client has seen, so a signature over any other conversation
  fails here.
- **The client's Finished exists.** `client_finished` writes this client's Finished only once the
  server's Finished has verified: sent earlier it would authenticate a transcript the server has
  not vouched for. It is sent under the handshake keys; the application keys are for what follows.
- **The fixture is signed by someone else.** The suites drive one deterministic flight (fixed client
  key and random, a stand-in server with a fixed key, an empty EncryptedExtensions, the pinned leaf
  as the Certificate). Its transcript hash is a constant, and the `CertificateVerify` over it was
  produced by `scripts/tls-fixtures.py` — Python's `cryptography` over OpenSSL — with the leaf's
  private key, a key this kernel does not have. The signature it accepts is one it could not have
  made. If a byte of the ClientHello ever changes, the host test names the new hash and the command
  that regenerates the fixture.

## The proof

`tlshandshake=12` on all three CPUs. New: past a pinned Certificate, a wrong scheme or a bad
signature in `CertificateVerify` is refused by name with no keys left behind; **under a pinned root
the server's CertificateVerify and Finished verify, application traffic keys exist, and the
client's Finished is the transcript's** (refused as out of order one message earlier); the same
valid signature over a transcript one byte different is refused as a bad signature. ADR-144's
negative (no verifier, no keys) is kept.

Host (`kernel-core/tests/tlshandshake.rs`): the fixture hash is the one the deterministic flight
produces; the fixture signature verifies over it under the leaf key; every bit of the signature and
every bit of the transcript hash flipped one at a time is refused.

Conformance contract 330 -> 332 core behaviours.

## What this does and does not prove

It proves the handshake state machine reaches `Done` only through a signature over the transcript
under a key the verifier named, and that every deviation is a named refusal. It does **not** yet
prove interoperation with a real server: the fixture flight's ServerHello is this tree's stand-in,
and no record layer carried these messages over a socket. The next rung carries the handshake over
ADR-143's records and ADR-140's TCP to a live TLS 1.3 server on the runner, the way `tcp-e2e.sh`
proved TCP — that is the moment this kernel speaks TLS, and until then it does not.

## Alternatives considered

**Signing the fixture in Rust.** Rejected: it would put a private-key path in the kernel, and a
signature the kernel could have made proves only self-agreement.

**Accepting any signature scheme the server picks.** Rejected: this client offered one; a server
choosing another has chosen something it was not offered.

## Consequences

`Handshake::application_keys` can now be `Some` — for exactly one shape of conversation. The
record layer, the TCP client and this handshake are three proved pieces not yet joined; joining them
against a live peer is the rung that remains for N2.
