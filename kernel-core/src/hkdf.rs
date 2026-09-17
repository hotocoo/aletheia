//! HKDF and the TLS 1.3 key schedule (REQ-SEC-TLS-001, ADR-141).
//!
//! Lethe's integration page names TLS 1.3 as stage N2, and a TLS 1.3 implementation is, before
//! anything else, a KEY SCHEDULE: a fixed sequence of extractions and expansions whose order is
//! part of the security argument. Deriving a traffic secret before the handshake secret exists is
//! not "early", it is deriving a key from zeros.
//!
//! So the schedule here is a state machine with named refusals rather than a bag of functions.
//! What it gives out depends on where it is, and asking out of order is a refusal that is counted,
//! never a silently-wrong key.
//!
//! Everything is built on the primitives this kernel already proves at boot
//! ([`crate::crypto::hmac_sha256`]); nothing new is invented at the bottom. Two properties follow
//! the rest of the tree:
//!
//! * **Bounded.** Expansion refuses more than RFC 5869's 255 blocks rather than looping, and
//!   every label and context is bounded by the wire format that carries it.
//! * **No allocation.** Every derivation writes into the caller's buffer or returns a fixed array.
//!
//! The suite proves the primitive against RFC 5869's published vectors, and the TLS-specific
//! layer against the exact byte encoding RFC 8446 §7.1 specifies.

use crate::crypto::hmac_sha256;

/// One HMAC-SHA-256 output, which is also the length of every secret in this schedule.
pub const HASH_LEN: usize = 32;
/// RFC 5869's bound: expansion produces at most 255 blocks.
pub const MAX_EXPAND: usize = 255 * HASH_LEN;
/// The longest label this encoder will write, bounded by the wire format that carries it
/// (`opaque label<7..255>`, of which "tls13 " takes six).
pub const MAX_LABEL: usize = 249;
/// The longest context, bounded the same way (`opaque context<0..255>`).
pub const MAX_CONTEXT: usize = 255;

/// Why a derivation did not happen. Each is a distinct fact; none is "error".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KdfRefusal {
    /// More output than RFC 5869 defines was asked for.
    TooMuchOutput,
    /// The label or context does not fit the wire format that carries it.
    LabelTooLong,
    /// The schedule is not at a stage where this secret exists yet.
    OutOfOrder,
}

/// HKDF-Extract (RFC 5869 §2.2): the salt is the HMAC key, the input keying material is the
/// message. A zero-length salt means a block of zeros, which is what TLS 1.3 uses at the start.
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; HASH_LEN] {
    hmac_sha256(salt, ikm)
}

/// HKDF-Expand (RFC 5869 §2.3) into the caller's buffer.
///
/// Refuses more than 255 blocks by name rather than wrapping the counter: a counter that wraps
/// repeats key material, which is the one failure mode this function must not have.
pub fn hkdf_expand(prk: &[u8; HASH_LEN], info: &[u8], out: &mut [u8]) -> Result<(), KdfRefusal> {
    if out.len() > MAX_EXPAND {
        return Err(KdfRefusal::TooMuchOutput);
    }
    // T(0) is empty; T(n) = HMAC(PRK, T(n-1) || info || n). The block buffer is reused, so a long
    // expansion costs no more memory than a short one.
    let mut t = [0u8; HASH_LEN];
    let mut have_t = false;
    let mut written = 0usize;
    let mut counter: u8 = 1;
    while written < out.len() {
        let mut message = [0u8; HASH_LEN + MAX_LABEL + MAX_CONTEXT + 8];
        let mut n = 0usize;
        if have_t {
            message[..HASH_LEN].copy_from_slice(&t);
            n = HASH_LEN;
        }
        if info.len() > message.len() - n - 1 {
            return Err(KdfRefusal::LabelTooLong);
        }
        message[n..n + info.len()].copy_from_slice(info);
        n += info.len();
        message[n] = counter;
        n += 1;
        t = hmac_sha256(prk, &message[..n]);
        have_t = true;
        let take = (out.len() - written).min(HASH_LEN);
        out[written..written + take].copy_from_slice(&t[..take]);
        written += take;
        counter = counter.wrapping_add(1);
    }
    Ok(())
}

