//! The pinned verifier: what this TLS client trusts, and how it decides (REQ-SEC-TLS-007, ADR-147).
//!
//! ADR-144's handshake takes a [`PeerVerifier`] as a constructor argument and shipped only
//! [`RefuseAllPeers`]. This module is the first verifier that can say yes, and it says yes to
//! exactly one shape: a leaf certificate signed DIRECTLY by one pinned Ed25519 root, speaking for
//! the expected name, inside its validity window at a time the caller supplies.
//!
//! ## Why a pin and not a store
//!
//! Lethe's TLS client validates "against a pinned trust root" (`docs/LETHE-INTEGRATION.md`, N2).
//! A root store is a list of parties allowed to speak for every name; a pin is one party allowed
//! to speak for the names this client will dial. The second is what a browser talking to a small
//! set of services needs, and it removes the whole of path building, name constraints and
//! intermediate-CA handling from the attack surface: there is no chain to walk, so there is no
//! chain-walking bug.
//!
//! ## Why the clock is an argument
//!
//! This kernel has no wall clock yet. A verifier that read "no clock" as "time zero" would find
//! every certificate not yet valid, and one that read it as "skip the check" would accept every
//! expired one. So the time is a constructor argument, a time of zero or less is a refusal, and
//! when the platform grows a clock it will hand a real time here rather than change this code.
//!
//! ## Order of checks
//!
//! The signature is checked FIRST. Nothing in an unsigned document is read as a fact: the validity
//! window and the names are consulted only once the pinned root has been shown to have said them.

use crate::ed25519;
use crate::tlshandshake::PeerVerifier;
use crate::x509::{parse_certificate, DerRefusal};

/// The most certificates this verifier will frame in one Certificate message. The decision rests
/// on the first; the rest are framed so a lying length is refused, and never read. A server sending
/// more than this is not describing a chain this client could use.
pub const MAX_CHAIN: usize = 4;

/// Why a peer was not trusted. Each is a different fact; "verification failed" would hide which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustRefusal {
    /// No usable time was supplied. Zero is not the epoch here; it is the absence of a clock.
    NoClock,
    /// The Certificate message's framing lies about its bytes, or carries a context this client
    /// never asked for.
    BadChain,
    /// The Certificate message carries no certificate at all.
    EmptyChain,
    /// More certificates than this verifier will frame.
    ChainTooLong,
    /// The leaf could not be read, and the reader says why.
    Certificate(DerRefusal),
    /// The leaf's signature does not verify under the pinned root.
    NotSignedByRoot,
    /// The supplied time is before the leaf's notBefore.
    NotYetValid,
    /// The supplied time is after the leaf's notAfter.
    Expired,
    /// The leaf does not speak for the expected name.
    WrongName,
}

/// One pinned Ed25519 root and the time to judge validity windows by.
#[derive(Clone, Copy, Debug)]
pub struct PinnedRoot {
    root: [u8; 32],
    now: i64,
}

impl PinnedRoot {
    /// Pin `root` and judge validity windows at `now` (seconds since the Unix epoch). A time of
    /// zero or less is refused: it is what a platform without a clock would pass, and neither
    /// reading of it is safe.
    pub fn new(root: [u8; 32], now: i64) -> Result<Self, TrustRefusal> {
        if now <= 0 {
            return Err(TrustRefusal::NoClock);
        }
        Ok(PinnedRoot { root, now })
    }

    /// The time this verifier judges by.
    pub fn now(&self) -> i64 {
        self.now
    }

    /// Decide whether `certificates` (a TLS 1.3 Certificate message body, as the server sent it)
    /// speaks for `expected_name`, and hand back the leaf's public key if it does. That key is what
    /// the handshake must check the server's CertificateVerify against.
    pub fn check(
        &self,
        expected_name: &[u8],
        certificates: &[u8],
    ) -> Result<[u8; 32], TrustRefusal> {
        let leaf = leaf_certificate(certificates)?;
        let cert = parse_certificate(leaf).map_err(TrustRefusal::Certificate)?;
        // The signature first. Until the root is shown to have signed this document, its dates
        // and names are claims, not facts.
        if ed25519::verify(&self.root, cert.tbs, &cert.signature).is_err() {
            return Err(TrustRefusal::NotSignedByRoot);
        }
        if self.now < cert.not_before {
            return Err(TrustRefusal::NotYetValid);
        }
        if self.now > cert.not_after {
            return Err(TrustRefusal::Expired);
        }
        if expected_name.is_empty() || !cert.speaks_for(expected_name) {
            return Err(TrustRefusal::WrongName);
        }
        Ok(cert.public_key)
    }
}

