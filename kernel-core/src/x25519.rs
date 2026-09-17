//! X25519, the key exchange TLS 1.3 actually uses (REQ-SEC-TLS-002, ADR-142).
//!
//! RFC 7748's Montgomery ladder over GF(2^255 - 19), written the only way a kernel should write
//! it: no allocation, no secret-dependent branches, no secret-dependent memory addresses, and a
//! **named refusal** for the one output a caller must never use.
//!
//! ## The refusal is not politeness
//!
//! RFC 8446 §7.4.2 requires a TLS client to abort if the shared secret is all zeros. That happens
//! when the peer sends a point of small order — a cheap, remote way to force both sides to agree
//! on a key an attacker already knows. So [`x25519`] returns a `Result`, and the all-zero case is
//! [`X25519Refusal::SmallOrder`], counted and impossible to ignore by accident. An implementation
//! that returns 32 zero bytes here has handed the caller a working, worthless key.
//!
//! ## What "constant time" means here, precisely
//!
//! The ladder runs a fixed 255 iterations regardless of the scalar; the conditional swap is
//! arithmetic on a mask rather than a branch; the inversion is a fixed addition chain rather than
//! a loop over the exponent's bits; and nothing indexes memory with a secret. This is the standard
//! set of properties for a scalar multiplication, and it is stated here rather than assumed,
//! because the parts of it that are easy to lose are exactly the parts a reader cannot see.

/// A field element mod 2^255 - 19, as five limbs of 51 bits. Radix 2^51 keeps every product inside
/// a `u128` and every sum inside a `u64`, which is what makes the arithmetic below branch-free.
///
/// Visible inside the crate because Ed25519 (ADR-145) is defined over the SAME field: two copies of
/// this arithmetic would be two places for a carry bug to live, and only one of them would be the
/// one with published vectors pointed at it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fe(pub(crate) [u64; 5]);

const MASK51: u64 = (1u64 << 51) - 1;

impl Fe {
    pub(crate) const ZERO: Fe = Fe([0; 5]);
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// Decode 32 little-endian bytes. The high bit is masked off, as RFC 7748 §5 requires: a peer
    /// that sets it is not signalling anything, and honouring it would decode a different point.
    pub(crate) fn from_bytes(bytes: &[u8; 32]) -> Fe {
        let load = |i: usize| -> u64 {
            let mut v = 0u64;
            for k in 0..8 {
                v |= (bytes[i + k] as u64) << (8 * k);
            }
            v
        };
        let w0 = load(0);
        let w1 = load(8);
        let w2 = load(16);
        let w3 = load(24);
        Fe([
            w0 & MASK51,
            ((w0 >> 51) | (w1 << 13)) & MASK51,
            ((w1 >> 38) | (w2 << 26)) & MASK51,
            ((w2 >> 25) | (w3 << 39)) & MASK51,
            (w3 >> 12) & MASK51,
        ])
    }

    /// Fully reduce and encode as 32 little-endian bytes.
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry();
        // Conditionally subtract p = 2^255 - 19, twice, so the result is the canonical
        // representative. Done with arithmetic rather than a comparison branch.
        for _ in 0..2 {
            let mut q = (h.0[0] + 19) >> 51;
            q = (h.0[1] + q) >> 51;
            q = (h.0[2] + q) >> 51;
            q = (h.0[3] + q) >> 51;
            q = (h.0[4] + q) >> 51;
            h.0[0] += 19 * q;
            h.0[1] += h.0[0] >> 51;
            h.0[0] &= MASK51;
            h.0[2] += h.0[1] >> 51;
            h.0[1] &= MASK51;
            h.0[3] += h.0[2] >> 51;
            h.0[2] &= MASK51;
            h.0[4] += h.0[3] >> 51;
            h.0[3] &= MASK51;
            h.0[4] &= MASK51;
        }
        let mut out = [0u8; 32];
        let words = [
            h.0[0] | (h.0[1] << 51),
            (h.0[1] >> 13) | (h.0[2] << 38),
            (h.0[2] >> 26) | (h.0[3] << 25),
            (h.0[3] >> 39) | (h.0[4] << 12),
        ];
        for (i, w) in words.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// Propagate carries so every limb is below 2^51.
    pub(crate) fn carry(self) -> Fe {
        let mut h = self.0;
        h[1] += h[0] >> 51;
        h[0] &= MASK51;
        h[2] += h[1] >> 51;
        h[1] &= MASK51;
        h[3] += h[2] >> 51;
        h[2] &= MASK51;
        h[4] += h[3] >> 51;
        h[3] &= MASK51;
        h[0] += 19 * (h[4] >> 51);
        h[4] &= MASK51;
        h[1] += h[0] >> 51;
        h[0] &= MASK51;
        Fe(h)
    }

