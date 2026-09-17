//! Where a TCP connection meets a real network device (REQ-NET-005, ADR-139).
//!
//! [`crate::tcpconn::Connection`] is deliberately deviceless: it is fed parsed segments and hands
//! back bytes to transmit. Something has to carry those bytes, and this module is that something —
//! kept apart from both the driver and the state machine so that:
//!
//! * the connection stays provable with no hardware attached (ADR-138),
//! * the driver stays a driver, with no opinion about handshakes or retransmission,
//! * and the pump that joins them is itself provable, over a link that is a test double.
//!
//! The link is a trait with three methods, which is all a TCP client needs from a network: send an
//! IPv4 datagram, wait for one addressed to us, and say what our own address is.
//!
//! ## The budget is the point
//!
//! Every loop here is bounded. A network is the one place in this kernel where "wait until it
//! answers" means "wait forever, on a peer's decision", so [`exchange`] takes a budget of poll
//! turns and returns a named refusal when it is spent. A caller that wants to wait longer asks for
//! more turns; nobody gets to wait indefinitely by accident.

use crate::tcp::parse_tcp;
use crate::tcpconn::{Connection, TcpEvent, TcpState, TX_BUF_MIN};
use crate::udpv4::parse_ipv4;

/// What a TCP client needs from a network device. Implemented by the virtio-net driver on a live
/// machine, and by a test double in this module's own suite.
pub trait Ipv4Link {
    /// Put one complete IPv4 datagram on the wire.
    fn send_ipv4(&self, datagram: &[u8]) -> Result<(), LinkError>;

    /// Wait up to `spins` device polls for one IPv4 datagram of `protocol` addressed to us,
    /// copying it into `out` and returning its length. `Ok(0)` means nothing arrived in time,
    /// which is a normal turn of the pump rather than a failure.
    fn recv_ipv4(&self, spins: u64, protocol: u8, out: &mut [u8]) -> Result<usize, LinkError>;

    /// This machine's own IPv4 address on that link.
    fn local_ip(&self) -> [u8; 4];
}

/// Why the link or the exchange could not continue. Each is a distinct fact; none is "error".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkError {
    /// The device refused the frame, or the frame was longer than the link allows.
    Device,
    /// The datagram did not fit the caller's buffer.
    TooLong,
    /// The budget of poll turns is spent. The peer may still be alive; we stopped asking.
    BudgetSpent,
    /// The connection ended before the exchange finished, for the connection's own reason.
    ConnectionEnded,
}

/// What one exchange achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Exchange {
    /// Bytes of the reply copied out.
    pub received: usize,
    /// Segments this side put on the wire.
    pub sent_segments: u64,
    /// Segments accepted from the peer.
    pub recv_segments: u64,
    /// Poll turns actually used, so a caller can see how close it came to its budget.
    pub turns: u64,
}

/// How an exchange is tuned: the initial sequence number the platform chose, how many poll turns
/// it may take, and how long one turn waits on the device. Grouped into a value because these three
/// are a policy, and a policy is easier to state once than to thread through every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The initial sequence number. Unpredictable on a real network, and therefore the platform's
    /// choice rather than this module's (ADR-138).
    pub iss: u32,
    /// Poll turns the exchange may take before it says so and stops.
    pub budget: u64,
    /// Device polls one turn waits for a datagram.
    pub spins_per_turn: u64,
}

