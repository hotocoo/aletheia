//! The TCP CONNECTION contract, proved on every CPU at boot (REQ-NET-004, ADR-138).
//!
//! Separate from [`crate::tcpconn`] so the state machine stays readable and neither file grows
//! past the size this tree keeps its modules to. The wire's own contract is
//! [`crate::tcp::tcp_suite`]; everything here is about what a connection DOES with segments a
//! peer sends, including a peer that lies, stalls, or disappears.
//!
//! The suite drives real segments through the real parser: each step builds bytes with
//! [`crate::tcp::build_segment`], parses them back with [`crate::tcp::parse_tcp`], and hands the
//! view to the connection. Nothing here constructs a `TcpView` by hand, so a connection cannot
//! pass this suite while disagreeing with what is actually on the wire.

use crate::tcp::{build_segment, parse_tcp, Segment, TcpView, ACK, FIN, PSH, RST, SYN};
use crate::tcpconn::{Connection, TcpEvent, TcpRefusal, TcpState, MAX_RETRIES, MSS, TX_BUF_MIN};
use crate::udpv4::parse_ipv4;

const CLIENT: [u8; 4] = [10, 0, 2, 15];
const SERVER: [u8; 4] = [10, 0, 2, 2];
const CPORT: u16 = 49152;
const SPORT: u16 = 80;
const RTO: u64 = 100;

/// Build a segment as the SERVER would send it, parse it back, and hand it to the connection. The
/// round trip through the wire is the point: the suite tests the stack, not a mock of it.
#[allow(clippy::too_many_arguments)]
fn deliver(
    conn: &mut Connection,
    wire: &mut [u8],
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
    now: u64,
) -> TcpEvent {
    let seg = Segment {
        seq,
        ack,
        flags,
        window,
        payload,
    };
    let Some(bytes) = build_segment(wire, 1, SERVER, CLIENT, SPORT, CPORT, &seg) else {
        return TcpEvent::Refused(TcpRefusal::NotOurs);
    };
    let n = bytes.len();
    let Ok(ip) = parse_ipv4(&wire[..n]) else {
        return TcpEvent::Refused(TcpRefusal::NotOurs);
    };
    let Ok(view) = parse_tcp(&ip) else {
        return TcpEvent::Refused(TcpRefusal::NotOurs);
    };
    conn.on_segment(&view, now)
}

/// Parse a segment the CONNECTION produced, so an assertion is about the bytes it put on the wire.
fn sent<'a>(buf: &'a [u8], n: usize) -> Option<TcpView<'a>> {
    let ip = parse_ipv4(&buf[..n]).ok()?;
    parse_tcp(&ip).ok()
}

/// Drive a connection to ESTABLISHED the way a real open does, returning the next tick.
fn handshake(conn: &mut Connection, tx: &mut [u8], rx: &mut [u8]) -> u64 {
    let mut now = 1_000u64;
    conn.open(0x1000, now).ok();
    let n = conn.poll_transmit(now, tx).unwrap_or(0);
    let syn = sent(tx, n);
    debug_assert!(syn.is_some_and(|s| s.has(SYN)));
    now += 1;
    deliver(conn, rx, 0x5000, 0x1001, SYN | ACK, 4096, &[], now);
    now + 1
}

