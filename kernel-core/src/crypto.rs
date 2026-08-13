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
    (x >> n) | (x << (32 - n))
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
}
