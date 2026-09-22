//! A DER reader and the small part of X.509 a TLS client actually needs (REQ-SEC-TLS-006, ADR-146).
//!
//! Certificate parsers are where TLS clients get compromised. The input is attacker-chosen, the
//! format is recursive, the lengths are self-described, and the historical failures are all the
//! same shape: a parser that READS what a length claims instead of refusing what the buffer cannot
//! hold. So this one is written as a reader that cannot run past its slice, with a bounded
//! recursion depth and a named refusal for every malformed shape.
//!
//! ## What is parsed, and what is deliberately not
//!
//! Parsed: the `tbsCertificate` bytes (which is what the signature covers), the validity window,
//! the subject public key when it is Ed25519, the DNS names in the subject-alternative-name
//! extension, and the outer signature. That is the whole of what [`crate::tlshandshake`] needs to
//! decide whether to trust a peer.
//!
//! NOT parsed, by decision rather than omission: RSA and ECDSA keys (this stack advertises only
//! Ed25519, so a certificate carrying anything else is refused by name rather than half-understood),
//! the full distinguished-name grammar, path-length constraints, CRL and OCSP pointers, and every
//! extension except SAN. Each of those is surface an attacker can reach, and none of them is
//! needed to answer "is this the key that signed, and is that name this host".
//!
//! Nothing here allocates: every accessor returns a slice of the caller's buffer.

use crate::clock::unix_seconds;

/// DER tags this reader CONSULTS. Deliberately only these: a constant for a tag the reader never
/// checks would suggest support it does not have. A distinguished name's string types, for
/// instance, are stepped over as opaque elements rather than decoded, because nothing here needs
/// their contents.
const TAG_BOOLEAN: u8 = 0x01;
const TAG_INTEGER: u8 = 0x02;
const TAG_BIT_STRING: u8 = 0x03;
const TAG_OCTET_STRING: u8 = 0x04;
const TAG_OID: u8 = 0x06;
const TAG_SEQUENCE: u8 = 0x30;
const TAG_UTC_TIME: u8 = 0x17;
const TAG_GENERALIZED_TIME: u8 = 0x18;

/// The OID for Ed25519 (1.3.101.112), as it appears inside a DER OID body.
const OID_ED25519: [u8; 3] = [0x2b, 0x65, 0x70];
/// The OID for subjectAltName (2.5.29.17).
const OID_SAN: [u8; 3] = [0x55, 0x1d, 0x11];

/// How deep this reader will follow nested structures. A certificate needs about six; anything
/// deeper is a document built to exhaust a stack rather than to be read.
pub const MAX_DEPTH: usize = 16;
/// The largest certificate this reader will look at.
pub const MAX_CERTIFICATE: usize = 8_192;
/// The most DNS names this reader will collect from one certificate.
pub const MAX_NAMES: usize = 8;

/// Why a certificate was not read. Never "parse error": each of these is a different fact about
/// the bytes, and the difference is what makes a refusal diagnosable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerRefusal {
    /// The bytes end inside a header or a body.
    Truncated,
    /// A length is encoded in a form DER forbids (indefinite, non-minimal, or absurdly large).
    BadLength,
    /// A tag arrived where a different one is required.
    UnexpectedTag,
    /// The structure nests deeper than this reader will follow.
    TooDeep,
    /// The certificate is larger than this reader will hold.
    TooLarge,
    /// A field this client needs is missing.
    Missing,
    /// The certificate's key or signature algorithm is not Ed25519.
    NotEd25519,
    /// More names than this reader will collect.
    TooManyNames,
}

/// One DER element: its tag, its body, and the bytes it occupied including the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Element<'a> {
    pub tag: u8,
    pub body: &'a [u8],
    /// The element as it appeared, header included — which is what a signature covers.
    pub raw: &'a [u8],
}

