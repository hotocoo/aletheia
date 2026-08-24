# ADR-070: Authority custody is a lifecycle, not a caller-supplied key

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** the custody and rotation halves of
ALET-P1-034 · **Builds on:** ADR-048 (capability lifetime across a reboot), ADR-069 (encryption at
rest is a lifecycle — the design template), ADR-004 (pure-Rust crypto posture), ADR-062
(fault-injection doctrine)

## Context

`capstore` could authenticate a persisted registry only under a 32-byte key the CALLER handed in on
every call (`save_authenticated` / `load_authenticated`). That moved the problem rather than closing
it: custody was nobody's, rotation was impossible — re-keying meant changing every call site at
once — and every boot re-asked the question a keystore exists to answer. The register row
(ALET-P1-034) named exactly this: key custody, rotation, secure-boot delivery, and one combined
transaction for image-plus-entities.

A second constraint shaped everything: **the kernel has no entropy source at boot.** The soak and
stress suites already document that their randomness is deterministic because no target has
entropy this early. So whatever lifecycle is built here cannot lean on a CSPRNG — randomness
cannot be the mechanism that makes nonces safe.

ADR-069 built the template on the hosted store: versioned keys under a root, constructed nonces
whose ledger is recoverable from authenticated data, refusals that name the fact they refused, and
crash-position proofs for every commit order. This ADR ports that discipline to authority itself.

## Decision

### The root is custody; working keys are derived

`CapVault::open(fs, dev, root)` takes the 32-byte root ONCE. The vault validates its length and
immediately derives the keystore sealing subkey `HMAC-SHA256(root, "aletheia.capvault/v1/keystore-seal")`,
retaining ONLY the subkey — the root itself is forgotten. Every working key descends from the root
behind domain-separated KDFs, so the root authenticates and wraps but never seals a capability
image, mirroring ADR-069's rule that a custody anchor never sits beside a ciphertext made under it.
Custody of the ROOT remains whoever calls `open`; in a booted system those bytes arrive from the
platform trust boundary, which stays REQ-BOOT-001 scope.

### Data keys are versioned, forward-chained, and retirement DESTROYS

The keystore object (`cap.keys`, one atomically replaced filesystem object) stores only the
RETAINED versions, each `{version, key[32], next_counter}`. Rotation derives version max+1 as
`HMAC(current_key, "aletheia.capvault/v1/rotate")` — a ONE-WAY chain. That makes retirement real:
when a version leaves the keystore, its key bytes exist nowhere, and an image naming it is refused
BY NAME (`RetiredVersion`). Retention is deliberate: rotate retains max so images still naming it
keep opening until a rekey retires them.

### Nonces are CONSTRUCTED, because there is no entropy to be probabilistic with

Every sealed object carries a 96-bit constructed nonce: a per-key deterministic prefix
(`HMAC(key, nonce-domain)[..4]`) concatenated with a monotone counter persisted IN the keystore.
Reuse is impossible BY CONSTRUCTION, not improbable by birthday bound. Two ledgers, two disciplines:

* The KEYSTORE's own counter lives inside the object it rewrites — each commit advances it before
  sealing, so the atomic replace alone keeps it strictly monotone.
* The IMAGE counter is RESERVED FIRST: the keystore commit bumps `next_counter`, then the image is
  sealed naming the reserved value. Reserve-first is what survives crashes: the gap wastes a
  number, while write-first could replay one after a crash and REUSE a nonce under the same key —
  the one failure AEAD cannot survive. Exhaustion at u64::MAX is a named refusal demanding
  rotation, never a wraparound.

### Both objects are AEAD-sealed; authentication precedes parsing

ChaCha20-Poly1305 (RFC 8439) joins kernel-core's own crypto — no new dependency, verified against
the RFC's block-function, Poly1305 and AEAD known-answer vectors, with the reduction carried in
three 64-bit limbs and every intermediate bounded by construction. Both objects bind their format
version and every cleartext header field into the AEAD additional authenticated data, so field
editing or object transposition fails authentication. A keystore that does not authenticate under
the supplied root is refused WHOLE (`KeystoreAuth`) — there is no partial load of a keystore,
because the entries a partial load would drop are the ones that make the rest readable.

### Rekey is a three-commit pivot

`rekey_image`: rotate (keystore gains max+1, keeps max), rewrite the image under the newest
version, retire every older version. Three atomic commits; EVERY crash position leaves some
complete keystore+image pair openable — old image with both retained, new image with both retained,
or new image newest-only — and afterwards a replayed pre-pivot image is refused BY NAME. The host
proof records the pivot's exact device-operation sequence, then fires a refusal OF THE RIGHT KIND
at every position and demands: the error surfaces, the protocol aborts consuming nothing further,
and the reopened world opens, authenticates, and authorizes identically.

### Proofs

* Host: `kernel-core/tests/capvault.rs`, ten tests — the exhaustive crash sweep above, exact
  exhaustion boundary (MAX-1 seals and reserves MAX; MAX refuses and stores nothing; rotation
  escapes without touching the root), retirement destroying keys inside the vault, multi-reopen
  cycles with strictly increasing counters, EXHAUSTIVE byte-by-bit and truncation sweeps over
  both stored objects through real rewrites, and a block-for-block proof that a wrong-root
  refusal changes NO byte.
* Proof POSTURE is host-only, mirroring ADR-069: the boot heap never frees (ADR-063), and this
  suite's sweep churn starves later boot suites of exactly the allocations they need — the first
  boot-gated version panicked the console suite. The gate-marker map is therefore deliberately
  unchanged; writing it that way also surfaced a LATENT gate defect (`vm-e2e-vbox.sh` still
  requiring "ALL 11" capability-lifetime invariants from a suite that has printed 14 since the
  authenticated-image checks landed), fixed in passing.

## Named non-claims

* Rolling back BOTH objects to a consistent older snapshot is undetectable without an external
  anchor — the same residual ADR-069 documents. It is PINNED by tests in both directions:
  keystore-alone rollback names `FutureVersion{requested, newest}`; the consistent older pair
  opens, and the test says so rather than leaving the residual implied.
* Secure-boot DELIVERY of the root stays REQ-BOOT-001. This ADR closes the store-side lifecycle,
  not platform key provisioning.
* The capability image and the entity records it describes still commit as separate transactions;
  deciding whether they must be one remains open under ALET-P1-034.
* AEAD cannot distinguish wrong-root from tampered-bytes; both are `KeystoreAuth` and the caller
  names the object.

## Consequences

`capstore`'s caller-keyed API remains for what it honestly is — a primitive for callers who hold a
key out-of-band — while the vault becomes the path that establishes custody internally. The
sandbox doctrine holds one more place: nothing in the trust boundary depends on randomness the
machine does not have.