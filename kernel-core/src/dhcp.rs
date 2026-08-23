//! DHCP — the machine ASKS the network where it lives, instead of being told by a constant (REQ-NET-003, ADR-060).
//!
//! The first networking slice hardcoded the guest address `10.0.2.15` because "that is what QEMU's
//! user network expects". That is spec recall about one simulator, not knowledge the machine had.
//! Every QEMU user-net backend also runs a DHCP server whose whole job is to ANSWER that question,
//! and DHCP rides on UDP — so a DISCOVER that draws a valid OFFER is simultaneously the proof of the
//! UDP round trip AND the replacement of an assumption with a measurement. The suite now asserts the
//! address the network OFFERS equals the address the driver claims; if someone changes either side,
//! the boot says so instead of silently talking to a network that stopped listening years ago.
//!
//! ## Scope and refusals
//!
//! Exactly one transaction is implemented — DISCOVER → OFFER — and the parser is a bounded walk that
//! refuses by name: short buffers, a non-reply op code, a broken magic cookie, a foreign transaction
//! id (an answer to SOMEONE ELSE's question is not an answer to ours), a truncated option, a message
//! type that is not an OFFER, and an offer of no address at all. Options are skipped by declared
//! length with every step bounds-checked, PAD (0) and END (255) honored per RFC 2131 §4.1 — a walk
//! that could run past the buffer would be an attacker-controlled loop in kernel space.
//!
//! Not implemented, on purpose: REQUEST/ACK (the lease is never TAKEN — the guest keeps its static
//! configuration and uses the OFFER only as cross-evidence), renewal, option overload, relay agents.

/// Ports DHCP speaks on (client 68 → server 67).
pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;

/// The RFC 2131 §2 legacy minimum packet size, OPTIONS INCLUDED — every real server pads to it, and
/// a client that accepts less is parsing something no peer sends.
pub const BOOTP_MIN_LEN: usize = 300;

/// The magic cookie at offset 236: RFC 1497 vendor extensions, carried into DHCP unchanged.
const COOKIE: [u8; 4] = [99, 130, 83, 99];

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
const HT_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;

const FLAG_BROADCAST: u16 = 0x8000;

const OPT_PAD: u8 = 0;
const OPT_END: u8 = 255;
const OPT_SUBNET: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;

const MSG_DISCOVER: u8 = 1;
const MSG_OFFER: u8 = 2;

/// Why these bytes are not an acceptable OFFER. Every refusal names itself; none is a silent shrug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpError {
    /// Shorter than the fixed header plus its magic cookie — nothing to even be wrong ABOUT.
    TooShort,
    /// The op field is not BOOTREPLY(2): a question came back where an answer was required.
    BadOp,
    /// The magic cookie does not match: these options do not speak this protocol.
    BadCookie,
    /// The transaction id belongs to some other exchange. Binding to the exact xid is what makes a
    /// late reply to an EARLIER ask unable to masquerade as this one's answer.
    XidMismatch,
    /// No message-type option, or it is not OFFER(2).
    NotAnOffer,
    /// An option's length runs past the end of the buffer — the walk refuses rather than reads.
    TruncatedOption,
    /// A well-formed OFFER that offers no address (yiaddr all zero).
    NoAddress,
}

/// What a server offered us. Only `yiaddr` is load-bearing for this slice; the rest are recorded
/// facts the console can show a human, each present only when the server actually sent it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The address offered to OUR hardware address.
    pub yiaddr: [u8; 4],
    /// Option 54 — which server said this.
    pub server_id: Option<[u8; 4]>,
    /// Option 1 — the netmask the server assumes for this network.
    pub subnet_mask: Option<[u8; 4]>,
    /// Option 3 — the router the server proposes.
    pub router: Option<[u8; 4]>,
    /// Option 51 — how long the server claims the offer would live (seconds). NOT taken: see scope.
    pub lease_secs: Option<u32>,
}

impl Offer {
    /// The zero offer, used only so a failed suite run can report before unwrapping.
    pub fn none() -> Self {
        Offer {
            yiaddr: [0; 4],
            server_id: None,
            subnet_mask: None,
            router: None,
            lease_secs: None,
        }
    }
}

