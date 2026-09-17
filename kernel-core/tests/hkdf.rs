//! Host proof of the key schedule (REQ-SEC-TLS-001, ADR-141).
//!
//! The boot suite proves the schedule against RFC 5869's published vectors and against RFC 8446's
//! byte encoding. These tests add the thing a boot suite cannot carry: **cross-implementation
//! agreement**. The expected values below were produced by an INDEPENDENT HKDF written against
//! RFC 5869 and RFC 8446 in Python (`hmac` + `hashlib` from the standard library), not by this
//! code. A key schedule that agrees only with itself derives keys no peer can reproduce, and that
//! failure is invisible to any test this module could write about itself.

use kernel_core::crypto::sha256;
use kernel_core::hkdf::{
    derive_secret, hkdf_expand, hkdf_expand_label, hkdf_extract, hkdf_suite, record_nonce,
    traffic_keys, KdfRefusal, KeySchedule, Stage, HASH_LEN, MAX_EXPAND,
};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn the_live_suite_passes_on_the_host_too() {
    let mut seen = 0;
    let n = hkdf_suite(|_, passed, name| {
        assert!(passed, "live invariant failed on the host: {name}");
        seen += 1;
    })
    .expect("the key-derivation suite holds on the host");
    assert_eq!(n, seen);
}

#[test]
fn traffic_keys_match_an_independent_implementation() {
    let secret = [0x33u8; HASH_LEN];
    let keys = traffic_keys(&secret).expect("traffic keys derive");
    assert_eq!(
        hex(&keys.key),
        "e5dec12c1402cd92f5752a557af2da8854e65dad6881eedba2d20c1b35d90e7f"
    );
    assert_eq!(hex(&keys.iv), "8bfd7ab1376b0dfdaa7d5b56");
}

#[test]
fn the_whole_schedule_matches_an_independent_implementation() {
    let transcript = sha256(b"client hello || server hello");
    let mut ks = KeySchedule::new(None);
    ks.mix_shared_secret(&[0x9fu8; 32]).expect("mix");
    let (c_hs, s_hs) = ks
        .handshake_traffic_secrets(&transcript)
        .expect("handshake");
    assert_eq!(
        hex(&c_hs),
        "6c6b8b310c0500a4b97bb94c2cde9c1102f9be1ecb01ea4c048032c6d96536c7"
    );
    assert_eq!(
        hex(&s_hs),
        "8cdf96063bf2b7d341e6e50aef43ea695da840a52db74d175e19584d9248a0cc"
    );
    ks.finish_handshake().expect("finish");
    let (c_ap, _s_ap) = ks
        .application_traffic_secrets(&transcript)
        .expect("application");
    assert_eq!(
        hex(&c_ap),
        "4f50855d30112a591160c4ab0d14a1c43d6aa1179fe5c70963aefa4fa9a92980"
    );
    assert_eq!(ks.stage(), Stage::Master);
}

#[test]
fn expansion_is_one_stream_at_every_length() {
    // Whatever length is asked for, the bytes are a prefix of the same stream: an implementation
    // that restarts the counter per call derives different keys for the same label.
    let prk = hkdf_extract(b"salt", b"input keying material");
    let mut long = [0u8; 200];
    hkdf_expand(&prk, b"info", &mut long).unwrap();
    for len in [1usize, 31, 32, 33, 64, 199] {
        let mut out = vec![0u8; len];
        hkdf_expand(&prk, b"info", &mut out).unwrap();
        assert_eq!(
            out[..],
            long[..len],
            "length {len} diverged from the stream"
        );
    }
}

#[test]
fn the_bound_is_the_bound_and_one_past_it_is_refused() {
    let prk = hkdf_extract(b"", b"x");
    let mut at_bound = vec![0u8; MAX_EXPAND];
    assert!(hkdf_expand(&prk, b"i", &mut at_bound).is_ok());
    let mut past = vec![0u8; MAX_EXPAND + 1];
    assert_eq!(
        hkdf_expand(&prk, b"i", &mut past),
        Err(KdfRefusal::TooMuchOutput)
    );
    // And the refusal wrote nothing: a partially-filled key buffer is worse than an empty one,
    // because it looks like key material.
    assert!(past.iter().all(|&b| b == 0));
}

#[test]
fn every_transcript_derives_its_own_secret() {
    // The transcript hash binds the keys to what was actually said. Two handshakes that differ in
    // one byte of one message must not share traffic secrets.
    let secret = [1u8; HASH_LEN];
    let a = derive_secret(&secret, b"c hs traffic", &sha256(b"hello")).unwrap();
    let b = derive_secret(&secret, b"c hs traffic", &sha256(b"hellp")).unwrap();
    assert_ne!(a, b);
}

#[test]
fn a_label_is_domain_separation_not_decoration() {
    // Expanding with the label "key" must not equal expanding with the raw info bytes "key": the
    // TLS structure is what keeps this key distinct from every other protocol's.
    let secret = [9u8; HASH_LEN];
    let mut labelled = [0u8; 32];
    hkdf_expand_label(&secret, b"key", &[], &mut labelled).unwrap();
    let mut raw = [0u8; 32];
    hkdf_expand(&secret, b"key", &mut raw).unwrap();
    assert_ne!(labelled, raw);
}

#[test]
fn no_two_records_share_a_nonce_under_one_key() {
    // The whole security of the AEAD rests on this. Sweep a range of sequence numbers, including
    // the byte boundaries where a wrong shift would collide, and require every nonce to differ.
    let keys = traffic_keys(&[0x44u8; HASH_LEN]).unwrap();
    let mut seen = std::collections::HashSet::new();
    for seq in (0u64..1000).chain([0xffff, 0x1_0000, 0xffff_ffff, u64::MAX - 1, u64::MAX]) {
        assert!(
            seen.insert(record_nonce(&keys.iv, seq)),
            "sequence {seq} reused a nonce"
        );
    }
}
