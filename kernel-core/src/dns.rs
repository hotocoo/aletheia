//! DNS — a name becomes an address, and the answer is never trust (REQ-NET-007, ADR-176).
//!
//! Until this module the operator typed every peer's address (`trust NAME IP PIN`), because the
//! machine had no way to ask. This is the asking: one A-record query over UDP (RFC 1035), one
//! bounded reader of the answer.
//!
//! ## What an answer is worth
//!
//! Nothing here is authenticated (no DNSSEC). An answer is a claim by whoever answered first, so it
//! is used as a claim: it tells the console where to DIAL. Whom to BELIEVE stays the pin: TLS still
//! verifies the peer's certificate chain against the root the operator named, for the name the
//! operator typed. A spoofed answer can send the console to the wrong machine; that machine cannot
//! pass for the right one. Resolution can cost availability, never authenticity.
//!
//! ## Scope and refusals
//!
//! A query asks for ONE name, type A, class IN, recursion desired. The reader refuses by name: a
//! short buffer, a question where an answer was required, an answer to someone else's question
//! (foreign id, or a question section that is not ours), a truncated answer (TC: this client has
//! no TCP fallback and will not pretend a partial answer is whole), a name the server says does not
//! exist, any other server error, and an answer with no address for our name.
//!
//! Names in the answer may be compressed (RFC 1035 §4.1.4). A pointer must point strictly BACKWARD
//! and a name may take at most `MAX_JUMPS` of them: a pointer loop is an attacker-controlled loop in
//! kernel space, and this bound is what makes it impossible. A CNAME chain is followed inside the
//! answer section, at most `MAX_CNAMES` links. At most `MAX_ADDRS` addresses are kept; the rest are
//! counted, not stored.
//!
//! Not implemented, on purpose: AAAA (the stack is IPv4), TCP fallback, caching, EDNS, DNSSEC,
//! search domains (a name is resolved exactly as typed).

/// The port a DNS server listens on.
pub const PORT: u16 = 53;
/// The longest name a query may carry, in its dotted text form (RFC 1035 §2.3.4: 255 octets on the
/// wire, which is 253 characters of text).
pub const MAX_NAME: usize = 253;
/// The longest label.
pub const MAX_LABEL: usize = 63;
/// Classic DNS over UDP: a message is at most 512 bytes (RFC 1035 §4.2.1).
pub const MAX_MESSAGE: usize = 512;
/// Addresses kept from one answer.
pub const MAX_ADDRS: usize = 4;
/// Compression pointers one name may follow.
pub const MAX_JUMPS: usize = 16;
/// CNAME links followed inside one answer.
pub const MAX_CNAMES: usize = 8;

const HEADER: usize = 12;
const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const CLASS_IN: u16 = 1;
const FLAG_QR: u16 = 0x8000;
const FLAG_TC: u16 = 0x0200;
const FLAG_RD: u16 = 0x0100;
const RCODE_NXDOMAIN: u8 = 3;

/// Why a name cannot be asked, or why these bytes are not an acceptable answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsError {
    /// The name is empty, too long, has an empty or over-long label, or a byte outside
    /// `a-z A-Z 0-9 -`, or a label that starts or ends with `-`.
    BadName,
    /// The output buffer cannot hold the query.
    NoRoom,
    /// Shorter than a header, or a record runs past the end of the message.
    Truncated,
    /// QR is not set: a question came back where an answer was required.
    NotAnAnswer,
    /// The id or the question section is not the one we asked: an answer to someone else.
    NotOurQuestion,
    /// TC is set: the server cut its answer, and this client has no TCP fallback.
    CutByServer,
    /// The server says the name does not exist (NXDOMAIN).
    NoSuchName,
    /// Any other non-zero response code.
    ServerFailure(u8),
    /// A compressed name is malformed: a forward or self pointer, too many jumps, a label past the
    /// end, or a name longer than 255 octets.
    BadCompression,
    /// The answer carries no A record for our name (or the end of its CNAME chain).
    NoAddress,
}

