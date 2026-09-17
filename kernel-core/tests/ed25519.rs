//! Host proof of signature verification (REQ-SEC-TLS-005, ADR-145).
//!
//! The boot suite proves RFC 8032's published vector on every CPU. These tests add signatures made
//! by an INDEPENDENT implementation (Python's `cryptography`, over OpenSSL) for keys and messages
//! this kernel never saw: a verifier that accepts only what it would have produced itself accepts
//! nothing from the world.

use kernel_core::ed25519::{ed25519_suite, verify, SignatureRefusal};
use kernel_core::sha512::{sha512, sha512_suite};

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

/// (public key, message, signature), all produced by OpenSSL.
const CASES: [(&str, &str, &str); 3] = [
    (
        "04d3be256c58caa83f87008d3537fe3928b814f2ef6fe09d0a00cd090a74cfa1",
        "616c657468656961206d6573736167652030",
        "51999859bf98b8b4e3685a7811ebd20117ab1b1df8bb2bd660e802d8a6fac013\
         264ffbbf48f9d907c758ecbbabef7c54ca33f89beb830e3bd187bfc43e4a2003",
    ),
    (
        "4eeaaadf130120ede39396a95a48a46377e1a81503b1161a777116e56c9c8174",
        "616c657468656961206d6573736167652031",
        "500e2b21344f000c086b0c14de892c4a128d022a57847116fefd7030233a0ac3\
         61bfdaa8676068583d5f180ac94073d60f245e9fc01b2f403acc082b2c7a1e02",
    ),
    (
        "5710507df12263139fcd4a386e6fa441ee7242f772fbea5227de8f3c00742b21",
        "616c657468656961206d6573736167652032",
        "77b54972a6aa60bbad61b5096e27b8ac65a011615c474fc3434ce3a326de9ddf\
         14a7ff7e8b677dbcfb940a8611d2e3d41be821e2447d249c7047e7bec58eee0f",
    ),
];

fn case(i: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (pk, msg, sig) = CASES[i];
    let sig: String = sig.chars().filter(|c| !c.is_whitespace()).collect();
    (unhex(pk), unhex(msg), unhex(&sig))
}

#[test]
fn the_live_suites_pass_on_the_host_too() {
    let mut seen = 0;
    ed25519_suite(|_, passed, name| {
        assert!(passed, "live invariant failed on the host: {name}");
        seen += 1;
    })
    .expect("the signature suite holds on the host");
    assert!(seen >= 7);

    sha512_suite(|_, passed, name| assert!(passed, "live digest invariant failed: {name}"))
        .expect("the digest suite holds on the host");
}

#[test]
fn signatures_from_an_independent_implementation_verify() {
    for i in 0..CASES.len() {
        let (pk, msg, sig) = case(i);
        assert_eq!(verify(&pk, &msg, &sig), Ok(()), "case {i} did not verify");
    }
}

#[test]
fn a_signature_never_verifies_under_another_case_s_key_or_message() {
    // Cross every key against every other case's message and signature: a verifier that ignores
    // part of its input passes the matching cases and fails here.
    for i in 0..CASES.len() {
        for j in 0..CASES.len() {
            if i == j {
                continue;
            }
            let (pk_i, msg_i, sig_i) = case(i);
            let (pk_j, msg_j, sig_j) = case(j);
            assert!(verify(&pk_j, &msg_i, &sig_i).is_err(), "key {j} took {i}");
            assert!(verify(&pk_i, &msg_j, &sig_i).is_err(), "msg {j} took {i}");
            assert!(verify(&pk_i, &msg_i, &sig_j).is_err(), "sig {j} took {i}");
        }
    }
}

#[test]
fn every_single_bit_of_a_signature_matters() {
    // Sweep the whole signature: there must be no bit an attacker can flip for free.
    let (pk, msg, sig) = case(0);
    for i in 0..sig.len() {
        for bit in 0..8 {
            let mut tampered = sig.clone();
            tampered[i] ^= 1 << bit;
            assert!(
                verify(&pk, &msg, &tampered).is_err(),
                "signature byte {i} bit {bit} was accepted after tampering"
            );
        }
    }
    // And the untampered signature still verifies, so the sweep proved verification rather than a
    // verifier that refuses everything.
    assert_eq!(verify(&pk, &msg, &sig), Ok(()));
}

#[test]
fn every_single_bit_of_the_message_matters() {
    let (pk, msg, sig) = case(1);
    for i in 0..msg.len() {
        for bit in 0..8 {
            let mut tampered = msg.clone();
            tampered[i] ^= 1 << bit;
            assert!(
                verify(&pk, &tampered, &sig).is_err(),
                "message byte {i} bit {bit} was accepted after tampering"
            );
        }
    }
}

#[test]
fn a_scalar_at_or_above_the_group_order_is_refused() {
    // Malleability: S + L is a second encoding of the same signature. Accepting it means one
    // message has two valid signatures, which breaks anything using a signature as an identifier.
    let (pk, msg, sig) = case(0);
    let order = unhex("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
    let mut plus_order = sig.clone();
    let mut carry = 0u16;
    for i in 0..32 {
        let sum = plus_order[32 + i] as u16 + order[i] as u16 + carry;
        plus_order[32 + i] = sum as u8;
        carry = sum >> 8;
    }
    assert!(verify(&pk, &msg, &plus_order).is_err());

    let mut at_order = sig;
    at_order[32..].copy_from_slice(&order);
    assert_eq!(
        verify(&pk, &msg, &at_order),
        Err(SignatureRefusal::NonCanonicalScalar)
    );
}

#[test]
fn the_digest_matches_the_reference_for_the_length_a_signature_hashes() {
    // Ed25519 hashes R || A || M, so 64 bytes is the shortest input it ever digests. This value
    // came from Python's hashlib, not from this code.
    assert_eq!(
        hex(&sha512(&[0u8; 64])),
        "7be9fda48f4179e611c698a73cff09faf72869431efee6eaad14de0cb44bbf66\
         503f752b7a8eb17083355f3ce6eb7d2806f236b25af96a24e22b887405c20081"
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .as_str()
    );
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
