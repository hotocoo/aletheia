//! A TCP connection as a bounded state machine with no device in it (REQ-NET-004, ADR-138).
//!
//! The wire lives in [`crate::tcp`]; what lives here is the part that has to be right when a peer
//! is hostile, slow, or gone: which segments are acceptable, what an acknowledgement means, when a
//! segment is retransmitted, and when a connection gives up and says so.
//!
//! The shape is the same one the file panel and the scheduler use, for the same reasons:
//!
//! * **No device.** The connection never touches virtio-net. It is FED parsed segments and it
//!   HANDS BACK bytes to transmit, so it can be proved on every CPU at boot with no NIC attached,
//!   and so a slow device can never be a stalled state machine.
//! * **No allocation, ever.** The send and receive buffers are fixed arrays sized at construction.
//!   On a heap that never frees (ADR-063), a per-segment allocation is a leak, and a connection
//!   handles a segment every time the wire does.
//! * **Total, with named refusals.** Every operation is defined in every state. A `send` before
//!   the handshake, an acknowledgement of data never sent, a segment for another port, a peer that
//!   never answers — each is a distinct refusal, counted, never a silent drop.
//!
//! ## What this connection deliberately does NOT do
//!
//! No reassembly of out-of-order segments (they are dropped and re-acknowledged, which makes the
//! peer retransmit), no selective acknowledgement, no window scaling, no RTT estimation (a fixed
//! retransmission timeout), and no simultaneous open. Each omission is a thing a browser's
//! transport can live without on this wire, and each is stated here rather than discovered by
//! someone reading the code for it.

use crate::tcp::{
    build_segment, seq_leq, seq_lt, Segment, TcpView, ACK, FIN, PSH, RST, SYN, TCP_HDR_MIN,
};
use crate::udpv4::IPV4_HDR_MIN;

/// Bytes staged for transmission but not yet acknowledged. A connection refuses to stage more
/// rather than growing, because growing is how a peer that stops acknowledging exhausts a heap.
pub const SEND_CAP: usize = 1024;
/// Bytes received and not yet read by the application above.
pub const RECV_CAP: usize = 2048;
/// The largest payload this stack puts in one segment. Conservative on purpose: 536 is the IPv4
/// default MSS every host must accept without knowing the path.
pub const MSS: usize = 536;
/// The smallest buffer `poll_transmit` can ever need.
pub const TX_BUF_MIN: usize = IPV4_HDR_MIN + TCP_HDR_MIN + MSS;
/// How many times one segment is retransmitted before the connection declares the peer gone.
pub const MAX_RETRIES: u8 = 5;

/// Where the connection is. RFC 793's state names, minus the ones a client that never listens can
/// never reach (LISTEN, SYN_RECEIVED).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    SynSent,
    Established,
    /// Our FIN is sent and unacknowledged.
    FinWait1,
    /// Our FIN is acknowledged; the peer may still send.
    FinWait2,
    /// Both sides sent FIN; ours is unacknowledged.
    Closing,
    /// The peer sent FIN first; we may still send.
    CloseWait,
    /// Our FIN followed the peer's and is unacknowledged.
    LastAck,
    /// Both FINs are acknowledged; the connection is over for this stack's purposes.
    TimeWait,
}

/// Why the connection did nothing. Named, so a refusal can be counted and tested rather than
/// inferred from an absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpRefusal {
    /// `open` on a connection that is not closed.
    AlreadyOpen,
    /// `send` before the handshake completed, or after the sending half was closed.
    NotEstablished,
    /// The staged bytes would not fit and the connection will not grow to hold them.
    SendFull,
    /// A segment whose ports are not this connection's pair.
    NotOurs,
    /// A segment outside the receive window: not the next byte expected. Dropped and
    /// re-acknowledged rather than reassembled.
    OutOfWindow,
    /// An acknowledgement of bytes that were never sent.
    AckTooHigh,
    /// The peer reset the connection.
    Reset,
    /// The retransmission budget is spent; the peer is treated as gone.
    PeerGone,
    /// Received data that does not fit the receive buffer. Dropped without advancing the
    /// acknowledgement, so the peer sends it again once the application has read.
    RecvFull,
}

/// What a received segment meant to the layer above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpEvent {
    /// Nothing that changes the caller's world.
    Nothing,
    /// The handshake completed; the connection may now carry bytes.
    Connected,
    /// Payload arrived and is readable with [`Connection::recv`].
    Data(usize),
    /// The peer closed its sending half.
    PeerClosed,
    /// The connection ended, for the named reason.
    Ended(TcpRefusal),
    /// The segment was refused, for the named reason.
    Refused(TcpRefusal),
}