impl PeerVerifier for PinnedRoot {
    fn verify(&self, expected_name: &[u8], certificates: &[u8]) -> Option<[u8; 32]> {
        self.check(expected_name, certificates).ok()
    }
}

/// Read a big-endian length of `width` bytes from the front of `bytes`.
fn length(bytes: &[u8], width: usize) -> Result<usize, TrustRefusal> {
    if bytes.len() < width {
        return Err(TrustRefusal::BadChain);
    }
    Ok(bytes[..width]
        .iter()
        .fold(0usize, |acc, &b| (acc << 8) | b as usize))
}

/// Frame a TLS 1.3 Certificate message body (RFC 8446 §4.4.2) and return the first certificate's
/// DER bytes, exactly as they appeared.
///
/// Every length is checked against the bytes that remain before anything is read past it, and the
/// list must fill the message to its last byte: a message with bytes left over is not one this
/// client agrees about with the server. The certificates after the first are framed, so their
/// lengths cannot lie, and their contents are never read.
pub fn leaf_certificate(certificates: &[u8]) -> Result<&[u8], TrustRefusal> {
    // certificate_request_context<0..2^8-1>: for server authentication RFC 8446 says it SHALL be
    // empty. A context this client never sent is a message from a different conversation.
    let context_len = length(certificates, 1)?;
    if context_len != 0 {
        return Err(TrustRefusal::BadChain);
    }
    let rest = &certificates[1..];
    let list_len = length(rest, 3)?;
    let list = &rest[3..];
    if list_len != list.len() {
        return Err(TrustRefusal::BadChain);
    }
    if list.is_empty() {
        return Err(TrustRefusal::EmptyChain);
    }
    let mut leaf: Option<&[u8]> = None;
    let mut count = 0usize;
    let mut cursor = list;
    while !cursor.is_empty() {
        count += 1;
        if count > MAX_CHAIN {
            return Err(TrustRefusal::ChainTooLong);
        }
        // cert_data<1..2^24-1>
        let cert_len = length(cursor, 3)?;
        let after_len = &cursor[3..];
        if cert_len == 0 || cert_len > after_len.len() {
            return Err(TrustRefusal::BadChain);
        }
        let (cert, after_cert) = after_len.split_at(cert_len);
        // extensions<0..2^16-1>
        let ext_len = length(after_cert, 2)?;
        let after_ext_len = &after_cert[2..];
        if ext_len > after_ext_len.len() {
            return Err(TrustRefusal::BadChain);
        }
        if leaf.is_none() {
            leaf = Some(cert);
        }
        cursor = &after_ext_len[ext_len..];
    }
    leaf.ok_or(TrustRefusal::EmptyChain)
}

/// Build a Certificate message body carrying `certificates` in order, with empty extensions and
/// an empty request context. This client never SENDS a Certificate; this exists so the suites and
/// the host tests can present a chain to the verifier exactly as a server would frame it.
pub fn certificate_message(certificates: &[&[u8]], out: &mut [u8]) -> Result<usize, TrustRefusal> {
    let list_len: usize = certificates.iter().map(|c| 3 + c.len() + 2).sum();
    let total = 1 + 3 + list_len;
    if total > out.len() || list_len >= 1 << 24 {
        return Err(TrustRefusal::BadChain);
    }
    out[0] = 0;
    out[1..4].copy_from_slice(&(list_len as u32).to_be_bytes()[1..]);
    let mut at = 4;
    for cert in certificates {
        if cert.is_empty() || cert.len() >= 1 << 24 {
            return Err(TrustRefusal::BadChain);
        }
        out[at..at + 3].copy_from_slice(&(cert.len() as u32).to_be_bytes()[1..]);
        at += 3;
        out[at..at + cert.len()].copy_from_slice(cert);
        at += cert.len();
        out[at..at + 2].copy_from_slice(&[0, 0]);
        at += 2;
    }
    Ok(total)
}

