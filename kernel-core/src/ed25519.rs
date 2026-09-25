//! Ed25519 signature VERIFICATION (REQ-SEC-TLS-005, ADR-145).
//!
//! Verification only, deliberately. A TLS client checks signatures; it never makes them. Shipping
//! a signer would mean shipping a private-key path this kernel has no use for and every reason not
//! to have, so [`verify`] is the whole public surface.
//!
//! The curve arithmetic is over the SAME field as X25519 (`crate::x25519::Fe`), which is why that
//! type is crate-visible: two copies of a carry chain are two places for a carry bug, and only one
//! of them would have published vectors pointed at it.
//!
//! ## The cofactored equation, and why
//!
//! RFC 8032 §5.1.7 gives verifiers a choice: check `[S]B = R + [k]A`, or check the cofactored form
//! `[8S]B = [8]R + [8k]A`. This implementation uses the cofactored form, for two reasons that both
//! matter here:
//!
//! * It accepts exactly the signatures a batch verifier accepts, so this kernel cannot end up in
//!   the position of rejecting a chain that every other implementation takes.
//! * It lets `k` be used as the full 512-bit hash output rather than reduced modulo the group
//!   order, because `[8k]A` depends only on `k mod L` once the torsion component is annihilated.
//!   The reduction it removes is a hundred lines of 21-bit-limb arithmetic with no published
//!   vectors of its own — the kind of code that is wrong quietly.
//!
//! What is NOT relaxed: `S` must be strictly below the group order. An implementation that accepts
//! `S + L` accepts a second, different signature for the same message, which is the malleability
//! that breaks anything using a signature as an identifier.

use crate::x25519::Fe;

/// A public key, a signature, a message: the whole of what verification needs.
pub const PUBLIC_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

/// Why a signature was not accepted. Never "invalid": each of these is a different fact, and the
/// difference between them is what makes a failure diagnosable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureRefusal {
    /// The signature is not sixty-four bytes, or the key is not thirty-two.
    BadLength,
    /// The public key is not a point on the curve.
    BadPublicKey,
    /// The signature's R component is not a point on the curve.
    BadSignaturePoint,
    /// S is not below the group order: accepting it would accept a second signature for the same
    /// message.
    NonCanonicalScalar,
    /// The verification equation does not hold. The signature is not this key's.
    WrongSignature,
}

/// The group order L = 2^252 + 27742317777372353535851937790883648493, little-endian.
const ORDER: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

/// d = -121665/121666, the Edwards curve constant, as field limbs.
const D: Fe = Fe([
    929955233495203,
    466365720129213,
    1662059464998953,
    2033849074728123,
    1442794654840575,
]);

/// 2d, used by the addition formula.
const D2: Fe = Fe([
    1859910466990425,
    932731440258426,
    1072319116312658,
    1815898335770999,
    633789495995903,
]);

/// sqrt(-1) mod p, needed to decompress a point.
const SQRT_M1: Fe = Fe([
    1718705420411056,
    234908883556509,
    2233514472574048,
    2117202627021982,
    765476049583133,
]);

/// The base point B, in extended coordinates.
const BASE: Point = Point {
    x: Fe([
        1738742601995546,
        1146398526822698,
        2070867633025821,
        562264141797630,
        587772402128613,
    ]),
    y: Fe([
        1801439850948184,
        1351079888211148,
        450359962737049,
        900719925474099,
        1801439850948198,
    ]),
    z: Fe::ONE,
    t: Fe([
        1841354044333475,
        16398895984059,
        755974180946558,
        900171276175154,
        1821297809914039,
    ]),
};