/// Write a complete DHCPDISCOVER into buf and return the bytes written. Broadcast flag SET (the
/// client has no address yet, so the server must answer on the wire, not to an address we lack);
/// minimum-length padded per RFC 2131 §2. Returns None when buf cannot hold the minimum — a
/// refusal, never a partial question.
pub fn write_discover(buf: &mut [u8], chaddr: [u8; 6], xid: u32) -> Option<&[u8]> {
    if buf.len() < BOOTP_MIN_LEN {
        return None;
    }
    let b = &mut buf[..BOOTP_MIN_LEN];
    b.fill(0); // sname/file/chaddr-pad/option padding are all defined-zero anyway
    b[0] = OP_REQUEST;
    b[1] = HT_ETHERNET;
    b[2] = HLEN_ETHERNET;
    b[3] = 0; // hops
    b[4..8].copy_from_slice(&xid.to_be_bytes());
    put_be16(b, 10, FLAG_BROADCAST); // secs stays 0: we have waited no time worth reporting
                                     // ciaddr/yiaddr/siaddr/giaddr remain zero — asking, not telling.
    b[28..34].copy_from_slice(&chaddr);
    b[236..240].copy_from_slice(&COOKIE);
    // Options: who we are, then END, then the padding the minimum length demands. Pad bytes ARE
    // option 0, so the tail is legal option space, not dead bytes.
    b[240] = OPT_MSG_TYPE;
    b[241] = 1;
    b[242] = MSG_DISCOVER;
    b[243] = OPT_END;
    Some(b)
}

/// Parse a received bootreply as an OFFER bound to `want_xid`. Structural facts first (length, op,
/// cookie, transaction), then a bounds-checked option walk, then the semantic requirements (it IS an
/// offer, and it offers AN ADDRESS).
pub fn parse_offer(buf: &[u8], want_xid: u32) -> Result<Offer, DhcpError> {
    if buf.len() < 240 {
        return Err(DhcpError::TooShort);
    }
    if buf[0] != OP_REPLY {
        return Err(DhcpError::BadOp);
    }
    if buf[236..240] != COOKIE {
        return Err(DhcpError::BadCookie);
    }
    let got_xid = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if got_xid != want_xid {
        return Err(DhcpError::XidMismatch);
    }

    let mut msg_type: Option<u8> = None;
    let mut o = Offer::none();
    let mut terminated = false; // the option list MUST reach END — an unterminated walk is a truncation
    let mut i = 240usize;
    while i < buf.len() {
        let kind = buf[i];
        match kind {
            OPT_PAD => {
                i += 1;
                continue;
            }
            OPT_END => {
                terminated = true;
                break;
            }
            _ => {}
        }
        // Every non-pad, non-end option carries its length byte; both bytes must exist, and the
        // value must fit inside the buffer — BEFORE anything is read from it.
        if i + 2 > buf.len() {
            return Err(DhcpError::TruncatedOption);
        }
        let len = buf[i + 1] as usize;
        if i + 2 + len > buf.len() {
            return Err(DhcpError::TruncatedOption);
        }
        let val = &buf[i + 2..i + 2 + len];
        match kind {
            // Guards live in the match arms themselves (collapsible-if lint, clippy 1.9x):
            // a short option value is SKIPPED by its declared length, never interpreted.
            OPT_MSG_TYPE if len >= 1 => {
                msg_type = Some(val[0]);
            }
            OPT_SERVER_ID if len == 4 => {
                o.server_id = Some([val[0], val[1], val[2], val[3]]);
            }
            OPT_SUBNET if len == 4 => {
                o.subnet_mask = Some([val[0], val[1], val[2], val[3]]);
            }
            OPT_ROUTER if len == 4 => {
                o.router = Some([val[0], val[1], val[2], val[3]]);
            }
            OPT_LEASE_TIME if len == 4 => {
                o.lease_secs = Some(u32::from_be_bytes([val[0], val[1], val[2], val[3]]));
            }
            _ => {} // unknown options are SKIPPED by their declared length — never interpreted
        }
        i += 2 + len;
    }

    if !terminated {
        // The buffer ended before END: whatever this is, it is not the whole option list, and a
        // parser that accepts prefixes would accept one crafted to look finished.
        return Err(DhcpError::TruncatedOption);
    }
    if msg_type != Some(MSG_OFFER) {
        return Err(DhcpError::NotAnOffer);
    }
    o.yiaddr = [buf[16], buf[17], buf[18], buf[19]];
    if o.yiaddr == [0; 4] {
        return Err(DhcpError::NoAddress);
    }
    Ok(o)
}

