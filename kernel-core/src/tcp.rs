//! TCP over IPv4, the wire half: segments parsed fail-closed, built with a correct checksum, and
//! sequence numbers compared the only way that is correct on a 32-bit wrapping space
//! (REQ-NET-004, ADR-138).
//!
//! This is the blocker Lethe's integration page names first (`docs/LETHE-INTEGRATION.md`, stage
//! N1): the stack speaks ARP, DHCP and UDP, and every protocol a browser needs sits on TCP. The
//! module is split the way the rest of this tree splits device work from policy — bytes here,
//! state machine in [`crate::tcpconn`] — so the connection can be proved without a device and the
//! wire can be proved without a connection.
//!
//! Three strictnesses are deliberate, and each is a refusal with a name rather than a silent drop:
//!
//! * **The checksum is verified over the pseudo-header**, so a segment delivered to the wrong host
//!   cannot verify. This is the only evidence the bytes the device DMAd up are the bytes a peer
//!   sent, and it is never skipped.
//! * **A data offset that lies about the buffer is refused.** The offset field is attacker-chosen
//!   on any real network; a stack that trusts it reads someone else's memory as payload.
//! * **Sequence numbers are compared as wrapping differences, never as integers.** `a < b` on
//!   `u32` is wrong for TCP at exactly the moment the space wraps, which is precisely when a bug
//!   there is unreachable by testing and reachable by a peer.

use crate::udpv4::{checksum, Ipv4View, IPV4_HDR_MIN};
use crate::virtionet::{be16, put_be16};

/// IPv4 protocol number for TCP (RFC 790).
pub const PROTOCOL_TCP: u8 = 6;
/// The fixed TCP header: twenty bytes, no options.
pub const TCP_HDR_MIN: usize = 20;
/// The largest header this parser will accept (data offset 15 = 60 bytes).
pub const TCP_HDR_MAX: usize = 60;

/// Control bits, named so a state machine reads as prose rather than as hex.
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;

/// Why a received segment is not a TCP segment this stack will look at. Each variant is a distinct
/// fact about the bytes, never "malformed".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    /// Fewer than the twenty fixed header bytes survived the trip.
    TooShort,
    /// The data offset is below 5, above 15, or runs past the bytes actually received.
    BadDataOffset,
    /// The checksum does not verify over the pseudo-header and the bytes as received.
    BadChecksum,
    /// A port of zero. Reserved, and never a peer this stack asked for.
    ZeroPort,
    /// The urgent flag. This stack has no out-of-band channel, so a segment claiming one is
    /// refused rather than silently read as ordinary data.
    UrgentUnsupported,
}

/// A parsed segment: the header fields a connection needs, plus the exact payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpView<'a> {
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: &'a [u8],
}

impl TcpView<'_> {
    /// Whether this segment carries a given control bit.
    pub fn has(&self, bit: u8) -> bool {
        self.flags & bit != 0
    }

    /// The sequence space this segment occupies: its payload, plus one for SYN and one for FIN.
    /// Acknowledgement arithmetic is defined on this, not on the payload length, which is why it
    /// lives here rather than at each call site that would get it wrong differently.
    pub fn seq_len(&self) -> u32 {
        let mut n = self.payload.len() as u32;
        if self.has(SYN) {
            n += 1;
        }
        if self.has(FIN) {
            n += 1;
        }
        n
    }
}

/// What to put on the wire: the same fields, from the sending side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: &'a [u8],
}