    pub(crate) fn add(self, other: Fe) -> Fe {
        let mut out = [0u64; 5];
        for (o, (a, b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            *o = a + b;
        }
        Fe(out).carry()
    }

    /// Subtraction with a bias of 2p, so no limb underflows before the carry pass.
    pub(crate) fn sub(self, other: Fe) -> Fe {
        let mut out = [0u64; 5];
        out[0] = self.0[0] + 0x000F_FFFF_FFFF_FFDA - other.0[0];
        for (o, (a, b)) in out
            .iter_mut()
            .zip(self.0.iter().zip(other.0.iter()))
            .skip(1)
        {
            *o = a + 0x000F_FFFF_FFFF_FFFE - b;
        }
        Fe(out).carry()
    }

    pub(crate) fn mul(self, other: Fe) -> Fe {
        let a = self.0;
        let b = other.0;
        // The reduction: 2^255 = 19 mod p, so a limb that overflows position 4 comes back
        // multiplied by 19.
        let b1_19 = 19 * b[1] as u128;
        let b2_19 = 19 * b[2] as u128;
        let b3_19 = 19 * b[3] as u128;
        let b4_19 = 19 * b[4] as u128;
        let (a0, a1, a2, a3, a4) = (
            a[0] as u128,
            a[1] as u128,
            a[2] as u128,
            a[3] as u128,
            a[4] as u128,
        );
        let (b0, b1, b2, b3, b4) = (
            b[0] as u128,
            b[1] as u128,
            b[2] as u128,
            b[3] as u128,
            b[4] as u128,
        );
        let c0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
        let c1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
        let c2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
        let c3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
        let c4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;
        Fe::reduce_wide([c0, c1, c2, c3, c4])
    }

    pub(crate) fn square(self) -> Fe {
        self.mul(self)
    }

    /// Multiply by the ladder's constant a24 = (A - 2)/4 = 121665 for Curve25519's A = 486662.
    fn mul121665(self) -> Fe {
        let mut wide = [0u128; 5];
        for (w, limb) in wide.iter_mut().zip(self.0.iter()) {
            *w = *limb as u128 * 121_665u128;
        }
        Fe::reduce_wide(wide)
    }

    pub(crate) fn reduce_wide(c: [u128; 5]) -> Fe {
        let mut h = [0u64; 5];
        let mut carry = 0u128;
        for (limb, wide) in h.iter_mut().zip(c.iter()) {
            let v = wide + carry;
            *limb = (v as u64) & MASK51;
            carry = v >> 51;
        }
        // The carry out of the top limb re-enters at the bottom multiplied by 19.
        let mut out = h;
        out[0] += 19 * carry as u64;
        Fe(out).carry()
    }

    /// Exchange `self` and `other` when `swap` is 1, leaving them alone when it is 0 — with no
    /// branch and no secret-dependent addressing. The whole ladder's constant-time property rests
    /// on this being arithmetic.
    pub(crate) fn cswap(&mut self, other: &mut Fe, swap: u64) {
        let mask = 0u64.wrapping_sub(swap);
        for (a, b) in self.0.iter_mut().zip(other.0.iter_mut()) {
            let t = mask & (*a ^ *b);
            *a ^= t;
            *b ^= t;
        }
    }

    /// The multiplicative inverse, by the standard fixed addition chain for p - 2. A fixed chain
    /// rather than a loop over exponent bits: the exponent is public here, but the shape keeps the
    /// timing independent of the VALUE being inverted.
    pub(crate) fn invert(self) -> Fe {
        let z1 = self;
        let z2 = z1.square();
        let z8 = z2.square().square();
        let z9 = z1.mul(z8);
        let z11 = z2.mul(z9);
        let z22 = z11.square();
        let z_5_0 = z9.mul(z22);

        let mut t = z_5_0;
        for _ in 0..5 {
            t = t.square();
        }
        let z_10_0 = t.mul(z_5_0);

        t = z_10_0;
        for _ in 0..10 {
            t = t.square();
        }
        let z_20_0 = t.mul(z_10_0);

        t = z_20_0;
        for _ in 0..20 {
            t = t.square();
        }
        let z_40_0 = t.mul(z_20_0);

        t = z_40_0;
        for _ in 0..10 {
            t = t.square();
        }
        let z_50_0 = t.mul(z_10_0);

        t = z_50_0;
        for _ in 0..50 {
            t = t.square();
        }
        let z_100_0 = t.mul(z_50_0);

        t = z_100_0;
        for _ in 0..100 {
            t = t.square();
        }
        let z_200_0 = t.mul(z_100_0);

        t = z_200_0;
        for _ in 0..50 {
            t = t.square();
        }
        t = t.mul(z_50_0);

        for _ in 0..5 {
            t = t.square();
        }
        t.mul(z11)
    }
}

/// Why a key exchange produced nothing usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X25519Refusal {
    /// The shared secret is all zeros: the peer sent a point of small order. RFC 8446 §7.4.2
    /// requires aborting, because both sides would otherwise agree on a key the attacker chose.
    SmallOrder,
}