/// Write the `HkdfLabel` structure RFC 8446 §7.1 specifies, exactly:
///
/// ```text
/// struct {
///     uint16 length;
///     opaque label<7..255>   = "tls13 " + Label;
///     opaque context<0..255> = Context;
/// } HkdfLabel;
/// ```
///
/// Exposed rather than hidden inside the expansion, because this encoding IS the domain
/// separation: two protocols that expand the same secret with the same label and different
/// framing derive different keys, and the only way to know which one this kernel writes is to be
/// able to look at the bytes.
pub fn write_hkdf_label(
    length: u16,
    label: &[u8],
    context: &[u8],
    out: &mut [u8],
) -> Result<usize, KdfRefusal> {
    const PREFIX: &[u8] = b"tls13 ";
    if label.len() > MAX_LABEL || context.len() > MAX_CONTEXT {
        return Err(KdfRefusal::LabelTooLong);
    }
    let total = 2 + 1 + PREFIX.len() + label.len() + 1 + context.len();
    if out.len() < total {
        return Err(KdfRefusal::TooMuchOutput);
    }
    out[0..2].copy_from_slice(&length.to_be_bytes());
    out[2] = (PREFIX.len() + label.len()) as u8;
    let mut n = 3;
    out[n..n + PREFIX.len()].copy_from_slice(PREFIX);
    n += PREFIX.len();
    out[n..n + label.len()].copy_from_slice(label);
    n += label.len();
    out[n] = context.len() as u8;
    n += 1;
    out[n..n + context.len()].copy_from_slice(context);
    Ok(total)
}

/// HKDF-Expand-Label (RFC 8446 §7.1).
pub fn hkdf_expand_label(
    secret: &[u8; HASH_LEN],
    label: &[u8],
    context: &[u8],
    out: &mut [u8],
) -> Result<(), KdfRefusal> {
    if out.len() > u16::MAX as usize {
        return Err(KdfRefusal::TooMuchOutput);
    }
    let mut info = [0u8; 2 + 1 + 6 + MAX_LABEL + 1 + MAX_CONTEXT];
    let n = write_hkdf_label(out.len() as u16, label, context, &mut info)?;
    hkdf_expand(secret, &info[..n], out)
}

/// Derive-Secret (RFC 8446 §7.1): expand a secret by label over a transcript hash, producing
/// another secret of the hash's own length.
pub fn derive_secret(
    secret: &[u8; HASH_LEN],
    label: &[u8],
    transcript_hash: &[u8; HASH_LEN],
) -> Result<[u8; HASH_LEN], KdfRefusal> {
    let mut out = [0u8; HASH_LEN];
    hkdf_expand_label(secret, label, transcript_hash, &mut out)?;
    Ok(out)
}

/// Where the schedule is. The order is RFC 8446 §7.1's, and it is enforced rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Only the early secret exists: no key exchange has happened.
    Early,
    /// The shared secret has been mixed in; handshake traffic secrets exist.
    Handshake,
    /// The handshake is finished; application traffic secrets exist.
    Master,
}

/// A TLS 1.3 key schedule over SHA-256.
///
/// It holds one secret at a time and refuses anything that belongs to a stage it has not reached.
/// This is the whole point: a traffic secret derived before the key exchange was mixed in is a key
/// derived from zeros, and it would be indistinguishable from a correct one to everything except
/// the peer.
pub struct KeySchedule {
    stage: Stage,
    secret: [u8; HASH_LEN],
    /// Refusals, counted, like every other refusal in this tree.
    pub refusals: u64,
}

