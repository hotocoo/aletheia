//! UDP over IPv4 — the kernel's first DATAGRAM service, parsed fail-closed at both ends (REQ-NET-003, ADR-060).
//!
//! The first networking slice proved the path with ICMP echo, whose replies are matched by identifier
//! and sequence. UDP is the transport underneath almost everything a real network speaks — DNS, DHCP,
//! NTP — and it has no sequence numbers: the ONLY thing binding a reply to a request is the port pair
//! and a checksum that covers the pseudo-header (source and destination addresses INSIDE the hash, so
//! a datagram delivered to the wrong host cannot verify). This module owns both directions:
//!
//! * **Build** — one function writes IPv4+UDP bytes with BOTH checksums correct, because a datagram
//!   with a wrong checksum is dropped by the peer IN SILENCE, exactly like ICMP.
//! * **Parse** — viewing a received datagram is refusing everything that is not one: short buffers,
//!   a version that is not 4, a header length that lies about the buffer, a claimed length that runs
//!   past the received bytes, and above all a checksum that does not verify against the addresses it
//!   was hashed with. Every failure names itself.
//!
//! ## One deliberate strictness, stated where it costs something
//!
//! RFC 768 lets an IPv4 sender transmit a ZERO UDP checksum ("not computed"). We REFUSE such datagrams
//! by name rather than accept them: the checksum is this module's only evidence that the bytes the
//! device DMAd up are the bytes a peer sent, and skipping the one verification we have because the
//! sender politely marked it absent would trade the strongest invariant here for interop nobody on
//! this wire needs (QEMU's user network always computes it). Fail closed, with the reason named.
//!
//! The internet checksum lives HERE (one implementation); the ICMP path re-uses it through a
//! re-export, so ICMP and UDP cannot drift into two definitions of "well formed".

use crate::virtionet::{be16, put_be16};

/// Minimum IPv4 header (IHL = 5): twenty bytes, no options.
pub const IPV4_HDR_MIN: usize = 20;
/// Fixed UDP header: eight bytes.
pub const UDP_HDR_LEN: usize = 8;
/// Largest header this parser will verify by checksum (IHL 15 = 60 bytes). An IHL beyond the buffer
/// is refused; one inside it is verified over its full declared extent.
const IPV4_HDR_MAX: usize = 60;
/// IPv4 protocol number for UDP (RFC 790).
pub const PROTOCOL_UDP: u8 = 17;

/// The internet checksum (RFC 1071): ones-complement sum of 16-bit big-endian words. This is THE
/// implementation; virtionet re-exports it so both protocols share one definition. A wrong value
/// means the peer drops the packet in silence, which is why an arriving reply is evidence the packet
/// was well FORMED, not merely well intentioned.
pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        sum += be16(bytes, i) as u32;
        i += 2;
    }
    if i < bytes.len() {
        // A trailing odd byte contributes as the HIGH byte of a 16-bit word; dropping it would make
        // two different payloads check the same.
        sum += (bytes[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Why a received packet is not an IPv4 datagram we will look at. Each variant is a distinct fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpError {
    /// Fewer bytes than the minimal fixed header.
    TooShort,
    /// The high nibble is not 4.
    BadVersion,
    /// IHL below 5, or the declared header length runs past the buffer.
    BadHeaderLength,
    /// Total length below the header, or past the bytes actually received.
    BadTotalLength,
    /// The header checksum does not verify over the header as received (field zeroed).
    BadHeaderChecksum,
}

/// The part of a parsed IPv4 header everything above it needs. `payload` is trimmed to the
/// total-length CLAIM — trailing slack (Ethernet padding) is not part of the datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4View<'a> {
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub protocol: u8,
    pub payload: &'a [u8],
}

/// Parse an IPv4 datagram: verify the fixed facts, verify the header checksum, and hand back the
/// addressing plus the exact payload the header claims. Nothing is taken on faith.
pub fn parse_ipv4<'a>(buf: &'a [u8]) -> Result<Ipv4View<'a>, IpError> {
    if buf.len() < IPV4_HDR_MIN {
        return Err(IpError::TooShort);
    }
    if buf[0] >> 4 != 4 {
        return Err(IpError::BadVersion);
    }
    let ihl = (buf[0] & 0x0F) as usize * 4;
    if ihl < IPV4_HDR_MIN || ihl > buf.len() {
        return Err(IpError::BadHeaderLength);
    }
    let total = be16(buf, 2) as usize;
    if total < ihl || total > buf.len() {
        return Err(IpError::BadTotalLength);
    }
    // Header checksum: recompute over the header AS RECEIVED with the field zeroed — the result is
    // the checksum the sender SHOULD have stored, so it must equal what was stored.
    let mut hdr = [0u8; IPV4_HDR_MAX];
    let n = ihl.min(IPV4_HDR_MAX);
    hdr[..n].copy_from_slice(&buf[..n]);
    hdr[10] = 0;
    hdr[11] = 0;
    if checksum(&hdr[..ihl]) != be16(buf, 10) {
        return Err(IpError::BadHeaderChecksum);
    }
    let mut src = [0u8; 4];
    let mut dst = [0u8; 4];
    src.copy_from_slice(&buf[12..16]);
    dst.copy_from_slice(&buf[16..20]);
    Ok(Ipv4View {
        src,
        dst,
        protocol: buf[9],
        payload: &buf[ihl..total],
    })
}