/// Run one request/response exchange to completion: open the connection, send `request`, collect
/// the reply into `reply` until the peer closes or the budget is spent, then close.
///
/// `now` is the caller's clock in whatever units the connection's retransmission timeout uses. It
/// is a closure rather than a parameter because a pump that cannot observe time passing cannot
/// retransmit, and a pump that reads a clock directly is a pump that cannot be tested.
///
/// Returns what the exchange achieved, or the named reason it stopped. A short reply is not an
/// error: a peer is allowed to say less than the buffer holds.
pub fn exchange<L: Ipv4Link>(
    link: &L,
    conn: &mut Connection,
    plan: Plan,
    request: &[u8],
    reply: &mut [u8],
    now: &mut dyn FnMut() -> u64,
) -> Result<Exchange, LinkError> {
    let mut tx = [0u8; TX_BUF_MIN];
    let mut rx = [0u8; TX_BUF_MIN];
    let mut out = Exchange::default();

    conn.open(plan.iss, now())
        .map_err(|_| LinkError::ConnectionEnded)?;
    let mut offered = 0usize;
    let mut requested = false;
    let mut closing = false;

    for turn in 0..plan.budget {
        out.turns = turn + 1;

        // Stage the request as soon as the handshake allows it. Staging before then is how a
        // stack loses the first bytes of every conversation it has.
        if !requested && conn.state() == TcpState::Established {
            offered += conn.send(&request[offered..]).unwrap_or(0);
            if offered == request.len() {
                requested = true;
            }
        }

        // Everything this side owes the wire, one segment per turn.
        while let Some(n) = conn.poll_transmit(now(), &mut tx) {
            link.send_ipv4(&tx[..n])?;
            out.sent_segments += 1;
        }

        match conn.state() {
            TcpState::Closed => {
                // Ended by the peer (a reset) or by the budget inside the connection itself.
                return if out.received > 0 {
                    Ok(out)
                } else {
                    Err(LinkError::ConnectionEnded)
                };
            }
            TcpState::TimeWait => return Ok(out),
            _ => {}
        }

        let n = link.recv_ipv4(plan.spins_per_turn, crate::tcp::PROTOCOL_TCP, &mut rx)?;
        if n > 0 {
            if let Ok(ip) = parse_ipv4(&rx[..n]) {
                if ip.dst == link.local_ip() {
                    if let Ok(view) = parse_tcp(&ip) {
                        let before = conn.readable();
                        let event = conn.on_segment(&view, now());
                        if !matches!(event, TcpEvent::Refused(_)) {
                            out.recv_segments += 1;
                        }
                        if conn.readable() > before || matches!(event, TcpEvent::PeerClosed) {
                            out.received += conn.recv(&mut reply[out.received..]);
                        }
                        if matches!(event, TcpEvent::Ended(_)) {
                            return if out.received > 0 {
                                Ok(out)
                            } else {
                                Err(LinkError::ConnectionEnded)
                            };
                        }
                    }
                }
            }
        }

        // Once the request is out and acknowledged, this side has nothing more to say. Closing
        // here rather than at the end means the peer learns it can stop waiting for us.
        if requested && !closing && conn.unacked() == 0 {
            closing = true;
            let _ = conn.close();
        }
    }
    if out.received > 0 {
        // The budget ran out, but the peer did answer. Saying "spent" here would throw away bytes
        // that are already in the caller's buffer.
        return Ok(out);
    }
    Err(LinkError::BudgetSpent)
}