impl KeySchedule {
    /// Start the schedule: Early-Secret = HKDF-Extract(0, PSK). With no pre-shared key the PSK is
    /// a block of zeros, which is what a first connection uses.
    pub fn new(psk: Option<&[u8; HASH_LEN]>) -> Self {
        let zeros = [0u8; HASH_LEN];
        let ikm = psk.unwrap_or(&zeros);
        KeySchedule {
            stage: Stage::Early,
            secret: hkdf_extract(&[0u8; HASH_LEN], ikm),
            refusals: 0,
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The schedule's current secret. Exposed for the suite and for a caller that must store it;
    /// it is not a traffic key and must never be used as one.
    pub fn secret(&self) -> [u8; HASH_LEN] {
        self.secret
    }

    /// Mix the key exchange's shared secret in: Handshake-Secret = HKDF-Extract(Derive-Secret(
    /// Early, "derived", ""), ECDHE). Refuses unless the schedule is at [`Stage::Early`].
    pub fn mix_shared_secret(&mut self, ecdhe: &[u8]) -> Result<(), KdfRefusal> {
        if self.stage != Stage::Early {
            self.refusals += 1;
            return Err(KdfRefusal::OutOfOrder);
        }
        let empty_hash = crate::crypto::sha256(&[]);
        let derived = derive_secret(&self.secret, b"derived", &empty_hash)?;
        self.secret = hkdf_extract(&derived, ecdhe);
        self.stage = Stage::Handshake;
        Ok(())
    }

    /// Finish the handshake: Master-Secret = HKDF-Extract(Derive-Secret(Handshake, "derived", ""),
    /// 0). Refuses unless the schedule is at [`Stage::Handshake`].
    pub fn finish_handshake(&mut self) -> Result<(), KdfRefusal> {
        if self.stage != Stage::Handshake {
            self.refusals += 1;
            return Err(KdfRefusal::OutOfOrder);
        }
        let empty_hash = crate::crypto::sha256(&[]);
        let derived = derive_secret(&self.secret, b"derived", &empty_hash)?;
        self.secret = hkdf_extract(&derived, &[0u8; HASH_LEN]);
        self.stage = Stage::Master;
        Ok(())
    }

    /// The client's and server's handshake traffic secrets, over the transcript so far. Refuses
    /// unless the schedule is at [`Stage::Handshake`].
    pub fn handshake_traffic_secrets(
        &mut self,
        transcript_hash: &[u8; HASH_LEN],
    ) -> Result<([u8; HASH_LEN], [u8; HASH_LEN]), KdfRefusal> {
        if self.stage != Stage::Handshake {
            self.refusals += 1;
            return Err(KdfRefusal::OutOfOrder);
        }
        Ok((
            derive_secret(&self.secret, b"c hs traffic", transcript_hash)?,
            derive_secret(&self.secret, b"s hs traffic", transcript_hash)?,
        ))
    }

    /// The client's and server's application traffic secrets. Refuses unless the schedule is at
    /// [`Stage::Master`].
    pub fn application_traffic_secrets(
        &mut self,
        transcript_hash: &[u8; HASH_LEN],
    ) -> Result<([u8; HASH_LEN], [u8; HASH_LEN]), KdfRefusal> {
        if self.stage != Stage::Master {
            self.refusals += 1;
            return Err(KdfRefusal::OutOfOrder);
        }
        Ok((
            derive_secret(&self.secret, b"c ap traffic", transcript_hash)?,
            derive_secret(&self.secret, b"s ap traffic", transcript_hash)?,
        ))
    }
}

/// A record-protection key and nonce derived from one traffic secret (RFC 8446 §7.3). The sizes
/// are ChaCha20-Poly1305's, which is the AEAD this kernel already proves at boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrafficKeys {
    pub key: [u8; 32],
    pub iv: [u8; 12],
}

/// Derive the record-protection key and IV from a traffic secret.
pub fn traffic_keys(secret: &[u8; HASH_LEN]) -> Result<TrafficKeys, KdfRefusal> {
    let mut key = [0u8; 32];
    let mut iv = [0u8; 12];
    hkdf_expand_label(secret, b"key", &[], &mut key)?;
    hkdf_expand_label(secret, b"iv", &[], &mut iv)?;
    Ok(TrafficKeys { key, iv })
}

/// The per-record nonce: the IV exclusive-ORed with the record's sequence number, right aligned
/// (RFC 8446 §5.3). A nonce reused across two records under one key destroys the AEAD's
/// guarantees entirely, which is why this is one function rather than a comment.
pub fn record_nonce(iv: &[u8; 12], sequence: u64) -> [u8; 12] {
    let mut nonce = *iv;
    let seq = sequence.to_be_bytes();
    for i in 0..8 {
        nonce[4 + i] ^= seq[i];
    }
    nonce
}

/// The key-derivation contract, proved on every CPU at boot.
pub fn hkdf_suite(
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

    // 1 — RFC 5869's first published vector, exactly. A key schedule that agrees with itself and
    //     with nobody else derives keys no peer can reproduce.
    {
        let ikm = [0x0bu8; 22];
        let salt: [u8; 13] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info: [u8; 10] = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
        let prk = hkdf_extract(&salt, &ikm);
        let want_prk: [u8; 32] = [
            0x07, 0x77, 0x09, 0x36, 0x2c, 0x2e, 0x32, 0xdf, 0x0d, 0xdc, 0x3f, 0x0d, 0xc4, 0x7b,
            0xba, 0x63, 0x90, 0xb6, 0xc7, 0x3b, 0xb5, 0x0f, 0x9c, 0x31, 0x22, 0xec, 0x84, 0x4a,
            0xd7, 0xc2, 0xb3, 0xe5,
        ];
        let mut okm = [0u8; 42];
        let expanded = hkdf_expand(&prk, &info, &mut okm).is_ok();
        let want_okm: [u8; 42] = [
            0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36,
            0x2f, 0x2a, 0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56,
            0xec, 0xc4, 0xc5, 0xbf, 0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
        ];
        check!(
            prk == want_prk && expanded && okm == want_okm,
            "hkdf: extraction and expansion match RFC 5869's published vector"
        );
    }

    // 2 — the same function with a zero-length salt and no info (RFC 5869's third vector shape),
    //     because TLS 1.3 starts exactly there and an implementation that special-cases the empty
    //     salt wrongly fails only at the beginning of every connection.
    {
        let prk = hkdf_extract(&[0u8; 32], &[0x0bu8; 22]);
        let want_prk: [u8; 32] = [
            0x19, 0xef, 0x24, 0xa3, 0x2c, 0x71, 0x7b, 0x16, 0x7f, 0x33, 0xa9, 0x1d, 0x6f, 0x64,
            0x8b, 0xdf, 0x96, 0x59, 0x67, 0x76, 0xaf, 0xdb, 0x63, 0x77, 0xac, 0x43, 0x4c, 0x1c,
            0x29, 0x3c, 0xcb, 0x04,
        ];
        let mut okm = [0u8; 42];
        let ok = hkdf_expand(&prk, &[], &mut okm).is_ok();
        let want_head: [u8; 8] = [0x8d, 0xa4, 0xe7, 0x75, 0xa5, 0x63, 0xc1, 0x8f];
        check!(
            prk == want_prk && ok && okm[..8] == want_head,
            "hkdf: an empty salt and empty info derive the published answer, not a special case"
        );
    }

    // 3 — expansion is bounded. RFC 5869 defines 255 blocks; a counter that wraps repeats key
    //     material, so more than that is refused by name rather than produced.
    {
        let prk = hkdf_extract(&[], &[1, 2, 3]);
        let mut small = [0u8; 64];
        let ok_small = hkdf_expand(&prk, b"x", &mut small).is_ok();
        // One byte past the bound, without allocating the whole thing: the check is on the bound.
        let refused =
            hkdf_expand(&prk, b"x", &mut [0u8; MAX_EXPAND + 1]) == Err(KdfRefusal::TooMuchOutput);
        check!(
            ok_small && refused,
            "hkdf: expansion beyond 255 blocks is refused by name, never wrapped"
        );
    }

    // 4 — the label encoding is EXACTLY RFC 8446's, byte for byte. This encoding is the domain
    //     separation between TLS and everything else that expands a secret; getting it wrong
    //     produces keys that are perfectly consistent and useless.
    {
        let mut buf = [0u8; 64];
        let n_written = write_hkdf_label(32, b"key", b"", &mut buf).unwrap_or(0);
        let want: [u8; 12] = [
            0x00, 0x20, // uint16 length = 32
            0x09, // label length = len("tls13 ") + len("key")
            b't', b'l', b's', b'1', b'3', b' ', b'k', b'e', b'y', // "tls13 key"
        ];
        let ok = n_written == want.len() + 1 && buf[..want.len()] == want && buf[want.len()] == 0;
        check!(
            ok,
            "hkdf: the TLS label structure is written exactly as RFC 8446 specifies"
        );
    }

    // 5 — a label or context too long for the wire format is refused rather than truncated. A
    //     truncated label is a DIFFERENT label, and it derives a different key in silence.
    {
        let secret = [7u8; HASH_LEN];
        let long_label = [b'a'; MAX_LABEL + 1];
        let long_context = [0u8; MAX_CONTEXT + 1];
        let mut out = [0u8; 32];
        let l = hkdf_expand_label(&secret, &long_label, &[], &mut out);
        let c = hkdf_expand_label(&secret, b"key", &long_context, &mut out);
        check!(
            l == Err(KdfRefusal::LabelTooLong) && c == Err(KdfRefusal::LabelTooLong),
            "hkdf: a label or context that does not fit the wire format is refused, not truncated"
        );
    }

    // 6 — the schedule is ORDERED. Every secret that belongs to a later stage is refused, and the
    //     refusal is counted: a traffic secret derived before the key exchange is a key derived
    //     from zeros that looks exactly like a correct one.
    {
        let mut ks = KeySchedule::new(None);
        let transcript = crate::crypto::sha256(b"transcript");
        let early_app = ks.application_traffic_secrets(&transcript);
        let early_hs = ks.handshake_traffic_secrets(&transcript);
        let early_finish = ks.finish_handshake();
        check!(
            early_app == Err(KdfRefusal::OutOfOrder)
                && early_hs == Err(KdfRefusal::OutOfOrder)
                && early_finish == Err(KdfRefusal::OutOfOrder)
                && ks.refusals == 3
                && ks.stage() == Stage::Early,
            "hkdf: the schedule refuses every secret that belongs to a stage it has not reached"
        );
    }

    // 7 — the schedule walks Early -> Handshake -> Master, each stage's secrets exist only there,
    //     and the two sides of every pair differ. A schedule that hands the same secret to both
    //     directions has built one key where the protocol requires two.
    {
        let mut ks = KeySchedule::new(None);
        let transcript = crate::crypto::sha256(b"client hello || server hello");
        let mixed = ks.mix_shared_secret(&[0x9fu8; 32]).is_ok();
        let hs = ks.handshake_traffic_secrets(&transcript);
        let finished = ks.finish_handshake().is_ok();
        let app = ks.application_traffic_secrets(&transcript);
        let no_hs_after = ks.handshake_traffic_secrets(&transcript) == Err(KdfRefusal::OutOfOrder);
        let ok = match (hs, app) {
            (Ok((c_hs, s_hs)), Ok((c_ap, s_ap))) => {
                c_hs != s_hs && c_ap != s_ap && c_hs != c_ap && s_hs != s_ap
            }
            _ => false,
        };
        check!(
            mixed && finished && ok && no_hs_after && ks.stage() == Stage::Master,
            "hkdf: the schedule walks its stages and every derived secret is distinct"
        );
    }

    // 8 — traffic keys and the per-record nonce. A nonce reused across two records under one key
    //     destroys the AEAD's guarantees entirely, so the sequence mixing is proved rather than
    //     trusted: every sequence number gives a different nonce, and zero gives the IV itself.
    {
        let secret = [0x33u8; HASH_LEN];
        let keys = traffic_keys(&secret);
        let ok = match keys {
            Ok(k) => {
                let n0 = record_nonce(&k.iv, 0);
                let n1 = record_nonce(&k.iv, 1);
                let n_big = record_nonce(&k.iv, 0x0102_0304_0506_0708);
                // The IV itself at sequence zero, a different nonce at every other sequence, and
                // the sequence lands in the LAST eight bytes.
                n0 == k.iv
                    && n1 != k.iv
                    && n1[11] == k.iv[11] ^ 1
                    && n_big[4] == k.iv[4] ^ 0x01
                    && n_big[..4] == k.iv[..4]
                    && k.key != [0u8; 32]
            }
            Err(_) => false,
        };
        check!(
            ok,
            "hkdf: traffic keys derive, and the record nonce mixes the sequence into the IV"
        );
    }

    // 9 — the same inputs derive the same secrets, and one changed bit of the shared secret
    //     changes everything downstream. Determinism is what lets two machines agree; sensitivity
    //     is what makes the agreement mean something.
    {
        let transcript = crate::crypto::sha256(b"t");
        let derive = |ecdhe: &[u8; 32]| {
            let mut ks = KeySchedule::new(None);
            ks.mix_shared_secret(ecdhe).ok();
            ks.finish_handshake().ok();
            ks.application_traffic_secrets(&transcript)
        };
        let a = derive(&[0x11u8; 32]);
        let b = derive(&[0x11u8; 32]);
        let mut flipped = [0x11u8; 32];
        flipped[31] ^= 0x01;
        let c = derive(&flipped);
        check!(
            a == b && a != c,
            "hkdf: derivation is deterministic, and one flipped bit changes every secret below it"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_derivation_invariant() {
        let mut seen = 0;
        let n = hkdf_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the key-derivation suite should hold");
        assert_eq!(n, 9);
        assert_eq!(seen, 9);
    }

    #[test]
    fn expansion_is_block_wise_and_continuous() {
        // Expanding 100 bytes must produce the same first 32 bytes as expanding 32: the output is
        // one stream, not per-length material.
        let prk = hkdf_extract(b"salt", b"ikm");
        let mut short = [0u8; 32];
        let mut long = [0u8; 100];
        hkdf_expand(&prk, b"info", &mut short).unwrap();
        hkdf_expand(&prk, b"info", &mut long).unwrap();
        assert_eq!(short[..], long[..32]);
    }

    #[test]
    fn every_label_derives_a_different_secret() {
        let secret = [5u8; HASH_LEN];
        let hash = crate::crypto::sha256(b"x");
        let a = derive_secret(&secret, b"c hs traffic", &hash).unwrap();
        let b = derive_secret(&secret, b"s hs traffic", &hash).unwrap();
        let c = derive_secret(&secret, b"c ap traffic", &hash).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }
}