/// One client connection.
pub struct Connection {
    state: TcpState,
    local: [u8; 4],
    remote: [u8; 4],
    lport: u16,
    rport: u16,

    /// Oldest unacknowledged sequence number.
    snd_una: u32,
    /// Next sequence number to use for new data.
    snd_nxt: u32,
    /// What the peer said it can receive.
    snd_wnd: u32,
    /// Next sequence number expected from the peer.
    rcv_nxt: u32,

    /// Staged bytes. `send[..send_len]` starts at `snd_una`; `in_flight` of them have been sent.
    send: [u8; SEND_CAP],
    send_len: usize,
    in_flight: usize,

    recv: [u8; RECV_CAP],
    recv_len: usize,

    /// The application asked to close; a FIN follows once the staged bytes are acknowledged.
    close_pending: bool,
    /// The sequence number our FIN occupies, once it has been sent. Recorded rather than
    /// recomputed: `snd_una` moves when the acknowledgement that covers the FIN arrives, so a
    /// derived answer would be computed from a number that has already changed.
    fin_seq: Option<u32>,
    /// Something arrived that the peer must be told we saw.
    ack_due: bool,
    /// The handshake's SYN has not gone out yet. A separate fact from the retransmission timer,
    /// because a connection opened at tick zero has no "one timeout ago" to point at, and must not
    /// wait a whole timeout before saying hello.
    syn_due: bool,

    /// Fixed retransmission timeout, in the caller's own tick units.
    rto: u64,
    last_tx: u64,
    retries: u8,
    ident: u16,