/// Sum the IPv4 pseudo-header (RFC 768) with the upper-layer datagram, unfolded. The pseudo-header is
/// NOT sent: both ends reconstruct it from their OWN view of the addresses, which is precisely why a
/// datagram re-addressed in flight fails its check instead of parsing.
fn pseudo_sum(src: [u8; 4], dst: [u8; 4], protocol: u8, upper: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    for pair in src.chunks_exact(2) {
        sum += (((pair[0] as u16) << 8) | pair[1] as u16) as u32;
    }
    for pair in dst.chunks_exact(2) {
        sum += (((pair[0] as u16) << 8) | pair[1] as u16) as u32;
    }
    sum += protocol as u32;
    sum += upper.len() as u32;
    let mut i = 0;
    while i + 1 < upper.len() {
        sum += be16(upper, i) as u32;
        i += 2;
    }
    if i < upper.len() {
        sum += (upper[i] as u32) << 8;
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// The UDP checksum value for a datagram given its addressing. Callers zero the checksum field first
/// (or call this on a copy that has it zeroed), then store the answer.
pub fn udp_checksum(src: [u8; 4], dst: [u8; 4], datagram_with_zeroed_ck: &[u8]) -> u16 {
    fold(pseudo_sum(src, dst, PROTOCOL_UDP, datagram_with_zeroed_ck))
}

/// Verify a RECEIVED datagram whose checksum field is still in place, without copying it: the
/// ones-complement sum of the pseudo-header plus the datagram INCLUDING its stored checksum folds
/// to exactly 0xFFFF when the stored value is the true complement. Every word of header and payload
/// participates — that is what makes a re-addressed datagram fail.
fn verify_checksum(src: [u8; 4], dst: [u8; 4], datagram: &[u8]) -> bool {
    // Protocol byte + UDP length are the pseudo-header's tail; the addresses follow.
    let mut sum: u32 = PROTOCOL_UDP as u32 + datagram.len() as u32;
    for pair in src.chunks_exact(2) {
        sum += (((pair[0] as u16) << 8) | pair[1] as u16) as u32;
    }
    for pair in dst.chunks_exact(2) {
        sum += (((pair[0] as u16) << 8) | pair[1] as u16) as u32;
    }
    let mut i = 0;
    while i + 1 < datagram.len() {
        sum += be16(datagram, i) as u32;
        i += 2;
    }
    if i < datagram.len() {
        sum += (datagram[i] as u32) << 8;
    }
    // Fold WITHOUT complementing: a valid datagram lands on all ones, not zero.
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum == 0xFFFF
}

/// Why a received segment is not a UDP datagram we will look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdpError {
    /// Fewer than the eight fixed header bytes survived the trip.
    TooShort,
    /// The length field is below 8 or runs past the bytes actually received.
    LengthMismatch,
    /// The checksum field is zero — unverifiable by policy (module docs), refused rather than trusted.
    ZeroChecksum,
    /// The checksum does not verify over the pseudo-header and the bytes as received.
    BadChecksum,
}

/// A parsed UDP header plus the exact payload the length field claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpView<'a> {
    pub sport: u16,
    pub dport: u16,
    pub payload: &'a [u8],
}

/// View-and-verify a received UDP segment sitting in an already-parsed IPv4 payload. The checksum is
/// verified over the WHOLE datagram as received — header and payload — against THIS header's source
/// and destination, with no copy and no buffer: the classic property that a ones-complement sum
/// INCLUDING the stored checksum folds to 0xFFFF. A datagram re-addressed in flight fails here,
/// which is the point.
pub fn parse_udp<'a>(ip: &Ipv4View<'a>) -> Result<UdpView<'a>, UdpError> {
    let d = ip.payload;
    if d.len() < UDP_HDR_LEN {
        return Err(UdpError::TooShort);
    }
    let ulen = be16(d, 4) as usize;
    if ulen < UDP_HDR_LEN || ulen > d.len() {
        return Err(UdpError::LengthMismatch);
    }
    let datagram = &d[..ulen];
    if be16(datagram, 6) == 0 {
        return Err(UdpError::ZeroChecksum);
    }
    if !verify_checksum(ip.src, ip.dst, datagram) {
        return Err(UdpError::BadChecksum);
    }
    Ok(UdpView {
        sport: be16(d, 0),
        dport: be16(d, 2),
        payload: &d[UDP_HDR_LEN..ulen],
    })
}

