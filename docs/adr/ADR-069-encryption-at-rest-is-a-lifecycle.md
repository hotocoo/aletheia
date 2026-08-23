# ADR-069: Encryption at rest is a lifecycle, not a key file

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P1-028, ALET-P1-029, ALET-P1-030 · **Builds on:** ADR-005 (content-addressed encrypted store), ADR-004 (pure-Rust crypto posture)

## Context

The hosted semantic store sealed every frame with ChaCha20-Poly1305 from its first commit — and
stopped there. One 32-byte key file wrapped everything; every record drew a fresh RANDOM 96-bit
nonce whose non-reuse was delegated to the CSPRNG and never checked; there was no rotation, no
retirement, no answer to what a content address MEANS once the bytes at rest are ciphertext; and
the log had no structural integrity — frames could be reordered or removed without detection.

Three register rows stayed open for exactly these reasons: P1-028 (key management), P1-029
(nonce lifecycle proven per object), P1-030 (encrypted content-addressing identity semantics).

## Decision

### Keys: a versioned keystore under a root (P1-028)

Data keys are VERSIONED entries in `keystore.bin`, sealed as one atomically-replaced object
under a key DERIVED from the root (`HMAC-SHA256(root, "aletheia/keystore/v1")`) — so the root
file never sits next to a ciphertext made directly under it, and the pre-existing root `key`
file keeps its role and its name: the custody anchor that wraps data keys.

* **Rotate** mints version max+1 with a fresh key AND a fresh nonce-space prefix; writes always
  use the newest; older frames stay readable while their version is retained.
* **Rekey** rewrites the whole log under the current version from an append-order journal mirror
  and retires every older version — atomic by temp+rename, so a crash leaves either the old
  complete log or the new complete log.
* **Refusals are named**: retirement above the newest version, retirement that would strand the
  write path, a frame naming a retired version, a keystore that fails authentication (wrong root
  or tamper — no partial load), a corrupt root file.
* Root/keystore files are written 0600 via temp-file+rename on unix. Custody of the ROOT remains
  what it was — operator-held — and secure-boot delivery of it stays REQ-BOOT-001 scope; this ADR
  closes the STORE-side lifecycle, not platform key provisioning.

### Nonces: constructed prefix||counter, recovered from the log itself (P1-029)

Every DATA frame nonce is CONSTRUCTED: a 32-bit per-version random prefix || a 64-bit monotone
counter. Reuse is impossible by construction rather than improbable by birthday bound:

* The counter ledger IS the log. After any crash or reopen, replay recovers each version's
  high-water mark from its own AUTHENTICATED frames and counters only move FORWARD; the
  keystore's stored counter is the creation-time value and is never trusted over the log.
* Exhaustion at `u64::MAX` is a NAMED refusal demanding rotation — never a wraparound, which
  would silently hand a new record a used nonce, the one failure AEAD cannot survive.
* The KEYSTORE object itself uses random nonces BY WRITTEN BOUND: it is rewritten only on
  rotate/retire (a bounded population, not per record), so collision probability is
  ~(rewrites)² / 2^97 — below 10^-17 for a million rotations. Unbounded populations get the
  constructed construction; bounded ones may state their bound.

### Frames: position-bound AEAD with named format history

A v2 frame is `"ALX1" | ver u8 | prefix[4] | counter u64 | ct||tag`, and the AEAD additional
authenticated data covers the frame's SEQUENCE NUMBER, its exact LENGTH and its KEY VERSION.
Therefore transposition, deletion, duplication, resend-at-another-position and mid-log resize all
fail authentication WITH THE POSITION NAMED. Trailing bytes that do not compose a whole frame are
a refusal, not silent residue.

**Identity semantics (P1-030), both halves proved:** the content address remains
SHA-256(PLAINTEXT) — identity, dedup and references are semantic facts that outlive the storage
encoding — while identical plaintexts produce DIFFERENT ciphertext frames (per-frame nonces), so
equality of content is invisible on the wire. The address is stable; the encoding is not.

### Legacy: detected by authentication, migrated wholesale

A PRE-ADR-069 log (bare nonce||ciphertext frames under the root key) is recognized by
trial-authentication of its first frame under each reading — a frame authenticates under exactly
one interpretation to within cryptographic probability, so whichever opening SUCCEEDS names the
format — and is then migrated whole into v2 before any append. Steady-state logs are ALWAYS pure
v2; the legacy reader exists only inside the one-time migrator, and migration is idempotent under
crash (temp+rename).

## Named non-claims

* **Tail truncation at an exact frame boundary is undetectable** without an external anchor
  (a head record carrying count+last-frame hash). "The last writes never happened" and "someone
  cut them off" are indistinguishable; the behavior is PINNED by a test so doc and reality
  cannot drift in either direction.
* AEAD cannot distinguish "wrong key" from "tampered bytes"; refusals say so and name the
  position and key version instead of pretending to know which.
* Concurrent writers to one store directory remain out of scope (single-writer assumption,
  unchanged since M1).
* Hardware roots / key custody / secure-boot delivery remain REQ-BOOT-001 architecture.

## Consequences and proof

`aletheia/tests/encryption_at_rest.rs` (11 proofs) plus three unit tests in-module: identity
and dedup under encryption; byte-distinct ciphertexts for equal plaintexts across stores; global
nonce distinctness with strictly increasing counters across reopen cycles; EVERY single-bit flip
of a whole log image refused (exhaustive sweep); transposition/deletion/duplication/resend
refused by position binding; torn tail vs boundary truncation pinned; wrong-root and keystore
tamper refused by name with no partial load; rotate → reopen → rekey collapsing to one version
with nothing orphaned; legacy detection + transparent migration through the UNCHANGED public API;
nonce exhaustion named. The full aletheia crate suite stays green (124 lib + integration).
