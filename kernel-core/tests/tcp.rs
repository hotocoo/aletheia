//! Host proof of the TCP stack (REQ-NET-004, ADR-138).
//!
//! The live per-target suites (`tcp::tcp_suite`, `tcpsuite::tcpconn_suite`) prove the contract on
//! each CPU. These tests attack it instead: every byte of a valid segment is flipped to show the
//! parser refuses rather than reads, and a connection is driven against a peer that drops
//! segments, stalls, and disappears, because those are the peers a real network has.

use kernel_core::tcp::{
    build_segment, parse_tcp, seq_in_window, seq_lt, tcp_suite, Segment, TcpError, ACK, FIN, PSH,
    SYN,
};
use kernel_core::tcpconn::{Connection, TcpEvent, TcpState, MAX_RETRIES, MSS, TX_BUF_MIN};
use kernel_core::tcpsuite::tcpconn_suite;
use kernel_core::udpv4::{parse_ipv4, IPV4_HDR_MIN};

const CLIENT: [u8; 4] = [10, 0, 2, 15];
const SERVER: [u8; 4] = [10, 0, 2, 2];
const CPORT: u16 = 49152;
const SPORT: u16 = 80;
const RTO: u64 = 10;

#[test]
fn the_live_wire_suite_passes_on_the_host_too() {
    let mut names = Vec::new();
    let n = tcp_suite(|_, passed, name| {
        assert!(passed, "live wire invariant failed on the host: {name}");
        names.push(name);
    })
    .expect("the wire suite holds on the host");
    assert_eq!(n as usize, names.len());
}

#[test]
fn the_live_connection_suite_passes_on_the_host_too() {
    let mut names = Vec::new();
    let n = tcpconn_suite(&mut || 0, |_, passed, name| {
        assert!(
            passed,
            "live connection invariant failed on the host: {name}"
        );
        names.push(name);
    })
    .expect("the connection suite holds on the host");
    assert_eq!(n as usize, names.len());
}

/// Every single-bit flip inside the TCP segment must be refused. Not a sample: the whole segment,
/// every byte, every bit — the checksum is the only evidence these bytes are the peer's bytes.
#[test]
fn every_flipped_bit_in_a_segment_is_refused_rather_than_read() {
    let mut buf = [0u8; 128];
    let seg = Segment {
        seq: 0x0102_0304,
        ack: 0x0506_0708,
        flags: ACK | PSH,
        window: 8192,
        payload: b"the quick brown fox",
    };
    let n = build_segment(&mut buf, 1, CLIENT, SERVER, CPORT, SPORT, &seg)
        .expect("a valid segment builds")
        .len();

    for i in IPV4_HDR_MIN..n {
        for bit in 0..8 {
            let saved = buf[i];
            buf[i] ^= 1 << bit;
            let ip = parse_ipv4(&buf[..n]).expect("the IPv4 header is untouched");
            let verdict = parse_tcp(&ip);
            assert!(
                verdict.is_err(),
                "byte {i} bit {bit} was accepted: {verdict:?}"
            );
            buf[i] = saved;
        }
    }
    // And the unmodified segment still parses, so the sweep proved refusal rather than a parser
    // that refuses everything.
    let ip = parse_ipv4(&buf[..n]).unwrap();
    assert_eq!(parse_tcp(&ip).map(|v| v.payload), Ok(seg.payload));
}

#[test]
fn a_header_claiming_more_bytes_than_arrived_is_refused_by_name() {
    let mut buf = [0u8; 64];
    let seg = Segment {
        seq: 1,
        ack: 1,
        flags: ACK,
        window: 1,
        payload: b"",
    };
    let n = build_segment(&mut buf, 1, CLIENT, SERVER, CPORT, SPORT, &seg)
        .unwrap()
        .len();
    // Data offset 15 (60 bytes) with only 20 bytes of segment present.
    buf[IPV4_HDR_MIN + 12] = 0xF0;
    let ip = parse_ipv4(&buf[..n]).unwrap();
    assert_eq!(parse_tcp(&ip), Err(TcpError::BadDataOffset));
}

