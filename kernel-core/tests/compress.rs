//! Compression at rest: round-trip exactness, named refusals, and the bounds that make a decoder
//! safe to point at bytes from an untrusted medium (ADR-024 storage stack).
//!
//! The property everything else rests on: `decompress(compress(x)) == x` for every input class the
//! durable objects actually contain — empty, tiny, highly repetitive (the common case), and
//! incompressible noise (where the envelope must fall back to RAW rather than grow). On top of
//! that: corruption of every header field is refused BY NAME, the declared length bounds decoding
//! before allocation, and encoding is deterministic.

use kernel_core::compress::{compress, decompress, DecompressError, HEADER_LEN, MAX_OUTPUT};

/// Deterministic pseudo-random bytes — incompressible by construction, which is the point.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    let mut s = seed | 1;
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        v.push((s >> 33) as u8);
    }
    v
}

#[test]
fn round_trip_is_exact_for_every_input_class_the_store_contains() {
    let mut cases: Vec<Vec<u8>> = Vec::new();
    cases.push(Vec::new());
    cases.push(vec![b'x']);
    cases.push(b"hello world, hello world, hello world".to_vec());
    // Store-shaped: repetitive structured text, the durable image's actual diet.
    let mut store_like = Vec::new();
    for i in 0..200u32 {
        store_like.extend_from_slice(
            format!(
                "entity {i} provenance kernel::persist content-hash 0x{idx:016x}",
                idx = i
            )
            .as_bytes(),
        );
    }
    cases.push(store_like);
    // Max-compression corner: one byte repeated.
    cases.push(vec![b'a'; 50_000]);
    // Overlapping-run corner: short period that forces self-overlapping match copies.
    cases.push(b"abcabcabcabcabcabcabcabcabcabc".to_vec());
    // Incompressible noise at several sizes, including non-multiples of the group width.
    cases.push(noise(1, 1));
    cases.push(noise(17, 2));
    cases.push(noise(4096, 3));
    cases.push(noise(100_003, 4));

    for case in &cases {
        let env = compress(case);
        let back = decompress(&env).expect("every case must round-trip");
        assert_eq!(
            &back,
            case,
            "round-trip drifted for {} input bytes",
            case.len()
        );
    }
}

#[test]
fn incompressible_input_is_stored_raw_and_never_grows_meaningfully() {
    let data = noise(8192, 7);
    let env = compress(&data);
    assert_eq!(
        env[4], 0,
        "noise must take the RAW algorithm, not LZSS tokens"
    );
    assert!(
        env.len() <= data.len() + HEADER_LEN,
        "RAW fallback must cost no more than the fixed header over the input"
    );
    assert_eq!(decompress(&env).unwrap(), data);
}

#[test]
fn repetitive_input_takes_the_lzss_algorithm_and_actually_compresses() {
    let mut data = Vec::new();
    for i in 0..500u32 {
        data.extend_from_slice(format!("the same durable line, number {i}, again\n").as_bytes());
    }
    let env = compress(&data);
    assert_eq!(env[4], 1, "repetitive text must take the LZSS algorithm");
    let payload_len = u32::from_le_bytes(env[9..13].try_into().unwrap()) as usize;
    assert!(
        payload_len < data.len() / 2,
        "this input should halve; it only reached {} of {}",
        payload_len,
        data.len()
    );
    assert_eq!(decompress(&env).unwrap(), data);
}

#[test]
fn encoding_is_deterministic() {
    let data = noise(3000, 11);
    assert_eq!(compress(&data), compress(&data));
}

#[test]
fn every_corrupted_header_field_is_a_named_refusal() {
    let data = b"some repetitive some repetitive some repetitive".to_vec();
    let env = compress(&data);

    let mut bad = env.clone();
    bad[0] = b'X';
    assert_eq!(decompress(&bad), Err(DecompressError::BadMagic));

    let mut bad = env.clone();
    bad[4] = 9;
    assert_eq!(decompress(&bad), Err(DecompressError::UnknownAlgorithm(9)));

    let mut bad = env.clone();
    bad[OFF_ORIG..OFF_ORIG + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(decompress(&bad), Err(DecompressError::DeclaredTooLarge));

    let mut bad = env.clone();
    bad[OFF_PAY..OFF_PAY + 4].copy_from_slice(&(env.len() as u32).to_le_bytes());
    assert_eq!(decompress(&bad), Err(DecompressError::BadPayloadLength));

    assert_eq!(
        decompress(&env[..HEADER_LEN - 1]),
        Err(DecompressError::TooShort)
    );

    // A flipped PAYLOAD byte in the middle of a large envelope decodes to same-shaped but wrong
    // bytes — precisely the checksum's catch.
    let mut big = Vec::new();
    for i in 0..500u32 {
        big.extend_from_slice(format!("durable line {i} of the store image\n").as_bytes());
    }
    let env = compress(&big);
    let mut bad = env.clone();
    let mid = HEADER_LEN + (bad.len() - HEADER_LEN) / 2;
    bad[mid] ^= 0xFF;
    assert_eq!(decompress(&bad), Err(DecompressError::ChecksumMismatch));

    // A flip in the token structure itself (here: the final payload byte) trips whichever
    // structural check sees it first — overrun, truncation, or checksum — but NEVER decodes to
    // silently wrong bytes. The assertion is that some named refusal fires.
    let mut bad = env.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    assert!(
        decompress(&bad).is_err(),
        "a corrupted payload must never decode successfully"
    );
}

const OFF_ORIG: usize = 5;
const OFF_PAY: usize = 9;

#[test]
fn the_declared_length_bounds_decoding_before_any_output_exists() {
    // A hostile or corrupt envelope may declare up to MAX_OUTPUT; anything beyond is refused
    // before the output vector is allocated, whatever the payload claims.
    let data = b"tiny".to_vec();
    let mut env = compress(&data);
    env[OFF_ORIG..OFF_ORIG + 4].copy_from_slice(&((MAX_OUTPUT + 1) as u32).to_le_bytes());
    assert_eq!(decompress(&env), Err(DecompressError::DeclaredTooLarge));

    // And a declaration the payload cannot possibly fill is an overrun refusal, not a hang:
    // decoding stops at the declared bound even with tokens still pending.
    let mut env = compress(&data);
    env[OFF_ORIG..OFF_ORIG + 4].copy_from_slice(&10_000u32.to_le_bytes());
    assert!(matches!(
        decompress(&env),
        Err(DecompressError::Underrun) | Err(DecompressError::Overrun)
    ));
}