fn put_be16(b: &mut [u8], at: usize, v: u16) {
    b[at] = (v >> 8) as u8;
    b[at + 1] = v as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const XID: u32 = 0x1A2B_3C4D;

    /// Build a bootreply the way a real server lays one out: header, cookie, options (in the given
    /// order), END, zero padding out to the legacy minimum.
    fn offer_bytes(xid: u32, yiaddr: [u8; 4], opts: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut b = vec![0u8; BOOTP_MIN_LEN];
        b[0] = OP_REPLY;
        b[1] = HT_ETHERNET;
        b[2] = HLEN_ETHERNET;
        b[4..8].copy_from_slice(&xid.to_be_bytes());
        b[16..20].copy_from_slice(&yiaddr);
        b[236..240].copy_from_slice(&COOKIE);
        let mut p = 240usize;
        for (kind, val) in opts {
            b[p] = *kind;
            b[p + 1] = val.len() as u8;
            b[p + 2..p + 2 + val.len()].copy_from_slice(val);
            p += 2 + val.len();
        }
        b[p] = OPT_END;
        b
    }

    fn four(a: [u8; 4]) -> Vec<u8> {
        a.to_vec()
    }

    #[test]
    fn a_full_server_style_offer_parses_with_every_field_extracted() {
        let b = offer_bytes(
            XID,
            [10, 0, 2, 15],
            &[
                (OPT_MSG_TYPE, vec![MSG_OFFER]),
                (OPT_SUBNET, four([255, 255, 255, 0])),
                (OPT_ROUTER, four([10, 0, 2, 2])),
                (OPT_LEASE_TIME, vec![0, 0, 0x0E, 0x10]), // 3600s
                (OPT_SERVER_ID, four([10, 0, 2, 2])),
                // An option this parser does not know, which it must SKIP by declared length.
                (220, vec![9, 9, 9]),
                (OPT_PAD, vec![]),
                (OPT_PAD, vec![]),
            ],
        );
        let o = parse_offer(&b, XID).expect("a well-formed offer parses");
        assert_eq!(o.yiaddr, [10, 0, 2, 15]);
        assert_eq!(o.server_id, Some([10, 0, 2, 2]));
        assert_eq!(o.subnet_mask, Some([255, 255, 255, 0]));
        assert_eq!(o.router, Some([10, 0, 2, 2]));
        assert_eq!(o.lease_secs, Some(3600));
    }

    #[test]
    fn an_offer_bearing_only_the_mandatory_facts_still_parses() {
        // yiaddr + message type are the only REQUIREMENTS; everything else is optional by design.
        let b = offer_bytes(XID, [192, 168, 7, 99], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
        let o = parse_offer(&b, XID).expect("minimal offer parses");
        assert_eq!(o.yiaddr, [192, 168, 7, 99]);
        assert_eq!(o.server_id, None);
        assert_eq!(o.lease_secs, None);
    }

    #[test]
    fn the_transaction_id_binds_the_answer_to_its_question() {
        let b = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
        assert_eq!(
            parse_offer(&b, XID ^ 1).unwrap_err(),
            DhcpError::XidMismatch
        );
        assert_eq!(parse_offer(&b, 0).unwrap_err(), DhcpError::XidMismatch);
    }

    #[test]
    fn every_cookie_byte_is_checked() {
        for byte in 0..4usize {
            let mut b = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
            b[236 + byte] ^= 0xFF;
            assert_eq!(
                parse_offer(&b, XID).unwrap_err(),
                DhcpError::BadCookie,
                "cookie byte {byte} unchecked"
            );
        }
    }

    #[test]
    fn a_question_is_not_an_answer_and_a_discover_is_not_an_offer() {
        let mut b = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
        b[0] = OP_REQUEST;
        assert_eq!(parse_offer(&b, XID).unwrap_err(), DhcpError::BadOp);

        let c = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_MSG_TYPE, vec![MSG_DISCOVER])]);
        assert_eq!(parse_offer(&c, XID).unwrap_err(), DhcpError::NotAnOffer);

        // And a reply with NO message-type option at all is not provably an offer.
        let d = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_SERVER_ID, four([10, 0, 2, 2]))]);
        assert_eq!(parse_offer(&d, XID).unwrap_err(), DhcpError::NotAnOffer);
    }

    #[test]
    fn truncation_is_refused_at_every_prefix_including_inside_options() {
        // Options: msg-type (3 bytes) + server-id (6 bytes) + END = offsets 240..250, END at 249.
        const END_AT: usize = 240 + 3 + 6;
        let full = offer_bytes(
            XID,
            [10, 0, 2, 15],
            &[
                (OPT_MSG_TYPE, vec![MSG_OFFER]),
                (OPT_SERVER_ID, four([10, 0, 2, 2])),
            ],
        );
        assert_eq!(full[END_AT], OPT_END);
        // Every cut that ends BEFORE the END option must be refused: the walk never saw a
        // terminator, so the buffer is truncated no matter how well-formed its prefix looks.
        for cut in 0..=END_AT {
            let r = parse_offer(&full[..cut], XID);
            assert!(r.is_err(), "prefix {cut} must not parse");
        }
        // Cuts AFTER the END are legal — the tail is only zero padding — and parse to the same offer.
        for cut in (END_AT + 1)..full.len() {
            let o = parse_offer(&full[..cut], XID).expect("padding is optional after END");
            assert_eq!(o.yiaddr, [10, 0, 2, 15]);
        }
        // Cut EXACTLY between an option's length byte and its value.
        let mut mid = full.clone();
        mid.truncate(240 + 3 + 2); // inside the 4-byte server-id value
        assert_eq!(
            parse_offer(&mid, XID).unwrap_err(),
            DhcpError::TruncatedOption
        );
    }

    #[test]
    fn an_option_length_running_past_the_buffer_is_refused_not_followed() {
        let mut b = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
        b[240] = OPT_SERVER_ID;
        b[241] = 200; // declares far more than remains
        assert_eq!(
            parse_offer(&b, XID).unwrap_err(),
            DhcpError::TruncatedOption
        );
    }

    #[test]
    fn an_offer_of_no_address_is_refused_after_it_proves_to_be_an_offer() {
        let b = offer_bytes(XID, [0, 0, 0, 0], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
        assert_eq!(parse_offer(&b, XID).unwrap_err(), DhcpError::NoAddress);
    }

    #[test]
    fn too_short_buffers_are_refused_before_any_field_is_trusted() {
        let full = offer_bytes(XID, [10, 0, 2, 15], &[(OPT_MSG_TYPE, vec![MSG_OFFER])]);
        for cut in 0..=239usize {
            assert!(matches!(
                parse_offer(&full[..cut], XID),
                Err(DhcpError::TooShort)
            ));
        }
    }

    #[test]
    fn the_discover_we_write_has_every_field_a_server_will_check() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let mut buf = [0u8; 512];
        let w = write_discover(&mut buf, mac, XID).expect("512 bytes hold the minimum");
        assert_eq!(w.len(), BOOTP_MIN_LEN);
        assert_eq!(w[0], OP_REQUEST);
        assert_eq!(w[1], HT_ETHERNET);
        assert_eq!(w[2], HLEN_ETHERNET);
        assert_eq!(&w[4..8], &XID.to_be_bytes());
        assert_eq!(u16::from_be_bytes([w[10], w[11]]), FLAG_BROADCAST);
        // Nothing told about ourselves but our hardware address.
        assert!(w[12..28].iter().all(|&b| b == 0));
        assert_eq!(&w[28..34], &mac);
        assert!(w[34..44].iter().all(|&b| b == 0));
        assert_eq!(&w[236..240], &COOKIE);
        assert_eq!(&w[240..243], &[OPT_MSG_TYPE, 1, MSG_DISCOVER]);
        assert_eq!(w[243], OPT_END);
        // The pad tail is legal option-zero space, not garbage.
        assert!(w[244..].iter().all(|&b| b == 0));
    }

    #[test]
    fn discover_into_a_buffer_that_cannot_hold_the_minimum_is_a_clean_refusal() {
        let mut small = [7u8; BOOTP_MIN_LEN - 1];
        assert!(write_discover(&mut small, [0; 6], XID).is_none());
        assert!(small.iter().all(|&b| b == 7), "no partial write leaked");
    }

    #[test]
    fn pad_and_end_options_are_walked_without_consuming_neighbors() {
        // Pads interleaved BETWEEN options must not desynchronize the walk.
        let b = offer_bytes(
            XID,
            [10, 0, 2, 15],
            &[
                (OPT_PAD, vec![]),
                (OPT_MSG_TYPE, vec![MSG_OFFER]),
                (OPT_PAD, vec![]),
                (OPT_PAD, vec![]),
                (OPT_SERVER_ID, four([10, 0, 2, 2])),
                (OPT_PAD, vec![]),
            ],
        );
        let o = parse_offer(&b, XID).expect("pads do not derail the walk");
        assert_eq!(o.server_id, Some([10, 0, 2, 2]));
    }
}
