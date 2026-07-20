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
