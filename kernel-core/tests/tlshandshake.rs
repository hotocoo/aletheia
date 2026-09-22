//! Host proof of the completed handshake (REQ-SEC-TLS-009, ADR-149).
//!
//! The boot suite drives one deterministic flight: a fixed client key and random, a stand-in
//! server with a fixed key and random, an empty EncryptedExtensions, the pinned fixture leaf as
//! the Certificate. Its transcript hash after the Certificate is therefore a constant, and the
//! server's CertificateVerify over it was signed by an INDEPENDENT implementation (Python's
//! `cryptography`, `scripts/tls-fixtures.py`) with the leaf's private key — a key this kernel does
//! not have and cannot use, so the signature it accepts is one it could not have made.
//!
//! If the ClientHello ever changes a byte, the hash below changes, the fixture signature no longer
//! verifies, and this test says what to run.

use kernel_core::tlshandshake::{
    certificate_verify_content, tlshandshake_suite, CERTIFICATE_VERIFY_CONTENT_LEN,
};
use kernel_core::trust::{FIXTURE_CERTIFICATE_VERIFY, FIXTURE_TRANSCRIPT_HASH, LEAF_KEY_FIXTURE};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_live_suite_passes_on_the_host_too() {
    let mut seen = 0;
    tlshandshake_suite(|_, passed, name| {
        assert!(
            passed,
            "live invariant failed on the host: {name}\n\
             If the ClientHello changed, regenerate the CertificateVerify fixture:\n\
             python3 scripts/tls-fixtures.py certificate-verify <hash printed by \
             the_fixture_transcript_hash_is_the_one_the_signature_covers>"
        );
        seen += 1;
    })
    .expect("handshake suite");
    assert_eq!(seen, 12);
}

#[test]
fn the_fixture_signature_verifies_over_the_fixture_hash_under_the_leaf_key() {
    let content = certificate_verify_content(&FIXTURE_TRANSCRIPT_HASH);
    assert_eq!(content.len(), CERTIFICATE_VERIFY_CONTENT_LEN);
    assert_eq!(&content[..64], &[0x20u8; 64][..]);
    assert_eq!(&content[64..97], b"TLS 1.3, server CertificateVerify");
    assert_eq!(content[97], 0);
    assert_eq!(&content[98..], &FIXTURE_TRANSCRIPT_HASH[..]);
    assert_eq!(
        kernel_core::ed25519::verify(&LEAF_KEY_FIXTURE, &content, &FIXTURE_CERTIFICATE_VERIFY),
        Ok(()),
        "fixture hash {} does not match its signature; regenerate with scripts/tls-fixtures.py",
        hex(&FIXTURE_TRANSCRIPT_HASH)
    );
}

#[test]
fn every_bit_of_the_certificate_verify_signature_matters() {
    let content = certificate_verify_content(&FIXTURE_TRANSCRIPT_HASH);
    for byte in 0..64 {
        for bit in 0..8 {
            let mut sig = FIXTURE_CERTIFICATE_VERIFY;
            sig[byte] ^= 1 << bit;
            assert!(
                kernel_core::ed25519::verify(&LEAF_KEY_FIXTURE, &content, &sig).is_err(),
                "byte {byte} bit {bit} of the signature was not refused"
            );
        }
    }
}

#[test]
fn every_bit_of_the_transcript_hash_matters() {
    for byte in 0..32 {
        for bit in 0..8 {
            let mut hash = FIXTURE_TRANSCRIPT_HASH;
            hash[byte] ^= 1 << bit;
            let content = certificate_verify_content(&hash);
            assert!(
                kernel_core::ed25519::verify(
                    &LEAF_KEY_FIXTURE,
                    &content,
                    &FIXTURE_CERTIFICATE_VERIFY
                )
                .is_err(),
                "a signature over a different transcript was accepted (byte {byte} bit {bit})"
            );
        }
    }
}

#[test]
fn the_fixture_transcript_hash_is_the_one_the_signature_covers() {
    use kernel_core::tlshandshake::{
        drive_fixture_flight, Handshake, FIXTURE_CLIENT_PRIVATE, FIXTURE_CLIENT_RANDOM,
    };
    use kernel_core::trust::{PinnedRoot, FIXTURE_NAME, FIXTURE_TIME, ROOT_KEY_FIXTURE};
    let verifier = PinnedRoot::new(ROOT_KEY_FIXTURE, FIXTURE_TIME).expect("clock");
    let mut hs = Handshake::new(
        verifier,
        FIXTURE_NAME,
        FIXTURE_CLIENT_PRIVATE,
        FIXTURE_CLIENT_RANDOM,
    )
    .expect("handshake");
    let hash = drive_fixture_flight(&mut hs).expect("the fixture flight reaches CertificateVerify");
    assert_eq!(
        hash,
        FIXTURE_TRANSCRIPT_HASH,
        "the deterministic flight now hashes to {}; regenerate with:\n  python3 scripts/tls-fixtures.py certificate-verify {}",
        hex(&hash),
        hex(&hash)
    );
}