/// A time inside the fixture's validity window: 2027-01-15T06:40:00Z.
pub const FIXTURE_TIME: i64 = 1_800_000_000;
/// The fixture's notBefore, 2026-01-01T00:00:00Z.
pub const FIXTURE_NOT_BEFORE: i64 = 1_767_225_600;
/// The fixture's notAfter, 2036-01-01T00:00:00Z.
pub const FIXTURE_NOT_AFTER: i64 = 2_082_758_400;
/// The name the fixture leaf speaks for.
pub const FIXTURE_NAME: &[u8] = b"aletheia.test";

/// The transcript hash at `WaitCertificateVerify` of the suites' deterministic flight
/// (`tlshandshake::drive_fixture_flight`), and the server CertificateVerify over it signed by
/// `scripts/tls-fixtures.py` with the leaf's private key — a key this kernel does not have.
pub const FIXTURE_TRANSCRIPT_HASH: [u8; 32] = [
    0x58, 0xc9, 0x88, 0xc9, 0x75, 0x12, 0x25, 0xb4, 0xdf, 0x79, 0xf9, 0xe2, 0x81, 0x22, 0x02, 0xbb,
    0xda, 0x8a, 0x71, 0x67, 0x1e, 0x29, 0xa4, 0xcb, 0x35, 0xff, 0x2a, 0xac, 0x33, 0x69, 0xd6, 0xc5,
];
pub const FIXTURE_CERTIFICATE_VERIFY: [u8; 64] = [
    0xe9, 0xde, 0x2a, 0x01, 0xc5, 0x85, 0x5a, 0x44, 0xce, 0x21, 0x53, 0x51, 0x87, 0xbd, 0x4c, 0x50,
    0x64, 0x66, 0x44, 0x4e, 0x3e, 0xb3, 0xfd, 0x2c, 0x68, 0xa5, 0xc5, 0x88, 0xc6, 0x2f, 0x74, 0xa5,
    0xf4, 0x9d, 0x00, 0x36, 0xf0, 0x0a, 0x9f, 0x13, 0xbb, 0xff, 0x6f, 0xfe, 0x70, 0x5d, 0xe7, 0xd4,
    0x9d, 0xbd, 0xe9, 0x79, 0x41, 0x3f, 0xf7, 0x89, 0x75, 0x08, 0xd5, 0x27, 0xc0, 0x12, 0x15, 0x0f,
];

