//! Component provenance — capability-secure signature verification for installed components
//! (ADR-025 Phase 1, gap-register Issue 7 hosted first slice).
//!
//! A component is a content-addressed `Application` entity (ADR-014); its provenance is a detached
//! signature over its content hash. This module holds the trust anchor and the sign/verify logic. The
//! model here is **symmetric** HMAC-SHA256 under a trusted key — enough to prove the boundary
//! (a tampered or unsigned component is refused under secure policy) with only the crate's existing
//! crypto. Asymmetric keys, a root→stage key hierarchy, measured boot, and rollback protection are the
//! production/hardware phases (ADR-025 Phase 2–3).
use crate::crypto::{ct_eq, hmac_sha256_hex};

/// The set of keys whose signatures are trusted to authorize a component launch. Empty by default —
/// with no trusted key, nothing verifies (so secure policy fails closed).
#[derive(Default, Clone)]
pub struct TrustStore {
    keys: Vec<[u8; 32]>,
}

impl TrustStore {
    pub fn new() -> Self {
        TrustStore { keys: Vec::new() }
    }

    /// Add a key to the trust anchor (idempotent).
    pub fn trust(&mut self, key: [u8; 32]) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Sign a component's content hash with the first (active) trusted key. `None` if no key is
    /// trusted — you cannot mint provenance without an anchor.
    pub fn sign(&self, content_hash_hex: &str) -> Option<String> {
        self.keys
            .first()
            .map(|k| hmac_sha256_hex(k, content_hash_hex.as_bytes()))
    }

    /// True iff `sig_hex` is a valid signature over `content_hash_hex` under ANY trusted key. A fresh
    /// (empty) trust store verifies nothing — fail closed. Comparison is constant-time.
    pub fn verify(&self, content_hash_hex: &str, sig_hex: &str) -> bool {
        self.keys
            .iter()
            .any(|k| ct_eq(&hmac_sha256_hex(k, content_hash_hex.as_bytes()), sig_hex))
    }
}

// ---------------------------------------------------------------------------
// Asymmetric provenance (ADR-025 Phase 2 / REQ-BOOT-002).
//
// The symmetric `TrustStore` above holds the SECRET signing key and both signs and verifies with it —
// enough to prove the boundary, but a verifier that holds the key can also forge. The asymmetric path
// fixes that: the SIGNER holds a private key (`SigningIdentity`); the VERIFIER's trust anchor
// (`AsymTrustStore`) holds PUBLIC keys ONLY, so possession of the trust store confers no ability to
// sign. It also models a real root→signing-key HIERARCHY: a trusted root ENDORSES a component-signing
// key, which signs components — so signing authority can be delegated and rotated without trusting
// every signer directly. Ed25519 (pure-Rust dalek; ADR-004). Still hardware-bound (ADR-025 Phase 3,
// REQ-BOOT-001): measured boot, a TPM/secure-enclave root of trust, and anti-rollback need persistent
// secure storage this hosted core does not have — those stay deferred, not claimed here.
// ---------------------------------------------------------------------------
use crate::crypto::{hex, unhex};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// A component-signing keypair (holds PRIVATE material). Only the holder can produce a signature; the
/// matching public key (installed in an [`AsymTrustStore`]) can only VERIFY — so a verifier, unlike
/// the symmetric [`TrustStore`], can never forge. Test identities come from a fixed 32-byte seed for a
/// deterministic hosted gate; production keys come from a CSPRNG / HSM.
pub struct SigningIdentity {
    key: SigningKey,
}

impl SigningIdentity {
    /// A deterministic identity from a 32-byte seed (reproducible in tests; NOT for production, where
    /// the seed must come from a CSPRNG or hardware key store).
    pub fn from_seed(seed: [u8; 32]) -> Self {
        SigningIdentity { key: SigningKey::from_bytes(&seed) }
    }

    /// This identity's PUBLIC key (32 bytes) — the only material a verifier ever needs.
    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// Sign a component's content-hash hex; returns the 64-byte Ed25519 signature as hex.
    pub fn sign(&self, content_hash_hex: &str) -> String {
        hex(&self.key.sign(content_hash_hex.as_bytes()).to_bytes())
    }

    /// Endorse a subordinate signing key — the root→signing-key hierarchy link. Returns the signature
    /// (hex) over the subordinate's public-key bytes; a verifier accepts the subordinate only if this
    /// endorsement verifies under a trusted root.
    pub fn endorse(&self, subordinate_public_key: &[u8; 32]) -> String {
        hex(&self.key.sign(subordinate_public_key).to_bytes())
    }
}

/// Verify an Ed25519 signature (hex) over `msg` under `public_key`. Every parse failure is a
/// verification failure (fail closed): a malformed key or signature can never be mistaken for valid.
fn ed25519_verify(public_key: &[u8; 32], msg: &[u8], sig_hex: &str) -> bool {
    let vk = match VerifyingKey::from_bytes(public_key) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let sig_arr: [u8; 64] = match unhex(sig_hex).and_then(|b| b.try_into().ok()) {
        Some(a) => a,
        None => return false,
    };
    vk.verify(msg, &Signature::from_bytes(&sig_arr)).is_ok()
}

/// Verifier-side trust anchor for asymmetric provenance: trusted ROOT public keys ONLY (no private
/// material). Empty ⇒ nothing verifies (fail closed). The asymmetric counterpart to [`TrustStore`];
/// the crucial difference is that holding this store confers NO ability to sign — a compromised
/// verifier still cannot forge a component signature.
#[derive(Default, Clone)]
pub struct AsymTrustStore {
    roots: Vec<[u8; 32]>,
}

