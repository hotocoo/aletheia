//! Host proofs for the second networking slice (REQ-NET-003, ADR-060).
//!
//! The live half of the UDP/DHCP/ARP-cache work can only be exercised against QEMU's user network —
//! that is what the three VM gates do, and why the OFFER invariant is a boot invariant. What IS
//! provable on the host is everything that decides whether those boots can pass: the wire format the
//! builder writes, the refusals the parser makes, the binding of an OFFER to its transaction id, and
//! the way the three modules compose into one honest exchange. Proving it here means a regression in
//! the stack's logic is caught by cargo test instead of by three QEMU boots — the same standard
//! virtioblk and virtiogpu set for their slices.
extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use kernel_core::arpcache::ArpCache;
use kernel_core::dhcp;
use kernel_core::udpv4;

const GUEST: [u8; 4] = [10, 0, 2, 15];
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
const XID: u32 = 0x1A2B_3C4D;

/// A server-style OFFER laid out the way QEMU's user-network DHCP server answers: header, cookie,
/// the standard options, END, zero padding to the legacy minimum.
fn slirp_style_offer(xid: u32, yiaddr: [u8; 4]) -> Vec<u8> {
    let mut b = vec![0u8; dhcp::BOOTP_MIN_LEN];
    b[0] = 2; // BOOTREPLY
    b[1] = 1; // htype ethernet
    b[2] = 6; // hlen
    b[4..8].copy_from_slice(&xid.to_be_bytes());
    b[16..20].copy_from_slice(&yiaddr);
    b[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let mut p = 240usize;
    let opt = |b: &mut Vec<u8>, p: &mut usize, kind: u8, val: &[u8]| {
        b[*p] = kind;
        b[*p + 1] = val.len() as u8;
        b[*p + 2..*p + 2 + val.len()].copy_from_slice(val);
        *p += 2 + val.len();
    };
    opt(&mut b, &mut p, 53, &[2]); // DHCPOFFER
    opt(&mut b, &mut p, 1, &[255, 255, 255, 0]); // subnet mask
    opt(&mut b, &mut p, 3, &[10, 0, 2, 2]); // router
    opt(&mut b, &mut p, 51, &[0, 0, 0, 60]); // lease 60s
    opt(&mut b, &mut p, 54, &[10, 0, 2, 2]); // server id
    opt(&mut b, &mut p, 6, &[10, 0, 2, 3]); // DNS — an option we do not interpret, must be skipped
    b[p] = 255;
    b
}

/// Wrap bytes as a UDP datagram the driver would receive.
fn wrap_udp(
    buf: &mut [u8],
    src: [u8; 4],
    dst: [u8; 4],
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Vec<u8> {
    udpv4::build_datagram(buf, 0x1234, src, dst, sport, dport, payload)
        .expect("fixture fits")
        .to_vec()
}

#[test]
fn a_discover_wrapped_for_the_wire_survives_the_full_receive_path() {
    // The TX half of the driver's DHCP exchange, replayed on the host: build the question, wrap it
    // exactly as udp_exchange does, and read it back through the whole parse chain.
    let mut disc = [0u8; dhcp::BOOTP_MIN_LEN];
    let question = dhcp::write_discover(&mut disc, [0x52, 0x54, 0, 0x12, 0x34, 0x56], XID)
        .expect("the minimum fits")
        .to_vec();
    let mut wire = [0u8; 600];
    let datagram = wrap_udp(
        &mut wire,
        GUEST,
        [255, 255, 255, 255],
        dhcp::CLIENT_PORT,
        dhcp::SERVER_PORT,
        &question,
    );

    let ip = udpv4::parse_ipv4(&datagram).expect("our own datagram parses");
    assert_eq!(ip.dst, [255, 255, 255, 255]);
    let u = udpv4::parse_udp(&ip).expect("our own segment parses");
    assert_eq!((u.sport, u.dport), (dhcp::CLIENT_PORT, dhcp::SERVER_PORT));
    assert_eq!(u.payload.len(), dhcp::BOOTP_MIN_LEN);
    // The question must NOT parse as an answer — a DISCOVER is op REQUEST, and the parser's very
    // first structural check refuses it. A parser that accepted our own question as an offer would
    // accept anything.
    assert_eq!(
        dhcp::parse_offer(u.payload, XID).unwrap_err(),
        dhcp::DhcpError::BadOp
    );
}

#[test]
fn a_server_style_offer_read_off_the_wire_yields_the_offered_facts() {
    let offer_bytes = slirp_style_offer(XID, GUEST);
    let mut wire = [0u8; 600];
    let datagram = wrap_udp(
        &mut wire,
        GATEWAY,
        GUEST,
        dhcp::SERVER_PORT,
        dhcp::CLIENT_PORT,
        &offer_bytes,
    );

    let ip = udpv4::parse_ipv4(&datagram).expect("the reply parses");
    assert_eq!((ip.src, ip.dst), (GATEWAY, GUEST));
    let u = udpv4::parse_udp(&ip).expect("the segment parses");
    let o = dhcp::parse_offer(u.payload, XID).expect("the offer parses");
    assert_eq!(o.yiaddr, GUEST);
    assert_eq!(o.server_id, Some([10, 0, 2, 2]));
    assert_eq!(o.subnet_mask, Some([255, 255, 255, 0]));
    assert_eq!(o.router, Some([10, 0, 2, 2]));
    assert_eq!(o.lease_secs, Some(60));
}

#[test]
fn every_bit_flip_in_a_received_offer_is_refused_or_changes_nothing() {
    // Exhaustive over one real OFFER: any single flipped bit either makes the exchange REFUSE the
    // datagram somewhere (checksums, structure, semantics) or lands in a byte the protocol defines
    // as padding — in which case the parsed answer is bit-identical. Nothing in between: no flip may
    // CHANGE the offer we accept without being caught.
    let offer_bytes = slirp_style_offer(XID, GUEST);
    let mut wire = [0u8; 600];
    let datagram = wrap_udp(
        &mut wire,
        GATEWAY,
        GUEST,
        dhcp::SERVER_PORT,
        dhcp::CLIENT_PORT,
        &offer_bytes,
    );
    let baseline = dhcp::parse_offer(
        udpv4::parse_udp(&udpv4::parse_ipv4(&datagram).unwrap())
            .unwrap()
            .payload,
        XID,
    )
    .unwrap();

    for byte in 0..datagram.len() {
        for bit in 0..8u32 {
            let mut damaged = datagram.clone();
            damaged[byte] ^= 1 << bit;
            let outcome = match udpv4::parse_ipv4(&damaged) {
                Err(_) => None,
                Ok(ip) => match udpv4::parse_udp(&ip) {
                    Err(_) => None,
                    Ok(u) => dhcp::parse_offer(u.payload, XID).ok(),
                },
            };
            match outcome {
                None => {} // refused somewhere in the chain — the flip was caught
                Some(o) => assert_eq!(
                    o, baseline,
                    "byte {byte} bit {bit}: a flipped bit CHANGED the accepted offer"
                ),
            }
        }
    }
}

#[test]
fn an_offer_is_bound_to_its_transaction_id_end_to_end() {
    let offer_bytes = slirp_style_offer(XID, GUEST);
    let mut wire = [0u8; 600];
    let datagram = wrap_udp(
        &mut wire,
        GATEWAY,
        GUEST,
        dhcp::SERVER_PORT,
        dhcp::CLIENT_PORT,
        &offer_bytes,
    );
    let u = udpv4::parse_udp(&udpv4::parse_ipv4(&datagram).unwrap()).unwrap();
    // The right bytes for a DIFFERENT question are not an answer to ours — the binding survives the
    // whole stack, not just the DHCP layer.
    assert_eq!(
        dhcp::parse_offer(u.payload, XID.wrapping_add(1)).unwrap_err(),
        dhcp::DhcpError::XidMismatch
    );
}

#[test]
fn the_cache_answers_only_what_the_wire_told_it() {
    let mut c = ArpCache::<4>::new();
    // Before anything is learned, every question misses — including the gateway's.
    for last in 0..=255u8 {
        assert!(c.lookup([10, 0, 2, last]).is_none());
    }
    c.insert(GATEWAY, [0x52, 0x54, 0, 0x12, 0x35, 0x02]);
    // The learned binding answers; the 255 neighbors do not, byte-exactly.
    assert_eq!(c.lookup(GATEWAY), Some([0x52, 0x54, 0, 0x12, 0x35, 0x02]));
    for last in (0..=255u8).filter(|&b| b != 2) {
        assert!(
            c.lookup([10, 0, 2, last]).is_none(),
            ".{last} must not alias .2"
        );
    }
}

#[test]
fn the_stack_refuses_a_reply_that_was_readdressed_in_flight() {
    // The pseudo-header's reason to exist, replayed as an attack: take a genuine OFFER to us and
    // rewrite its destination address. Every layer above the Ethernet must notice.
    let offer_bytes = slirp_style_offer(XID, GUEST);
    let mut wire = [0u8; 600];
    let datagram = wrap_udp(
        &mut wire,
        GATEWAY,
        GUEST,
        dhcp::SERVER_PORT,
        dhcp::CLIENT_PORT,
        &offer_bytes,
    );
    let mut stolen = datagram.clone();
    stolen[16..20].copy_from_slice(&[10, 0, 2, 99]); // someone else's address, written over ours
    stolen[10] = 0; // the thief fixes the IP header checksum too — a competent rewrite
    stolen[11] = 0;
    let ck = udpv4::checksum(&stolen[..20]);
    stolen[10] = (ck >> 8) as u8;
    stolen[11] = ck as u8;
    let ip = udpv4::parse_ipv4(&stolen).expect("an honest-looking header");
    assert_eq!(
        udpv4::parse_udp(&ip).unwrap_err(),
        udpv4::UdpError::BadChecksum
    );
}

#[test]
fn a_truncated_offer_never_parses_before_its_end_option() {
    // Locate the END option in the fixture rather than hardcoding its offset — the test must
    // survive the option list growing or shrinking. A naive search for the byte 0xFF would hit the
    // subnet mask's VALUE (255.255.255.0), so this walks options exactly as the parser does: PAD
    // steps one byte, anything else advances by its declared length.
    let offer_bytes = slirp_style_offer(XID, GUEST);
    let end_at = {
        let mut p = 240usize;
        loop {
            match offer_bytes[p] {
                255 => break p,
                0 => p += 1,
                _ => p += 2 + offer_bytes[p + 1] as usize,
            }
        }
    };
    // Every cut that ends BEFORE END must be refused: the walk never saw a terminator, so the
    // buffer is truncated no matter how well-formed its prefix looks.
    for cut in 0..=end_at {
        assert!(
            dhcp::parse_offer(&offer_bytes[..cut], XID).is_err(),
            "cut {cut} (before/at END) must not parse"
        );
    }
    // Cuts AFTER END only drop zero padding, which the protocol defines as optional — the parsed
    // answer must be IDENTICAL to parsing the whole thing.
    let baseline = dhcp::parse_offer(&offer_bytes, XID).unwrap();
    for cut in (end_at + 1)..offer_bytes.len() {
        let o = dhcp::parse_offer(&offer_bytes[..cut], XID)
            .unwrap_or_else(|e| panic!("cut {cut} after END refused: {e:?}"));
        assert_eq!(
            o, baseline,
            "cut {cut}: padding truncation changed the offer"
        );
    }
}

#[test]
fn the_wire_format_constants_agree_with_the_layout_the_parser_assumes() {
    // A header-size constant drifting from the offsets the parsers hardcode would parse fiction.
    assert_eq!(udpv4::IPV4_HDR_MIN, 20);
    assert_eq!(udpv4::UDP_HDR_LEN, 8);
    assert_eq!(dhcp::BOOTP_MIN_LEN, 300);
    assert_eq!(dhcp::CLIENT_PORT, 68);
    assert_eq!(dhcp::SERVER_PORT, 67);
    // And the cookie sits exactly where the parser reads it.
    let b = slirp_style_offer(XID, GUEST);
    assert_eq!(&b[236..240], &[99, 130, 83, 99]);
}