/// A real Ed25519 leaf for `aletheia.test`, issued by a separate root through OpenSSL (Python's
/// `cryptography`), valid 2026-01-01 to 2036-01-01. Parsing and checking something this kernel
/// generated would prove only that it agrees with itself.
pub const LEAF_FIXTURE: [u8; 260] = [
    0x30, 0x82, 0x01, 0x00, 0x30, 0x81, 0xb3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x02, 0x20, 0x02,
    0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x30, 0x1d, 0x31, 0x1b, 0x30, 0x19, 0x06, 0x03, 0x55,
    0x04, 0x03, 0x0c, 0x12, 0x41, 0x6c, 0x65, 0x74, 0x68, 0x65, 0x69, 0x61, 0x20, 0x54, 0x65, 0x73,
    0x74, 0x20, 0x52, 0x6f, 0x6f, 0x74, 0x30, 0x1e, 0x17, 0x0d, 0x32, 0x36, 0x30, 0x31, 0x30, 0x31,
    0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0x17, 0x0d, 0x33, 0x36, 0x30, 0x31, 0x30, 0x31, 0x30,
    0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0x30, 0x18, 0x31, 0x16, 0x30, 0x14, 0x06, 0x03, 0x55, 0x04,
    0x03, 0x0c, 0x0d, 0x61, 0x6c, 0x65, 0x74, 0x68, 0x65, 0x69, 0x61, 0x2e, 0x74, 0x65, 0x73, 0x74,
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00, 0xa0, 0x9a, 0xa5, 0xf4,
    0x7a, 0x67, 0x59, 0x80, 0x2f, 0xf9, 0x55, 0xf8, 0xdc, 0x2d, 0x2a, 0x14, 0xa5, 0xc9, 0x9d, 0x23,
    0xbe, 0x97, 0xf8, 0x64, 0x12, 0x7f, 0xf9, 0x38, 0x34, 0x55, 0xa4, 0xf0, 0xa3, 0x1c, 0x30, 0x1a,
    0x30, 0x18, 0x06, 0x03, 0x55, 0x1d, 0x11, 0x04, 0x11, 0x30, 0x0f, 0x82, 0x0d, 0x61, 0x6c, 0x65,
    0x74, 0x68, 0x65, 0x69, 0x61, 0x2e, 0x74, 0x65, 0x73, 0x74, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65,
    0x70, 0x03, 0x41, 0x00, 0x41, 0x8c, 0x0b, 0x52, 0x24, 0x7f, 0x5e, 0x6f, 0x0d, 0x18, 0xe8, 0x2f,
    0xfe, 0x1e, 0x0d, 0xd0, 0x80, 0x4f, 0xb4, 0xb2, 0xd3, 0xff, 0x6b, 0xff, 0x9b, 0x5b, 0x14, 0x50,
    0x68, 0xde, 0x2d, 0xa5, 0x23, 0x5e, 0x9e, 0x5b, 0x71, 0xcf, 0xdb, 0x89, 0x61, 0xe9, 0x8b, 0x09,
    0x60, 0xac, 0x0e, 0x8b, 0x8e, 0x55, 0x2d, 0x55, 0x28, 0xea, 0x4a, 0x66, 0x58, 0x44, 0xde, 0x56,
    0x64, 0x7c, 0x3a, 0x04,
];

/// The public key of the root that issued [`LEAF_FIXTURE`]: the pin.
pub const ROOT_KEY_FIXTURE: [u8; 32] = [
    0xd0, 0x4a, 0xb2, 0x32, 0x74, 0x2b, 0xb4, 0xab, 0x3a, 0x13, 0x68, 0xbd, 0x46, 0x15, 0xe4, 0xe6,
    0xd0, 0x22, 0x4a, 0xb7, 0x1a, 0x01, 0x6b, 0xaf, 0x85, 0x20, 0xa3, 0x32, 0xc9, 0x77, 0x87, 0x37,
];

/// The leaf's own public key, as OpenSSL put it in the certificate.
pub const LEAF_KEY_FIXTURE: [u8; 32] = [
    0xa0, 0x9a, 0xa5, 0xf4, 0x7a, 0x67, 0x59, 0x80, 0x2f, 0xf9, 0x55, 0xf8, 0xdc, 0x2d, 0x2a, 0x14,
    0xa5, 0xc9, 0x9d, 0x23, 0xbe, 0x97, 0xf8, 0x64, 0x12, 0x7f, 0xf9, 0x38, 0x34, 0x55, 0xa4, 0xf0,
];

