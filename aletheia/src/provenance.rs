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
}