/// The connection contract, proved on every CPU at boot. `used_bytes` reports the CALLER's heap
/// watermark, because a claim about allocation must be measured where allocation happens.
pub fn tcpconn_suite(
    used_bytes: &mut dyn FnMut() -> usize,
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

    let mut tx = [0u8; TX_BUF_MIN];
    let mut rx = [0u8; TX_BUF_MIN];

    // 1 — a fresh connection is closed, owes the wire nothing, and refuses to send by name. A
    //     stack that quietly buffers before the handshake is a stack that loses those bytes.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let idle = c.poll_transmit(10, &mut tx).is_none();
        let refused = c.send(b"early") == Err(TcpRefusal::NotEstablished);
        check!(
            c.state() == TcpState::Closed && idle && refused && c.refusals == 1,
            "tcpconn: a closed connection transmits nothing and refuses to send by name"
        );
    }

    // 2 — open emits exactly one SYN, and nothing more until the timer expires. A connection that
    //     retransmits on every poll is a flood with a state machine attached.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        c.open(0x2000, 500).ok();
        let first = c.poll_transmit(500, &mut tx).unwrap_or(0);
        let is_syn = sent(&tx, first).is_some_and(|s| {
            s.has(SYN) && !s.has(ACK) && s.seq == 0x2000 && s.sport == CPORT && s.dport == SPORT
        });
        let quiet = c.poll_transmit(500 + RTO - 1, &mut tx).is_none();
        check!(
            is_syn && quiet && c.segments_out == 1 && c.state() == TcpState::SynSent,
            "tcpconn: open sends one SYN and stays quiet until the timer expires"
        );
    }

    // 3 — a peer that never answers is declared GONE after the budget, not waited on forever.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        c.open(0x3000, 0).ok();
        // Stepped one tick at a time so the SPACING is observed: each wait doubles (RFC 6298
        // section 5.5), and after MAX_RETRIES sends the peer is gone, not waited on forever.
        let mut sends: [u64; MAX_RETRIES as usize + 1] = [0; MAX_RETRIES as usize + 1];
        let mut n = 0usize;
        let mut now = 0u64;
        while now <= RTO * 64 && c.state() != TcpState::Closed {
            if c.poll_transmit(now, &mut tx).is_some() && n < sends.len() {
                sends[n] = now;
                n += 1;
            }
            now += 1;
        }
        let doubling = (1..n).all(|i| sends[i] - sends[i - 1] == RTO << (i as u32 - 1));
        check!(
            n == MAX_RETRIES as usize && doubling && c.state() == TcpState::Closed,
            "tcpconn: an unanswered SYN is retransmitted to the budget, then the peer is gone"
        );
    }

    // 4 — the handshake completes ONLY on a SYN+ACK that acknowledges our SYN. An acknowledgement
    //     of a sequence number we never sent is a peer guessing, and is refused by name.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        c.open(0x4000, 0).ok();
        c.poll_transmit(0, &mut tx);
        let wrong = deliver(&mut c, &mut rx, 0x9000, 0x4099, SYN | ACK, 4096, &[], 1);
        let still_syn_sent = c.state() == TcpState::SynSent;
        let right = deliver(&mut c, &mut rx, 0x9000, 0x4001, SYN | ACK, 4096, &[], 2);
        check!(
            wrong == TcpEvent::Refused(TcpRefusal::AckTooHigh)
                && still_syn_sent
                && right == TcpEvent::Connected
                && c.state() == TcpState::Established,
            "tcpconn: only a SYN+ACK acknowledging our own SYN opens the connection"
        );
    }

    // 5 — staged data goes out bounded by the peer's window and by one MSS, and an acknowledgement
    //     frees exactly the bytes it covers.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        let payload = [b'x'; MSS + 64];
        let staged = c.send(&payload).unwrap_or(0);
        let n1 = c.poll_transmit(now, &mut tx).unwrap_or(0);
        let first = sent(&tx, n1);
        let first_ok = first.is_some_and(|s| s.payload.len() == MSS && s.has(ACK) && s.has(PSH));
        // The peer acknowledges half of what is in flight; exactly that much is freed.
        let half = (MSS / 2) as u32;
        deliver(
            &mut c,
            &mut rx,
            0x5001,
            0x1001u32.wrapping_add(half),
            ACK,
            4096,
            &[],
            now + 1,
        );
        check!(
            staged == MSS + 64
                && first_ok
                && c.unacked() == staged - half as usize
                && c.bytes_out == MSS as u64,
            "tcpconn: data is sent one MSS at a time and an acknowledgement frees exactly its bytes"
        );
    }

    // 6 — a peer's zero window stops transmission. A stack that sends anyway is the reason the
    //     other end has no memory left.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        deliver(&mut c, &mut rx, 0x5001, 0x1001, ACK, 0, &[], now);
        c.send(b"blocked").ok();
        let quiet_data = match c.poll_transmit(now + 1, &mut tx) {
            None => true,
            Some(n1) => sent(&tx, n1).is_some_and(|s| s.payload.is_empty()),
        };
        check!(
            quiet_data && c.unacked() == b"blocked".len(),
            "tcpconn: a zero window stops data, and the bytes stay staged rather than lost"
        );
    }

    // 7 — an acknowledgement of bytes never sent is refused by name. Believing it would move the
    //     send window past what exists and make every later comparison nonsense.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        let before = c.refusals;
        // One thousand bytes beyond what was ever sent. (A number half the sequence space away
        // is not "ahead" at all under wrapping comparison — it reads as an old duplicate, and is
        // ignored rather than refused, which is the correct answer for it.)
        let ev = deliver(&mut c, &mut rx, 0x5001, 0x1001 + 1000, ACK, 4096, &[], now);
        check!(
            ev == TcpEvent::Refused(TcpRefusal::AckTooHigh)
                && c.refusals == before + 1
                && c.state() == TcpState::Established,
            "tcpconn: an acknowledgement of data never sent is refused and changes nothing"
        );
    }

    // 8 — data that arrives in order is readable exactly once, and the acknowledgement advances by
    //     exactly its length.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        let ev = deliver(
            &mut c,
            &mut rx,
            0x5001,
            0x1001,
            ACK | PSH,
            4096,
            b"HTTP/1.1",
            now,
        );
        let mut out = [0u8; 16];
        let got = c.recv(&mut out);
        let drained = c.recv(&mut out) == 0;
        let n1 = c.poll_transmit(now + 1, &mut tx).unwrap_or(0);
        let acked = sent(&tx, n1).is_some_and(|s| s.ack == 0x5001 + 8 && s.has(ACK));
        check!(
            ev == TcpEvent::Data(8)
                && got == 8
                && &out[..8] == b"HTTP/1.1"
                && drained
                && acked
                && c.bytes_in == 8,
            "tcpconn: in-order data is readable once and acknowledged by exactly its length"
        );
    }

    // 9 — an out-of-order segment is dropped and RE-acknowledged, never reassembled. The peer's
    //     retransmission is what makes progress, and the refusal is counted.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        let ev = deliver(
            &mut c,
            &mut rx,
            0x5001 + 100,
            0x1001,
            ACK | PSH,
            4096,
            b"future",
            now,
        );
        let n1 = c.poll_transmit(now + 1, &mut tx).unwrap_or(0);
        let reacked = sent(&tx, n1).is_some_and(|s| s.ack == 0x5001);
        check!(
            ev == TcpEvent::Refused(TcpRefusal::OutOfWindow) && c.readable() == 0 && reacked,
            "tcpconn: an out-of-order segment is dropped, re-acknowledged and counted"
        );
    }

    // 10 — data with no room is dropped WITHOUT advancing the acknowledgement, so the peer sends it
    //      again after the application reads. Advancing here is how a stack loses bytes silently.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let mut now = handshake(&mut c, &mut tx, &mut rx);
        let chunk = [b'y'; MSS];
        let mut seq = 0x5001u32;
        let mut refused = TcpEvent::Nothing;
        // Fill the receive buffer, then send one more segment into a full one.
        for _ in 0..(crate::tcpconn::RECV_CAP / MSS + 1) {
            refused = deliver(&mut c, &mut rx, seq, 0x1001, ACK | PSH, 4096, &chunk, now);
            if refused == TcpEvent::Refused(TcpRefusal::RecvFull) {
                break;
            }
            seq = seq.wrapping_add(MSS as u32);
            now += 1;
        }
        let n1 = c.poll_transmit(now + 1, &mut tx).unwrap_or(0);
        let stalled = sent(&tx, n1).is_some_and(|s| s.ack == seq);
        check!(
            refused == TcpEvent::Refused(TcpRefusal::RecvFull) && stalled,
            "tcpconn: data with no room is dropped without acknowledging it"
        );
    }

    // 11 — a reset ends the connection by name, at once, and discards what was staged.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        c.send(b"in flight").ok();
        let ev = deliver(&mut c, &mut rx, 0x5001, 0x1001, RST, 0, &[], now);
        check!(
            ev == TcpEvent::Ended(TcpRefusal::Reset)
                && c.state() == TcpState::Closed
                && c.unacked() == 0
                && c.poll_transmit(now + RTO, &mut tx).is_none(),
            "tcpconn: a reset ends the connection by name and it transmits nothing after"
        );
    }

    // 12 — a close sends the FIN only after the staged bytes are acknowledged, and the peer's own
    //      FIN carries the connection through to TIME-WAIT.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let mut now = handshake(&mut c, &mut tx, &mut rx);
        c.send(b"bye").ok();
        let n1 = c.poll_transmit(now, &mut tx).unwrap_or(0);
        let data_first = sent(&tx, n1).is_some_and(|s| s.payload == b"bye" && !s.has(FIN));
        c.close().ok();
        // Nothing staged is acknowledged yet, so no FIN may go out.
        let no_fin_yet = match c.poll_transmit(now, &mut tx) {
            None => true,
            Some(k) => sent(&tx, k).is_some_and(|s| !s.has(FIN)),
        };
        now += 1;
        deliver(&mut c, &mut rx, 0x5001, 0x1001 + 3, ACK, 4096, &[], now);
        let n2 = c.poll_transmit(now, &mut tx).unwrap_or(0);
        let fin = sent(&tx, n2).is_some_and(|s| s.has(FIN) && s.has(ACK));
        let in_fin_wait = c.state() == TcpState::FinWait1;
        now += 1;
        deliver(&mut c, &mut rx, 0x5001, 0x1001 + 4, ACK, 4096, &[], now);
        let fin_acked = c.state() == TcpState::FinWait2;
        now += 1;
        let peer_fin = deliver(
            &mut c,
            &mut rx,
            0x5001,
            0x1001 + 4,
            ACK | FIN,
            4096,
            &[],
            now,
        );
        check!(
            data_first
                && no_fin_yet
                && fin
                && in_fin_wait
                && fin_acked
                && peer_fin == TcpEvent::PeerClosed
                && c.state() == TcpState::TimeWait,
            "tcpconn: a close sends FIN after the staged bytes and walks to TIME-WAIT"
        );
    }

    // 13 — the peer closing first puts the connection in CLOSE-WAIT, where the application may
    //      still send, and our own close then walks it to LAST-ACK and TIME-WAIT.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let mut now = handshake(&mut c, &mut tx, &mut rx);
        let ev = deliver(&mut c, &mut rx, 0x5001, 0x1001, ACK | FIN, 4096, &[], now);
        let close_wait = c.state() == TcpState::CloseWait;
        let may_send = c.send(b"last words").is_ok();
        now += 1;
        c.poll_transmit(now, &mut tx);
        deliver(&mut c, &mut rx, 0x5002, 0x1001 + 10, ACK, 4096, &[], now);
        c.close().ok();
        now += 1;
        let n1 = c.poll_transmit(now, &mut tx).unwrap_or(0);
        let fin = sent(&tx, n1).is_some_and(|s| s.has(FIN));
        let last_ack = c.state() == TcpState::LastAck;
        now += 1;
        deliver(&mut c, &mut rx, 0x5002, 0x1001 + 11, ACK, 4096, &[], now);
        check!(
            ev == TcpEvent::PeerClosed
                && close_wait
                && may_send
                && fin
                && last_ack
                && c.state() == TcpState::TimeWait,
            "tcpconn: the peer's FIN opens CLOSE-WAIT, and ours closes the connection from there"
        );
    }

    // 14 — a segment for another port pair is not ours. It is refused by name and never counted as
    //      a segment this connection received.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let now = handshake(&mut c, &mut tx, &mut rx);
        let seg = Segment {
            seq: 0x5001,
            ack: 0x1001,
            flags: ACK | PSH,
            window: 4096,
            payload: b"not yours",
        };
        let before = c.segments_in;
        let ev = match build_segment(&mut rx, 9, SERVER, CLIENT, 443, CPORT, &seg) {
            Some(bytes) => {
                let k = bytes.len();
                match parse_ipv4(&rx[..k]).ok().and_then(|ip| parse_tcp(&ip).ok()) {
                    Some(view) => c.on_segment(&view, now),
                    None => TcpEvent::Nothing,
                }
            }
            None => TcpEvent::Nothing,
        };
        check!(
            ev == TcpEvent::Refused(TcpRefusal::NotOurs)
                && c.segments_in == before
                && c.readable() == 0,
            "tcpconn: a segment for another port pair is refused by name and not counted as ours"
        );
    }

    // 15 — a whole connection — handshake, data both ways, close — allocates NOTHING. On a heap
    //      that never frees, a per-segment allocation is a leak measured in connections.
    {
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, RTO);
        let mut now = handshake(&mut c, &mut tx, &mut rx);
        let before = used_bytes();
        let payload = [b'z'; 64];
        let mut seq = 0x5001u32;
        let mut ack = 0x1001u32;
        #[allow(clippy::explicit_counter_loop)]
        for _ in 0..200 {
            c.send(&payload).ok();
            let k = c.poll_transmit(now, &mut tx).unwrap_or(0);
            let flight = sent(&tx, k).map(|s| s.payload.len() as u32).unwrap_or(0);
            ack = ack.wrapping_add(flight);
            deliver(&mut c, &mut rx, seq, ack, ACK | PSH, 4096, &payload, now);
            seq = seq.wrapping_add(payload.len() as u32);
            let mut sink = [0u8; 64];
            c.recv(&mut sink);
            now += 1;
        }
        let after = used_bytes();
        check!(
            after == before && c.segments_out > 100 && c.bytes_in > 1000,
            "tcpconn: two hundred segments in and out allocate nothing at all"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_connection_invariant() {
        let mut seen = 0;
        let n = tcpconn_suite(&mut || 0, |_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the tcp connection suite should hold");
        assert_eq!(n, 15);
        assert_eq!(seen, 15);
    }
}