/// Is `a` strictly before `b` in the wrapping sequence space (RFC 793's `<`)?
///
/// The subtraction is the whole point: `u32` comparison says `0x0000_0001 < 0xFFFF_FFFF`, and the
/// sequence space says the opposite. The difference is interpreted as a signed distance, which is
/// correct for any two numbers less than 2^31 apart — every pair a connection can legitimately
/// hold.
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// Is `a` at or before `b` in the wrapping sequence space?
pub fn seq_leq(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// Does `seq` fall inside the window `[start, start + len)`, wrapping included? A zero-length
/// window contains nothing, which is what makes a closed receive window fail closed.
pub fn seq_in_window(seq: u32, start: u32, len: u32) -> bool {
    if len == 0 {
        return false;
    }
    seq.wrapping_sub(start) < len
}

/// The ones-complement sum of the TCP pseudo-header plus the segment as it will sit on the wire.
/// Callers zero the checksum field first; the returned value is what belongs in it.
pub fn tcp_checksum(src: [u8; 4], dst: [u8; 4], segment_with_zeroed_ck: &[u8]) -> u16 {
    let mut sum = pseudo_sum(src, dst, segment_with_zeroed_ck);
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Verify a RECEIVED segment whose checksum field is still in place, without copying it: the
/// ones-complement sum of the pseudo-header plus the segment INCLUDING its stored checksum folds
/// to exactly 0xFFFF when the stored value is the true complement.
fn verify_checksum(src: [u8; 4], dst: [u8; 4], segment: &[u8]) -> bool {
    let mut sum = pseudo_sum(src, dst, segment);
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum == 0xFFFF
}

/// The pseudo-header (source, destination, protocol, TCP length) summed with the segment's own
/// words. One implementation, used by both the builder and the verifier, so the two cannot drift
/// into two definitions of "correct".
fn pseudo_sum(src: [u8; 4], dst: [u8; 4], segment: &[u8]) -> u32 {
    let mut sum: u32 = PROTOCOL_TCP as u32 + segment.len() as u32;
    for pair in src.as_chunks::<2>().0 {
        sum += u16::from_be_bytes(*pair) as u32;
    }
    for pair in dst.as_chunks::<2>().0 {
        sum += u16::from_be_bytes(*pair) as u32;
    }
    let mut i = 0;
    while i + 1 < segment.len() {
        sum += be16(segment, i) as u32;
        i += 2;
    }
    if i < segment.len() {
        // A trailing odd byte contributes as the HIGH byte of a 16-bit word; dropping it would
        // make two different payloads check the same.
        sum += (segment[i] as u32) << 8;
    }
    sum
}

/// View-and-verify a received TCP segment sitting in an already-parsed IPv4 payload. Nothing is
/// taken on faith: the offset is checked against the bytes received, the checksum is verified
/// against the addresses it was hashed with, and options are skipped rather than interpreted —
/// this stack negotiates nothing, so an option it does not understand must not change its mind.
pub fn parse_tcp<'a>(ip: &Ipv4View<'a>) -> Result<TcpView<'a>, TcpError> {
    let d = ip.payload;
    if d.len() < TCP_HDR_MIN {
        return Err(TcpError::TooShort);
    }
    let offset = (d[12] >> 4) as usize * 4;
    if !(TCP_HDR_MIN..=TCP_HDR_MAX).contains(&offset) || offset > d.len() {
        return Err(TcpError::BadDataOffset);
    }
    if !verify_checksum(ip.src, ip.dst, d) {
        return Err(TcpError::BadChecksum);
    }
    let sport = be16(d, 0);
    let dport = be16(d, 2);
    if sport == 0 || dport == 0 {
        return Err(TcpError::ZeroPort);
    }
    let flags = d[13];
    if flags & URG != 0 {
        return Err(TcpError::UrgentUnsupported);
    }
    Ok(TcpView {
        sport,
        dport,
        seq: u32::from_be_bytes([d[4], d[5], d[6], d[7]]),
        ack: u32::from_be_bytes([d[8], d[9], d[10], d[11]]),
        flags,
        window: be16(d, 14),
        payload: &d[offset..],
    })
}