/// The root's own self-signed certificate. A server may send it after the leaf; this verifier
/// frames it and never reads it.
pub const ROOT_CERTIFICATE_FIXTURE: [u8; 258] = [
    0x30, 0x81, 0xff, 0x30, 0x81, 0xb2, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x02, 0x10, 0x01, 0x30,
    0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x30, 0x1d, 0x31, 0x1b, 0x30, 0x19, 0x06, 0x03, 0x55, 0x04,
    0x03, 0x0c, 0x12, 0x41, 0x6c, 0x65, 0x74, 0x68, 0x65, 0x69, 0x61, 0x20, 0x54, 0x65, 0x73, 0x74,
    0x20, 0x52, 0x6f, 0x6f, 0x74, 0x30, 0x1e, 0x17, 0x0d, 0x32, 0x36, 0x30, 0x31, 0x30, 0x31, 0x30,
    0x30, 0x30, 0x30, 0x30, 0x30, 0x5a, 0x17, 0x0d, 0x33, 0x36, 0x30, 0x31, 0x30, 0x31, 0x30, 0x30,
    0x30, 0x30, 0x30, 0x30, 0x5a, 0x30, 0x1d, 0x31, 0x1b, 0x30, 0x19, 0x06, 0x03, 0x55, 0x04, 0x03,
    0x0c, 0x12, 0x41, 0x6c, 0x65, 0x74, 0x68, 0x65, 0x69, 0x61, 0x20, 0x54, 0x65, 0x73, 0x74, 0x20,
    0x52, 0x6f, 0x6f, 0x74, 0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    0xd0, 0x4a, 0xb2, 0x32, 0x74, 0x2b, 0xb4, 0xab, 0x3a, 0x13, 0x68, 0xbd, 0x46, 0x15, 0xe4, 0xe6,
    0xd0, 0x22, 0x4a, 0xb7, 0x1a, 0x01, 0x6b, 0xaf, 0x85, 0x20, 0xa3, 0x32, 0xc9, 0x77, 0x87, 0x37,
    0xa3, 0x16, 0x30, 0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff, 0x04, 0x08,
    0x30, 0x06, 0x01, 0x01, 0xff, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03,
    0x41, 0x00, 0xff, 0xca, 0x6f, 0xb2, 0x27, 0xdb, 0xfb, 0xf4, 0x35, 0xf6, 0x22, 0x5c, 0xd1, 0x3c,
    0x55, 0xdf, 0x5d, 0x9e, 0x2d, 0xb7, 0x31, 0xc1, 0x85, 0x24, 0xfb, 0xe2, 0x85, 0x21, 0x3f, 0xe7,
    0x0d, 0xf6, 0x2c, 0x3a, 0x2b, 0xe7, 0xcc, 0xa2, 0xb7, 0x7f, 0xf1, 0x65, 0x1f, 0x28, 0xa4, 0xe5,
    0x12, 0x01, 0xe6, 0xc5, 0x27, 0xf4, 0xcf, 0x8f, 0xc5, 0x39, 0x75, 0x32, 0x2e, 0x96, 0x5f, 0xe5,
    0x1e, 0x03,
];