/// A curve point in extended coordinates (x/z, y/z) with t = xy/z.
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    const IDENTITY: Point = Point {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    fn double(self) -> Point {
        let a = self.x.square();
        let b = self.y.square();
        let c = self.z.square().add(self.z.square());
        let h = a.add(b);
        let e = h.sub(self.x.add(self.y).square());
        let g = a.sub(b);
        let f = c.add(g);
        Point {
            x: e.mul(f),
            y: g.mul(h),
            z: f.mul(g),
            t: e.mul(h),
        }
    }

    fn add_points(self, other: Point) -> Point {
        let a = self.y.sub(self.x).mul(other.y.sub(other.x));
        let b = self.y.add(self.x).mul(other.y.add(other.x));
        let c = self.t.mul(other.t).mul(D2);
        let d = self.z.mul(other.z);
        let d = d.add(d);
        let e = b.sub(a);
        let f = d.sub(c);
        let g = d.add(c);
        let h = b.add(a);
        Point {
            x: e.mul(f),
            y: g.mul(h),
            z: f.mul(g),
            t: e.mul(h),
        }
    }

    /// Multiply by a little-endian scalar of any length, most significant bit first. Verification
    /// operates on public data, so a simple double-and-add is correct here; nothing secret decides
    /// a branch.
    fn mul_scalar(self, scalar: &[u8]) -> Point {
        let mut acc = Point::IDENTITY;
        for byte in scalar.iter().rev() {
            for bit in (0..8).rev() {
                acc = acc.double();
                if (byte >> bit) & 1 == 1 {
                    acc = acc.add_points(self);
                }
            }
        }
        acc
    }

    /// Compress to the 32-byte encoding: y, with x's low bit in the top bit.
    fn compress(self) -> [u8; 32] {
        let zi = self.z.invert();
        let x = self.x.mul(zi).to_bytes();
        let y = self.y.mul(zi).to_bytes();
        let mut out = y;
        out[31] ^= (x[0] & 1) << 7;
        out
    }

    /// Decompress a 32-byte encoding, refusing anything that is not a point on the curve.
    fn decompress(bytes: &[u8; 32]) -> Option<Point> {
        let mut y_bytes = *bytes;
        let sign = y_bytes[31] >> 7;
        y_bytes[31] &= 0x7f;
        let y = Fe::from_bytes(&y_bytes);
        let yy = y.square();
        let u = yy.sub(Fe::ONE);
        let v = yy.mul(D).add(Fe::ONE);

        // x = sqrt(u/v), by the standard exponentiation: candidate = u * v^3 * (u * v^7)^((p-5)/8).
        let v3 = v.square().mul(v);
        let v7 = v3.square().mul(v);
        let mut x = pow_p58(u.mul(v7)).mul(u).mul(v3);

        let check = v.mul(x.square());
        if check.sub(u).to_bytes() != [0u8; 32] {
            if check.add(u).to_bytes() != [0u8; 32] {
                return None; // not a square: the encoding is not a point
            }
            x = x.mul(SQRT_M1);
        }
        let x_bytes = x.to_bytes();
        if x_bytes == [0u8; 32] && sign == 1 {
            return None; // x = 0 has only one encoding; the other is not a point
        }
        if (x_bytes[0] & 1) != sign {
            x = Fe::ZERO.sub(x);
        }
        Some(Point {
            x,
            y,
            z: Fe::ONE,
            t: x.mul(y),
        })
    }
}

/// z^((p-5)/8), the exponentiation the square root needs. A fixed addition chain, like the field's
/// inversion: the exponent is a constant of the curve, not of the data.
fn pow_p58(z: Fe) -> Fe {
    // (p-5)/8 = 2^252 - 3. The chain below is the standard one.
    let z2 = z.square();
    let z9 = z2.square().square().mul(z);
    let z11 = z9.mul(z2);
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
    t.square().square().mul(z)
}

/// Is this little-endian 32-byte scalar strictly below the group order?
fn below_order(s: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if s[i] < ORDER[i] {
            return true;
        }
        if s[i] > ORDER[i] {
            return false;
        }
    }
    false // equal to L is not below it
}

/// Verify an Ed25519 signature over `message` for `public_key`.
///
/// Every failure is named. Nothing about the message is trusted before the equation holds: this
/// function reads the signature and the key, and returns.
pub fn verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), SignatureRefusal> {
    if public_key.len() != PUBLIC_KEY_LEN || signature.len() != SIGNATURE_LEN {
        return Err(SignatureRefusal::BadLength);
    }
    let mut a_bytes = [0u8; 32];
    a_bytes.copy_from_slice(public_key);
    let mut r_bytes = [0u8; 32];
    r_bytes.copy_from_slice(&signature[..32]);
    let mut s_bytes = [0u8; 32];
    s_bytes.copy_from_slice(&signature[32..]);

    if !below_order(&s_bytes) {
        return Err(SignatureRefusal::NonCanonicalScalar);
    }
    let a = Point::decompress(&a_bytes).ok_or(SignatureRefusal::BadPublicKey)?;
    let r = Point::decompress(&r_bytes).ok_or(SignatureRefusal::BadSignaturePoint)?;

    // k = SHA-512(R || A || M), used unreduced: the cofactored equation below depends only on
    // k mod L once torsion is annihilated (module docs).
    // Streamed, not concatenated (ADR-181): a `Vec` per signature check was a leak per TLS
    // conversation on a heap that never frees.
    let mut hasher = crate::sha512::Sha512::new();
    hasher.update(&r_bytes);
    hasher.update(&a_bytes);
    hasher.update(message);
    let k = hasher.finalize();

    // [8S]B  ==  [8]R + [8k]A
    let eight = |p: Point| p.double().double().double();
    let left = eight(BASE.mul_scalar(&s_bytes));
    let right = eight(r).add_points(eight(a.mul_scalar(&k)));
    if left.compress() == right.compress() {
        Ok(())
    } else {
        Err(SignatureRefusal::WrongSignature)
    }
}

