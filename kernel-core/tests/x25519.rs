//! Host proof of the key exchange (REQ-SEC-TLS-002, ADR-142).
//!
//! The boot suite proves RFC 7748's published vectors on every CPU. These tests add what a boot
//! suite cannot afford: **cross-implementation agreement over many key pairs**, and the RFC's own
//! iterated ladder run a thousand times.
//!
//! Every expected value below came from an INDEPENDENT implementation (Python's `cryptography`
//! library, which wraps OpenSSL), not from this code. A ladder that agrees only with itself
//! produces shared secrets no peer can reach, and no test written about this module could see it.

use kernel_core::x25519::{clamp, public_key, x25519, X25519Refusal};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("hex");
    }
    out
}

/// sha256([i]) as a private key, with the public key and the shared secret against a second key —
/// all three taken from the independent implementation.
const CASES: [(&str, &str, &str, &str); 4] = [
    (
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
        "e500eab1ea22fee959eb818af78bbf61beb96d8f6b87636d3813756dd61cb36d",
        "7a63297475bd617516d8792856e3892d8290f00687c9ce840eb4e0d1d7fcbc54",
        "f8dfa8ae84c56a747fca95d65c07ef67b70316728eece7410f48fd80a703a942",
    ),
    (
        "4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a",
        "1fbecfcede5636a406600be3ab8b8dd8c087f8850910ef08be5e64ba5ce5f503",
        "044d8e00b60ea854e0b8e2f86935bc0e5a5618b85560d089c4f1be4fc6e55658",
        "5a7e90cfaebd1624251b9bc0427ef139865ab44e51ce06351a6b65657f67f101",
    ),
    (
        "dbc1b4c900ffe48d575b5da5c638040125f65db0fe3e24494b76ea986457d986",
        "ecd1cb8bb5a2d0bed303b753a421ffdbf206b2addd48c76227834f07c9fb3d00",
        "a3a4f2cb1e0520293187e2de5ba3f3f7a47cbf41ab73b897792420a4aa9deb1e",
        "7d5279e62029b905af112f0a31f62ce4fc909d41f8f4fd9c568d914f491ee86b",
    ),
    (
        "084fed08b978af4d7d196a7446a86b58009e636b611db16211b65a9aadff29c5",
        "bfd7bb86bad1328ade491f84422fa0d899a632f44778eaccda5b72588fa6087e",
        "637769572e7514f154e36555dc1974f35f0a2b62ae5340f7fa2868447c59f070",
        "bc8a0013efd4a19f2ee1c72d0488e92699b6f11e4ae32a1173e07406b0955305",
    ),
];

#[test]
fn public_keys_match_an_independent_implementation() {
    for (sk, pk, _peer, _shared) in CASES {
        let got = public_key(&unhex(sk)).expect("a public key derives");
        assert_eq!(hex(&got), pk, "private key {sk}");
    }
}

#[test]
fn shared_secrets_match_an_independent_implementation() {
    for (sk, _pk, peer, shared) in CASES {
        let got = x25519(&unhex(sk), &unhex(peer)).expect("an exchange completes");
        assert_eq!(hex(&got), shared, "private key {sk}");
    }
}

#[test]
fn both_sides_of_every_exchange_agree() {
    // The property the protocol rests on, over every pair of the sample keys rather than one.
    for (i, (sk_a, _, _, _)) in CASES.iter().enumerate() {
        for (j, (sk_b, _, _, _)) in CASES.iter().enumerate() {
            let a = unhex(sk_a);
            let b = unhex(sk_b);
            let pa = public_key(&a).unwrap();
            let pb = public_key(&b).unwrap();
            assert_eq!(
                x25519(&a, &pb).unwrap(),
                x25519(&b, &pa).unwrap(),
                "pair ({i}, {j}) disagreed"
            );
        }
    }
}

#[test]
fn the_iterated_ladder_matches_the_rfc_after_one_and_after_a_thousand_rounds() {
    // RFC 7748 §5.2's iteration: k and u start at the base point, and each round feeds the result
    // back in. It is the strongest single test of a ladder, because an error anywhere in the field
    // arithmetic diverges and never comes back.
    let mut k = [0u8; 32];
    k[0] = 9;
    let mut u = k;
    for round in 1..=1000 {
        let next = x25519(&k, &u).expect("the iterated ladder never hits a small-order point");
        u = k;
        k = next;
        if round == 1 {
            assert_eq!(
                hex(&k),
                "422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079"
            );
        }
    }
    assert_eq!(
        hex(&k),
        "684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51"
    );
}

#[test]
fn every_small_order_point_is_refused_by_name() {
    // The published small-order points of Curve25519. A TLS client that accepts any of these has
    // agreed on a key the attacker chose (RFC 8446 §7.4.2).
    let small_order = [
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0100000000000000000000000000000000000000000000000000000000000000",
        "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
        "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    ];
    let sk = unhex(CASES[0].0);
    for point in small_order {
        assert_eq!(
            x25519(&sk, &unhex(point)),
            Err(X25519Refusal::SmallOrder),
            "small-order point {point} was not refused"
        );
    }
}

#[test]
fn clamping_is_idempotent_and_inside_the_function() {
    for (sk, _, peer, shared) in CASES {
        let raw = unhex(sk);
        let once = clamp(&raw);
        assert_eq!(clamp(&once), once, "clamping twice changed the scalar");
        // And the clamped form exchanges to the same secret as the raw one.
        assert_eq!(hex(&x25519(&once, &unhex(peer)).unwrap()), shared);
    }
}

#[test]
fn a_flipped_bit_anywhere_changes_the_secret() {
    let sk = unhex(CASES[0].0);
    let peer = unhex(CASES[0].2);
    let base = x25519(&sk, &peer).unwrap();
    for bit in [0usize, 7, 64, 128, 250] {
        let mut other = peer;
        other[bit / 8] ^= 1 << (bit % 8);
        if let Ok(secret) = x25519(&sk, &other) {
            assert_ne!(secret, base, "bit {bit} did not change the secret");
        }
    }
}