/// Write a complete IPv4+UDP datagram into buf, both checksums correct, and return the slice that was
/// written. Returns None when the buffer cannot hold it — a refusal, never a partial write.
pub fn build_datagram<'a>(
    buf: &'a mut [u8],
    ident: u16,
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Option<&'a [u8]> {
    let total = IPV4_HDR_MIN + UDP_HDR_LEN + payload.len();
    if total > u16::MAX as usize || buf.len() < total {
        return None;
    }
    {
        let ip = &mut buf[..IPV4_HDR_MIN];
        ip.fill(0);
        ip[0] = 0x45; // version 4, IHL 5 (no options)
        put_be16(ip, 2, total as u16);
        put_be16(ip, 4, ident); // caller-chosen, so a reply can be traced to its ask
        put_be16(ip, 6, 0x4000); // don't fragment: this stack reassembles nothing, by stated scope
        ip[8] = 64; // TTL
        ip[9] = PROTOCOL_UDP;
        ip[12..16].copy_from_slice(&src);
        ip[16..20].copy_from_slice(&dst);
        let ck = checksum(ip);
        put_be16(ip, 10, ck);
    }
    let ulen = UDP_HDR_LEN + payload.len();
    {
        let u = &mut buf[IPV4_HDR_MIN..IPV4_HDR_MIN + ulen];
        u.fill(0);
        put_be16(u, 0, sport);
        put_be16(u, 2, dport);
        put_be16(u, 4, ulen as u16);
        put_be16(u, 6, 0); // zeroed before the checksum is computed over it
        u[UDP_HDR_LEN..].copy_from_slice(payload);
        let mut c = udp_checksum(src, dst, u);
        if c == 0 {
            // Ones-complement zero has TWO representations, and RFC 768 reserves 0x0000 for
            // "sender did not compute" — which this module's own receiver refuses BY NAME. A
            // datagram whose sum folds to exactly 0xFFFF would otherwise be dropped by our own
            // stack, so it is transmitted in the other representation instead.
            c = 0xFFFF;
        }
        put_be16(u, 6, c);
    }
    Some(&buf[..total])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const SRC: [u8; 4] = [10, 0, 2, 15];
    const DST: [u8; 4] = [10, 0, 2, 2];

    #[test]
    fn a_built_datagram_parses_back_to_what_was_written() {
        let mut buf = [0u8; 128];
        let payload = b"aletheia-udp-01";
        let wrote = build_datagram(&mut buf, 0xBEEF, SRC, DST, 40000, 53, payload).unwrap();
        assert_eq!(wrote.len(), IPV4_HDR_MIN + UDP_HDR_LEN + payload.len());

        let ip = parse_ipv4(wrote).expect("our own datagram must parse");
        assert_eq!(ip.src, SRC);
        assert_eq!(ip.dst, DST);
        assert_eq!(ip.protocol, PROTOCOL_UDP);
        let u = parse_udp(&ip).expect("our own segment must parse");
        assert_eq!(u.sport, 40000);
        assert_eq!(u.dport, 53);
        assert_eq!(u.payload, payload.as_slice());
    }

    #[test]
    fn every_single_bit_flip_anywhere_in_the_datagram_is_refused() {
        // The load-bearing property: NO byte of the received unit is unprotected. The IP header is
        // covered by its own checksum; the UDP header and payload are covered by the pseudo-header
        // checksum. Exhaustive over the actual wire unit, not sampled.
        let payload = b"flip-me";
        let len = IPV4_HDR_MIN + UDP_HDR_LEN + payload.len();
        for byte in 0..len {
            for bit in 0..8u32 {
                let mut buf = [0u8; 128];
                let wrote = build_datagram(&mut buf, 1, SRC, DST, 68, 67, payload).unwrap();
                let mut damaged = [0u8; 128];
                damaged[..len].copy_from_slice(&wrote[..len]);
                damaged[byte] ^= 1 << bit;
                let refused = match parse_ipv4(&damaged[..len]) {
                    Err(_) => true,
                    Ok(ip) => parse_udp(&ip).is_err(),
                };
                assert!(
                    refused,
                    "byte {byte} bit {bit}: a flipped bit parsed as a valid datagram"
                );
            }
        }
    }

    #[test]
    fn every_truncation_of_a_valid_datagram_is_refused() {
        let payload = b"truncate-me";
        let mut buf = [0u8; 128];
        let wrote = build_datagram(&mut buf, 2, SRC, DST, 1000, 2000, payload).unwrap();
        for cut in 0..wrote.len() {
            assert!(
                parse_ipv4(&wrote[..cut]).is_err(),
                "length {cut} must not parse"
            );
        }
    }

    #[test]
    fn a_claimed_length_that_lies_in_either_direction_is_refused() {
        let mut buf = [0u8; 128];
        let wrote = build_datagram(&mut buf, 3, SRC, DST, 1, 2, b"abcd").unwrap();
        let uoff = IPV4_HDR_MIN + 4;

        // Claim SHORTER than the fixed header.
        let mut short = wrote.to_vec();
        short[uoff] = 0;
        short[uoff + 1] = 4;
        assert_eq!(
            parse_udp(&parse_ipv4(&short).unwrap()).unwrap_err(),
            UdpError::LengthMismatch
        );

        // Claim LONGER than the bytes actually received (IP total length still honest).
        let mut long = wrote.to_vec();
        long[uoff + 1] = 40;
        assert_eq!(
            parse_udp(&parse_ipv4(&long).unwrap()).unwrap_err(),
            UdpError::LengthMismatch
        );
    }

    #[test]
    fn a_zero_checksum_is_refused_by_name_not_accepted_by_optimism() {
        let mut buf = [0u8; 128];
        let wrote = build_datagram(&mut buf, 4, SRC, DST, 5, 6, b"x").unwrap();
        let mut unset = wrote.to_vec();
        unset[IPV4_HDR_MIN + 6] = 0;
        unset[IPV4_HDR_MIN + 7] = 0;
        let err = parse_udp(&parse_ipv4(&unset).unwrap()).unwrap_err();
        assert_eq!(err, UdpError::ZeroChecksum);
    }

    #[test]
    fn the_pseudo_header_really_covers_the_addresses() {
        // Same bytes, different destination address: the checksum MUST differ, or a datagram
        // addressed to another host could be re-addressed in flight and still verify.
        let mut a = [0u8; 128];
        let mut b = [0u8; 128];
        let da = build_datagram(&mut a, 7, SRC, DST, 9, 9, b"addr")
            .unwrap()
            .to_vec();
        let db = build_datagram(&mut b, 7, SRC, [10, 0, 2, 3], 9, 9, b"addr")
            .unwrap()
            .to_vec();
        assert_ne!(
            da[IPV4_HDR_MIN + 6..IPV4_HDR_MIN + 8],
            db[IPV4_HDR_MIN + 6..IPV4_HDR_MIN + 8],
            "re-addressing must change the checksum"
        );

        // And a datagram whose addressing was rewritten in flight fails verification. The rewrite
        // is done COMPETENTLY — the IP header checksum is recomputed, so the only thing that can
        // catch the theft is the pseudo-header inside the UDP checksum, which is exactly the layer
        // this property is about.
        let mut stolen = da.clone();
        stolen[16..20].copy_from_slice(&[10, 0, 2, 3]); // rewrite dst under it
        stolen[10] = 0;
        stolen[11] = 0;
        let ck = checksum(&stolen[..IPV4_HDR_MIN]);
        stolen[10] = (ck >> 8) as u8;
        stolen[11] = ck as u8;
        let ip = parse_ipv4(&stolen).expect("the header itself was made honest again");
        assert_eq!(parse_udp(&ip).unwrap_err(), UdpError::BadChecksum);
    }

    #[test]
    fn an_odd_length_payload_checks_differently_than_its_even_neighbor() {
        // The trailing-odd-byte rule: a final lone byte counts as the HIGH half of its word, so
        // "abc" and "abcd" must produce different checksums — two payloads that checked the same
        // would make one accepted datagram stand for two different byte streams.
        let stored = |payload: &[u8]| {
            let mut buf = [0u8; 128];
            let w = build_datagram(&mut buf, 8, SRC, DST, 1, 1, payload).unwrap();
            [w[IPV4_HDR_MIN + 6], w[IPV4_HDR_MIN + 7]]
        };
        assert_ne!(
            stored(b"abc"),
            stored(b"abcd"),
            "odd and even neighbors must not share a checksum"
        );
    }

    #[test]
    fn odd_and_even_payloads_both_round_trip_byte_exactly() {
        for payload in [&b"abc"[..], &b"abcd"[..], &b"a"[..]] {
            let mut buf = [0u8; 128];
            let wrote = build_datagram(&mut buf, 11, SRC, DST, 1, 1, payload).unwrap();
            let u = parse_udp(&parse_ipv4(wrote).unwrap()).unwrap();
            assert_eq!(u.payload, payload);
        }
    }

    #[test]
    fn a_checksum_that_computes_to_zero_is_transmitted_as_all_ones_and_still_round_trips() {
        // Ones-complement sums have TWO zero representations. Find (deterministically, by scanning
        // a 16-bit payload word) the datagram whose pseudo-sum folds to exactly 0xFFFF — the one
        // case where the naive builder would store 0x0000, which our own receiver refuses BY NAME.
        let mut found: Option<[u8; 2]> = None;
        for w in 0..=65535u16 {
            let payload = [(w >> 8) as u8, (w & 0xFF) as u8];
            let mut buf = [0u8; 128];
            let dgram = build_datagram(&mut buf, 1, SRC, DST, 7, 7, &payload).unwrap();
            // The probe must ask what the checksum WOULD BE with the field zeroed — feeding the
            // stored value back would make every valid datagram look like the zero case.
            let mut z = [0u8; UDP_HDR_LEN + 2];
            z[..UDP_HDR_LEN].copy_from_slice(&dgram[IPV4_HDR_MIN..IPV4_HDR_MIN + UDP_HDR_LEN]);
            z[6] = 0;
            z[7] = 0;
            z[UDP_HDR_LEN..].copy_from_slice(&payload);
            if udp_checksum(SRC, DST, &z) == 0 {
                found = Some(payload);
                break;
            }
        }
        let payload = found.expect("a 16-bit scan must contain the folding-to-zero case");
        // The BUILDER must have applied the RFC 768 rule: all-ones on the wire, never 0x0000.
        let mut buf = [0u8; 128];
        let wrote = build_datagram(&mut buf, 1, SRC, DST, 7, 7, &payload).unwrap();
        assert_eq!(
            &wrote[IPV4_HDR_MIN + 6..IPV4_HDR_MIN + 8],
            &[0xFF, 0xFF],
            "a computed-zero checksum is transmitted as 0xFFFF"
        );
        // ...and the datagram our own builder produced must be accepted by our own parser.
        let ip = parse_ipv4(wrote).unwrap();
        let u = parse_udp(&ip).expect("our own all-ones datagram parses");
        assert_eq!(u.payload, payload);
    }

    #[test]
    fn a_buffer_too_small_for_the_write_is_a_refusal_never_a_partial() {
        let mut tiny = [7u8; IPV4_HDR_MIN + UDP_HDR_LEN + 3];
        assert!(build_datagram(&mut tiny, 9, SRC, DST, 1, 1, b"toolong").is_none());
        // The buffer was left untouched — no half-written datagram to leak out a later send.
        assert!(tiny.iter().all(|&b| b == 7));
    }

    #[test]
    fn an_ip_header_with_options_is_parsed_to_its_real_payload_boundary() {
        // Hand-build a 24-byte-header datagram (IHL 6): the payload must start AFTER the options,
        // and the checksum must verify over all 24 declared header bytes — not just the fixed 20.
        let mut buf = [0u8; 64];
        let base = build_datagram(&mut buf, 12, SRC, DST, 3, 4, b"opts")
            .unwrap()
            .to_vec();
        let mut opts = Vec::with_capacity(base.len() + 4);
        opts.extend_from_slice(&base[..20]);
        opts.extend_from_slice(&[1u8, 1, 1, 1]); // four NOP options
        opts.extend_from_slice(&base[20..]);
        opts[0] = 0x46; // IHL 6
                        // The total length grows by exactly the options' length — set FIRST, because it is a header
                        // field the checksum must cover. A checksum computed over stale bytes is the classic bug.
        let total = ((opts[2] as usize) << 8 | opts[3] as usize) + 4;
        opts[2] = (total >> 8) as u8;
        opts[3] = total as u8;
        // Recompute the header checksum honestly: field zeroed, over the full 24 declared bytes.
        opts[10] = 0;
        opts[11] = 0;
        let ck = checksum(&opts[..24]);
        opts[10] = (ck >> 8) as u8;
        opts[11] = ck as u8;
        let ip = parse_ipv4(&opts).expect("an options header is legal IPv4");
        let u = parse_udp(&ip).expect("the segment behind the options parses");
        assert_eq!(u.payload, b"opts");
        assert_eq!(ip.payload.len(), UDP_HDR_LEN + 4);
    }
}