impl DnsError {
    /// The refusal as a sentence the console prints.
    pub fn describe(self) -> &'static str {
        match self {
            DnsError::BadName => "that is not a name this resolver will ask for",
            DnsError::NoRoom => "the query does not fit",
            DnsError::Truncated => "the answer is shorter than it claims",
            DnsError::NotAnAnswer => "the server sent a question back",
            DnsError::NotOurQuestion => "the answer is to someone else's question",
            DnsError::CutByServer => "the server cut its answer short (no TCP fallback here)",
            DnsError::NoSuchName => "the server says that name does not exist",
            DnsError::ServerFailure(_) => "the server failed to answer",
            DnsError::BadCompression => "the answer's names are malformed",
            DnsError::NoAddress => "the answer holds no address for that name",
        }
    }
}

/// What one answer said about one name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The addresses kept, in answer order; only the first `count` are meaningful.
    pub addrs: [[u8; 4]; MAX_ADDRS],
    /// How many addresses were kept.
    pub count: usize,
    /// Addresses the answer held beyond `MAX_ADDRS`: counted, not stored.
    pub dropped: usize,
    /// The smallest TTL among the kept addresses, in seconds.
    pub ttl: u32,
    /// CNAME links followed to reach them.
    pub cnames: usize,
}

impl Resolved {
    pub fn addresses(&self) -> &[[u8; 4]] {
        &self.addrs[..self.count]
    }
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

/// Is `name` a hostname this resolver will ask for? A trailing dot is not accepted: the name is
/// resolved exactly as typed, with no search domains to make it mean something else.
pub fn name_is_askable(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > MAX_NAME {
        return false;
    }
    name.split(|&b| b == b'.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL
            && label
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
            && label[0] != b'-'
            && label[label.len() - 1] != b'-'
    })
}

/// Write a query for `name`'s A records with message id `id` into `out`; returns its length.
pub fn write_query(out: &mut [u8], id: u16, name: &[u8]) -> Result<usize, DnsError> {
    if !name_is_askable(name) {
        return Err(DnsError::BadName);
    }
    let len = HEADER + name.len() + 2 + 4;
    if out.len() < len {
        return Err(DnsError::NoRoom);
    }
    out[..HEADER].fill(0);
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&FLAG_RD.to_be_bytes());
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    let mut at = HEADER;
    for label in name.split(|&b| b == b'.') {
        out[at] = label.len() as u8;
        out[at + 1..at + 1 + label.len()].copy_from_slice(label);
        at += 1 + label.len();
    }
    out[at] = 0;
    at += 1;
    out[at..at + 2].copy_from_slice(&TYPE_A.to_be_bytes());
    out[at + 2..at + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
    Ok(at + 4)
}

/// A name read out of a message, lower-cased, as wire labels (length-prefixed, no terminator).
struct Name {
    buf: [u8; 255],
    len: usize,
}

impl Name {
    fn eq(&self, other: &Name) -> bool {
        self.buf[..self.len] == other.buf[..other.len]
    }
}

/// Read the (possibly compressed) name at `at`; returns the name and the offset just past it in the
/// record that contains it.
fn read_name(msg: &[u8], at: usize) -> Result<(Name, usize), DnsError> {
    let mut name = Name {
        buf: [0; 255],
        len: 0,
    };
    let mut pos = at;
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *msg.get(pos).ok_or(DnsError::Truncated)? as usize;
        match len & 0xC0 {
            0x00 if len == 0 => {
                return Ok((name, end.unwrap_or(pos + 1)));
            }
            0x00 => {
                let label = msg.get(pos + 1..pos + 1 + len).ok_or(DnsError::Truncated)?;
                if name.len + 1 + len > name.buf.len() - 1 {
                    return Err(DnsError::BadCompression);
                }
                name.buf[name.len] = len as u8;
                for (i, &b) in label.iter().enumerate() {
                    name.buf[name.len + 1 + i] = b.to_ascii_lowercase();
                }
                name.len += 1 + len;
                pos += 1 + len;
            }
            0xC0 => {
                let target = (be16(msg, pos).ok_or(DnsError::Truncated)? & 0x3FFF) as usize;
                // Strictly backward, and bounded: together these make a loop impossible.
                if target >= pos || jumps == MAX_JUMPS {
                    return Err(DnsError::BadCompression);
                }
                jumps += 1;
                if end.is_none() {
                    end = Some(pos + 2);
                }
                pos = target;
            }
            // 0x40 and 0x80 are reserved label types.
            _ => return Err(DnsError::BadCompression),
        }
    }
}