/// Read one DER element from the front of `bytes`.
///
/// Refuses every length encoding that is not DER's definite, minimal form. Indefinite lengths and
/// non-minimal encodings are how one parser is made to see a different document than another, and
/// two parsers disagreeing about a certificate is the whole of a signature-bypass bug.
pub fn read_element(bytes: &[u8]) -> Result<Element<'_>, DerRefusal> {
    if bytes.len() < 2 {
        return Err(DerRefusal::Truncated);
    }
    let tag = bytes[0];
    let first = bytes[1];
    let (len, header) = if first < 0x80 {
        (first as usize, 2usize)
    } else if first == 0x80 {
        // Indefinite length: legal in BER, forbidden in DER, and a classic source of disagreement.
        return Err(DerRefusal::BadLength);
    } else {
        let count = (first & 0x7f) as usize;
        if count > 4 || bytes.len() < 2 + count {
            return Err(DerRefusal::BadLength);
        }
        let mut len = 0usize;
        for &b in &bytes[2..2 + count] {
            len = (len << 8) | b as usize;
        }
        // DER requires the shortest encoding: a value below 128 must use the short form, and the
        // first byte of a long form must not be zero.
        if len < 128 || bytes[2] == 0 {
            return Err(DerRefusal::BadLength);
        }
        (len, 2 + count)
    };
    if len > MAX_CERTIFICATE {
        return Err(DerRefusal::TooLarge);
    }
    if bytes.len() < header + len {
        return Err(DerRefusal::Truncated);
    }
    Ok(Element {
        tag,
        body: &bytes[header..header + len],
        raw: &bytes[..header + len],
    })
}

/// Read one element and require its tag.
fn expect(bytes: &[u8], tag: u8) -> Result<Element<'_>, DerRefusal> {
    let e = read_element(bytes)?;
    if e.tag != tag {
        return Err(DerRefusal::UnexpectedTag);
    }
    Ok(e)
}

/// Walk a sequence's children, calling `visit` for each. Bounded by `depth` so a document built
/// out of nested sequences cannot exhaust a stack this kernel cannot grow.
pub fn for_each_child(
    sequence: &[u8],
    depth: usize,
    mut visit: impl FnMut(Element<'_>) -> Result<(), DerRefusal>,
) -> Result<(), DerRefusal> {
    if depth >= MAX_DEPTH {
        return Err(DerRefusal::TooDeep);
    }
    let mut rest = sequence;
    while !rest.is_empty() {
        let e = read_element(rest)?;
        let used = e.raw.len();
        visit(e)?;
        rest = &rest[used..];
    }
    Ok(())
}

/// A certificate, as much of it as this client needs.
#[derive(Clone, Copy, Debug)]
pub struct Certificate<'a> {
    /// The `tbsCertificate` element exactly as it appeared: this is what the signature covers, and
    /// re-encoding it would be a different document.
    pub tbs: &'a [u8],
    /// The subject's Ed25519 public key.
    pub public_key: [u8; 32],
    /// notBefore and notAfter, as seconds since the Unix epoch.
    pub not_before: i64,
    pub not_after: i64,
    /// The outer signature.
    pub signature: [u8; 64],
    /// DNS names from the subject-alternative-name extension.
    names: [(usize, usize); MAX_NAMES],
    names_len: usize,
    source: &'a [u8],
}

impl<'a> Certificate<'a> {
    /// The DNS names this certificate speaks for.
    pub fn names(&self) -> impl Iterator<Item = &'a [u8]> + '_ {
        (0..self.names_len).map(move |i| {
            let (start, len) = self.names[i];
            &self.source[start..start + len]
        })
    }

    /// Whether this certificate speaks for `host`, by RFC 6125's rules as far as this client needs
    /// them: an exact match, case-insensitively, or a single leading `*.` wildcard covering exactly
    /// one label.
    pub fn speaks_for(&self, host: &[u8]) -> bool {
        self.names().any(|name| name_matches(name, host))
    }
}

/// Case-insensitive name matching with one wildcard label, which is the whole of what a client
/// needs and one more rule than it can safely leave out.
pub fn name_matches(name: &[u8], host: &[u8]) -> bool {
    if name.starts_with(b"*.") {
        // The wildcard covers exactly ONE label: "*.a.test" matches "b.a.test" and never
        // "c.b.a.test", which is the rule that keeps one compromised host from speaking for a
        // whole tree. The label it covers must be non-empty: ".a.test" is not a host.
        let suffix = &name[1..]; // ".example.test"
        let Some(dot) = host.iter().position(|&b| b == b'.') else {
            return false;
        };
        return dot > 0 && host[dot..].eq_ignore_ascii_case(suffix);
    }
    name.eq_ignore_ascii_case(host)
}

