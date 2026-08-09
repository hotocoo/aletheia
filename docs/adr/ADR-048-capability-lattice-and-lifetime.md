# ADR-048 — The authority lattice, and what a capability is across a reboot

**Status:** Accepted
**Date:** 2026-08-09
**Requirements:** REQ-CAP-007 (authority lattice), REQ-CAP-008 (capability lifetime)
**Closes:** GAPS4 `ALET-P1-027` (capability scope formal composability), `ALET-P1-026` (capability
lifetime/persistence model)
**Supersedes nothing.** Extends ADR-003 (capability security), ADR-027 (capability concurrency),
ADR-038 (durable content-verified store).

---

## Context

Two of the oldest open rows in the GAPS4 register are the same question asked at two different
timescales.

`ALET-P1-027` asks what **narrower** means. Every capability guarantee in this repository rests on
attenuation — a delegation may only produce equal-or-narrower authority — and until now that phrase
had no definition anywhere. `CapEngine::delegate` compared the parent and child field by field,
inline, with whichever predicate was at hand, and `docs/INVARIANT-CONTRACTS.md` had no section for
it. Attenuation was the load-bearing property with the least written down about it.

`ALET-P1-026` asks how long a capability lives. ADR-038 made *entities* durable and said in as many
words that authority is not: `CapEngine` is born empty at every boot. That is the safe default and a
real limitation — an operating system whose authority evaporates on restart cannot have a durable
subject at all, because every capability would have to be re-minted by something with more authority
still, at a point in the boot where nothing has authenticated anyone.

The two belong in one decision because **the answer to the second is an application of the first**.
Persisting authority is the dangerous direction; what makes it safe is being able to re-run the
admission test on the way back in, and that test is exactly the lattice.

---

## Decision

### 1. The lattice lives once, in `kernel-core/src/capalg.rs`

Three partial orders — over action patterns, over scopes, over constraints — and their conjunction,
`attenuates(parent, child)`. `CapEngine::delegate` applies it when a capability is created, and
`capstore::load` applies it to every parent/child edge in a persisted registry. One implementation,
because two copies of "narrower" is two places for authority to widen.

The properties are stated in `docs/INVARIANT-CONTRACTS.md` §INV-CAP-SCOPE and proved by
**exhaustion** in `kernel-core/tests/capalg.rs`: the scope lattice is finite, so its soundness,
reflexivity and transitivity are asserted over the whole of it rather than sampled.

### 2. Covering and attenuation are different relations

An action pattern denotes a set of concrete actions — its *reach*. Two different questions are asked
of patterns:

| question | asked by | relation |
|---|---|---|
| is this concrete action inside the pattern's reach? | `evaluate` | `action_covers(pattern, action)` |
| is the child pattern's reach a subset of the parent's? | `delegate` | `action_attenuates(parent, child)` |

**`delegate` was asking the first question with the child's pattern in the action slot.** That is a
category error, and it reads as harmless because the two relations agree on every pattern whose only
`*` is a trailing one. They disagree as soon as one appears anywhere else:

```text
parent  "entity.*.*"   reach = { "entity.*" } ∪ { "entity.*.<anything>" }
child   "entity.*"     reach = { "entity" }   ∪ { "entity.<anything>" }

action_covers("entity.*.*", "entity.*")      == true    // the child STRING is in the parent's reach
action_attenuates("entity.*.*", "entity.*")  == false   // the child's REACH is not
```

With the old test that delegation was **accepted**, and the child then authorized `entity.delete`,
which its parent could never authorize. This is a privilege amplification through the one mechanism
the whole security model says cannot amplify. It is fixed in both engines — `kernel-core/src/spine.rs`
and the hosted `aletheia/src/capabilities.rs` — because a component proved safe on one and run on
the other is not proved safe at all, and it is pinned as spine invariant 12 in the cross-architecture
conformance contract.

### 3. The scope order is deliberately incomplete, never unsound

`scope_attenuates` refuses `Type(T) → Entities([…])` in both directions. A `Target` carries an id and
an etype **independently**, so `Entities([5])` authorizes `{id: 5, etype: anything}` — including
types a `Type(T)` parent never reached, and `Type(T)` reaches ids the entity set never named. Neither
is a subset of the other, and deciding the case for a *particular* entity would need a store lookup
the capability engine does not have and must not acquire: an authority check that reads the store is
an authority check that can be starved. The cost is a delegation that must be re-minted from a wider
root; the alternative is a scope order that is sometimes wrong.

`Entities([])` and `None` are recognized as the same scope — both reach nothing — so the narrowest
possible delegation is legal rather than refused on a spelling.

### 4. A persisted registry is untrusted input

`kernel-core/src/capstore.rs` serializes the engine and, far more importantly, states what a reload
must **refuse**. There is no partial load: the parts a partial load would drop — the revocation list,
a parent record — are the parts that make the rest safe. `load` refuses the whole image on any of:

| refusal | why it is not merely hygiene |
|---|---|
| `Checksum` / `Truncated` / `TrailingBytes` / `BadMagic` / `BadVersion` / `BadEncoding` | the bytes are not the bytes that were written |
| `Duplicate` | two records claiming one id make "which capability is this" undecidable |
| `Orphan` | authority whose ancestor is absent can never be revoked by it |
| `Cycle` | authority that justifies itself, and a revocation walk with no end |
| `Amplified` | a child claiming more than its parent — a delegation that never passed `delegate` |
| `ClockRewound` | see below |
| `IdReusable` | see below |

### 5. The clock is part of the capability

`Constraints::expires_at` is compared against the engine's logical clock. Persist a capability that
expired at 1000, reload it under a clock that restarts at 0, and it is live again: the expiry did not
fail, the frame of reference moved. `save` stamps the clock it was taken under; `load` refuses a
clock earlier than that stamp. Aletheia has no trusted wall clock — what it has is a value that must
never decrease, and `ClockRewound` is the check that says so.

### 6. An id must never be minted twice

Token ids come from `next_id ^ secret`. Persist the registry and lose the counter and the next boot
re-mints ids that are already held — a new capability inheriting an old handle, or a **revoked** id,
so a token that was killed authorizes again. Both the counter and the secret are in the image, and
`load` proves the property directly rather than trusting the pair: every stored id must be one the
counter has already passed.

### 7. The revocation cascade is re-derived, not replayed

The image carries a revocation list, but `load` does not trust its extent: it closes the set under
the parent edges it has already verified. A store edited to name a revoked parent and not its
descendants — the smallest edit that resurrects the most authority — comes back with the whole
subtree still dead.

**Writing that check found a real defect in the first draft of the loader.** The re-derivation was
initially written as `for r in already_revoked { self.revoke(r) }`, and `revoke` descends only when
its `insert` reports the id as newly revoked — every seed was already in the set, so the walk stopped
at the first node and every descendant came back **live**. The invariant that thins the list
(`the_cascade_is_recomputed_when_the_image_lists_only_its_root`) is why that is a comment in
`spine.rs` rather than a resurrection in the field, and it is the reason the invariant thins the list
rather than trusting a well-formed one.

---

## Consequences

**Proved.** 96 core behaviors in `scripts/conformance.sh` (was 88) — eight new, six of them
refusals. Spine invariants 11 → 13 on all three targets; a new 11-invariant
`[cap] CAPABILITY-LIFETIME` suite on all three targets and on the host, wired into the aarch64,
RISC-V, x86-64/QEMU and x86-64/VirtualBox gates. On the host: 10 exhaustive lattice proofs plus a
20 000-chain rejection campaign, and 19 capability-store proofs including a per-byte, per-bit
corruption sweep and a per-prefix truncation sweep.

**Not claimed — the image is checksummed, not authenticated.** The checksum catches corruption and
truncation and the bit sweep proves it covers every byte, but whoever can write the block can write a
matching checksum. Signing it needs a key whose own lifetime is `ALET-P1-028` (key management), still
open. So this ADR makes a persisted registry *self-consistent and non-amplifying*; it does not make
it *authentic*, and the module says so in its own docs. Any tamper that stays inside the lattice —
narrowing a capability, deleting one, marking one revoked — is accepted, because all three are things
the holder's own authority already permits.

**Not claimed — nothing writes the image to a disk yet.** `save`/`load` are the model and its
admission test, proved in kernel space on every target; wiring them through `persist.rs` onto the
durable medium is the follow-on, and it is registered rather than implied (see the register row for
`ALET-P1-026`). The reason for the split is that the ordering question — whether a capability store
commits in the same transaction as the entities whose authority it describes — is a filesystem
decision (ADR-035/038) and deserves its own wave, not a paragraph here.

**Not claimed — the logical clock is still supplied by the caller.** `CapEngine::new(secret, now)`
takes `now` as a parameter and every target passes a constant. The monotonicity rule is enforced;
where a real monotonic value comes from on a machine with no trusted time source is a separate
problem, and pretending otherwise would be the kind of claim this register exists to prevent.

## Alternatives considered

**Leave attenuation inline and just document it.** Rejected: the amplification above was found by
writing the relation down as a function and sweeping it, not by reading the code. A documented
predicate that no test exercises as an order is a comment.

**Persist capabilities inside the ADR-038 entity store rather than as their own image.** Rejected for
now — the entity store re-verifies content addresses, which is the right check for content and the
wrong one for authority; authority needs the *lattice* re-checked, and folding the two would make one
of them implicit. Keeping the capability image separate also keeps the ordering decision (§ "not
claimed") open rather than silently answered.

**Refuse persistence entirely and keep the empty-at-boot posture.** Rejected: it is not actually the
conservative choice. It forces a bootstrap path that mints wide authority at every start, which is a
worse security property than a registry that must survive an admission test.