/// Write a complete IPv4+TCP segment into `buf`, both checksums correct, and return the slice that
/// was written. `None` when the buffer cannot hold it — a refusal, never a partial write, because
/// a half-written segment on a shared transmit buffer is a segment some other caller sends.
pub fn build_segment<'a>(
    buf: &'a mut [u8],
    ident: u16,
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    seg: &Segment<'_>,
) -> Option<&'a [u8]> {
    let tcp_len = TCP_HDR_MIN + seg.payload.len();
    let total = IPV4_HDR_MIN + tcp_len;
    if total > u16::MAX as usize || buf.len() < total {
        return None;
    }
    {
        let ip = &mut buf[..IPV4_HDR_MIN];
        ip.fill(0);
        ip[0] = 0x45; // version 4, IHL 5 (no options)
        put_be16(ip, 2, total as u16);
        put_be16(ip, 4, ident);
        put_be16(ip, 6, 0x4000); // don't fragment: this stack reassembles nothing, by stated scope
        ip[8] = 64; // TTL
        ip[9] = PROTOCOL_TCP;
        ip[12..16].copy_from_slice(&src);
        ip[16..20].copy_from_slice(&dst);
        let ck = checksum(ip);
        put_be16(ip, 10, ck);
    }
    {
        let t = &mut buf[IPV4_HDR_MIN..total];
        t.fill(0);
        put_be16(t, 0, sport);
        put_be16(t, 2, dport);
        t[4..8].copy_from_slice(&seg.seq.to_be_bytes());
        t[8..12].copy_from_slice(&seg.ack.to_be_bytes());
        t[12] = (TCP_HDR_MIN as u8 / 4) << 4; // data offset 5, no options
        t[13] = seg.flags;
        put_be16(t, 14, seg.window);
        put_be16(t, 16, 0); // zeroed before the checksum is computed over it
        t[TCP_HDR_MIN..].copy_from_slice(seg.payload);
        let ck = tcp_checksum(src, dst, t);
        put_be16(t, 16, ck);
    }
    Some(&buf[..total])
}

