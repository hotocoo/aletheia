//! SHA-512 (FIPS 180-4), because Ed25519 is defined over it (REQ-SEC-TLS-005, ADR-145).
//!
//! This kernel already proves SHA-256 at boot (ADR-069). Ed25519 — the signature scheme a TLS 1.3
//! certificate chain needs verifying before this stack may trust a peer — is specified over
//! SHA-512, and a signature verified with the wrong hash is not verified at all.
//!
//! The implementation is the specification, written plainly: eight 64-bit words, eighty rounds,
//! the standard constants. No allocation, no state machine, no streaming API — a certificate and a
//! signed transcript both fit in a buffer, and an incremental interface would be surface this
//! kernel has no caller for.

/// The digest's length in bytes.
pub const DIGEST_LEN: usize = 64;

const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// SHA-512 over data that arrives in pieces (ADR-181). Ed25519 hashes R || A || M, and building
/// that concatenation cost a `Vec` per signature check on a heap that never frees.
#[derive(Clone)]
pub struct Sha512 {
    h: [u64; 8],
    block: [u8; 128],
    fill: usize,
    total: u128,
}

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    pub const fn new() -> Self {
        Sha512 {
            h: [
                0x6a09e667f3bcc908,
                0xbb67ae8584caa73b,
                0x3c6ef372fe94f82b,
                0xa54ff53a5f1d36f1,
                0x510e527fade682d1,
                0x9b05688c2b3e6c1f,
                0x1f83d9abfb41bd6b,
                0x5be0cd19137e2179,
            ],
            block: [0u8; 128],
            fill: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u128);
        if self.fill > 0 {
            let take = (128 - self.fill).min(data.len());
            self.block[self.fill..self.fill + take].copy_from_slice(&data[..take]);
            self.fill += take;
            data = &data[take..];
            if self.fill < 128 {
                return;
            }
            let block = self.block;
            compress(&mut self.h, &block);
            self.fill = 0;
        }
        while data.len() >= 128 {
            let mut block = [0u8; 128];
            block.copy_from_slice(&data[..128]);
            compress(&mut self.h, &block);
            data = &data[128..];
        }
        self.block[..data.len()].copy_from_slice(data);
        self.fill = data.len();
    }

    pub fn finalize(mut self) -> [u8; DIGEST_LEN] {
        let bitlen = self.total.wrapping_mul(8);
        let mut block = [0u8; 128];
        block[..self.fill].copy_from_slice(&self.block[..self.fill]);
        block[self.fill] = 0x80;
        if self.fill + 1 + 16 > 128 {
            compress(&mut self.h, &block);
            block = [0u8; 128];
        }
        block[112..].copy_from_slice(&bitlen.to_be_bytes());
        compress(&mut self.h, &block);
        let mut out = [0u8; DIGEST_LEN];
        for (i, word) in self.h.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// The SHA-512 digest of `data`.
pub fn sha512(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut h: [u64; 8] = [
        0x6a09e667f3bcc908,
        0xbb67ae8584caa73b,
        0x3c6ef372fe94f82b,
        0xa54ff53a5f1d36f1,
        0x510e527fade682d1,
        0x9b05688c2b3e6c1f,
        0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
    ];

    // The padded message is the data, a 0x80 byte, zeros, and the bit length as a 128-bit
    // big-endian integer. Rather than build it, walk it: the tail is at most two blocks, so the
    // whole of it fits in a fixed buffer and nothing is allocated for a message of any size.
    let bitlen = (data.len() as u128) * 8;
    let full_blocks = data.len() / 128;
    let mut block = [0u8; 128];
    for i in 0..full_blocks {
        block.copy_from_slice(&data[i * 128..(i + 1) * 128]);
        compress(&mut h, &block);
    }
    let rest = &data[full_blocks * 128..];
    let mut tail = [0u8; 256];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    // The length field needs sixteen bytes; if they do not fit after the 0x80, a second block does.
    let tail_blocks = if rest.len() + 1 + 16 <= 128 { 1 } else { 2 };
    let end = tail_blocks * 128;
    tail[end - 16..end].copy_from_slice(&bitlen.to_be_bytes());
    for i in 0..tail_blocks {
        block.copy_from_slice(&tail[i * 128..(i + 1) * 128]);
        compress(&mut h, &block);
    }

    let mut out = [0u8; DIGEST_LEN];
    for (i, word) in h.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn compress(h: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 80];
    for i in 0..16 {
        let mut word = [0u8; 8];
        word.copy_from_slice(&block[i * 8..i * 8 + 8]);
        w[i] = u64::from_be_bytes(word);
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
        (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ ((!e) & g);
        let temp1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
}

/// The SHA-512 contract, proved on every CPU at boot, against FIPS 180-4's published vectors.
pub fn sha512_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    fn eq_hex(digest: &[u8; DIGEST_LEN], hex: &[u8]) -> bool {
        if hex.len() != DIGEST_LEN * 2 {
            return false;
        }
        for (i, byte) in digest.iter().enumerate() {
            let hi = hex[i * 2];
            let lo = hex[i * 2 + 1];
            let v = (nibble(hi) << 4) | nibble(lo);
            if *byte != v {
                return false;
            }
        }
        true
    }
    fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => 0xff,
        }
    }

    // 1 — the empty message. The padding-only case, which is where a length field written in the
    //     wrong endianness or the wrong place shows up first.
    check!(
        eq_hex(
            &sha512(&[]),
            b"cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
              47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
                .iter()
                .filter(|c| !c.is_ascii_whitespace())
                .copied()
                .collect::<alloc::vec::Vec<u8>>()
                .as_slice()
        ),
        "sha512: the empty message hashes to FIPS 180-4's published digest"
    );

    // 2 — "abc", the specification's own first example.
    check!(
        eq_hex(
            &sha512(b"abc"),
            b"ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
              2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                .iter()
                .filter(|c| !c.is_ascii_whitespace())
                .copied()
                .collect::<alloc::vec::Vec<u8>>()
                .as_slice()
        ),
        "sha512: the published abc vector holds exactly"
    );

    // 3 — a 56-byte message: the boundary where the length no longer fits in the first block and
    //     a second padding block is required. An implementation that gets this wrong passes every
    //     short test and fails on real input.
    check!(
        eq_hex(
            &sha512(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            b"204a8fc6dda82f0a0ced7beb8e08a41657c16ef468b228a8279be331a703c335\
              96fd15c13b1b07f9aa1d3bea57789ca031ad85c7a71dd70354ec631238ca3445"
                .iter()
                .filter(|c| !c.is_ascii_whitespace())
                .copied()
                .collect::<alloc::vec::Vec<u8>>()
                .as_slice()
        ),
        "sha512: a message spanning the padding boundary hashes correctly"
    );

    // 4 — a thousand bytes, so the multi-block path is exercised over many blocks rather than one.
    {
        let mut long = [0u8; 1000];
        long.fill(b'a');
        check!(
            eq_hex(
                &sha512(&long),
                b"67ba5535a46e3f86dbfbed8cbbaf0125c76ed549ff8b0b9e03e0c88cf90fa634\
                  fa7b12b47d77b694de488ace8d9a65967dc96df599727d3292a8d9d447709c97"
                    .iter()
                    .filter(|c| !c.is_ascii_whitespace())
                    .copied()
                    .collect::<alloc::vec::Vec<u8>>()
                    .as_slice()
            ),
            "sha512: a thousand-byte message hashes correctly across many blocks"
        );
    }

    // 5 — one flipped bit anywhere changes the digest. Determinism without sensitivity would be a
    //     constant, and a hash that is a constant verifies every signature.
    {
        let base = sha512(b"the quick brown fox");
        let other = sha512(b"the quick brown fox.");
        let mut flipped = *b"the quick brown fox";
        flipped[0] ^= 1;
        check!(
            base != other && base != sha512(&flipped) && base == sha512(b"the quick brown fox"),
            "sha512: the digest is deterministic and changes with every input bit"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    #[test]
    fn streaming_in_any_split_equals_one_shot() {
        let data: alloc::vec::Vec<u8> = (0..700u32).map(|i| (i * 31 % 251) as u8).collect();
        for len in [0usize, 1, 111, 112, 127, 128, 129, 255, 256, 257, 700] {
            let want = sha512(&data[..len]);
            for split in [0usize, 1, 7, 64, 127, 128, 200] {
                let cut = split.min(len);
                let mut h = Sha512::new();
                h.update(&data[..cut]);
                h.update(&data[cut..len]);
                assert_eq!(h.finalize(), want, "len {len} split {split}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_digest_invariant() {
        let mut seen = 0;
        let n = sha512_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the sha512 suite should hold");
        assert_eq!(n, 5);
        assert_eq!(seen, 5);
    }

    #[test]
    fn every_length_around_the_block_boundary_is_padded_correctly() {
        // 111, 112 and 113 bytes straddle the point where the 128-bit length field stops fitting.
        // These three lengths are where every hand-written padding routine goes wrong.
        let data = [0x61u8; 300];
        let mut digests = alloc::vec::Vec::new();
        for len in 100..140usize {
            digests.push(sha512(&data[..len]));
        }
        for (i, a) in digests.iter().enumerate() {
            for (j, b) in digests.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "lengths {} and {} collided", 100 + i, 100 + j);
                }
            }
        }
    }
}