/// The trust contract, proved on every CPU at boot.
pub fn trust_suite(
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

    let mut message = [0u8; 1024];
    let len = certificate_message(&[&LEAF_FIXTURE], &mut message).unwrap_or(0);
    let chain = &message[..len];
    let pinned = PinnedRoot::new(ROOT_KEY_FIXTURE, FIXTURE_TIME);

    // 1 — the shape this verifier says yes to: a leaf the pinned root signed, for the expected
    //     name, inside its window. The key it hands back is the one OpenSSL put in the leaf, and
    //     the verifier this kernel shipped until now still refuses the same chain.
    {
        let ok = match pinned {
            Ok(p) => {
                p.check(FIXTURE_NAME, chain) == Ok(LEAF_KEY_FIXTURE)
                    && p.verify(FIXTURE_NAME, chain) == Some(LEAF_KEY_FIXTURE)
                    && crate::tlshandshake::RefuseAllPeers
                        .verify(FIXTURE_NAME, chain)
                        .is_none()
            }
            Err(_) => false,
        };
        check!(
            ok,
            "trust: a leaf the pinned root signed, for the expected name, in its window, is accepted"
        );
    }

    // 2 — the root's identity is the whole decision. The same leaf under a pin one bit away is
    //     refused as unsigned; nothing else about it is consulted.
    {
        let mut other = ROOT_KEY_FIXTURE;
        other[0] ^= 0x01;
        let ok = match PinnedRoot::new(other, FIXTURE_TIME) {
            Ok(p) => p.check(FIXTURE_NAME, chain) == Err(TrustRefusal::NotSignedByRoot),
            Err(_) => false,
        };
        check!(
            ok,
            "trust: the same leaf under a different pin is refused as unsigned"
        );
    }

    // 3 — a certificate that vouches for itself is not a chain. ADR-146's self-signed fixture
    //     parses and its own signature verifies, and this verifier still refuses it: the question
    //     is never "is this signed" but "did the ROOT sign this".
    {
        let mut self_signed = [0u8; 512];
        let n2 = certificate_message(&[&crate::x509::SELF_SIGNED_FIXTURE], &mut self_signed)
            .unwrap_or(0);
        let ok = match pinned {
            Ok(p) => {
                p.check(FIXTURE_NAME, &self_signed[..n2]) == Err(TrustRefusal::NotSignedByRoot)
            }
            Err(_) => false,
        };
        check!(
            ok,
            "trust: a certificate that vouches for itself is refused rather than believed"
        );
    }

    // 4 — the root signed it, and it is still refused for a name it does not carry. A parent
    //     domain, a subdomain and an empty name are each the wrong name.
    {
        let ok = match pinned {
            Ok(p) => {
                p.check(b"other.test", chain) == Err(TrustRefusal::WrongName)
                    && p.check(b"sub.aletheia.test", chain) == Err(TrustRefusal::WrongName)
                    && p.check(b"", chain) == Err(TrustRefusal::WrongName)
                    && p.check(b"ALETHEIA.TEST", chain) == Ok(LEAF_KEY_FIXTURE)
            }
            Err(_) => false,
        };
        check!(
            ok,
            "trust: a leaf for another name is refused by name even though the root signed it"
        );
    }

    // 5 — the window is judged at the supplied time, both edges inclusive. One second before
    //     notBefore is not yet valid; one second after notAfter is expired.
    {
        let at =
            |t: i64| PinnedRoot::new(ROOT_KEY_FIXTURE, t).map(|p| p.check(FIXTURE_NAME, chain));
        let ok = at(FIXTURE_NOT_BEFORE - 1) == Ok(Err(TrustRefusal::NotYetValid))
            && at(FIXTURE_NOT_BEFORE) == Ok(Ok(LEAF_KEY_FIXTURE))
            && at(FIXTURE_NOT_AFTER) == Ok(Ok(LEAF_KEY_FIXTURE))
            && at(FIXTURE_NOT_AFTER + 1) == Ok(Err(TrustRefusal::Expired));
        check!(
            ok,
            "trust: before notBefore is not yet valid, after notAfter is expired, both edges inclusive"
        );
    }

    // 6 — no verifier exists without a clock. Zero and negative times are refused at
    //     construction, never read as the epoch and never read as "skip the window".
    {
        let ok = matches!(
            PinnedRoot::new(ROOT_KEY_FIXTURE, 0),
            Err(TrustRefusal::NoClock)
        ) && matches!(
            PinnedRoot::new(ROOT_KEY_FIXTURE, -1),
            Err(TrustRefusal::NoClock)
        ) && matches!(
            PinnedRoot::new(ROOT_KEY_FIXTURE, i64::MIN),
            Err(TrustRefusal::NoClock)
        ) && PinnedRoot::new(ROOT_KEY_FIXTURE, 1).is_ok();
        check!(
            ok,
            "trust: a verifier cannot be built without a clock; zero is not the epoch"
        );
    }

    // 7 — one changed byte anywhere the signature covers is refused as unsigned: in the serial
    //     number inside the tbs, and in the signature itself. The host suite sweeps every bit.
    {
        let mut tbs_changed = LEAF_FIXTURE;
        tbs_changed[15] ^= 0x01; // the last byte of the serial number
        let mut sig_changed = LEAF_FIXTURE;
        let last = sig_changed.len() - 1;
        sig_changed[last] ^= 0x01;
        let mut m1 = [0u8; 512];
        let n1 = certificate_message(&[&tbs_changed], &mut m1).unwrap_or(0);
        let mut m2 = [0u8; 512];
        let n2 = certificate_message(&[&sig_changed], &mut m2).unwrap_or(0);
        let ok = match pinned {
            Ok(p) => {
                p.check(FIXTURE_NAME, &m1[..n1]) == Err(TrustRefusal::NotSignedByRoot)
                    && p.check(FIXTURE_NAME, &m2[..n2]) == Err(TrustRefusal::NotSignedByRoot)
            }
            Err(_) => false,
        };
        check!(
            ok,
            "trust: one changed byte in the tbs or the signature is refused as unsigned"
        );
    }

    // 8 — the Certificate message's framing is checked to its last byte. A context this client
    //     never sent, a list length that lies in either direction, a zero-length certificate, an
    //     extensions length past the end, and every truncation are each refused, never read.
    {
        let mut with_context = [0u8; 1024];
        with_context[0] = 1;
        with_context[1] = 0xAA;
        with_context[2..len + 1].copy_from_slice(&chain[1..]);
        let mut list_long = [0u8; 1024];
        list_long[..len].copy_from_slice(chain);
        list_long[3] = list_long[3].wrapping_add(1);
        let mut list_short = [0u8; 1024];
        list_short[..len].copy_from_slice(chain);
        list_short[3] = list_short[3].wrapping_sub(1);
        let zero_cert = [0u8, 0, 0, 5, 0, 0, 0, 0, 0];
        let mut ext_lies = [0u8; 1024];
        ext_lies[..len].copy_from_slice(chain);
        ext_lies[len - 1] = 0x10;
        let mut every_truncation_refused = true;
        for cut in 0..len {
            if leaf_certificate(&chain[..cut]).is_ok() {
                every_truncation_refused = false;
                break;
            }
        }
        let empty = [0u8, 0, 0, 0];
        check!(
            leaf_certificate(&with_context[..len + 1]) == Err(TrustRefusal::BadChain)
                && leaf_certificate(&list_long[..len]) == Err(TrustRefusal::BadChain)
                && leaf_certificate(&list_short[..len]) == Err(TrustRefusal::BadChain)
                && leaf_certificate(&zero_cert) == Err(TrustRefusal::BadChain)
                && leaf_certificate(&ext_lies[..len]) == Err(TrustRefusal::BadChain)
                && leaf_certificate(&empty) == Err(TrustRefusal::EmptyChain)
                && leaf_certificate(&[]) == Err(TrustRefusal::BadChain)
                && every_truncation_refused
                && leaf_certificate(chain) == Ok(&LEAF_FIXTURE[..]),
            "trust: the chain message's framing is checked to its end; a length that lies is refused"
        );
    }

    // 9 — the decision rests on the leaf and the pin. Certificates after the leaf are framed and
    //     never read: the root's own certificate, or bytes that are no certificate at all, change
    //     nothing. More entries than this verifier frames is refused by name.
    {
        let mut with_root = [0u8; 1024];
        let n1 = certificate_message(&[&LEAF_FIXTURE, &ROOT_CERTIFICATE_FIXTURE], &mut with_root)
            .unwrap_or(0);
        let junk = [0xEEu8; 40];
        let mut with_junk = [0u8; 1024];
        let n2 = certificate_message(&[&LEAF_FIXTURE, &junk], &mut with_junk).unwrap_or(0);
        let mut too_many = [0u8; 1024];
        let n3 = certificate_message(&[&LEAF_FIXTURE, &junk, &junk, &junk, &junk], &mut too_many)
            .unwrap_or(0);
        let ok = match pinned {
            Ok(p) => {
                p.check(FIXTURE_NAME, &with_root[..n1]) == Ok(LEAF_KEY_FIXTURE)
                    && p.check(FIXTURE_NAME, &with_junk[..n2]) == Ok(LEAF_KEY_FIXTURE)
                    && p.check(FIXTURE_NAME, &too_many[..n3]) == Err(TrustRefusal::ChainTooLong)
            }
            Err(_) => false,
        };
        check!(
            ok,
            "trust: certificates after the leaf are framed and never read; too many is refused"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_trust_invariant() {
        let mut seen = 0;
        let n = trust_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the trust suite should hold");
        assert_eq!(n, 9);
        assert_eq!(seen, 9);
    }

    #[test]
    fn a_built_message_frames_back_to_the_same_certificates() {
        let a = [0x30u8, 0x01, 0xAA];
        let b = [0x30u8, 0x02, 0xBB, 0xCC];
        let mut out = [0u8; 64];
        let n = certificate_message(&[&a, &b], &mut out).expect("fits");
        assert_eq!(n, 1 + 3 + (3 + 3 + 2) + (3 + 4 + 2));
        assert_eq!(leaf_certificate(&out[..n]), Ok(&a[..]));
        let mut small = [0u8; 8];
        assert_eq!(
            certificate_message(&[&a], &mut small),
            Err(TrustRefusal::BadChain)
        );
    }
}