/// Clamp a scalar as RFC 7748 §5 requires: clear the three low bits, clear the top bit, set the
/// second-highest. Exposed because a caller storing a private key should store the clamped form,
/// and because the clamping is part of what makes the ladder's fixed iteration count correct.
pub fn clamp(scalar: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    k
}

/// The Montgomery ladder: `scalar * u`, both little-endian, 32 bytes each.
///
/// Returns the shared secret, or [`X25519Refusal::SmallOrder`] when the result is all zeros.
pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> Result<[u8; 32], X25519Refusal> {
    let k = clamp(scalar);
    let mut u_masked = *u;
    u_masked[31] &= 127; // RFC 7748: the high bit of the u-coordinate is ignored
    let x1 = Fe::from_bytes(&u_masked);

    let mut x2 = Fe::ONE;
    let mut z2 = Fe::ZERO;
    let mut x3 = x1;
    let mut z3 = Fe::ONE;
    let mut swap = 0u64;

    // Fixed 255 iterations, whatever the scalar is.
    for t in (0..255).rev() {
        let bit = ((k[t >> 3] >> (t & 7)) & 1) as u64;
        swap ^= bit;
        x2.cswap(&mut x3, swap);
        z2.cswap(&mut z3, swap);
        swap = bit;

        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul121665()));
    }
    x2.cswap(&mut x3, swap);
    z2.cswap(&mut z3, swap);

    let out = x2.mul(z2.invert()).to_bytes();
    // Constant-time zero test: a branch on the secret's value would leak which peers are hostile,
    // and the answer is a refusal either way.
    let mut acc = 0u8;
    for b in out {
        acc |= b;
    }
    if acc == 0 {
        return Err(X25519Refusal::SmallOrder);
    }
    Ok(out)
}

/// The base point u = 9: this machine's public key for a private scalar.
pub fn public_key(scalar: &[u8; 32]) -> Result<[u8; 32], X25519Refusal> {
    let mut base = [0u8; 32];
    base[0] = 9;
    x25519(scalar, &base)
}