/// The wire form of a typed name, for comparison with names read from the answer.
fn wire_name(name: &[u8]) -> Name {
    let mut n = Name {
        buf: [0; 255],
        len: 0,
    };
    for label in name.split(|&b| b == b'.') {
        n.buf[n.len] = label.len() as u8;
        for (i, &b) in label.iter().enumerate() {
            n.buf[n.len + 1 + i] = b.to_ascii_lowercase();
        }
        n.len += 1 + label.len();
    }
    n
}

/// Read the answer to our query for `name` with id `id`.
pub fn parse_answer(msg: &[u8], id: u16, name: &[u8]) -> Result<Resolved, DnsError> {
    if !name_is_askable(name) {
        return Err(DnsError::BadName);
    }
    if msg.len() < HEADER {
        return Err(DnsError::Truncated);
    }
    let flags = be16(msg, 2).ok_or(DnsError::Truncated)?;
    if flags & FLAG_QR == 0 {
        return Err(DnsError::NotAnAnswer);
    }
    if be16(msg, 0) != Some(id) || be16(msg, 4) != Some(1) {
        return Err(DnsError::NotOurQuestion);
    }
    if flags & FLAG_TC != 0 {
        return Err(DnsError::CutByServer);
    }
    match (flags & 0x000F) as u8 {
        0 => {}
        RCODE_NXDOMAIN => return Err(DnsError::NoSuchName),
        other => return Err(DnsError::ServerFailure(other)),
    }
    let answers = be16(msg, 6).ok_or(DnsError::Truncated)? as usize;

    // The question section must be OUR question, not merely carry our id.
    let asked = wire_name(name);
    let (qname, at) = read_name(msg, HEADER)?;
    if !qname.eq(&asked) || be16(msg, at) != Some(TYPE_A) || be16(msg, at + 2) != Some(CLASS_IN) {
        return Err(DnsError::NotOurQuestion);
    }
    let first_record = at + 4;

    // The name whose A records count: ours, then each CNAME target in turn. The answer section is
    // walked once per link, so an out-of-order chain still resolves, and MAX_CNAMES bounds the work.
    let mut want = asked;
    let mut out = Resolved {
        addrs: [[0; 4]; MAX_ADDRS],
        count: 0,
        dropped: 0,
        ttl: u32::MAX,
        cnames: 0,
    };
    loop {
        let mut next: Option<Name> = None;
        let mut at = first_record;
        for _ in 0..answers {
            let (owner, after) = read_name(msg, at)?;
            let rtype = be16(msg, after).ok_or(DnsError::Truncated)?;
            let class = be16(msg, after + 2).ok_or(DnsError::Truncated)?;
            let ttl = be32(msg, after + 4).ok_or(DnsError::Truncated)?;
            let rdlen = be16(msg, after + 8).ok_or(DnsError::Truncated)? as usize;
            let rdata = after + 10;
            if msg.len() < rdata + rdlen {
                return Err(DnsError::Truncated);
            }
            if owner.eq(&want) && class == CLASS_IN {
                if rtype == TYPE_A && rdlen == 4 {
                    if out.count < MAX_ADDRS {
                        out.addrs[out.count].copy_from_slice(&msg[rdata..rdata + 4]);
                        out.count += 1;
                        out.ttl = out.ttl.min(ttl);
                    } else {
                        out.dropped += 1;
                    }
                } else if rtype == TYPE_CNAME && next.is_none() {
                    next = Some(read_name(msg, rdata)?.0);
                }
            }
            at = rdata + rdlen;
        }
        if out.count > 0 {
            return Ok(out);
        }
        match next {
            Some(target) if out.cnames < MAX_CNAMES => {
                out.cnames += 1;
                want = target;
            }
            _ => return Err(DnsError::NoAddress),
        }
    }
}

