//! Host proof of the TLS 1.3 record layer (REQ-SEC-TLS-003, ADR-143).
//!
//! The boot suite proves the layer against itself: what it seals, it opens. That is necessary and
//! not sufficient — a record layer that frames, pads or nonces differently from the specification
//! is perfectly self-consistent and cannot talk to anything.
//!
//! So the record below was produced by an INDEPENDENT implementation: Python's `cryptography`
//! (OpenSSL's ChaCha20-Poly1305) over the key schedule of ADR-141, with the header as associated
//! data and the nonce as IV xor sequence. This kernel must open it, byte for byte, and must
//! produce exactly the same bytes when sealing the same content.

use kernel_core::hkdf::traffic_keys;
use kernel_core::tlsrecord::{
    tlsrecord_suite, ContentType, RecordLayer, RecordRefusal, HEADER_LEN, MAX_PLAINTEXT,
};

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A handshake record carrying "client hello", sealed under the traffic keys of the secret
/// 0x11..11 at sequence zero, by OpenSSL rather than by this code.
const REFERENCE: &str = "170303001d640ba4f118035698a2b27c8833f080ae3664edae1f3edc6ac7fc8cb572";

#[test]
fn the_live_suite_passes_on_the_host_too() {
    let mut seen = 0;
    let n = tlsrecord_suite(|_, passed, name| {
        assert!(passed, "live invariant failed on the host: {name}");
        seen += 1;
    })
    .expect("the record-layer suite holds on the host");
    assert_eq!(n, seen);
}

#[test]
fn a_record_from_an_independent_implementation_opens_here() {
    let keys = traffic_keys(&[0x11u8; 32]).unwrap();
    let mut layer = RecordLayer::new(keys, keys);
    let wire = unhex(REFERENCE);
    let mut out = [0u8; 64];
    let (ty, len, used) = layer
        .open(&wire, &mut out)
        .expect("the reference record opens");
    assert_eq!(ty, ContentType::Handshake);
    assert_eq!(&out[..len], b"client hello");
    assert_eq!(used, wire.len());
}

#[test]
fn sealing_the_same_content_produces_the_same_bytes() {
    // The other direction of interoperability: not only must this kernel read what OpenSSL wrote,
    // it must write what OpenSSL would have written.
    let keys = traffic_keys(&[0x11u8; 32]).unwrap();
    let mut layer = RecordLayer::new(keys, keys);
    let mut wire = [0u8; 64];
    let n = layer
        .seal(ContentType::Handshake, b"client hello", 0, &mut wire)
        .expect("a record seals");
    assert_eq!(hex(&wire[..n]), REFERENCE);
}

#[test]
fn a_full_size_record_round_trips() {
    // The limit itself, not a comfortable value below it: off-by-one framing shows up only here.
    let keys = traffic_keys(&[0x55u8; 32]).unwrap();
    let mut a = RecordLayer::new(keys, keys);
    let mut b = RecordLayer::new(keys, keys);
    let content = vec![0xABu8; MAX_PLAINTEXT];
    let mut wire = vec![0u8; MAX_PLAINTEXT + 512];
    let mut got = vec![0u8; MAX_PLAINTEXT + 512];
    let n = a
        .seal(ContentType::ApplicationData, &content, 0, &mut wire)
        .expect("a full-size record seals");
    let (ty, len, used) = b.open(&wire[..n], &mut got).expect("and opens");
    assert_eq!(ty, ContentType::ApplicationData);
    assert_eq!(&got[..len], &content[..]);
    assert_eq!(used, n);

    // One byte past the limit is refused rather than split or truncated.
    let too_much = vec![0u8; MAX_PLAINTEXT + 1];
    assert_eq!(
        a.seal(ContentType::ApplicationData, &too_much, 0, &mut wire),
        Err(RecordRefusal::TooLong)
    );
}

#[test]
fn a_long_stream_of_records_never_repeats_a_nonce_or_loses_order() {
    // Two thousand records in one direction: the sequence must advance every time, the bytes must
    // differ every time, and the reader must accept them in exactly the order they were written.
    let keys = traffic_keys(&[0x66u8; 32]).unwrap();
    let mut a = RecordLayer::new(keys, keys);
    let mut b = RecordLayer::new(keys, keys);
    let mut seen = std::collections::HashSet::new();
    let mut wire = [0u8; 256];
    let mut got = [0u8; 256];
    for i in 0..2000u32 {
        let content = i.to_be_bytes();
        let n = a
            .seal(ContentType::ApplicationData, &content, 0, &mut wire)
            .expect("seals");
        assert!(
            seen.insert(wire[HEADER_LEN..n].to_vec()),
            "record {i} repeated a ciphertext, which means a repeated nonce"
        );
        let (ty, len, _) = b.open(&wire[..n], &mut got).expect("opens in order");
        assert_eq!(ty, ContentType::ApplicationData);
        assert_eq!(&got[..len], &content[..]);
    }
    assert_eq!(a.write.sequence(), 2000);
    assert_eq!(b.read.sequence(), 2000);
}

#[test]
fn every_single_bit_of_a_record_is_authenticated() {
    // Sweep the whole record, one bit at a time. Not a sample: the header is associated data and
    // the body is ciphertext, and there must be no byte an attacker can change for free.
    let keys = traffic_keys(&[0x77u8; 32]).unwrap();
    let mut a = RecordLayer::new(keys, keys);
    let mut wire = [0u8; 1024];
    let mut got = [0u8; 256];
    let n = a
        .seal(ContentType::Handshake, b"finished", 0, &mut wire)
        .expect("seals");
    for i in 0..n {
        for bit in 0..8 {
            let mut b = RecordLayer::new(keys, keys);
            wire[i] ^= 1 << bit;
            let verdict = b.open(&wire, &mut got);
            assert!(
                verdict.is_err(),
                "byte {i} bit {bit} was accepted after tampering: {verdict:?}"
            );
            wire[i] ^= 1 << bit;
        }
    }
    // And the untampered record still opens, so the sweep proved authentication rather than a
    // layer that refuses everything.
    let mut b = RecordLayer::new(keys, keys);
    assert!(b.open(&wire[..n], &mut got).is_ok());
}

#[test]
fn padding_is_invisible_to_the_reader_and_visible_on_the_wire() {
    let keys = traffic_keys(&[0x88u8; 32]).unwrap();
    let mut got = [0u8; 256];
    let mut lengths = Vec::new();
    for pad in [0usize, 1, 17, 255] {
        let mut a = RecordLayer::new(keys, keys);
        let mut b = RecordLayer::new(keys, keys);
        let mut wire = [0u8; 512];
        let n = a
            .seal(ContentType::ApplicationData, b"secret", pad, &mut wire)
            .expect("seals");
        lengths.push(n);
        let (ty, len, _) = b.open(&wire[..n], &mut got).expect("opens");
        assert_eq!(ty, ContentType::ApplicationData);
        assert_eq!(
            &got[..len],
            b"secret",
            "padding {pad} leaked into the content"
        );
    }
    // Each amount of padding produced a distinct wire length: the padding is real, not dropped.
    let mut sorted = lengths.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), lengths.len());
    assert_eq!(
        a_padding_of_255_is_refused_at_256(),
        Err(RecordRefusal::TooLong)
    );
}

fn a_padding_of_255_is_refused_at_256() -> Result<usize, RecordRefusal> {
    let keys = traffic_keys(&[0x88u8; 32]).unwrap();
    let mut a = RecordLayer::new(keys, keys);
    let mut wire = [0u8; 512];
    a.seal(ContentType::ApplicationData, b"secret", 256, &mut wire)
}