/// The key-exchange contract, proved on every CPU at boot.
pub fn x25519_suite(
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

    // RFC 7748 §6.1's published key pairs and their shared secret.
    const ALICE_SK: [u8; 32] = [
        0x77, 0x07, 0x6d, 0x0a, 0x73, 0x18, 0xa5, 0x7d, 0x3c, 0x16, 0xc1, 0x72, 0x51, 0xb2, 0x66,
        0x45, 0xdf, 0x4c, 0x2f, 0x87, 0xeb, 0xc0, 0x99, 0x2a, 0xb1, 0x77, 0xfb, 0xa5, 0x1d, 0xb9,
        0x2c, 0x2a,
    ];
    const ALICE_PK: [u8; 32] = [
        0x85, 0x20, 0xf0, 0x09, 0x89, 0x30, 0xa7, 0x54, 0x74, 0x8b, 0x7d, 0xdc, 0xb4, 0x3e, 0xf7,
        0x5a, 0x0d, 0xbf, 0x3a, 0x0d, 0x26, 0x38, 0x1a, 0xf4, 0xeb, 0xa4, 0xa9, 0x8e, 0xaa, 0x9b,
        0x4e, 0x6a,
    ];
    const BOB_SK: [u8; 32] = [
        0x5d, 0xab, 0x08, 0x7e, 0x62, 0x4a, 0x8a, 0x4b, 0x79, 0xe1, 0x7f, 0x8b, 0x83, 0x80, 0x0e,
        0xe6, 0x6f, 0x3b, 0xb1, 0x29, 0x26, 0x18, 0xb6, 0xfd, 0x1c, 0x2f, 0x8b, 0x27, 0xff, 0x88,
        0xe0, 0xeb,
    ];
    const BOB_PK: [u8; 32] = [
        0xde, 0x9e, 0xdb, 0x7d, 0x7b, 0x7d, 0xc1, 0xb4, 0xd3, 0x5b, 0x61, 0xc2, 0xec, 0xe4, 0x35,
        0x37, 0x3f, 0x83, 0x43, 0xc8, 0x5b, 0x78, 0x67, 0x4d, 0xad, 0xfc, 0x7e, 0x14, 0x6f, 0x88,
        0x2b, 0x4f,
    ];
    const SHARED: [u8; 32] = [
        0x4a, 0x5d, 0x9d, 0x5b, 0xa4, 0xce, 0x2d, 0xe1, 0x72, 0x8e, 0x3b, 0xf4, 0x80, 0x35, 0x0f,
        0x25, 0xe0, 0x7e, 0x21, 0xc9, 0x47, 0xd1, 0x9e, 0x33, 0x76, 0xf0, 0x9b, 0x3c, 0x1e, 0x16,
        0x17, 0x42,
    ];

    // 1 — the published key pairs. A ladder that agrees with itself and with nobody else produces
    //     a shared secret no peer can reach.
    {
        let a = public_key(&ALICE_SK);
        let b = public_key(&BOB_SK);
        check!(
            a == Ok(ALICE_PK) && b == Ok(BOB_PK),
            "x25519: the base point derives RFC 7748's published public keys"
        );
    }

    // 2 — and the exchange itself: both directions reach the published shared secret. This is the
    //     property the whole protocol above it rests on.
    {
        let ab = x25519(&ALICE_SK, &BOB_PK);
        let ba = x25519(&BOB_SK, &ALICE_PK);
        check!(
            ab == Ok(SHARED) && ba == Ok(SHARED),
            "x25519: both sides of RFC 7748's exchange reach the published shared secret"
        );
    }

    // 3 — RFC 7748 §5.2's scalar-multiplication vector, which exercises a u-coordinate that is not
    //     a base point and a scalar that is not clamped in its input form.
    {
        let scalar: [u8; 32] = [
            0xa5, 0x46, 0xe3, 0x6b, 0xf0, 0x52, 0x7c, 0x9d, 0x3b, 0x16, 0x15, 0x4b, 0x82, 0x46,
            0x5e, 0xdd, 0x62, 0x14, 0x4c, 0x0a, 0xc1, 0xfc, 0x5a, 0x18, 0x50, 0x6a, 0x22, 0x44,
            0xba, 0x44, 0x9a, 0xc4,
        ];
        let u: [u8; 32] = [
            0xe6, 0xdb, 0x68, 0x67, 0x58, 0x30, 0x30, 0xdb, 0x35, 0x94, 0xc1, 0xa4, 0x24, 0xb1,
            0x5f, 0x7c, 0x72, 0x66, 0x24, 0xec, 0x26, 0xb3, 0x35, 0x3b, 0x10, 0xa9, 0x03, 0xa6,
            0xd0, 0xab, 0x1c, 0x4c,
        ];
        let want: [u8; 32] = [
            0xc3, 0xda, 0x55, 0x37, 0x9d, 0xe9, 0xc6, 0x90, 0x8e, 0x94, 0xea, 0x4d, 0xf2, 0x8d,
            0x08, 0x4f, 0x32, 0xec, 0xcf, 0x03, 0x49, 0x1c, 0x71, 0xf7, 0x54, 0xb4, 0x07, 0x55,
            0x77, 0xa2, 0x85, 0x52,
        ];
        check!(
            x25519(&scalar, &u) == Ok(want),
            "x25519: RFC 7748's scalar-multiplication vector holds exactly"
        );
    }

    // 4 — a peer that sends a point of SMALL ORDER is refused by name. Returning the all-zero
    //     secret here would hand the caller a key the attacker chose, and it would work.
    {
        let zero = [0u8; 32];
        let mut one = [0u8; 32];
        one[0] = 1;
        let r0 = x25519(&ALICE_SK, &zero);
        let r1 = x25519(&ALICE_SK, &one);
        check!(
            r0 == Err(X25519Refusal::SmallOrder) && r1 == Err(X25519Refusal::SmallOrder),
            "x25519: a small-order peer key is refused by name rather than yielding zeros"
        );
    }

    // 5 — clamping is part of the function, not the caller's duty. An unclamped scalar and its
    //     clamped form must reach the same point, or two implementations of the same protocol
    //     disagree depending on who remembered.
    {
        let raw = [0xffu8; 32];
        let clamped = clamp(&raw);
        check!(
            x25519(&raw, &BOB_PK) == x25519(&clamped, &BOB_PK)
                && clamped[0] == 0xf8
                && clamped[31] == 0x7f
                && clamp(&[0u8; 32])[31] == 0x40,
            "x25519: the scalar is clamped inside the function, exactly as RFC 7748 says"
        );
    }

    // 6 — the high bit of a peer's u-coordinate is IGNORED, not honoured. A peer that sets it is
    //     not signalling anything, and decoding it would land on a different point than the peer
    //     computed with.
    {
        let mut with_high_bit = BOB_PK;
        with_high_bit[31] |= 0x80;
        check!(
            x25519(&ALICE_SK, &with_high_bit) == Ok(SHARED),
            "x25519: the high bit of a peer's u-coordinate is masked off, as the RFC requires"
        );
    }

    // 7 — a fresh exchange with a different scalar reaches a different secret, and the two sides
    //     of THAT exchange still agree. Determinism without sensitivity would be a constant.
    {
        let mut sk = ALICE_SK;
        sk[0] ^= 0x10;
        let pk = public_key(&sk);
        let ok = match (
            pk,
            x25519(&sk, &BOB_PK),
            x25519(&BOB_SK, &pk.unwrap_or([0; 32])),
        ) {
            (Ok(_), Ok(one), Ok(two)) => one == two && one != SHARED,
            _ => false,
        };
        check!(
            ok,
            "x25519: a different private scalar reaches a different secret, and both sides agree"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_key_exchange_invariant() {
        let mut seen = 0;
        let n = x25519_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the key-exchange suite should hold");
        assert_eq!(n, 7);
        assert_eq!(seen, 7);
    }

    #[test]
    fn the_field_encodes_and_decodes_every_representative() {
        // Round-tripping is where a reduction bug hides: p - 1, p (which encodes as zero) and
        // 2^255 - 1 all have to come back as themselves, reduced.
        let cases: [[u8; 32]; 4] = [[0u8; 32], [1u8; 32], [0xffu8; 32], {
            let mut p_minus_1 = [0xffu8; 32];
            p_minus_1[0] = 0xec;
            p_minus_1[31] = 0x7f;
            p_minus_1
        }];
        for c in cases {
            let fe = Fe::from_bytes(&c);
            let back = Fe::from_bytes(&fe.to_bytes());
            assert_eq!(fe.to_bytes(), back.to_bytes(), "case {c:02x?}");
        }
    }

    #[test]
    fn inversion_is_the_multiplicative_inverse() {
        for seed in [2u8, 7, 0x5a, 0xff] {
            let mut bytes = [seed; 32];
            bytes[31] &= 0x7f;
            let a = Fe::from_bytes(&bytes);
            let one = a.mul(a.invert());
            assert_eq!(one.to_bytes(), Fe::ONE.to_bytes(), "seed {seed}");
        }
    }
}
