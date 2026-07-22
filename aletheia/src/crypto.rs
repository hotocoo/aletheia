//! Content addressing (SHA-256) + AEAD encryption at rest (ChaCha20-Poly1305). ADR-005.
use crate::domain::{AlethError, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes { s.push_str(&format!("{:02x}", b)); }
    s
}

pub fn sha256_hex(bytes: &[u8]) -> String { hex(&Sha256::digest(bytes)) }

pub fn random_token() -> String { hex(&rand::random::<[u8; 16]>()) }

pub fn random_key() -> [u8; 32] { rand::random::<[u8; 32]>() }

/// HMAC-SHA256 (RFC 2104), built on the already-present `sha2` — no extra dependency. Used for
/// symmetric component-signature provenance (ADR-025 Phase 1); asymmetric keys + a key hierarchy are
/// the production/hardware phases.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let mut res = [0u8; 32];
    res.copy_from_slice(&outer.finalize());
    res
}

pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    hex(&hmac_sha256(key, msg))
}

/// Constant-time equality for two equal-length strings — no early exit, so a signature comparison
/// does not leak the match prefix via timing.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Authenticated encryption. Sealed layout: 12-byte nonce || ciphertext+tag.
pub struct Cipher {
    inner: ChaCha20Poly1305,
}
impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Cipher { inner: ChaCha20Poly1305::new(Key::from_slice(key)) }
    }
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce_bytes: [u8; 12] = rand::random();
        let ct = self
            .inner
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .expect("aead seal");
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        out
    }
    pub fn open(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 12 { return Err(AlethError::persistence("ciphertext too short")); }
        let (nonce, ct) = data.split_at(12);
        self.inner
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| AlethError::persistence("aead open failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_matches_rfc4231_test_case_1() {
        // RFC 4231 §4.2: key = 20 bytes of 0x0b, data = "Hi There".
        let key = [0x0bu8; 20];
        let got = hmac_sha256_hex(&key, b"Hi There");
        assert_eq!(got, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
    }

    #[test]
    fn ct_eq_distinguishes_and_rejects_length_mismatch() {
        assert!(ct_eq("abcd", "abcd"));
        assert!(!ct_eq("abcd", "abce"));
        assert!(!ct_eq("abcd", "abcde"));
    }
}