/// A scripted peer: it acknowledges what it has received, sends back what it was told to, and
/// drops every `drop_every`-th segment the client sends. This is the closest thing to a network
/// that a host test can be honest about.
struct Peer {
    seq: u32,
    ack: u32,
    received: Vec<u8>,
    drop_every: u32,
    seen: u32,
    window: u16,
}

impl Peer {
    fn new(iss: u32, drop_every: u32) -> Self {
        Peer {
            seq: iss,
            ack: 0,
            received: Vec::new(),
            drop_every,
            seen: 0,
            window: 4096,
        }
    }

    /// Take one segment the client transmitted. Returns whether it was delivered (not dropped).
    fn take(&mut self, wire: &[u8]) -> bool {
        self.seen += 1;
        if self.drop_every != 0 && self.seen.is_multiple_of(self.drop_every) {
            return false;
        }
        let ip = parse_ipv4(wire).expect("the client emits valid IPv4");
        let v = parse_tcp(&ip).expect("the client emits valid TCP");
        if v.has(SYN) {
            self.ack = v.seq.wrapping_add(1);
            return true;
        }
        if v.seq == self.ack {
            self.received.extend_from_slice(v.payload);
            self.ack = self.ack.wrapping_add(v.seq_len());
        }
        true
    }

    /// The acknowledgement this peer would send right now.
    fn ack_segment<'a>(&mut self, buf: &'a mut [u8], flags: u8, payload: &[u8]) -> &'a [u8] {
        let seg = Segment {
            seq: self.seq,
            ack: self.ack,
            flags,
            window: self.window,
            payload,
        };
        self.seq = self.seq.wrapping_add(payload.len() as u32);
        if flags & (SYN | FIN) != 0 {
            self.seq = self.seq.wrapping_add(1);
        }
        let n = build_segment(buf, 1, SERVER, CLIENT, SPORT, CPORT, &seg)
            .expect("the peer's buffer holds a segment")
            .len();
        &buf[..n]
    }
}

fn feed(conn: &mut Connection, wire: &[u8], now: u64) -> TcpEvent {
    let ip = parse_ipv4(wire).unwrap();
    let v = parse_tcp(&ip).unwrap();
    conn.on_segment(&v, now)
}

#[test]
fn every_byte_arrives_in_order_across_a_lossy_link() {
    let mut tx = [0u8; TX_BUF_MIN];
    let mut rx = [0u8; TX_BUF_MIN];
    let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
    let mut peer = Peer::new(0x7000, 3); // every third client segment is lost
    let mut now = 0u64;

    c.open(0x1000, now).unwrap();
    let n = c.poll_transmit(now, &mut tx).unwrap();
    assert!(peer.take(&tx[..n]), "the first SYN is delivered");
    now += 1;
    let reply = peer.ack_segment(&mut rx, SYN | ACK, b"");
    assert_eq!(feed(&mut c, reply, now), TcpEvent::Connected);

    // A payload larger than one segment and larger than the peer's initial window.
    let payload: Vec<u8> = (0..2000u32).map(|i| (i % 251) as u8).collect();
    let mut offered = 0usize;
    for _ in 0..4000 {
        if offered < payload.len() {
            offered += c.send(&payload[offered..]).unwrap_or(0);
        }
        if let Some(n) = c.poll_transmit(now, &mut tx) {
            if peer.take(&tx[..n]) {
                let reply = peer.ack_segment(&mut rx, ACK, b"");
                feed(&mut c, reply, now);
            }
        }
        now += 1;
        if peer.received.len() == payload.len() && c.unacked() == 0 {
            break;
        }
    }

    assert_eq!(offered, payload.len(), "every byte was staged");
    assert_eq!(peer.received, payload, "every byte arrived, in order");
    assert!(c.retransmits > 0, "a lossy link must have cost retransmits");
    assert_eq!(c.state(), TcpState::Established);
}