    /// Counters. Every one of these is a fact a boot log or an operator can ask for.
    pub segments_in: u64,
    pub segments_out: u64,
    pub retransmits: u64,
    pub refusals: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl Connection {
    /// A closed connection between this address/port and that one. Nothing is sent, and nothing is
    /// allocated here or ever after.
    pub fn new(local: [u8; 4], lport: u16, remote: [u8; 4], rport: u16, rto: u64) -> Self {
        Connection {
            state: TcpState::Closed,
            local,
            remote,
            lport,
            rport,
            snd_una: 0,
            snd_nxt: 0,
            snd_wnd: 0,
            rcv_nxt: 0,
            send: [0; SEND_CAP],
            send_len: 0,
            in_flight: 0,
            recv: [0; RECV_CAP],
            recv_len: 0,
            close_pending: false,
            fin_seq: None,
            ack_due: false,
            syn_due: false,
            rto: rto.max(1),
            last_tx: 0,
            retries: 0,
            ident: 0,
            segments_in: 0,
            segments_out: 0,
            retransmits: 0,
            refusals: 0,
            bytes_in: 0,
            bytes_out: 0,
        }
    }

    pub fn state(&self) -> TcpState {
        self.state
    }

    /// Bytes readable right now.
    pub fn readable(&self) -> usize {
        self.recv_len
    }

    /// Bytes staged and not yet acknowledged.
    pub fn unacked(&self) -> usize {
        self.send_len
    }

    /// Room left for staged bytes.
    pub fn writable(&self) -> usize {
        SEND_CAP - self.send_len
    }

    /// Begin the handshake with an initial sequence number the caller chose.
    ///
    /// The ISN is an argument rather than a constant because it must be unpredictable on a real
    /// network, and choosing it is the platform's job — the same posture as every other place in
    /// this tree where policy is handed in rather than invented at the bottom.
    pub fn open(&mut self, iss: u32, now: u64) -> Result<(), TcpRefusal> {
        if self.state != TcpState::Closed {
            self.refusals += 1;
            return Err(TcpRefusal::AlreadyOpen);
        }
        self.snd_una = iss;
        self.snd_nxt = iss;
        self.state = TcpState::SynSent;
        self.retries = 0;
        self.syn_due = true;
        self.last_tx = now;
        Ok(())
    }

    /// Stage bytes for transmission, returning how many were accepted. A short accept is the
    /// truth about a bounded buffer; a refusal is what a caller gets when nothing fits.
    pub fn send(&mut self, bytes: &[u8]) -> Result<usize, TcpRefusal> {
        if !matches!(self.state, TcpState::Established | TcpState::CloseWait) || self.close_pending
        {
            self.refusals += 1;
            return Err(TcpRefusal::NotEstablished);
        }
        let room = self.writable();
        if room == 0 {
            self.refusals += 1;
            return Err(TcpRefusal::SendFull);
        }
        let take = bytes.len().min(room);
        self.send[self.send_len..self.send_len + take].copy_from_slice(&bytes[..take]);
        self.send_len += take;
        Ok(take)
    }

    /// Read received bytes, returning how many were copied out.
    pub fn recv(&mut self, out: &mut [u8]) -> usize {
        let take = self.recv_len.min(out.len());
        out[..take].copy_from_slice(&self.recv[..take]);
        self.recv.copy_within(take..self.recv_len, 0);
        self.recv_len -= take;
        take
    }

    /// Ask to close the sending half. Staged bytes are still sent; the FIN follows them.
    pub fn close(&mut self) -> Result<(), TcpRefusal> {
        match self.state {
            TcpState::Established | TcpState::CloseWait => {
                self.close_pending = true;
                Ok(())
            }
            TcpState::Closed => {
                self.refusals += 1;
                Err(TcpRefusal::NotEstablished)
            }
            // Already closing: asking twice is not an error, and must not send a second FIN.
            _ => Ok(()),
        }
    }

    /// Hand the connection a parsed segment. Returns what it meant.
    pub fn on_segment(&mut self, seg: &TcpView<'_>, now: u64) -> TcpEvent {
        if seg.sport != self.rport || seg.dport != self.lport {
            self.refusals += 1;
            return TcpEvent::Refused(TcpRefusal::NotOurs);
        }
        self.segments_in += 1;
        if seg.has(RST) {
            self.state = TcpState::Closed;
            self.send_len = 0;
            self.in_flight = 0;
            self.refusals += 1;
            return TcpEvent::Ended(TcpRefusal::Reset);
        }
        match self.state {
            TcpState::Closed | TcpState::TimeWait => TcpEvent::Nothing,
            TcpState::SynSent => self.on_syn_ack(seg, now),
            _ => self.on_open_segment(seg, now),
        }
    }

    /// The handshake's second segment. Anything that is not SYN+ACK acknowledging exactly our SYN
    /// is refused: an acknowledgement of something we never sent is either a bug or a peer
    /// guessing at our sequence space.
    fn on_syn_ack(&mut self, seg: &TcpView<'_>, now: u64) -> TcpEvent {
        if !(seg.has(SYN) && seg.has(ACK)) {
            self.refusals += 1;
            return TcpEvent::Refused(TcpRefusal::OutOfWindow);
        }
        if seg.ack != self.snd_nxt.wrapping_add(1) {
            self.refusals += 1;
            return TcpEvent::Refused(TcpRefusal::AckTooHigh);
        }
        self.snd_una = seg.ack;
        self.snd_nxt = seg.ack;
        self.rcv_nxt = seg.seq.wrapping_add(1);
        self.snd_wnd = seg.window as u32;
        self.state = TcpState::Established;
        self.ack_due = true;
        self.retries = 0;
        self.last_tx = now;
        TcpEvent::Connected
    }

    /// Every state after the handshake: acknowledgement first, then data, then the peer's FIN.
    fn on_open_segment(&mut self, seg: &TcpView<'_>, now: u64) -> TcpEvent {
        self.snd_wnd = seg.window as u32;
        if seg.has(ACK) {
            if let Err(r) = self.apply_ack(seg.ack, now) {
                self.refusals += 1;
                return TcpEvent::Refused(r);
            }
        }
        // Out of order, or a retransmission of something already taken: acknowledge what we DO
        // have and drop the rest. This stack reassembles nothing, by stated scope, so the peer's
        // retransmission is what makes progress.
        if seg.seq != self.rcv_nxt {
            if seg.seq_len() > 0 {
                self.ack_due = true;
                self.refusals += 1;
                return TcpEvent::Refused(TcpRefusal::OutOfWindow);
            }
            return self.after_ack_event();
        }
        let mut event = TcpEvent::Nothing;
        if !seg.payload.is_empty() {
            if seg.payload.len() > RECV_CAP - self.recv_len {
                // No room: do NOT advance the acknowledgement. The peer will send it again after
                // the application reads, which is exactly what a closed window means.
                self.ack_due = true;
                self.refusals += 1;
                return TcpEvent::Refused(TcpRefusal::RecvFull);
            }
            self.recv[self.recv_len..self.recv_len + seg.payload.len()]
                .copy_from_slice(seg.payload);
            self.recv_len += seg.payload.len();
            self.rcv_nxt = self.rcv_nxt.wrapping_add(seg.payload.len() as u32);
            self.bytes_in += seg.payload.len() as u64;
            self.ack_due = true;
            event = TcpEvent::Data(seg.payload.len());
        }
        if seg.has(FIN) {
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            self.ack_due = true;
            self.state = match self.state {
                TcpState::Established => TcpState::CloseWait,
                TcpState::FinWait1 => TcpState::Closing,
                TcpState::FinWait2 => TcpState::TimeWait,
                other => other,
            };
            return TcpEvent::PeerClosed;
        }
        if matches!(event, TcpEvent::Nothing) {
            return self.after_ack_event();
        }
        event
    }

    /// What a pure acknowledgement meant: possibly the end of the connection.
    fn after_ack_event(&mut self) -> TcpEvent {
        match self.state {
            TcpState::TimeWait => TcpEvent::Ended(TcpRefusal::PeerGone),
            _ => TcpEvent::Nothing,
        }
    }

    /// Advance the send window. An acknowledgement above what was sent is refused rather than
    /// believed: it would move `snd_una` past `snd_nxt` and make every later comparison nonsense.
    fn apply_ack(&mut self, ack: u32, now: u64) -> Result<(), TcpRefusal> {
        if seq_lt(self.snd_nxt, ack) {
            return Err(TcpRefusal::AckTooHigh);
        }
        if seq_leq(ack, self.snd_una) {
            // A duplicate acknowledgement. Not an error, and not progress either.
            return Ok(());
        }
        let mut advanced = ack.wrapping_sub(self.snd_una) as usize;
        self.snd_una = ack;
        self.retries = 0;
        self.last_tx = now;
        // Our FIN occupies one byte of the sequence space, and it is the LAST one, so an
        // acknowledgement that covers it is what moves the closing states forward.
        if self
            .fin_seq
            .is_some_and(|fs| seq_leq(fs.wrapping_add(1), ack))
        {
            advanced = advanced.saturating_sub(1);
            self.state = match self.state {
                TcpState::FinWait1 => TcpState::FinWait2,
                TcpState::Closing | TcpState::LastAck => TcpState::TimeWait,
                other => other,
            };
        }
        let drop = advanced.min(self.send_len);
        self.send.copy_within(drop..self.send_len, 0);
        self.send_len -= drop;
        self.in_flight = self.in_flight.saturating_sub(drop);
        Ok(())
    }

    /// The sequence number our FIN will occupy: one past the last staged byte.
    fn next_fin_seq(&self) -> u32 {
        self.snd_una.wrapping_add(self.send_len as u32)
    }

    /// Build the next segment this connection owes the wire, if any, into `buf`.
    ///
    /// One segment per call, in priority order: the handshake, then new data the peer's window
    /// allows, then a retransmission whose timer expired, then a FIN, then a bare acknowledgement.
    /// Returning the LENGTH rather than a slice keeps the borrow local, so a caller can hand the
    /// same buffer to its device without fighting the borrow checker.
    pub fn poll_transmit(&mut self, now: u64, buf: &mut [u8]) -> Option<usize> {
        if buf.len() < IPV4_HDR_MIN + TCP_HDR_MIN {
            return None;
        }
        let expired = now.saturating_sub(self.last_tx) >= self.rto;
        match self.state {
            TcpState::Closed | TcpState::TimeWait => None,
            TcpState::SynSent => {
                // The SYN backs off (RFC 6298 section 5.5): each unanswered SYN doubles the wait
                // before the next, so five tries cover ~31 RTOs rather than 5. With a fixed RTO a
                // peer - or an emulator's NAT on a loaded host - that took longer than a second to
                // answer was declared gone (2026-09-25, `https-e2e.sh` x86-64 at load average 46).
                let backoff = self.rto << u32::from(self.retries.saturating_sub(1).min(4));
                let expired = now.saturating_sub(self.last_tx) >= backoff;
                if !self.syn_due && !expired {
                    return None;
                }
                if self.retries >= MAX_RETRIES {
                    self.state = TcpState::Closed;
                    self.refusals += 1;
                    return None;
                }
                if !self.syn_due {
                    self.retransmits += 1;
                }
                self.syn_due = false;
                self.retries += 1;
                self.last_tx = now;
                self.emit(
                    buf,
                    Segment {
                        seq: self.snd_nxt,
                        ack: 0,
                        flags: SYN,
                        window: RECV_CAP as u16,
                        payload: &[],
                    },
                )
            }
            _ => self.poll_open(now, expired, buf),
        }
    }

    fn poll_open(&mut self, now: u64, expired: bool, buf: &mut [u8]) -> Option<usize> {
        // A peer that has stopped acknowledging is gone once the budget is spent. Saying so is the
        // difference between a closed connection and a connection that waits forever.
        if self.in_flight > 0 && expired && self.retries >= MAX_RETRIES {
            self.state = TcpState::Closed;
            self.refusals += 1;
            return None;
        }
        let window = self.snd_wnd.min(SEND_CAP as u32) as usize;
        let unsent = self.send_len - self.in_flight;

        // 1. New data, bounded by the peer's window and one MSS.
        if unsent > 0 && self.in_flight < window {
            let room = (window - self.in_flight).min(MSS).min(unsent);
            if room > 0 {
                let start = self.in_flight;
                let seq = self.snd_una.wrapping_add(start as u32);
                let mut payload = [0u8; MSS];
                payload[..room].copy_from_slice(&self.send[start..start + room]);
                self.in_flight += room;
                self.snd_nxt = self.snd_una.wrapping_add(self.in_flight as u32);
                self.bytes_out += room as u64;
                self.ack_due = false;
                self.last_tx = now;
                self.retries = 1;
                return self.emit(
                    buf,
                    Segment {
                        seq,
                        ack: self.rcv_nxt,
                        flags: ACK | PSH,
                        window: (RECV_CAP - self.recv_len) as u16,
                        payload: &payload[..room],
                    },
                );
            }
        }

        // 2. A retransmission of the oldest unacknowledged bytes.
        if self.in_flight > 0 && expired {
            let room = self.in_flight.min(MSS);
            let mut payload = [0u8; MSS];
            payload[..room].copy_from_slice(&self.send[..room]);
            self.retries += 1;
            self.retransmits += 1;
            self.last_tx = now;
            self.ack_due = false;
            return self.emit(
                buf,
                Segment {
                    seq: self.snd_una,
                    ack: self.rcv_nxt,
                    flags: ACK | PSH,
                    window: (RECV_CAP - self.recv_len) as u16,
                    payload: &payload[..room],
                },
            );
        }

        // 3. Our FIN, once everything staged is acknowledged.
        if self.close_pending && self.send_len == 0 {
            let retransmit = self.fin_seq.is_some() && expired;
            if self.fin_seq.is_none() || retransmit {
                if retransmit {
                    if self.retries >= MAX_RETRIES {
                        self.state = TcpState::Closed;
                        self.refusals += 1;
                        return None;
                    }
                    self.retransmits += 1;
                }
                let seq = self.fin_seq.unwrap_or_else(|| self.next_fin_seq());
                if self.fin_seq.is_none() {
                    self.fin_seq = Some(seq);
                    self.snd_nxt = seq.wrapping_add(1);
                    self.state = match self.state {
                        TcpState::Established => TcpState::FinWait1,
                        TcpState::CloseWait => TcpState::LastAck,
                        other => other,
                    };
                }
                self.retries += 1;
                self.last_tx = now;
                self.ack_due = false;
                return self.emit(
                    buf,
                    Segment {
                        seq,
                        ack: self.rcv_nxt,
                        flags: ACK | FIN,
                        window: (RECV_CAP - self.recv_len) as u16,
                        payload: &[],
                    },
                );
            }
        }

        // 4. A bare acknowledgement for something that arrived.
        if self.ack_due {
            self.ack_due = false;
            return self.emit(
                buf,
                Segment {
                    seq: self.snd_nxt,
                    ack: self.rcv_nxt,
                    flags: ACK,
                    window: (RECV_CAP - self.recv_len) as u16,
                    payload: &[],
                },
            );
        }
        None
    }

    /// Put one segment on the caller's buffer and count it.
    fn emit(&mut self, buf: &mut [u8], seg: Segment<'_>) -> Option<usize> {
        self.ident = self.ident.wrapping_add(1);
        let n = build_segment(
            buf,
            self.ident,
            self.local,
            self.remote,
            self.lport,
            self.rport,
            &seg,
        )?
        .len();
        self.segments_out += 1;
        Some(n)
    }
}