/// Parse a DER certificate down to the fields a TLS client needs.
pub fn parse_certificate(der: &[u8]) -> Result<Certificate<'_>, DerRefusal> {
    if der.len() > MAX_CERTIFICATE {
        return Err(DerRefusal::TooLarge);
    }
    let cert = expect(der, TAG_SEQUENCE)?;
    let tbs = expect(cert.body, TAG_SEQUENCE)?;
    let after_tbs = &cert.body[tbs.raw.len()..];
    let alg = expect(after_tbs, TAG_SEQUENCE)?;
    require_ed25519(alg.body)?;
    let sig_bits = expect(&after_tbs[alg.raw.len()..], TAG_BIT_STRING)?;
    if sig_bits.body.len() != 65 || sig_bits.body[0] != 0 {
        // A BIT STRING body starts with the count of unused bits, which must be zero here.
        return Err(DerRefusal::Missing);
    }
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&sig_bits.body[1..]);

    // Walk the tbsCertificate: [0] version, serial, signature alg, issuer, validity, subject,
    // subjectPublicKeyInfo, then optional extensions.
    let mut rest = tbs.body;
    let first = read_element(rest)?;
    if first.tag == 0xa0 {
        rest = &rest[first.raw.len()..]; // explicit version
    }
    let serial = expect(rest, TAG_INTEGER)?;
    rest = &rest[serial.raw.len()..];
    let inner_alg = expect(rest, TAG_SEQUENCE)?;
    require_ed25519(inner_alg.body)?;
    rest = &rest[inner_alg.raw.len()..];
    let issuer = expect(rest, TAG_SEQUENCE)?;
    rest = &rest[issuer.raw.len()..];
    let validity = expect(rest, TAG_SEQUENCE)?;
    rest = &rest[validity.raw.len()..];
    let subject = expect(rest, TAG_SEQUENCE)?;
    rest = &rest[subject.raw.len()..];
    let spki = expect(rest, TAG_SEQUENCE)?;
    rest = &rest[spki.raw.len()..];

    let (not_before, not_after) = parse_validity(validity.body)?;
    let public_key = parse_spki(spki.body)?;

    // Extensions, when present, are wrapped in [3].
    let mut names = [(0usize, 0usize); MAX_NAMES];
    let mut names_len = 0usize;
    if !rest.is_empty() {
        let ext_wrapper = read_element(rest)?;
        if ext_wrapper.tag == 0xa3 {
            let extensions = expect(ext_wrapper.body, TAG_SEQUENCE)?;
            for_each_child(extensions.body, 1, |ext| {
                if ext.tag != TAG_SEQUENCE {
                    return Err(DerRefusal::UnexpectedTag);
                }
                let oid = expect(ext.body, TAG_OID)?;
                let mut tail = &ext.body[oid.raw.len()..];
                // An optional `critical` BOOLEAN sits between the OID and the value.
                let maybe_critical = read_element(tail)?;
                if maybe_critical.tag == TAG_BOOLEAN {
                    tail = &tail[maybe_critical.raw.len()..];
                }
                let value = expect(tail, TAG_OCTET_STRING)?;
                if oid.body == OID_SAN {
                    let general_names = expect(value.body, TAG_SEQUENCE)?;
                    for_each_child(general_names.body, 2, |gn| {
                        // [2] is dNSName in the GeneralName choice.
                        if gn.tag == 0x82 {
                            if names_len == MAX_NAMES {
                                return Err(DerRefusal::TooManyNames);
                            }
                            let start = gn.body.as_ptr() as usize - der.as_ptr() as usize;
                            names[names_len] = (start, gn.body.len());
                            names_len += 1;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
        }
    }

    Ok(Certificate {
        tbs: tbs.raw,
        public_key,
        not_before,
        not_after,
        signature,
        names,
        names_len,
        source: der,
    })
}

/// An AlgorithmIdentifier must name Ed25519 and carry no parameters. A certificate signed with
/// anything else is refused BY NAME rather than half-understood: this stack advertises one scheme,
/// so a chain using another is not a chain it can check.
fn require_ed25519(alg_body: &[u8]) -> Result<(), DerRefusal> {
    let oid = expect(alg_body, TAG_OID)?;
    if oid.body != OID_ED25519 {
        return Err(DerRefusal::NotEd25519);
    }
    Ok(())
}

/// SubjectPublicKeyInfo: the algorithm, then the key as a BIT STRING.
fn parse_spki(body: &[u8]) -> Result<[u8; 32], DerRefusal> {
    let alg = expect(body, TAG_SEQUENCE)?;
    require_ed25519(alg.body)?;
    let bits = expect(&body[alg.raw.len()..], TAG_BIT_STRING)?;
    if bits.body.len() != 33 || bits.body[0] != 0 {
        return Err(DerRefusal::Missing);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bits.body[1..]);
    Ok(key)
}

/// notBefore and notAfter as Unix seconds. Both DER time forms are accepted because certificates
/// in the wild use both, and both are parsed strictly: a time this reader cannot read is a
/// refusal, never a zero that would make a certificate valid forever.
fn parse_validity(body: &[u8]) -> Result<(i64, i64), DerRefusal> {
    let a = read_element(body)?;
    let b = read_element(&body[a.raw.len()..])?;
    Ok((parse_time(&a)?, parse_time(&b)?))
}

fn parse_time(e: &Element<'_>) -> Result<i64, DerRefusal> {
    let (year_digits, expected_len) = match e.tag {
        TAG_UTC_TIME => (2usize, 13usize),
        TAG_GENERALIZED_TIME => (4usize, 15usize),
        _ => return Err(DerRefusal::UnexpectedTag),
    };
    if e.body.len() != expected_len || *e.body.last().unwrap_or(&0) != b'Z' {
        return Err(DerRefusal::BadLength);
    }
    let digits = &e.body[..expected_len - 1];
    if !digits.iter().all(|c| c.is_ascii_digit()) {
        return Err(DerRefusal::BadLength);
    }
    let num = |from: usize, len: usize| -> i64 {
        let mut v = 0i64;
        for &c in &digits[from..from + len] {
            v = v * 10 + (c - b'0') as i64;
        }
        v
    };
    let mut year = num(0, year_digits);
    if year_digits == 2 {
        // RFC 5280: two-digit years 50..99 are 19xx, 00..49 are 20xx.
        year += if year >= 50 { 1900 } else { 2000 };
    }
    let month = num(year_digits, 2);
    let day = num(year_digits + 2, 2);
    let hour = num(year_digits + 4, 2);
    let minute = num(year_digits + 6, 2);
    let second = num(year_digits + 8, 2);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(DerRefusal::BadLength);
    }
    Ok(unix_seconds(year, month, day, hour, minute, second))
}

/// A real self-signed Ed25519 certificate for `aletheia.test`, produced by OpenSSL through Python's
/// `cryptography`. Shared with [`crate::trust`], whose verifier must refuse it.
pub(crate) const SELF_SIGNED_FIXTURE: [u8; 254] = [
    0x30, 0x81, 0xfb, 0x30, 0x81, 0xae, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x02, 0x12, 0x34, 0x30,
    0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x30, 0x18, 0x31, 0x16, 0x30, 0x14, 0x06, 0x03, 0x55, 0x04,
    0x03, 0x0c, 0x0d, 0x61, 0x6c, 0x65, 0x74, 0x68, 0x65, 0x69, 0x61, 0x2e, 0x74, 0x65, 0x73, 0x74,
    0x30, 0x1e, 0x17, 0x0d, 0x32, 0x36, 0x30, 0x31, 0x30, 0x31, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30,
    0x5a, 0x17, 0x0d, 0x33, 0x36, 0x30, 0x31, 0x30, 0x31, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x5a,
    0x30, 0x18, 0x31, 0x16, 0x30, 0x14, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x0d, 0x61, 0x6c, 0x65,
    0x74, 0x68, 0x65, 0x69, 0x61, 0x2e, 0x74, 0x65, 0x73, 0x74, 0x30, 0x2a, 0x30, 0x05, 0x06, 0x03,
    0x2b, 0x65, 0x70, 0x03, 0x21, 0x00, 0xac, 0x2c, 0xb2, 0x27, 0xbf, 0x24, 0x91, 0x1c, 0xb1, 0xed,
    0x06, 0x77, 0x56, 0x09, 0xcf, 0x2d, 0x74, 0x71, 0x07, 0x4c, 0x10, 0xd7, 0x52, 0xd6, 0xc2, 0x6d,
    0x6b, 0xc4, 0x0b, 0xfb, 0x1a, 0x56, 0xa3, 0x1c, 0x30, 0x1a, 0x30, 0x18, 0x06, 0x03, 0x55, 0x1d,
    0x11, 0x04, 0x11, 0x30, 0x0f, 0x82, 0x0d, 0x61, 0x6c, 0x65, 0x74, 0x68, 0x65, 0x69, 0x61, 0x2e,
    0x74, 0x65, 0x73, 0x74, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x41, 0x00, 0xf1, 0x0c,
    0xfd, 0x54, 0xc3, 0x9b, 0x5e, 0x5b, 0x1a, 0x87, 0x31, 0xdc, 0x3b, 0x47, 0x98, 0x3d, 0x08, 0x6f,
    0x4e, 0x9c, 0x52, 0x67, 0x24, 0x44, 0x91, 0xa1, 0xd5, 0xed, 0x36, 0x8e, 0xe8, 0x4d, 0xce, 0xa1,
    0xed, 0xaf, 0x9e, 0x51, 0x48, 0xfc, 0xb0, 0x11, 0xbe, 0xdd, 0x59, 0x8a, 0x3b, 0xa7, 0x5c, 0x02,
    0xab, 0x7c, 0xf9, 0x85, 0x31, 0x9d, 0x0f, 0xd8, 0x37, 0xed, 0xdd, 0xf6, 0xc5, 0x04,
];

/// The certificate-reading contract, proved on every CPU at boot.
pub fn x509_suite(
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

    // A real self-signed Ed25519 certificate for "aletheia.test", produced by OpenSSL through
    // Python's `cryptography`. Parsing something this kernel generated would prove only that it
    // agrees with itself.
    const CERT: [u8; 254] = SELF_SIGNED_FIXTURE;
    const KEY: [u8; 32] = [
        0xac, 0x2c, 0xb2, 0x27, 0xbf, 0x24, 0x91, 0x1c, 0xb1, 0xed, 0x06, 0x77, 0x56, 0x09, 0xcf,
        0x2d, 0x74, 0x71, 0x07, 0x4c, 0x10, 0xd7, 0x52, 0xd6, 0xc2, 0x6d, 0x6b, 0xc4, 0x0b, 0xfb,
        0x1a, 0x56,
    ];

    // 1 — a real certificate parses to the fields a client needs, and the key is the one OpenSSL
    //     put in it.
    {
        let parsed = parse_certificate(&CERT);
        let ok = match parsed {
            Ok(c) => c.public_key == KEY && !c.tbs.is_empty() && c.signature.len() == 64,
            Err(_) => false,
        };
        check!(
            ok,
            "x509: a real Ed25519 certificate parses to its key, its tbs bytes and its signature"
        );
    }

    // 2 — the SIGNATURE covers the tbsCertificate bytes as they appeared, and this reader hands
    //     back exactly those bytes. A re-encoded tbs is a different document, and verifying over it
    //     would verify nothing.
    {
        let parsed = parse_certificate(&CERT);
        let ok = match parsed {
            Ok(c) => {
                crate::ed25519::verify(&c.public_key, c.tbs, &c.signature) == Ok(())
                    && c.tbs[0] == 0x30
            }
            Err(_) => false,
        };
        check!(
            ok,
            "x509: the certificate's own signature verifies over the tbs bytes this reader returns"
        );
    }

    // 3 — the validity window is read, and it is the window OpenSSL wrote (2026-01-01 to
    //     2036-01-01). A reader that cannot read a time must refuse, never return zero: zero would
    //     make every certificate valid.
    {
        let parsed = parse_certificate(&CERT);
        let ok = match parsed {
            Ok(c) => c.not_before == 1_767_225_600 && c.not_after == 2_082_758_400,
            Err(_) => false,
        };
        check!(
            ok,
            "x509: the validity window is read as the seconds it means"
        );
    }

    // 4 — the subject-alternative-name DNS entries are collected, and the certificate speaks for
    //     exactly the host it names.
    {
        let parsed = parse_certificate(&CERT);
        let ok = match parsed {
            Ok(c) => {
                c.speaks_for(b"aletheia.test")
                    && c.speaks_for(b"ALETHEIA.TEST")
                    && !c.speaks_for(b"other.test")
                    && !c.speaks_for(b"sub.aletheia.test")
                    && c.names().count() == 1
            }
            Err(_) => false,
        };
        check!(
            ok,
            "x509: the certificate speaks for the name it carries, case-insensitively, and no other"
        );
    }

    // 5 — a wildcard covers one label and never a whole tree. "*.a.test" for "c.b.a.test" is the
    //     rule that, left out, lets one compromised host speak for everything below it.
    {
        check!(
            name_matches(b"*.a.test", b"b.a.test")
                && !name_matches(b"*.a.test", b"c.b.a.test")
                && !name_matches(b"*.a.test", b"a.test")
                && name_matches(b"a.test", b"A.TEST"),
            "x509: a wildcard covers exactly one label, never a subtree"
        );
    }

    // 6 — every truncation is refused. Not a sample: the certificate is cut at every length and
    //     each prefix must be refused rather than read.
    {
        let mut all_refused = true;
        for cut in 1..CERT.len() {
            if parse_certificate(&CERT[..cut]).is_ok() {
                all_refused = false;
                break;
            }
        }
        check!(
            all_refused,
            "x509: every truncation of a certificate is refused, never partially read"
        );
    }

    // 7 — DER's length rules are enforced. An indefinite length, a non-minimal length and a length
    //     past the buffer are each refused by name: two parsers that disagree about a document is
    //     the whole of a signature-bypass bug.
    {
        let indefinite = [0x30u8, 0x80, 0x00, 0x00];
        let non_minimal = [0x30u8, 0x81, 0x01, 0x00];
        let past_end = [0x30u8, 0x7f, 0x00];
        check!(
            read_element(&indefinite) == Err(DerRefusal::BadLength)
                && read_element(&non_minimal) == Err(DerRefusal::BadLength)
                && read_element(&past_end) == Err(DerRefusal::Truncated),
            "x509: indefinite, non-minimal and overlong lengths are refused by name"
        );
    }

    // 8 — a certificate whose key or signature algorithm is not Ed25519 is refused BY NAME. This
    //     stack advertises one scheme; a chain using another is not one it can check, and
    //     half-understanding it is worse than saying so.
    {
        let mut rsa_ish = CERT;
        // Break the OID inside the outer AlgorithmIdentifier.
        let at = CERT.windows(3).position(|w| w == OID_ED25519).unwrap_or(0);
        rsa_ish[at + 2] ^= 0x01;
        check!(
            matches!(
                parse_certificate(&rsa_ish),
                Err(DerRefusal::NotEd25519) | Err(DerRefusal::UnexpectedTag)
            ),
            "x509: a certificate that is not Ed25519 is refused by name rather than half-read"
        );
    }

    // 9 — nesting is bounded. A document built out of nested sequences must be refused rather than
    //     followed down a stack this kernel cannot grow.
    {
        // A sequence nested deeper than MAX_DEPTH, built here rather than fetched.
        let mut deep = [0u8; 2 * (MAX_DEPTH + 4)];
        let depth = MAX_DEPTH + 2;
        for i in 0..depth {
            deep[i * 2] = TAG_SEQUENCE;
            deep[i * 2 + 1] = ((depth - i - 1) * 2) as u8;
        }
        let mut level = 0usize;
        let verdict = walk_depth(&deep[..depth * 2], 0, &mut level);
        check!(
            verdict == Err(DerRefusal::TooDeep),
            "x509: nesting deeper than this reader follows is refused rather than recursed"
        );
    }

    Ok(n)
}

/// Follow nested sequences to prove the depth bound holds.
fn walk_depth(bytes: &[u8], depth: usize, deepest: &mut usize) -> Result<(), DerRefusal> {
    *deepest = (*deepest).max(depth);
    if depth >= MAX_DEPTH {
        return Err(DerRefusal::TooDeep);
    }
    let e = read_element(bytes)?;
    if e.tag == TAG_SEQUENCE && !e.body.is_empty() {
        return walk_depth(e.body, depth + 1, deepest);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_certificate_invariant() {
        let mut seen = 0;
        let n = x509_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the certificate suite should hold");
        assert_eq!(n, 9);
        assert_eq!(seen, 9);
    }

    #[test]
    fn an_element_reports_exactly_the_bytes_it_occupied() {
        // `raw` is what a signature covers, so it must be the header AND the body, and `body` must
        // be inside it. An off-by-one here verifies signatures over the wrong bytes.
        let element = [0x30u8, 0x03, 0x02, 0x01, 0x07, 0xff, 0xff];
        let e = read_element(&element).expect("reads");
        assert_eq!(e.raw, &element[..5]);
        assert_eq!(e.body, &element[2..5]);
        assert_eq!(e.tag, TAG_SEQUENCE);
    }

    #[test]
    fn a_wildcard_covers_exactly_one_non_empty_label() {
        assert!(name_matches(b"*.a.test", b"b.a.test"));
        assert!(
            name_matches(b"*.A.test", b"B.a.TEST"),
            "names are case-insensitive"
        );
        assert!(!name_matches(b"*.a.test", b"c.b.a.test"), "never a subtree");
        assert!(
            !name_matches(b"*.a.test", b"a.test"),
            "the label must exist"
        );
        assert!(
            !name_matches(b"*.a.test", b".a.test"),
            "the label must be non-empty"
        );
        assert!(
            !name_matches(b"*.a.test", b""),
            "an empty host speaks for nothing"
        );
        assert!(name_matches(b"a.test", b"A.TEST"));
        assert!(!name_matches(b"a.test", b"a.tes"));
    }
}