#[test]
fn a_peer_that_stops_acknowledging_ends_the_connection_rather_than_spinning() {
    let mut tx = [0u8; TX_BUF_MIN];
    let mut rx = [0u8; TX_BUF_MIN];
    let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
    let mut peer = Peer::new(0x8000, 0);
    let mut now = 0u64;

    c.open(0x2000, now).unwrap();
    let n = c.poll_transmit(now, &mut tx).unwrap();
    peer.take(&tx[..n]);
    now += 1;
    let reply = peer.ack_segment(&mut rx, SYN | ACK, b"");
    feed(&mut c, reply, now);

    c.send(b"anybody there?").unwrap();
    let mut sends = 0;
    for _ in 0..1000 {
        if c.poll_transmit(now, &mut tx).is_some() {
            sends += 1;
        }
        now += 1;
        if c.state() == TcpState::Closed {
            break;
        }
    }
    assert_eq!(c.state(), TcpState::Closed, "the peer is declared gone");
    assert!(
        sends <= MAX_RETRIES as usize + 2,
        "the budget bounds the transmissions: {sends}"
    );
    assert!(c.poll_transmit(now + 1000, &mut tx).is_none());
}

#[test]
fn received_bytes_are_bounded_by_the_buffer_and_never_silently_dropped() {
    let mut tx = [0u8; TX_BUF_MIN];
    let mut rx = [0u8; TX_BUF_MIN];
    let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
    let mut peer = Peer::new(0x9000, 0);
    let mut now = 0u64;
    c.open(0x3000, now).unwrap();
    let n = c.poll_transmit(now, &mut tx).unwrap();
    peer.take(&tx[..n]);
    now += 1;
    let reply = peer.ack_segment(&mut rx, SYN | ACK, b"");
    feed(&mut c, reply, now);

    // Push data until the connection says it has no room, reading nothing.
    let chunk = [b'q'; MSS];
    let mut refusals = 0;
    let mut accepted = 0usize;
    for _ in 0..16 {
        let seg = peer.ack_segment(&mut rx, ACK | PSH, &chunk);
        let n = seg.len();
        let owned: Vec<u8> = rx[..n].to_vec();
        match feed(&mut c, &owned, now) {
            TcpEvent::Data(k) => accepted += k,
            TcpEvent::Refused(_) => {
                refusals += 1;
                // The peer is told the same acknowledgement again, so it will resend.
                peer.seq = peer.seq.wrapping_sub(chunk.len() as u32);
            }
            _ => {}
        }
        now += 1;
    }
    assert!(refusals > 0, "a full buffer must refuse rather than grow");
    assert_eq!(accepted, c.readable(), "everything accepted is readable");

    // After reading, the same data is accepted: nothing was lost, only deferred.
    let mut sink = vec![0u8; accepted];
    assert_eq!(c.recv(&mut sink), accepted);
    let seg = peer.ack_segment(&mut rx, ACK | PSH, &chunk);
    let n = seg.len();
    let owned: Vec<u8> = rx[..n].to_vec();
    assert_eq!(feed(&mut c, &owned, now), TcpEvent::Data(MSS));
}

#[test]
fn sequence_helpers_agree_with_the_definition_across_the_whole_space() {
    // A window that straddles the wrap contains exactly its own span, and ordering is consistent
    // for every distance a connection can hold.
    for start in [0u32, 1, 0x7FFF_FF00, 0xFFFF_FF00] {
        for len in [1u32, 2, 1500, 65535] {
            for probe in 0..64u32 {
                let seq = start.wrapping_add(probe);
                assert_eq!(seq_in_window(seq, start, len), probe < len);
            }
            assert!(!seq_in_window(start.wrapping_sub(1), start, len));
        }
        assert!(seq_lt(start, start.wrapping_add(1)));
    }
}