/// Build an answer, for the suite and the host tests: our question echoed, then `records` as
/// `(owner offset or None for the question name, type, ttl, rdata)`. Owner `None` compresses to the
/// question (offset 12), exactly as real servers do.
#[cfg(test)]
pub fn build_answer_for_tests(
    id: u16,
    name: &[u8],
    rcode: u8,
    records: &[(u16, u32, &[u8])],
) -> alloc::vec::Vec<u8> {
    let mut m = alloc::vec![0u8; MAX_MESSAGE];
    let q = write_query(&mut m, id, name).expect("an askable name");
    m.truncate(q);
    m[2] = 0x81;
    m[3] = 0x80 | rcode;
    m[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (rtype, ttl, rdata) in records {
        m.extend_from_slice(&[0xC0, 0x0C]);
        m.extend_from_slice(&rtype.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        m.extend_from_slice(&ttl.to_be_bytes());
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(rdata);
    }
    m
}

/// The resolver's contract, proved on every CPU at boot: fixed messages, no device, no heap.
pub fn dns_suite(
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
    const NAME: &[u8] = b"aletheia.test";
    // An answer: our question, then two A records whose owner compresses to the question.
    const ANSWER: [u8; 63] = [
        0x12, 0x34, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0, //
        8, b'a', b'l', b'e', b't', b'h', b'e', b'i', b'a', 4, b't', b'e', b's', b't', 0, 0, 1, 0,
        1, 0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 10, 0, 2, 2, //
        0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 10, 0, 2, 3,
    ];

    // 1 - a query is exactly the RFC 1035 shape: header, labels, type A, class IN, RD set.
    let mut q = [0u8; 64];
    let qlen = write_query(&mut q, 0x1234, NAME);
    check!(
        qlen == Ok(31)
            && q[..2] == ANSWER[..2]
            && q[2..12] == [1, 0, 0, 1, 0, 0, 0, 0, 0, 0]
            && q[12..31] == ANSWER[12..31],
        "dns: a query is header, labels, type A and class IN with recursion desired, nothing else"
    );

    // 2 - names that are not hostnames are refused before a byte is written.
    let mut long = [b'a'; MAX_NAME + 1];
    long[63] = b'.';
    check!(
        [&b""[..], b"a..b", b"-a.b", b"a-.b", b"a b", b"a.b.", b"\xffx", &long[..]]
            .iter()
            .all(|bad| write_query(&mut q, 1, bad) == Err(DnsError::BadName))
            && write_query(&mut q[..10], 1, NAME) == Err(DnsError::NoRoom),
        "dns: an empty, spaced, dashed-edge, trailing-dot, non-ASCII or over-long name is refused by name"
    );

    // 3 - the answer yields both addresses, in order, and the smaller TTL.
    let r = parse_answer(&ANSWER, 0x1234, NAME);
    check!(
        matches!(r, Ok(r) if r.addresses() == [[10, 0, 2, 2], [10, 0, 2, 3]] && r.ttl == 30 && r.cnames == 0),
        "dns: an answer yields its A records in order with the smallest TTL"
    );

    // 4 - someone else's answer: wrong id, or the right id for a different question.
    check!(
        parse_answer(&ANSWER, 0x1235, NAME) == Err(DnsError::NotOurQuestion)
            && parse_answer(&ANSWER, 0x1234, b"aletheia.tesx") == Err(DnsError::NotOurQuestion),
        "dns: an answer with a foreign id, or to a different name, is someone else's and refused"
    );

    // 5 - a question back, a cut answer, NXDOMAIN and SERVFAIL are each refused by name.
    let with = |b2: u8, b3: u8| {
        let mut m = ANSWER;
        m[2] = b2;
        m[3] = b3;
        m
    };
    check!(
        parse_answer(&with(0x01, 0x80), 0x1234, NAME) == Err(DnsError::NotAnAnswer)
            && parse_answer(&with(0x83, 0x80), 0x1234, NAME) == Err(DnsError::CutByServer)
            && parse_answer(&with(0x81, 0x83), 0x1234, NAME) == Err(DnsError::NoSuchName)
            && parse_answer(&with(0x81, 0x82), 0x1234, NAME) == Err(DnsError::ServerFailure(2)),
        "dns: a question sent back, a truncated answer, NXDOMAIN and SERVFAIL are refused by name"
    );

    // 6 - a pointer loop cannot loop: a self pointer and a forward pointer are refused.
    let mut self_ptr = ANSWER;
    self_ptr[31] = 0xC0;
    self_ptr[32] = 31;
    let mut fwd = ANSWER;
    fwd[31] = 0xC0;
    fwd[32] = 47;
    check!(
        parse_answer(&self_ptr, 0x1234, NAME) == Err(DnsError::BadCompression)
            && parse_answer(&fwd, 0x1234, NAME) == Err(DnsError::BadCompression),
        "dns: a self or forward compression pointer is refused, so a pointer loop cannot run"
    );

    // 7 - every truncation of the answer is refused, never read past its end.
    let mut all_short = true;
    for cut in 0..ANSWER.len() {
        if parse_answer(&ANSWER[..cut], 0x1234, NAME).is_ok() {
            all_short = false;
        }
    }
    check!(
        all_short,
        "dns: every prefix of an answer is refused as truncated, never read past its end"
    );

    // 8 - a record owned by another name is not our address.
    let mut other = ANSWER;
    other[6..8].copy_from_slice(&[0, 1]); // only the first record
    other[31] = 0xC0;
    other[32] = 21; // owner "test", not "aletheia.test"
    check!(
        parse_answer(&other[..47], 0x1234, NAME) == Err(DnsError::NoAddress),
        "dns: an A record owned by a different name is not an address for ours"
    );

    // 9 - a CNAME chain is followed to its end: www.aletheia.test is an alias whose rdata points
    //     at "aletheia.test" inside the question, and the A record is owned by that target.
    const CHAIN: [u8; 65] = [
        0x22, 0x22, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0, //
        3, b'w', b'w', b'w', 8, b'a', b'l', b'e', b't', b'h', b'e', b'i', b'a', 4, b't', b'e',
        b's', b't', 0, 0, 1, 0, 1, //
        0xC0, 0x0C, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xC0, 0x10, //
        0xC0, 0x10, 0, 1, 0, 1, 0, 0, 0, 45, 0, 4, 10, 0, 2, 2,
    ];
    let r = parse_answer(&CHAIN, 0x2222, b"www.aletheia.test");
    check!(
        matches!(r, Ok(r) if r.addresses() == [[10, 0, 2, 2]] && r.cnames == 1 && r.ttl == 45),
        "dns: a CNAME is followed to the A record its target owns, and the link is counted"
    );

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_holds_on_the_host() {
        let mut failed = None;
        let n = dns_suite(|i, ok, name| {
            if !ok {
                failed = Some((i, name));
            }
        })
        .unwrap_or_else(|(i, name)| panic!("dns invariant {i} failed: {name}"));
        assert_eq!(n, 9);
        assert!(failed.is_none());
    }

    #[test]
    fn a_cname_chain_resolves_to_its_end_and_is_bounded() {
        // The chain itself is boot invariant 9. Here: a CNAME whose target is itself is followed at
        // most MAX_CNAMES times, then refused as NoAddress, never looped on.
        let name = b"www.example.test";
        let m = build_answer_for_tests(8, name, 0, &[(TYPE_CNAME, 90, &[0xC0, 0x0C])]);
        assert_eq!(parse_answer(&m, 8, name), Err(DnsError::NoAddress));
    }

    #[test]
    fn more_addresses_than_kept_are_counted_not_stored() {
        let name = b"many.test";
        let recs: alloc::vec::Vec<(u16, u32, &[u8])> =
            (0..6).map(|_| (TYPE_A, 5, &[1u8, 2, 3, 4][..])).collect();
        let m = build_answer_for_tests(9, name, 0, &recs);
        let r = parse_answer(&m, 9, name).unwrap();
        assert_eq!((r.count, r.dropped), (MAX_ADDRS, 2));
    }

    #[test]
    fn names_compare_without_case() {
        let m = build_answer_for_tests(3, b"Aletheia.TEST", 0, &[(TYPE_A, 1, &[10, 0, 2, 2])]);
        assert!(parse_answer(&m, 3, b"aletheia.test").is_ok());
    }
}