/// The TCP WIRE contract, proved on every CPU at boot. The connection's own contract is
/// [`crate::tcpconn::tcpconn_suite`]; this one is about bytes only.
pub fn tcp_suite(
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

    const SRC: [u8; 4] = [10, 0, 2, 15];
    const DST: [u8; 4] = [10, 0, 2, 2];
    let mut buf = [0u8; 256];

    // 1 — what is built parses back to what was written, field for field. A builder and a parser
    //     that disagree would make every later invariant a statement about this module's opinion
    //     of itself.
    {
        let seg = Segment {
            seq: 0x1234_5678,
            ack: 0x9ABC_DEF0,
            flags: ACK | PSH,
            window: 4096,
            payload: b"GET / HTTP/1.1\r\n",
        };
        let wire = build_segment(&mut buf, 7, SRC, DST, 49152, 80, &seg).unwrap_or(&[]);
        let ok = match crate::udpv4::parse_ipv4(wire) {
            Ok(ip) => match parse_tcp(&ip) {
                Ok(v) => {
                    v.sport == 49152
                        && v.dport == 80
                        && v.seq == seg.seq
                        && v.ack == seg.ack
                        && v.flags == seg.flags
                        && v.window == seg.window
                        && v.payload == seg.payload
                        && ip.protocol == PROTOCOL_TCP
                }
                Err(_) => false,
            },
            Err(_) => false,
        };
        check!(
            ok,
            "tcp: a built segment parses back to exactly what was written"
        );
    }

    // 2 — one flipped byte anywhere is refused. The checksum is the only evidence the bytes are
    //     the peer's bytes, so it must fail for a payload byte, a header byte and an address byte
    //     alike.
    {
        let seg = Segment {
            seq: 1,
            ack: 2,
            flags: ACK,
            window: 1024,
            payload: b"payload",
        };
        let n_wire = build_segment(&mut buf, 1, SRC, DST, 1234, 80, &seg)
            .map(|w| w.len())
            .unwrap_or(0);
        let mut ok = n_wire > 0;
        for i in [IPV4_HDR_MIN + 4, IPV4_HDR_MIN + 14, IPV4_HDR_MIN + 21] {
            let saved = buf[i];
            buf[i] ^= 0x01;
            // The IPv4 header checksum still holds (the flip is inside the TCP segment), so the
            // refusal must come from the TCP layer by name.
            ok &= match crate::udpv4::parse_ipv4(&buf[..n_wire]) {
                Ok(ip) => parse_tcp(&ip) == Err(TcpError::BadChecksum),
                Err(_) => false,
            };
            buf[i] = saved;
        }
        check!(
            ok,
            "tcp: a segment with any flipped byte is refused by checksum, not read"
        );
    }

    // 3 — a segment addressed to a different host cannot verify here. This is the pseudo-header's
    //     entire purpose, and it is the property a stack that checksums only the segment loses.
    {
        let seg = Segment {
            seq: 5,
            ack: 6,
            flags: ACK,
            window: 512,
            payload: b"x",
        };
        let n_wire = build_segment(&mut buf, 2, SRC, DST, 1234, 80, &seg)
            .map(|w| w.len())
            .unwrap_or(0);
        let ip = Ipv4View {
            src: SRC,
            dst: [10, 0, 2, 3], // the same bytes, delivered to a different address
            protocol: PROTOCOL_TCP,
            payload: &buf[IPV4_HDR_MIN..n_wire],
        };
        check!(
            parse_tcp(&ip) == Err(TcpError::BadChecksum),
            "tcp: a segment re-addressed in flight fails the pseudo-header checksum"
        );
    }

    // 4 — a data offset that lies about the buffer is refused before the payload is taken. A
    //     stack that trusts this field reads memory that is not the segment.
    {
        let seg = Segment {
            seq: 1,
            ack: 1,
            flags: ACK,
            window: 1,
            payload: b"ab",
        };
        let n_wire = build_segment(&mut buf, 3, SRC, DST, 1234, 80, &seg)
            .map(|w| w.len())
            .unwrap_or(0);
        let mut ok = n_wire > 0;
        for offset_nibble in [0x40u8, 0xF0] {
            buf[IPV4_HDR_MIN + 12] = offset_nibble;
            let ip = Ipv4View {
                src: SRC,
                dst: DST,
                protocol: PROTOCOL_TCP,
                payload: &buf[IPV4_HDR_MIN..n_wire],
            };
            ok &= parse_tcp(&ip) == Err(TcpError::BadDataOffset);
        }
        check!(
            ok,
            "tcp: a data offset below the header or past the buffer is refused by name"
        );
    }

    // 5 — a truncated segment is refused rather than read as a short header.
    {
        let ip = Ipv4View {
            src: SRC,
            dst: DST,
            protocol: PROTOCOL_TCP,
            payload: &buf[..TCP_HDR_MIN - 1],
        };
        check!(
            parse_tcp(&ip) == Err(TcpError::TooShort),
            "tcp: fewer bytes than a header is refused as too short"
        );
    }

    // 6 — the sequence space wraps, and the comparison must wrap with it. This is the invariant an
    //     integer comparison passes for four billion values and fails for the ones a peer picks.
    {
        let ok = seq_lt(0xFFFF_FFFF, 0x0000_0001)
            && !seq_lt(0x0000_0001, 0xFFFF_FFFF)
            && seq_lt(1, 2)
            && !seq_lt(2, 1)
            && !seq_lt(7, 7)
            && seq_leq(7, 7)
            && seq_in_window(0x0000_0000, 0xFFFF_FFFE, 4)
            && !seq_in_window(0x0000_0002, 0xFFFF_FFFE, 4)
            && !seq_in_window(5, 5, 0);
        check!(
            ok,
            "tcp: sequence comparison is wrapping, and a zero window contains nothing"
        );
    }

    // 7 — the sequence space a segment occupies counts its controls, not only its bytes. Every
    //     acknowledgement in the state machine is arithmetic on this number.
    {
        let mk = |flags: u8, payload: &'static [u8]| TcpView {
            sport: 1,
            dport: 2,
            seq: 0,
            ack: 0,
            flags,
            window: 0,
            payload,
        };
        let ok = mk(SYN, b"").seq_len() == 1
            && mk(FIN, b"").seq_len() == 1
            && mk(ACK, b"").seq_len() == 0
            && mk(ACK | PSH, b"abcd").seq_len() == 4
            && mk(SYN | FIN, b"ab").seq_len() == 4;
        check!(
            ok,
            "tcp: a segment's sequence length counts SYN and FIN as well as its bytes"
        );
    }

    // 8 — a zero port and an urgent pointer are refused by name. Neither is something this stack
    //     asked for, and both are cheaper to refuse than to reason about later.
    {
        let seg = Segment {
            seq: 1,
            ack: 1,
            flags: ACK | URG,
            window: 1,
            payload: b"",
        };
        let n_wire = build_segment(&mut buf, 4, SRC, DST, 1234, 80, &seg)
            .map(|w| w.len())
            .unwrap_or(0);
        let ip = Ipv4View {
            src: SRC,
            dst: DST,
            protocol: PROTOCOL_TCP,
            payload: &buf[IPV4_HDR_MIN..n_wire],
        };
        let urgent_refused = parse_tcp(&ip) == Err(TcpError::UrgentUnsupported);

        let seg0 = Segment {
            seq: 1,
            ack: 1,
            flags: ACK,
            window: 1,
            payload: b"",
        };
        let n0 = build_segment(&mut buf, 5, SRC, DST, 0, 80, &seg0)
            .map(|w| w.len())
            .unwrap_or(0);
        let ip0 = Ipv4View {
            src: SRC,
            dst: DST,
            protocol: PROTOCOL_TCP,
            payload: &buf[IPV4_HDR_MIN..n0],
        };
        check!(
            urgent_refused && parse_tcp(&ip0) == Err(TcpError::ZeroPort),
            "tcp: an urgent segment and a zero port are refused by name"
        );
    }

    // 9 — a buffer too small is a refusal, never a partial write. The transmit buffer is shared,
    //     so a half-written segment is a segment somebody else transmits.
    {
        let seg = Segment {
            seq: 1,
            ack: 1,
            flags: ACK,
            window: 1,
            payload: b"0123456789",
        };
        let mut tiny = [0xAAu8; IPV4_HDR_MIN + TCP_HDR_MIN + 4];
        let refused = build_segment(&mut tiny, 6, SRC, DST, 1234, 80, &seg).is_none();
        check!(
            refused && tiny.iter().all(|&b| b == 0xAA),
            "tcp: a buffer too small refuses and writes nothing at all"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_wire_invariant() {
        let mut seen = 0;
        let n = tcp_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the tcp wire suite should hold");
        assert_eq!(n, 9);
        assert_eq!(seen, 9);
    }

    #[test]
    fn sequence_comparison_is_antisymmetric_across_the_wrap() {
        // Every pair less than 2^31 apart must order consistently, including across zero.
        for base in [0u32, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF] {
            for delta in [1u32, 2, 1024, 0x7FFF_FFFE] {
                let a = base;
                let b = base.wrapping_add(delta);
                assert!(seq_lt(a, b), "{a:#x} should precede {b:#x}");
                assert!(!seq_lt(b, a), "{b:#x} should not precede {a:#x}");
                assert!(seq_leq(a, b) && !seq_leq(b, a));
            }
        }
    }

    #[test]
    fn a_window_contains_exactly_its_own_span() {
        let start = 0xFFFF_FFF0u32;
        for i in 0..32u32 {
            let seq = start.wrapping_add(i);
            assert_eq!(seq_in_window(seq, start, 16), i < 16, "offset {i}");
        }
    }
}
