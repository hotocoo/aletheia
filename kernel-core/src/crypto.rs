//! Small no-std cryptographic primitives needed by kernel persistence.
//!
//! This module deliberately contains no key storage or boot policy. It supplies SHA-256 and
//! HMAC-SHA256 so callers can authenticate bytes with a key they obtained from a trusted boundary.
//! A caller-supplied key is not itself a secure-boot chain; capability-image key lifecycle remains a
//! separate requirement.

use alloc::vec;
use alloc::vec::Vec;

const INITIAL: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const ROUND: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

#[inline]
const fn rotate_right(x: u32, n: u32) -> u32 {
    // `rotate_right` is const-stable and intrinsified; the manual shift-or pair it replaces is
    // the same function with one more instruction and a clippy lint on modern toolchains.
    x.rotate_right(n)
}

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0u32; 64];
    for (i, word) in schedule[..16].iter_mut().enumerate() {
        let off = i * 4;
        *word = u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
    }
    for i in 16..64 {
        let x = schedule[i - 15];
        let y = schedule[i - 2];
        let small_sigma0 = rotate_right(x, 7) ^ rotate_right(x, 18) ^ (x >> 3);
        let small_sigma1 = rotate_right(y, 17) ^ rotate_right(y, 19) ^ (y >> 10);
        schedule[i] = schedule[i - 16]
            .wrapping_add(small_sigma0)
            .wrapping_add(schedule[i - 7])
            .wrapping_add(small_sigma1);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];
    for i in 0..64 {
        let big_sigma1 = rotate_right(e, 6) ^ rotate_right(e, 11) ^ rotate_right(e, 25);
        let choose = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(big_sigma1)
            .wrapping_add(choose)
            .wrapping_add(ROUND[i])
            .wrapping_add(schedule[i]);
        let big_sigma0 = rotate_right(a, 2) ^ rotate_right(a, 13) ^ rotate_right(a, 22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = big_sigma0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// SHA-256 digest, RFC 6234-compatible.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = INITIAL;
    let mut offset = 0usize;
    while offset + 64 <= data.len() {
        let mut block = [0u8; 64];
        block.copy_from_slice(&data[offset..offset + 64]);
        compress(&mut state, &block);
        offset += 64;
    }

    let remaining = data.len() - offset;
    let mut block = [0u8; 64];
    block[..remaining].copy_from_slice(&data[offset..]);
    block[remaining] = 0x80;
    if remaining >= 56 {
        compress(&mut state, &block);
        block = [0u8; 64];
    }
    let bit_len = (data.len() as u64).wrapping_mul(8);
    block[56..].copy_from_slice(&bit_len.to_be_bytes());
    compress(&mut state, &block);

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 for arbitrary key lengths.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut normalized = [0u8; BLOCK];
    if key.len() > BLOCK {
        normalized[..32].copy_from_slice(&sha256(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= normalized[i];
        opad[i] ^= normalized[i];
    }

    let mut inner = Vec::with_capacity(BLOCK + message.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(message);
    let inner_hash = sha256(&inner);

    let mut outer = vec![0u8; BLOCK + inner_hash.len()];
    outer[..BLOCK].copy_from_slice(&opad);
    outer[BLOCK..].copy_from_slice(&inner_hash);
    sha256(&outer)
}

/// Constant-time comparison for fixed-size authentication tags.
pub fn ct_eq_32(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for i in 0..32 {
        difference |= left[i] ^ right[i];
    }
    difference == 0
}

// ---------------------------------------------------------------------------
// AEAD: ChaCha20-Poly1305 (RFC 8439)
// ---------------------------------------------------------------------------

/// Why an authenticated open refused. AEAD cannot distinguish a wrong key from tampered bytes —
/// the same property the hosted store documents — so both are one refusal and the caller names
/// the object, not the cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AeadError {
    /// The input cannot hold even an authentication tag.
    Truncated,
    /// The tag did not verify. The plaintext is withheld — a failed open yields nothing.
    Authenticate,
}

const CHACHA_CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

fn chacha_quarter(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(7);
}

/// One ChaCha20 block function (RFC 8439 section 2.3): the 64-byte keystream block for a counter.
pub fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; 64] {
    let mut k = [0u32; 8];
    for i in 0..8 {
        k[i] = u32::from_le_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
    }
    let mut n = [0u32; 3];
    for i in 0..3 {
        n[i] = u32::from_le_bytes([
            nonce[4 * i],
            nonce[4 * i + 1],
            nonce[4 * i + 2],
            nonce[4 * i + 3],
        ]);
    }
    let initial = [
        CHACHA_CONSTANTS[0],
        CHACHA_CONSTANTS[1],
        CHACHA_CONSTANTS[2],
        CHACHA_CONSTANTS[3],
        k[0],
        k[1],
        k[2],
        k[3],
        k[4],
        k[5],
        k[6],
        k[7],
        counter,
        n[0],
        n[1],
        n[2],
    ];
    let mut st = initial;
    for _ in 0..10 {
        chacha_quarter(&mut st, 0, 4, 8, 12);
        chacha_quarter(&mut st, 1, 5, 9, 13);
        chacha_quarter(&mut st, 2, 6, 10, 14);
        chacha_quarter(&mut st, 3, 7, 11, 15);
        chacha_quarter(&mut st, 0, 5, 10, 15);
        chacha_quarter(&mut st, 1, 6, 11, 12);
        chacha_quarter(&mut st, 2, 7, 8, 13);
        chacha_quarter(&mut st, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        let word = st[i].wrapping_add(initial[i]);
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// Poly1305 over a message under a one-time 32-byte key (RFC 8439 section 2.5): the first half is
/// the clamped multiplier r, the second half the one-time addend s. The accumulator is kept fully
/// reduced modulo p = 2^130 - 5 after every block, which bounds every intermediate by construction.
fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    // The RFC 8439 clamp 0x0FFFFFFC0FFFFFFC0FFFFFFC0FFFFFFF split over the two little-endian
    // limbs: byte 0 stays whole (the low limb ends FFFF), while the top nibble of every
    // 32-bit word and the low two bits of each later word's first byte are cleared.
    let r0 = u64::from_le_bytes(key[0..8].try_into().unwrap()) & 0x0fff_fffc_0fff_ffff;
    let r1 = u64::from_le_bytes(key[8..16].try_into().unwrap()) & 0x0fff_fffc_0fff_fffc;
    let s0 = u64::from_le_bytes(key[16..24].try_into().unwrap());
    let s1 = u64::from_le_bytes(key[24..32].try_into().unwrap());

    // Accumulator h < p < 2^130 as three little-endian 64-bit limbs (h2 is a small carry).
    let (mut h0, mut h1, mut h2) = (0u64, 0u64, 0u64);

    for chunk in msg.chunks(16) {
        // n = chunk || 0x01 as a 17-byte little-endian integer, so n < 2^129.
        let mut blk = [0u8; 17];
        blk[..chunk.len()].copy_from_slice(chunk);
        blk[chunk.len()] = 1;
        let n0 = u64::from_le_bytes(blk[0..8].try_into().unwrap());
        let n1 = u64::from_le_bytes(blk[8..16].try_into().unwrap());

        // h += n, bounded by 2^130 + 2^129 < 2^131. For a FULL 16-byte chunk the appended
        // 0x01 is the 2^128 bit and rides in h2; a SHORT final chunk appends it one byte
        // lower, where it is already inside n1 (or n0) and must NOT be double-counted.
        let (t, c1) = h0.overflowing_add(n0);
        h0 = t;
        let (t, c2) = h1.carrying_add(n1, c1);
        h1 = t;
        h2 += c2 as u64 + if chunk.len() == 16 { 1 } else { 0 };

        // h *= r mod p. With h < 2^131 and r < 2^124 the product fits 256 bits exactly.
        let mut prod = [0u64; 4];
        mul_add_at(&mut prod, 0, h0, r0);
        mul_add_at(&mut prod, 1, h0, r1);
        mul_add_at(&mut prod, 1, h1, r0);
        mul_add_at(&mut prod, 2, h1, r1);
        mul_add_at(&mut prod, 2, h2, r0);
        mul_add_at(&mut prod, 3, h2, r1);

        // Reduce mod 2^130 - 5 in limb arithmetic: P = A + B*2^130 with A the low 130 bits of P
        // and B = P >> 130 (< 2^126). Since 2^130 == 5 (mod p), P == A + 5B, and
        // A + 5B < 2^130 + 5*2^126 < 2p, so ONE conditional subtract finishes canonically.
        // Bit 130 sits in prod[2] bit 2.
        let a = (prod[0], prod[1], prod[2] & 0b11);
        let b0 = (prod[2] >> 2) | (prod[3] << 62);
        let b1 = prod[3] >> 2;
        // 5B = (B << 2) + B, held as three limbs. The two-bit barrel shift moves b0's top
        // bits INTO s1 — forgetting that shift-in is exactly how a silent 2^65 goes missing.
        let s0 = b0.wrapping_shl(2);
        let s1 = b1.wrapping_shl(2) | (b0 >> 62);
        let c2 = b1 >> 62;
        let (t0, c3) = s0.overflowing_add(b0);
        let (t1, c4) = s1.carrying_add(b1, c3);
        let t2 = c2 + c4 as u64;
        // A + 5B.
        let (u0, d1) = a.0.overflowing_add(t0);
        let (u1, d2) = a.1.carrying_add(t1, d1);
        let u2 = a.2 + t2 + d2 as u64;
        // Bound: A < 2^130 and 5B < 2^128 give R < 5*2^128 < 2p, so u2 (bits 128.. of R)
        // is at most 4 and ONE conditional subtract yields the canonical representative.
        // One conditional subtract of p = 2^130 - 5 leaves the accumulator fully reduced:
        // p = 0x3FFF_FFFF_FFFF_FFFF_FFFF_FFFF_FFFF_FFFFB, i.e. limbs (...FFFB, ...FFFF, 3).
        const P0: u64 = 0xFFFF_FFFF_FFFF_FFFB;
        const P1: u64 = u64::MAX;
        const P2: u64 = 3;
        // (u2,u1,u0) >= (3, MAX, P0) reduces to this: P1 being the limb maximum makes any
        // strict u1 comparison vacuous, so only the exact-top-limb case remains.
        let ge = u2 > P2 || (u2 == P2 && u1 == P1 && u0 >= P0);
        if ge {
            let (w0, br) = u0.overflowing_sub(P0);
            let (w1, br2) = u1.borrowing_sub(P1, br);
            let (w2, _) = u2.borrowing_sub(P2, br2);
            h0 = w0;
            h1 = w1;
            h2 = w2;
        } else {
            h0 = u0;
            h1 = u1;
            h2 = u2;
        }
    }

    // tag = (h + s) mod 2^128.
    let (t0, carry) = h0.overflowing_add(s0);
    let t1 = h1.wrapping_add(s1).wrapping_add(carry as u64);
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&t0.to_le_bytes());
    out[8..16].copy_from_slice(&t1.to_le_bytes());
    out
}

/// Multiply two 64-bit values and fold the 128-bit product into a 256-bit little-endian
/// accumulator starting at limb `at`, propagating carries all the way up.
fn mul_add_at(prod: &mut [u64; 4], at: usize, x: u64, y: u64) {
    let m = (x as u128) * (y as u128);
    let lo = m as u64;
    let hi = (m >> 64) as u64;
    add_with_carry(prod, at, lo);
    // The high half of a product landing in the top limb would mean P >= 2^256, which the
    // bounds above exclude; the guard keeps a mathematically-impossible carry from panicking.
    if at + 1 < prod.len() {
        add_with_carry(prod, at + 1, hi);
    } else {
        debug_assert_eq!(hi, 0);
    }
}

fn add_with_carry(prod: &mut [u64; 4], at: usize, x: u64) {
    let (t, mut c) = prod[at].overflowing_add(x);
    prod[at] = t;
    let mut j = at + 1;
    while c && j < 4 {
        let (t, c2) = prod[j].overflowing_add(1);
        prod[j] = t;
        c = c2;
        j += 1;
    }
}

/// The MAC data layout RFC 8439 section 2.8 fixes: pad16(AAD) || pad16(ciphertext) ||
/// len(AAD) || len(ciphertext), lengths as little-endian u64.
fn aead_mac_data(aad: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    fn pad16(out: &mut Vec<u8>, data: &[u8]) {
        out.extend_from_slice(data);
        let rem = data.len() % 16;
        if rem != 0 {
            out.extend(core::iter::repeat_n(0u8, 16 - rem));
        }
    }
    let mut mac = Vec::with_capacity(aad.len() + ciphertext.len() + 64);
    pad16(&mut mac, aad);
    pad16(&mut mac, ciphertext);
    mac.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    mac.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    mac
}

/// Authenticated encryption (RFC 8439): returns ciphertext || 16-byte tag. Block 0 of the
/// keystream is the one-time Poly1305 key and encryption starts at block counter 1, so a caller
/// must never reuse (key, nonce) — which is precisely what the constructed nonces upstream
/// guarantee.
pub fn aead_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let poly_block = chacha20_block(key, 0, nonce);
    let mut poly_key = [0u8; 32];
    poly_key.copy_from_slice(&poly_block[..32]);

    let blocks = plaintext.len().div_ceil(64);
    let mut ct = Vec::with_capacity(plaintext.len());
    for i in 0..blocks {
        let ks = chacha20_block(key, 1 + i as u32, nonce);
        let start = i * 64;
        let end = core::cmp::min(start + 64, plaintext.len());
        for (j, b) in plaintext[start..end].iter().enumerate() {
            ct.push(b ^ ks[j]);
        }
    }

    let tag = poly1305(&poly_key, &aead_mac_data(aad, &ct));
    ct.extend_from_slice(&tag);
    ct
}

/// Authenticated decryption: verifies the tag BEFORE releasing any bytes. A failed open yields
/// AeadError::Authenticate and no plaintext — never a decryption of unauthenticated data.
pub fn aead_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, AeadError> {
    if sealed.len() < 16 {
        return Err(AeadError::Truncated);
    }
    let split = sealed.len() - 16;
    let (ct, tag) = sealed.split_at(split);

    let poly_block = chacha20_block(key, 0, nonce);
    let mut poly_key = [0u8; 32];
    poly_key.copy_from_slice(&poly_block[..32]);
    let expected = poly1305(&poly_key, &aead_mac_data(aad, ct));

    let mut supplied = [0u8; 16];
    supplied.copy_from_slice(tag);
    // Constant-time compare of a fixed-size tag.
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= expected[i] ^ supplied[i];
    }
    if diff != 0 {
        return Err(AeadError::Authenticate);
    }

    let blocks = ct.len().div_ceil(64);
    let mut pt = Vec::with_capacity(ct.len());
    for i in 0..blocks {
        let ks = chacha20_block(key, 1 + i as u32, nonce);
        let start = i * 64;
        let end = core::cmp::min(start + 64, ct.len());
        for (j, b) in ct[start..end].iter().enumerate() {
            pt.push(b ^ ks[j]);
        }
    }
    Ok(pt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn hex(bytes: &[u8; 32]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize] as char);
            out.push(DIGITS[(byte & 0xf) as usize] as char);
        }
        out
    }

    #[test]
    fn sha256_matches_standard_vector() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case_one() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn tag_compare_is_exact() {
        let tag = sha256(b"same");
        assert!(ct_eq_32(&tag, &tag));
        let mut changed = tag;
        changed[0] ^= 1;
        assert!(!ct_eq_32(&tag, &changed));
    }
    // ---- RFC 8439 known-answer vectors -------------------------------------------------------

    #[test]
    fn chacha20_block_matches_rfc8439_232() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce = [0u8, 0, 0, 0x09, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let ks = chacha20_block(&key, 1, &nonce);
        let expected: [u8; 64] = [
            0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20,
            0x71, 0xc4, 0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a,
            0xc3, 0xd4, 0x6c, 0x4e, 0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2,
            0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2, 0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9,
            0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
        ];
        assert_eq!(ks, expected);
    }

    #[test]
    fn poly1305_matches_rfc8439_252() {
        let key: [u8; 32] = [
            0x85, 0xd6, 0xbe, 0x78, 0x57, 0x55, 0x6d, 0x33, 0x7f, 0x44, 0x52, 0xfe, 0x42, 0xd5,
            0x06, 0xa8, 0x01, 0x03, 0x80, 0x8a, 0xfb, 0x0d, 0xb2, 0xfd, 0x4a, 0xbf, 0xf6, 0xaf,
            0x41, 0x49, 0xf5, 0x1b,
        ];
        let msg = b"Cryptographic Forum Research Group";
        let tag = poly1305(&key, msg);
        let expected: [u8; 16] = [
            0xa8, 0x06, 0x1d, 0xc1, 0x30, 0x51, 0x36, 0xc6, 0xc2, 0x2b, 0x8b, 0xaf, 0x0c, 0x01,
            0x27, 0xa9,
        ];
        assert_eq!(tag, expected);
    }

    #[test]
    fn aead_matches_rfc8439_282_sunscreen_vector() {
        let key: [u8; 32] = core::array::from_fn(|i| 0x80 + i as u8);
        let nonce: [u8; 12] = [
            0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
        ];
        let aad: [u8; 12] = [
            0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
        ];
        let plaintext = b"Ladies and Gentlemen of the class of \x2799: If I could offer you only one tip for the future, sunscreen would be it.";
        let sealed = aead_seal(&key, &nonce, &aad, plaintext);
        let ct = &sealed[..sealed.len() - 16];
        let tag = &sealed[sealed.len() - 16..];
        assert_eq!(&ct[0..8], &[0xd3, 0x1a, 0x8d, 0x34, 0x64, 0x8e, 0x60, 0xdb]);
        assert_eq!(
            &ct[106..114],
            &[0xd2, 0x65, 0x86, 0xce, 0xc6, 0x4b, 0x61, 0x16]
        );
        assert_eq!(
            tag,
            &[
                0x1a, 0xe1, 0x0b, 0x59, 0x4f, 0x09, 0xe2, 0x6a, 0x7e, 0x90, 0x2e, 0xcb, 0xd0, 0x60,
                0x06, 0x91
            ]
        );
        let opened = aead_open(&key, &nonce, &aad, &sealed).expect("open");
        assert_eq!(opened, plaintext.to_vec());
    }
}