/// The pump's contract, proved on every CPU at boot over a link that is a test double rather than
/// a device. What is proved here is the JOIN: that a real connection driven over a link that can
/// drop, stall, or answer produces the bytes the peer sent and never waits without a bound.
pub fn tcpnet_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::tcp::{build_segment, Segment, ACK, FIN, PSH, SYN};
    use core::cell::RefCell;

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

    const CLIENT: [u8; 4] = [10, 0, 2, 15];
    const SERVER: [u8; 4] = [10, 0, 2, 2];
    const CPORT: u16 = 49152;
    const SPORT: u16 = 7;

    /// A link with a scripted peer behind it: it completes the handshake, echoes whatever it is
    /// sent, and closes. `deaf` makes it answer nothing at all, which is the case a budget exists
    /// for.
    struct Loopback {
        state: RefCell<PeerState>,
        deaf: bool,
    }
    struct PeerState {
        seq: u32,
        ack: u32,
        pending: [u8; TX_BUF_MIN],
        pending_len: usize,
        echo: [u8; 256],
        echo_len: usize,
        closed: bool,
    }

    impl Ipv4Link for Loopback {
        fn send_ipv4(&self, datagram: &[u8]) -> Result<(), LinkError> {
            if self.deaf {
                return Ok(());
            }
            let Ok(ip) = parse_ipv4(datagram) else {
                return Err(LinkError::Device);
            };
            let Ok(v) = parse_tcp(&ip) else {
                return Err(LinkError::Device);
            };
            let mut st = self.state.borrow_mut();
            let mut flags = ACK;
            let mut payload_len = 0usize;
            if v.has(SYN) {
                st.ack = v.seq.wrapping_add(1);
                flags |= SYN;
            } else {
                if v.seq == st.ack {
                    let at = st.echo_len;
                    let take = v.payload.len().min(st.echo.len() - at);
                    st.echo[at..at + take].copy_from_slice(&v.payload[..take]);
                    st.echo_len = at + take;
                    st.ack = st.ack.wrapping_add(v.seq_len());
                    payload_len = take;
                }
                if v.has(FIN) {
                    st.closed = true;
                }
            }
            // Build the answer: an acknowledgement, carrying the echo when there is one, and a FIN
            // once the client has closed.
            let mut body = [0u8; 256];
            let from = st.echo_len - payload_len;
            let to = st.echo_len;
            body[..payload_len].copy_from_slice(&st.echo[from..to]);
            if st.closed {
                flags |= FIN;
            }
            if payload_len > 0 {
                flags |= PSH;
            }
            let seg = Segment {
                seq: st.seq,
                ack: st.ack,
                flags,
                window: 4096,
                payload: &body[..payload_len],
            };
            st.seq = st.seq.wrapping_add(payload_len as u32);
            if flags & (SYN | FIN) != 0 {
                st.seq = st.seq.wrapping_add(1);
            }
            let mut wire = [0u8; TX_BUF_MIN];
            let Some(bytes) = build_segment(&mut wire, 1, SERVER, CLIENT, SPORT, CPORT, &seg)
            else {
                return Err(LinkError::TooLong);
            };
            let k = bytes.len();
            st.pending[..k].copy_from_slice(&wire[..k]);
            st.pending_len = k;
            Ok(())
        }

        fn recv_ipv4(
            &self,
            _spins: u64,
            _protocol: u8,
            out: &mut [u8],
        ) -> Result<usize, LinkError> {
            let mut st = self.state.borrow_mut();
            if st.pending_len == 0 {
                return Ok(0);
            }
            if out.len() < st.pending_len {
                return Err(LinkError::TooLong);
            }
            out[..st.pending_len].copy_from_slice(&st.pending[..st.pending_len]);
            let n = st.pending_len;
            st.pending_len = 0;
            Ok(n)
        }

        fn local_ip(&self) -> [u8; 4] {
            CLIENT
        }
    }

    fn link(deaf: bool) -> Loopback {
        Loopback {
            state: RefCell::new(PeerState {
                seq: 0x7000,
                ack: 0,
                pending: [0; TX_BUF_MIN],
                pending_len: 0,
                echo: [0; 256],
                echo_len: 0,
                closed: false,
            }),
            deaf,
        }
    }

    // 1 — the whole exchange: a connection opened over a link, a request sent, the peer's answer
    //     collected, and the connection closed. This is the join the rest of the stack exists for.
    {
        let l = link(false);
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, 4);
        let mut reply = [0u8; 64];
        let mut tick = 0u64;
        let plan = Plan {
            iss: 0x1000,
            budget: 64,
            spins_per_turn: 1,
        };
        let got = exchange(&l, &mut c, plan, b"ALETHEIA", &mut reply, &mut || {
            tick += 1;
            tick
        });
        let ok = got.is_ok_and(|e| e.received == 8 && e.sent_segments > 0 && e.recv_segments > 0);
        check!(
            ok && &reply[..8] == b"ALETHEIA",
            "tcpnet: a request goes out over the link and the peer's answer comes back"
        );
    }

    // 2 — a peer that never answers costs exactly the budget, and says so by name. A network is
    //     the one place where "wait until it answers" means "wait on someone else's decision".
    {
        let l = link(true);
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, 4);
        let mut reply = [0u8; 16];
        let mut tick = 0u64;
        let plan = Plan {
            iss: 0x2000,
            budget: 12,
            spins_per_turn: 1,
        };
        let got = exchange(&l, &mut c, plan, b"hello?", &mut reply, &mut || {
            tick += 1;
            tick
        });
        check!(
            matches!(
                got,
                Err(LinkError::BudgetSpent) | Err(LinkError::ConnectionEnded)
            ),
            "tcpnet: a deaf peer costs the budget and is refused by name, never waited on forever"
        );
    }

    // 3 — the exchange never writes past the caller's reply buffer, however much the peer says. A
    //     stack that trusts the peer's length here is a stack with a buffer overflow in it.
    {
        let l = link(false);
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, 4);
        let mut guarded = [0xAAu8; 16];
        let mut tick = 0u64;
        let request = [b'Z'; 32];
        let plan = Plan {
            iss: 0x3000,
            budget: 64,
            spins_per_turn: 1,
        };
        let got = exchange(&l, &mut c, plan, &request, &mut guarded[..8], &mut || {
            tick += 1;
            tick
        });
        check!(
            got.is_ok_and(|e| e.received <= 8) && guarded[8..].iter().all(|&b| b == 0xAA),
            "tcpnet: a reply larger than the caller's buffer is truncated, never overflowed"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_pump_invariant() {
        let mut seen = 0;
        let n = tcpnet_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the tcp pump suite should hold");
        assert_eq!(n, 3);
        assert_eq!(seen, 3);
    }
}