impl AsymTrustStore {
    pub fn new() -> Self {
        AsymTrustStore { roots: Vec::new() }
    }

    /// Trust a ROOT public key (idempotent).
    pub fn trust_root(&mut self, public_key: [u8; 32]) {
        if !self.roots.contains(&public_key) {
            self.roots.push(public_key);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Direct verify: a trusted ROOT key itself signed the component's content hash.
    pub fn verify_direct(&self, content_hash_hex: &str, sig_hex: &str) -> bool {
        self.roots
            .iter()
            .any(|r| ed25519_verify(r, content_hash_hex.as_bytes(), sig_hex))
    }

    /// Hierarchical verify (root → signing key → component): a trusted root ENDORSED `signing_key`
    /// (`endorsement_hex` over `signing_key`'s public bytes) AND `signing_key` signed the component
    /// (`sig_hex` over its content hash). Both links must hold — a signing key not endorsed by a
    /// trusted root is rejected, and a valid endorsement with a bad component signature is rejected.
    pub fn verify_chain(
        &self,
        content_hash_hex: &str,
        signing_key: &[u8; 32],
        endorsement_hex: &str,
        sig_hex: &str,
    ) -> bool {
        let endorsed = self
            .roots
            .iter()
            .any(|r| ed25519_verify(r, signing_key, endorsement_hex));
        endorsed && ed25519_verify(signing_key, content_hash_hex.as_bytes(), sig_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_under_a_trusted_key() {
        let mut ts = TrustStore::new();
        ts.trust([7u8; 32]);
        let sig = ts.sign("deadbeef").expect("a trusted key can sign");
        assert!(ts.verify("deadbeef", &sig), "a signature over the same hash verifies");
        assert!(!ts.verify("cafef00d", &sig), "the signature does not verify over a different hash");
    }

    #[test]
    fn empty_trust_store_fails_closed() {
        let ts = TrustStore::new();
        assert!(ts.sign("deadbeef").is_none(), "no anchor => cannot sign");
        assert!(!ts.verify("deadbeef", "0000"), "no anchor => nothing verifies");
    }

    #[test]
    fn untrusted_keys_signature_is_rejected() {
        let mut trusted = TrustStore::new();
        trusted.trust([1u8; 32]);
        let mut attacker = TrustStore::new();
        attacker.trust([2u8; 32]);
        let forged = attacker.sign("deadbeef").unwrap();
        assert!(!trusted.verify("deadbeef", &forged), "a signature from an untrusted key is rejected");
    }

    // --- Asymmetric provenance (REQ-BOOT-002) ---

    #[test]
    fn asym_public_key_verifies_and_verifier_cannot_forge() {
        // The verifier's trust anchor holds the PUBLIC key only; the signer holds the private key.
        let signer = SigningIdentity::from_seed([3u8; 32]);
        let mut store = AsymTrustStore::new();
        store.trust_root(signer.public_key());

        let sig = signer.sign("deadbeef");
        assert!(store.verify_direct("deadbeef", &sig), "a signature over the same hash verifies");
        assert!(
            !store.verify_direct("cafef00d", &sig),
            "the signature does not verify over a different hash (tamper detected)"
        );
        // A different (untrusted) signer cannot produce a signature the store accepts — and the store,
        // holding only public keys, has no API to sign at all: the verifier cannot forge.
        let attacker = SigningIdentity::from_seed([9u8; 32]);
        assert!(
            !store.verify_direct("deadbeef", &attacker.sign("deadbeef")),
            "an untrusted key's signature is rejected"
        );
    }

    #[test]
    fn asym_empty_store_fails_closed() {
        let store = AsymTrustStore::new();
        assert!(store.is_empty());
        let signer = SigningIdentity::from_seed([3u8; 32]);
        assert!(
            !store.verify_direct("deadbeef", &signer.sign("deadbeef")),
            "no trusted root => nothing verifies (fail closed)"
        );
        assert!(!store.verify_direct("deadbeef", "not-hex"), "malformed signature fails closed");
    }

    #[test]
    fn key_hierarchy_root_endorses_signing_key() {
        // root (trust anchor) --endorses--> signing key --signs--> component.
        let root = SigningIdentity::from_seed([1u8; 32]);
        let signer = SigningIdentity::from_seed([2u8; 32]);
        let mut store = AsymTrustStore::new();
        store.trust_root(root.public_key());

        let endorsement = root.endorse(&signer.public_key());
        let sig = signer.sign("deadbeef");
        assert!(
            store.verify_chain("deadbeef", &signer.public_key(), &endorsement, &sig),
            "a root-endorsed signing key's component signature verifies through the chain"
        );

        // An UNENDORSED signing key is rejected even with a valid component signature.
        let rogue = SigningIdentity::from_seed([8u8; 32]);
        assert!(
            !store.verify_chain("deadbeef", &rogue.public_key(), &endorsement, &rogue.sign("deadbeef")),
            "a signing key the root never endorsed is rejected"
        );
        // A valid endorsement but a TAMPERED component signature is rejected (both links required).
        assert!(
            !store.verify_chain("cafef00d", &signer.public_key(), &endorsement, &sig),
            "a valid endorsement does not rescue a component signature over a different hash"
        );
        // An endorsement from an UNTRUSTED root is rejected.
        let other_root = SigningIdentity::from_seed([7u8; 32]);
        let bad_endorsement = other_root.endorse(&signer.public_key());
        assert!(
            !store.verify_chain("deadbeef", &signer.public_key(), &bad_endorsement, &sig),
            "an endorsement from an untrusted root is rejected"
        );
    }
}
