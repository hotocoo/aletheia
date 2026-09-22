//! Host proof of the pinned verifier (REQ-SEC-TLS-007, ADR-147).
//!
//! The boot suite proves the decision on every CPU with a handful of verifications. These tests
//! spend what a host can afford: every bit of the leaf's signature and every bit of its
//! to-be-signed bytes flipped one at a time, and every truncation of the chain message, each of
//! which must be refused and none of which may be accepted.

use kernel_core::tlshandshake::{PeerVerifier, RefuseAllPeers};
use kernel_core::trust::{
    certificate_message, leaf_certificate, trust_suite, PinnedRoot, TrustRefusal, FIXTURE_NAME,
    FIXTURE_TIME, LEAF_FIXTURE, LEAF_KEY_FIXTURE, ROOT_CERTIFICATE_FIXTURE, ROOT_KEY_FIXTURE,
};

fn chain_of(certs: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![0u8; 4096];
    let n = certificate_message(certs, &mut out).expect("fits");
    out.truncate(n);
    out
}

fn pinned() -> PinnedRoot {
    PinnedRoot::new(ROOT_KEY_FIXTURE, FIXTURE_TIME).expect("a clock was supplied")
}

#[test]
fn the_live_suite_passes_on_the_host_too() {
    let mut seen = 0;
    trust_suite(|_, passed, name| {
        assert!(passed, "live invariant failed on the host: {name}");
        seen += 1;
    })
    .expect("trust suite");
    assert_eq!(seen, 9);
}

#[test]
fn the_fixture_chain_is_accepted_and_hands_back_the_leaf_key() {
    let chain = chain_of(&[&LEAF_FIXTURE]);
    assert_eq!(pinned().check(FIXTURE_NAME, &chain), Ok(LEAF_KEY_FIXTURE));
    assert_eq!(
        pinned().verify(FIXTURE_NAME, &chain),
        Some(LEAF_KEY_FIXTURE)
    );
    assert!(
        RefuseAllPeers.verify(FIXTURE_NAME, &chain).is_none(),
        "the default stays fail-closed"
    );
}

/// The leaf's signature is the last 64 bytes of the certificate; every bit of it flipped must be
/// refused as unsigned.
#[test]
fn every_bit_of_the_signature_matters() {
    let sig_start = LEAF_FIXTURE.len() - 64;
    for byte in sig_start..LEAF_FIXTURE.len() {
        for bit in 0..8 {
            let mut leaf = LEAF_FIXTURE;
            leaf[byte] ^= 1 << bit;
            let chain = chain_of(&[&leaf]);
            assert_eq!(
                pinned().check(FIXTURE_NAME, &chain),
                Err(TrustRefusal::NotSignedByRoot),
                "byte {byte} bit {bit} of the signature was not refused"
            );
        }
    }
}

/// Every bit of everything BEFORE the signature flipped: the outer header, the tbs, the outer
/// algorithm and the BIT STRING header. Each must be refused — by the reader or by the root —
/// and none may be accepted. A flip that still parses must fail the signature, because the root
/// signed different bytes.
#[test]
fn every_bit_before_the_signature_matters() {
    let sig_start = LEAF_FIXTURE.len() - 64;
    let mut refused_by_reader = 0usize;
    let mut refused_by_root = 0usize;
    for byte in 0..sig_start {
        for bit in 0..8 {
            let mut leaf = LEAF_FIXTURE;
            leaf[byte] ^= 1 << bit;
            let chain = chain_of(&[&leaf]);
            match pinned().check(FIXTURE_NAME, &chain) {
                Ok(_) => panic!("byte {byte} bit {bit} flipped and the chain was still accepted"),
                Err(TrustRefusal::NotSignedByRoot) => refused_by_root += 1,
                Err(TrustRefusal::Certificate(_)) => refused_by_reader += 1,
                // A flip inside the SAN or the validity field can parse to a different name or a
                // different window, but the signature is checked first, so neither is reachable.
                Err(other) => panic!("byte {byte} bit {bit}: unexpected refusal {other:?}"),
            }
        }
    }
    assert!(refused_by_root > 0 && refused_by_reader > 0);
}

#[test]
fn every_truncation_of_the_chain_message_is_refused() {
    let chain = chain_of(&[&LEAF_FIXTURE, &ROOT_CERTIFICATE_FIXTURE]);
    for cut in 0..chain.len() {
        assert!(
            leaf_certificate(&chain[..cut]).is_err(),
            "a chain message cut at {cut} was framed"
        );
        assert!(pinned().check(FIXTURE_NAME, &chain[..cut]).is_err());
    }
    assert_eq!(leaf_certificate(&chain), Ok(&LEAF_FIXTURE[..]));
    assert_eq!(pinned().check(FIXTURE_NAME, &chain), Ok(LEAF_KEY_FIXTURE));
}

#[test]
fn a_trailing_byte_is_a_message_this_client_does_not_agree_about() {
    let mut chain = chain_of(&[&LEAF_FIXTURE]);
    chain.push(0);
    assert_eq!(leaf_certificate(&chain), Err(TrustRefusal::BadChain));
}

#[test]
fn the_window_is_judged_at_the_supplied_time() {
    let chain = chain_of(&[&LEAF_FIXTURE]);
    let at = |t: i64| {
        PinnedRoot::new(ROOT_KEY_FIXTURE, t)
            .unwrap()
            .check(FIXTURE_NAME, &chain)
    };
    assert_eq!(at(1_767_225_599), Err(TrustRefusal::NotYetValid));
    assert_eq!(at(1_767_225_600), Ok(LEAF_KEY_FIXTURE));
    assert_eq!(at(2_082_758_400), Ok(LEAF_KEY_FIXTURE));
    assert_eq!(at(2_082_758_401), Err(TrustRefusal::Expired));
    assert_eq!(at(i64::MAX), Err(TrustRefusal::Expired));
}

#[test]
fn a_verifier_needs_a_clock() {
    for t in [0i64, -1, i64::MIN] {
        assert!(matches!(
            PinnedRoot::new(ROOT_KEY_FIXTURE, t),
            Err(TrustRefusal::NoClock)
        ));
    }
}

#[test]
fn the_root_presented_as_a_leaf_speaks_for_no_host() {
    // The root signed itself, so the signature verifies under the pin; it carries no SAN, so it
    // speaks for nothing. The refusal is the name, which is the honest one.
    let chain = chain_of(&[&ROOT_CERTIFICATE_FIXTURE]);
    assert_eq!(
        pinned().check(FIXTURE_NAME, &chain),
        Err(TrustRefusal::WrongName)
    );
}