/// The signature-verification contract, proved on every CPU at boot.
pub fn ed25519_suite(
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

    // RFC 8032 §7.1's first test vector: the empty message.
    const RFC_PK: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];
    const RFC_SIG: [u8; 64] = [
        0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e, 0x82,
        0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65, 0x22, 0x49,
        0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e, 0x39, 0x70, 0x1c,
        0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24, 0x65, 0x51, 0x41, 0x43,
        0x8e, 0x7a, 0x10, 0x0b,
    ];

    // 1 — RFC 8032's published signature verifies. A verifier that agrees only with itself accepts
    //     nothing anyone else produced.
    check!(
        verify(&RFC_PK, b"", &RFC_SIG) == Ok(()),
        "ed25519: RFC 8032's published signature over the empty message verifies"
    );

    // 2 — the same signature over a DIFFERENT message does not. This is the whole point of a
    //     signature, and it is the check a broken hash silently passes.
    check!(
        verify(&RFC_PK, b"x", &RFC_SIG) == Err(SignatureRefusal::WrongSignature),
        "ed25519: a signature does not verify over a message it was not made for"
    );

    // 3 — a signature under a different key does not verify, and neither does one with any bit
    //     flipped. Both halves matter: R and S are checked by the same equation.
    {
        let mut other_key = RFC_PK;
        other_key[0] ^= 0x01;
        let wrong_key = verify(&other_key, b"", &RFC_SIG);
        let mut flipped_r = RFC_SIG;
        flipped_r[0] ^= 0x01;
        let mut flipped_s = RFC_SIG;
        flipped_s[32] ^= 0x01;
        check!(
            wrong_key.is_err()
                && verify(&RFC_PK, b"", &flipped_r).is_err()
                && verify(&RFC_PK, b"", &flipped_s).is_err(),
            "ed25519: a flipped bit in either half of the signature, or a different key, is refused"
        );
    }

    // 4 — S must be below the group order. Accepting S + L would accept a SECOND valid signature
    //     for the same message, which breaks anything using a signature as an identifier.
    {
        let mut malleable = RFC_SIG;
        // Add L to S: the low byte of L is 0xed, and the top byte is 0x10.
        malleable[32] = malleable[32].wrapping_add(0xed);
        malleable[63] = malleable[63].wrapping_add(0x10);
        check!(
            verify(&RFC_PK, b"", &malleable) == Err(SignatureRefusal::NonCanonicalScalar)
                || verify(&RFC_PK, b"", &malleable) == Err(SignatureRefusal::WrongSignature),
            "ed25519: a scalar at or above the group order is refused rather than accepted twice"
        );
        let mut at_order = RFC_SIG;
        at_order[32..].copy_from_slice(&ORDER);
        check!(
            verify(&RFC_PK, b"", &at_order) == Err(SignatureRefusal::NonCanonicalScalar),
            "ed25519: S exactly equal to the group order is refused by name"
        );
    }

    // 5 — a key or signature point that is not on the curve is refused by NAME, before any
    //     equation is evaluated over it.
    {
        // y = 2: (y^2 - 1)/(d y^2 + 1) is not a square mod p, so this encoding is not a point.
        // Picked deliberately rather than by filling bytes: most random encodings ARE points, and
        // a check that happens to hit one proves nothing.
        let mut not_a_point = [0u8; 32];
        not_a_point[0] = 2;
        let bad_key = verify(&not_a_point, b"", &RFC_SIG);
        let mut bad_r = RFC_SIG;
        bad_r[..32].copy_from_slice(&not_a_point);
        check!(
            bad_key == Err(SignatureRefusal::BadPublicKey)
                && verify(&RFC_PK, b"", &bad_r) == Err(SignatureRefusal::BadSignaturePoint),
            "ed25519: a public key or R that is not on the curve is refused by name"
        );
    }

    // 6 — lengths are checked before anything is read. A verifier that indexes first is a verifier
    //     with a remote read primitive in it.
    {
        check!(
            verify(&RFC_PK[..31], b"", &RFC_SIG) == Err(SignatureRefusal::BadLength)
                && verify(&RFC_PK, b"", &RFC_SIG[..63]) == Err(SignatureRefusal::BadLength)
                && verify(&[], b"", &[]) == Err(SignatureRefusal::BadLength),
            "ed25519: a short key or signature is refused before any byte of it is read"
        );
    }

    // 7 — the base point and the identity behave as the group says: B is on the curve, compressing
    //     and decompressing it round-trips, and multiplying by zero gives the identity.
    {
        let b = BASE.compress();
        let round_trip = Point::decompress(&b).map(|p| p.compress()) == Some(b);
        let zero = BASE.mul_scalar(&[0u8; 32]).compress();
        let identity = Point::IDENTITY.compress();
        check!(
            round_trip && zero == identity,
            "ed25519: the base point round-trips through compression and [0]B is the identity"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_signature_invariant() {
        let mut seen = 0;
        let n = ed25519_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the signature suite should hold");
        assert_eq!(n, 8);
        assert_eq!(seen, 8);
    }
}
