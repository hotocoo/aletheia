# ADR-067: The supply chain is verified, live, and recorded

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P1-023, ALET-P2-050 · **Builds on:** ADR-025 (component provenance, REQ-BOOT-002), ADR-066 (the ABI explicitly versioned)

## Context

The pieces of a component supply chain existed on either side of the installation boundary and
never met. `AsymTrustStore` could verify a root→signing-key→component chain — in its own unit
tests. `install_signed_component` verified ONE direct signature against a SYMMETRIC key store.
Nothing installed a chain-signed artifact, nothing recorded WHICH chain vouched for what was
admitted, and nothing could UNSAY the verdict later: a signing key compromised after admission kept
verifying forever, because trust decisions were frozen at install time.

Writing the wave found the defect that proves the category: **the spawn path skipped provenance
entirely** (`prepare_spawn` loaded stored code straight into execution). Under secure policy,
an unsigned application that had slipped into the store was one `spawn` host call away from
RUNNING — a side door around every signature check (ALET-P2-050).

## Decision

* **Chain verification crosses the boundary.**
  `SysCore::install_verified_component` accepts an ed25519 DIRECT root signature or an ENDORSED
  CHAIN (root endorses signer; signer signs the content hash), verifies it against public keys
  only, and admits nothing that fails. The verification never instantiates guest code.
* **Evidence is recorded.** The admitted entity's metadata carries the whole chain — kind, root,
  signer, component signature, endorsement — plus the ABI version from the shared admission gate,
  so every install path answers "what is this and who vouched for it" without re-parsing bytes.
* **Trust decisions are LIVE.** The stored evidence is re-judged against CURRENT trust at EVERY
  launch by one gate (`launch_provenance_ok`) shared by `run_installed` AND the spawn path:
  revoking a signer goes dark at the next launch for everything that key ever signed — rotation
  after compromise without reinstalling anything. Refusals are audited with the gate that fired.
* **Faults are named per LINK.** RevokedSigner / UnendorsedSigner / BadComponentSignature are three
  different events needing three different responses — rotate-and-re-sign-everything, reject-the-key,
  reject-the-artifact — so the refusal says which.

The same wave unified code admission across ALL install paths (`admit_wasm`): a validly signed
module that cannot run is still not installable — the supply chain vouches for code, and the
platform refuses code it cannot load, whatever the paperwork says.

## Consequences

Seven proofs in `aletheia/tests/component_supply_chain.rs`: root-signed and chain-signed artifacts
install and run under secure policy; an unendorsed signer is refused at install BY NAME; a valid
signature over swapped bytes is refused because signatures cover the content hash; revocation goes
LIVE at the next launch of an already-admitted component; the spawn side door is closed and its
attempt audited; a verified child still composes normally under secure policy; and ad-hoc runs stay
refused.

Named non-claims: exactly one chain shape exists (root→signer→artifact; deeper hierarchies would be
new relation work, not an extension by convention); revocation has no expiry semantics or
transitive-closure policy beyond the two-level chain; the symmetric HMAC path remains accepted under
secure policy as the ADR-025 Phase 1 legacy — removing it is a migration decision, not drift;
hardware-bound roots, measured boot, and anti-rollback remain ADR-025 Phase 3 / REQ-BOOT-001.
