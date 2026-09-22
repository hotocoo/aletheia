//! TLS 1.3 over a live connection: where the handshake, the record layer and the transport meet
//! (REQ-SEC-TLS-010, ADR-151).
//!
//! ADR-144 built the handshake without a wire, ADR-143 the record layer without a handshake, and
//! ADR-139/140 a TCP client without either. Each is proved on its own. This module is the JOIN:
//! one bounded pump that opens a connection, sends the ClientHello as a plaintext record, feeds
//! every record the peer sends to the right layer, completes the handshake, sends this client's
//! Finished under the handshake keys, switches to the application keys, sends the request and
//! collects the answer. Nothing here decides anything cryptographic; it only carries bytes between
//! parts that do, and refuses by name when a byte arrives where it does not belong.
//!
//! ## What the pump refuses
//!
//! * a record whose outer type is not one TLS 1.3 uses, or an encrypted record before there are
//!   keys to open it with;
//! * application data before the handshake finished (a server that talks before it is verified is
//!   not one this client listens to);
//! * a fatal alert, by its description byte;
//! * any refusal from the handshake or the record layer, unchanged; those layers already say why.
//!
//! ## What stays outside
//!
//! The verifier, the clock, the name and the ephemeral key are the caller's: this module takes a
//! [`Handshake`] already built with them. Entropy in particular is the platform's business and is
//! named as such in `netstatic` (ADR-151 records that this kernel has no entropy device yet).
//!
//! Every loop is bounded by the caller's [`Plan`], as in [`crate::tcpnet::exchange`].

use alloc::boxed::Box;

use crate::tcp::{parse_tcp, PROTOCOL_TCP};
use crate::tcpconn::{Connection, TcpEvent, TcpState, TX_BUF_MIN};
use crate::tcpnet::{Ipv4Link, LinkError, Plan};
use crate::tlshandshake::{Handshake, HandshakeRefusal, HandshakeStage, PeerVerifier, MAX_MESSAGE};
use crate::tlsrecord::{
    ContentType, RecordLayer, RecordRefusal, HEADER_LEN, MAX_CIPHERTEXT, MAX_PLAINTEXT, MAX_RECORD,
};
use crate::udpv4::parse_ipv4;

/// Outer record types, as they appear on the wire.
pub const RECORD_CHANGE_CIPHER_SPEC: u8 = 20;
pub const RECORD_ALERT: u8 = 21;
pub const RECORD_HANDSHAKE: u8 = 22;
pub const RECORD_APPLICATION_DATA: u8 = 23;
/// The legacy version every record carries (RFC 8446 §5.1).
const LEGACY_VERSION: [u8; 2] = [0x03, 0x03];
/// The alert that means "I am done talking", not "something is wrong".
pub const ALERT_CLOSE_NOTIFY: u8 = 0;
/// Post-handshake message types this client accepts and ignores, or refuses.
const MSG_NEW_SESSION_TICKET: u8 = 4;
/// Bytes this side has framed and not yet handed to the connection.
const PENDING_CAP: usize = 4096;
/// One received IPv4 datagram, up to an Ethernet MTU. A real peer sends segments as large as the
/// path allows (1460 bytes of payload over user-mode networking), not the 536-byte MSS this side
/// offers; a buffer sized to our own MSS refused every server flight as "too long".
const RX_CAP: usize = 1500;

/// Why the conversation stopped. Each carries the layer's own reason where there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsRefusal {
    /// The link or the connection, in its own words.
    Link(LinkError),
    /// The handshake, in its own words.
    Handshake(HandshakeRefusal),
    /// The record layer, in its own words.
    Record(RecordRefusal),
    /// The peer sent a fatal alert with this description byte.
    Alert(u8),
    /// The peer sent application data before the handshake finished.
    DataBeforeFinished,
    /// A record whose outer type is not one TLS 1.3 uses, or an encrypted record before keys.
    UnexpectedRecord(u8),
    /// A post-handshake message this client does not process (a KeyUpdate, for one).
    UnexpectedMessage(u8),
    /// More bytes arrived than a record can be, with no record boundary among them.
    StreamFull,
    /// This side had more to send than it can stage.
    NoRoom,
}

impl TlsRefusal {
    /// One line a person at a console can act on.
    pub fn describe(&self) -> &'static str {
        match self {
            TlsRefusal::Link(LinkError::BudgetSpent) => {
                "the peer did not finish inside this machine's budget"
            }
            TlsRefusal::Link(LinkError::ConnectionEnded) => {
                "the peer refused or reset the connection"
            }
            TlsRefusal::Link(LinkError::TooLong) => "a datagram did not fit this machine's buffer",
            TlsRefusal::Link(LinkError::Device) => "the network device refused the frame",
            TlsRefusal::Handshake(HandshakeRefusal::PeerUnverified) => {
                "the peer's certificate is not one the pinned root signed for that name"
            }
            TlsRefusal::Handshake(HandshakeRefusal::BadSignature) => {
                "the peer could not prove it holds the key its certificate names"
            }
            TlsRefusal::Handshake(HandshakeRefusal::BadFinished) => {
                "the peer's Finished does not match the conversation this client saw"
            }
            TlsRefusal::Handshake(HandshakeRefusal::Downgrade) => {
                "the peer tried to negotiate a version below TLS 1.3"
            }
            TlsRefusal::Handshake(HandshakeRefusal::NotTls13) => "the peer does not speak TLS 1.3",
            TlsRefusal::Handshake(HandshakeRefusal::UnsupportedChoice) => {
                "the peer chose a suite, group or scheme this client did not offer"
            }
            TlsRefusal::Handshake(_) => "the handshake ended by name (see the refusal counters)",
            TlsRefusal::Record(RecordRefusal::Fatal) => {
                "a record failed authentication; the connection is over"
            }
            TlsRefusal::Record(_) => "a record was malformed; the connection is over",
            TlsRefusal::Alert(ALERT_CLOSE_NOTIFY) => "the peer closed before answering",
            TlsRefusal::Alert(_) => "the peer sent a fatal alert",
            TlsRefusal::DataBeforeFinished => "the peer sent data before the handshake finished",
            TlsRefusal::UnexpectedRecord(_) => "the peer sent a record TLS 1.3 does not use here",
            TlsRefusal::UnexpectedMessage(_) => {
                "the peer sent a message this client does not process"
            }
            TlsRefusal::StreamFull => "the peer sent more than one record can hold",
            TlsRefusal::NoRoom => "this machine could not stage what it had to send",
        }
    }
}

/// What one conversation achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TlsOutcome {
    /// Bytes of decrypted application data copied out.
    pub received: usize,
    /// The peer answered more than the caller's buffer holds; the rest was dropped, not written.
    pub truncated: bool,
    /// Protected records opened and produced.
    pub records_in: u64,
    pub records_out: u64,
    /// Segments this side put on the wire and accepted from the peer.
    pub sent_segments: u64,
    pub recv_segments: u64,
    /// Poll turns used.
    pub turns: u64,
    /// The peer's public key, as the verifier named it.
    pub peer_key: [u8; 32],
}

/// What a console reports about a conversation: enough for a person to know whom they spoke to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TlsReport {
    pub received: usize,
    pub peer_key: [u8; 32],
    pub records_in: u64,
    pub records_out: u64,
}

impl From<TlsOutcome> for TlsReport {
    fn from(o: TlsOutcome) -> Self {
        TlsReport {
            received: o.received,
            peer_key: o.peer_key,
            records_in: o.records_in,
            records_out: o.records_out,
        }
    }
}

/// Derive an ephemeral X25519 private scalar and a client random from seed material.
///
/// The seed is the PLATFORM's: this function only separates it into two values that must not be
/// equal. It does not make weak material strong, and ADR-151 names the seed this kernel has today
/// (timer readings) as insufficient for anything but a demonstration on a private network.
pub fn ephemeral_material(seed: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut key_in = [0u8; 96];
    let take = seed.len().min(64);
    key_in[..take].copy_from_slice(&seed[..take]);
    key_in[64..70].copy_from_slice(b"x25519");
    let private = crate::crypto::sha256(&key_in[..70]);
    key_in[64..70].copy_from_slice(b"random");
    let random = crate::crypto::sha256(&key_in[..70]);
    (private, random)
}

/// The pump's scratch, allocated once per conversation. Sized for one whole record in flight plus
/// one whole handshake message being reassembled: a peer may split either across segments.
struct Workspace {
    stream: [u8; MAX_RECORD],
    stream_len: usize,
    messages: [u8; MAX_MESSAGE + 4],
    messages_len: usize,
    plain: [u8; MAX_PLAINTEXT],
    pending: [u8; PENDING_CAP],
    pending_len: usize,
    pending_sent: usize,
    tx: [u8; TX_BUF_MIN],
    rx: [u8; RX_CAP],
}

/// One conversation's state. Kept as a struct so the borrow of each part is explicit and the
/// pump's steps can be read one at a time.
/// The pump's reusable state: the workspace and, once a conversation has produced keys, the record
/// layer. Built ONCE by whoever opens conversations (a console keeps one for its lifetime) and
/// handed to every [`exchange`]: together they are ninety kilobytes, and on a heap that never frees
/// a pump built per conversation would be a leak with a keystroke for a trigger.
pub struct TlsPump {
    records: Option<RecordLayer>,
    work: Box<Workspace>,
}

impl TlsPump {
    pub fn new() -> Self {
        TlsPump {
            records: None,
            work: Box::new(Workspace {
                stream: [0u8; MAX_RECORD],
                stream_len: 0,
                messages: [0u8; MAX_MESSAGE + 4],
                messages_len: 0,
                plain: [0u8; MAX_PLAINTEXT],
                pending: [0u8; PENDING_CAP],
                pending_len: 0,
                pending_sent: 0,
                tx: [0u8; TX_BUF_MIN],
                rx: [0u8; RX_CAP],
            }),
        }
    }

    /// Forget the previous conversation. The record layer, if any, keeps its allocation and is
    /// re-keyed when the next handshake produces keys; until then it is not consulted.
    fn begin(&mut self) {
        let w = &mut *self.work;
        w.stream_len = 0;
        w.messages_len = 0;
        w.pending_len = 0;
        w.pending_sent = 0;
    }

    /// Install handshake keys: re-key the existing layer or build the one this pump will keep.
    fn install_keys(&mut self, write: crate::hkdf::TrafficKeys, read: crate::hkdf::TrafficKeys) {
        match self.records.as_mut() {
            Some(r) => r.reset(write, read),
            None => self.records = Some(RecordLayer::new(write, read)),
        }
    }
}

impl Default for TlsPump {
    fn default() -> Self {
        Self::new()
    }
}

struct Session<'a, V: PeerVerifier> {
    hs: &'a mut Handshake<V>,
    pump: &'a mut TlsPump,
    keyed: bool,
    request: &'a [u8],
    reply: &'a mut [u8],
    out: TlsOutcome,
    hello_sent: bool,
    done: bool,
    peer_closed: bool,
    close_requested: bool,
    closed: bool,
}

impl<'a, V: PeerVerifier> Session<'a, V> {
    fn queue(&mut self, bytes: &[u8]) -> Result<(), TlsRefusal> {
        let w = &mut *self.pump.work;
        if w.pending_len + bytes.len() > PENDING_CAP {
            return Err(TlsRefusal::NoRoom);
        }
        w.pending[w.pending_len..w.pending_len + bytes.len()].copy_from_slice(bytes);
        w.pending_len += bytes.len();
        Ok(())
    }

    /// Hand staged bytes to the connection, as many as it will take this turn.
    fn flush_pending(&mut self, conn: &mut Connection) {
        let w = &mut *self.pump.work;
        while w.pending_sent < w.pending_len {
            match conn.send(&w.pending[w.pending_sent..w.pending_len]) {
                Ok(0) | Err(_) => break,
                Ok(n) => w.pending_sent += n,
            }
        }
        if w.pending_sent == w.pending_len {
            w.pending_len = 0;
            w.pending_sent = 0;
        }
    }

    /// Frame a plaintext handshake record around `message` and stage it.
    fn queue_plaintext_handshake(&mut self, message: &[u8]) -> Result<(), TlsRefusal> {
        let header = [
            RECORD_HANDSHAKE,
            LEGACY_VERSION[0],
            LEGACY_VERSION[1],
            (message.len() >> 8) as u8,
            message.len() as u8,
        ];
        self.queue(&header)?;
        self.queue(message)?;
        self.out.records_out += 1;
        Ok(())
    }

    /// Protect `content` under the current write keys and stage it.
    fn queue_sealed(&mut self, ty: ContentType, content: &[u8]) -> Result<(), TlsRefusal> {
        let mut sealed = [0u8; 1024 + HEADER_LEN + 1 + 16];
        let Some(records) = self.pump.records.as_mut() else {
            return Err(TlsRefusal::UnexpectedRecord(RECORD_APPLICATION_DATA));
        };
        if content.len() > 1024 {
            return Err(TlsRefusal::NoRoom);
        }
        let n = records
            .seal(ty, content, 0, &mut sealed)
            .map_err(TlsRefusal::Record)?;
        self.queue(&sealed[..n])?;
        self.out.records_out += 1;
        Ok(())
    }

    /// Everything the connection has for us goes into the record stream.
    fn drain_connection(&mut self, conn: &mut Connection) -> Result<(), TlsRefusal> {
        let w = &mut *self.pump.work;
        loop {
            if w.stream_len == MAX_RECORD {
                // Full, and (as the caller checks next) with no whole record in it.
                if conn.readable() > 0 {
                    return Err(TlsRefusal::StreamFull);
                }
                return Ok(());
            }
            let got = conn.recv(&mut w.stream[w.stream_len..]);
            if got == 0 {
                return Ok(());
            }
            w.stream_len += got;
        }
    }

    /// Walk every whole record at the front of the stream.
    fn process_records(&mut self) -> Result<(), TlsRefusal> {
        loop {
            let (ty, total) = {
                let w = &*self.pump.work;
                if w.stream_len < HEADER_LEN {
                    return Ok(());
                }
                let len = u16::from_be_bytes([w.stream[3], w.stream[4]]) as usize;
                if len == 0 || len > MAX_CIPHERTEXT {
                    return Err(TlsRefusal::Record(RecordRefusal::BadHeader));
                }
                let total = HEADER_LEN + len;
                if w.stream_len < total {
                    return Ok(());
                }
                (w.stream[0], total)
            };
            match ty {
                // Middlebox compatibility: a server may send ChangeCipherSpec at any point during
                // the handshake; it carries nothing and is dropped unread (RFC 8446 §5).
                RECORD_CHANGE_CIPHER_SPEC => {}
                RECORD_ALERT if !self.keyed => {
                    let w = &*self.pump.work;
                    let description = if total >= HEADER_LEN + 2 {
                        w.stream[HEADER_LEN + 1]
                    } else {
                        0xFF
                    };
                    return Err(TlsRefusal::Alert(description));
                }
                RECORD_HANDSHAKE if !self.keyed => {
                    self.on_handshake_bytes(HEADER_LEN, total)?;
                }
                RECORD_APPLICATION_DATA if self.keyed => {
                    self.on_protected_record(total)?;
                }
                other => return Err(TlsRefusal::UnexpectedRecord(other)),
            }
            let w = &mut *self.pump.work;
            w.stream.copy_within(total..w.stream_len, 0);
            w.stream_len -= total;
        }
    }

    /// Open the protected record occupying `stream[..total]` and route its content.
    fn on_protected_record(&mut self, total: usize) -> Result<(), TlsRefusal> {
        let (ty, n) = {
            let w = &mut *self.pump.work;
            let records = self
                .pump
                .records
                .as_mut()
                .ok_or(TlsRefusal::UnexpectedRecord(RECORD_APPLICATION_DATA))?;
            let (ty, n, consumed) = records
                .open(&w.stream[..total], &mut w.plain)
                .map_err(TlsRefusal::Record)?;
            if consumed != total {
                return Err(TlsRefusal::Record(RecordRefusal::BadHeader));
            }
            (ty, n)
        };
        self.out.records_in += 1;
        match ty {
            ContentType::Handshake => self.on_plain_handshake(n),
            ContentType::ApplicationData => {
                if !self.done {
                    return Err(TlsRefusal::DataBeforeFinished);
                }
                let room = self.reply.len() - self.out.received;
                let take = n.min(room);
                self.reply[self.out.received..self.out.received + take]
                    .copy_from_slice(&self.pump.work.plain[..take]);
                self.out.received += take;
                if take < n {
                    self.out.truncated = true;
                }
                Ok(())
            }
            ContentType::Alert => {
                let w = &*self.pump.work;
                if n >= 2 && w.plain[1] == ALERT_CLOSE_NOTIFY {
                    self.peer_closed = true;
                    self.close_requested = true;
                    return Ok(());
                }
                Err(TlsRefusal::Alert(if n >= 2 { w.plain[1] } else { 0xFF }))
            }
            ContentType::ChangeCipherSpec => Err(TlsRefusal::Record(RecordRefusal::BadInnerType)),
        }
    }

    /// Handshake bytes from a PLAINTEXT record: `stream[from..to]`.
    fn on_handshake_bytes(&mut self, from: usize, to: usize) -> Result<(), TlsRefusal> {
        {
            let w = &mut *self.pump.work;
            let n = to - from;
            if w.messages_len + n > w.messages.len() {
                return Err(TlsRefusal::Handshake(HandshakeRefusal::BadLength));
            }
            let at = w.messages_len;
            let mut i = 0;
            while i < n {
                w.messages[at + i] = w.stream[from + i];
                i += 1;
            }
            w.messages_len += n;
        }
        self.drain_messages()
    }

    /// Handshake bytes from a DECRYPTED record: `plain[..n]`.
    fn on_plain_handshake(&mut self, n: usize) -> Result<(), TlsRefusal> {
        {
            let w = &mut *self.pump.work;
            if w.messages_len + n > w.messages.len() {
                return Err(TlsRefusal::Handshake(HandshakeRefusal::BadLength));
            }
            let at = w.messages_len;
            let mut i = 0;
            while i < n {
                w.messages[at + i] = w.plain[i];
                i += 1;
            }
            w.messages_len += n;
        }
        self.drain_messages()
    }

    /// Feed every whole handshake message in the accumulator to the handshake.
    fn drain_messages(&mut self) -> Result<(), TlsRefusal> {
        loop {
            let (msg_type, total) = {
                let w = &*self.pump.work;
                if w.messages_len < 4 {
                    return Ok(());
                }
                let len =
                    u32::from_be_bytes([0, w.messages[1], w.messages[2], w.messages[3]]) as usize;
                if len > MAX_MESSAGE {
                    return Err(TlsRefusal::Handshake(HandshakeRefusal::BadLength));
                }
                if w.messages_len < 4 + len {
                    return Ok(());
                }
                (w.messages[0], 4 + len)
            };
            {
                let w = &*self.pump.work;
                let message = &w.messages[..total];
                if !self.keyed {
                    self.hs
                        .server_hello(message)
                        .map_err(TlsRefusal::Handshake)?;
                    let (client, server) = self
                        .hs
                        .handshake_keys()
                        .ok_or(TlsRefusal::Handshake(HandshakeRefusal::Kdf))?;
                    self.pump.install_keys(client, server);
                    self.keyed = true;
                } else if self.done {
                    match msg_type {
                        MSG_NEW_SESSION_TICKET => {}
                        other => return Err(TlsRefusal::UnexpectedMessage(other)),
                    }
                } else {
                    let stage = self
                        .hs
                        .server_flight(message)
                        .map_err(TlsRefusal::Handshake)?;
                    if stage == HandshakeStage::Done {
                        self.complete_handshake()?;
                    }
                }
            }
            let w = &mut *self.pump.work;
            w.messages.copy_within(total..w.messages_len, 0);
            w.messages_len -= total;
        }
    }

    /// The server's Finished verified: send ours under the handshake keys, switch both directions
    /// to the application keys, and send the request.
    fn complete_handshake(&mut self) -> Result<(), TlsRefusal> {
        let (client_app, server_app) = self
            .hs
            .application_keys()
            .ok_or(TlsRefusal::Handshake(HandshakeRefusal::Kdf))?;
        self.out.peer_key = self.hs.peer_key().unwrap_or([0u8; 32]);
        let mut finished = [0u8; 64];
        let n = self
            .hs
            .client_finished(&mut finished)
            .map_err(TlsRefusal::Handshake)?;
        self.queue_sealed(ContentType::Handshake, &finished[..n])?;
        if let Some(records) = self.pump.records.as_mut() {
            records.reset(client_app, server_app);
        }
        self.done = true;
        let request = self.request;
        self.queue_sealed(ContentType::ApplicationData, request)?;
        Ok(())
    }

    fn finish(self) -> Result<TlsOutcome, TlsRefusal> {
        if self.done && (self.out.received > 0 || self.peer_closed) {
            Ok(self.out)
        } else {
            Err(TlsRefusal::Link(LinkError::ConnectionEnded))
        }
    }
}

/// Run one TLS 1.3 conversation to completion over `link`: open `conn`, hand-shake with the
/// peer `hs` was built for, send `request` protected, collect the protected answer into `reply`
/// until the peer closes or the budget is spent.
///
/// `hs` carries the verifier, the name, the ephemeral key and the random; all four are the
/// caller's decisions. `pump` is the caller's too, built once and reused (see [`TlsPump`]). `now`
/// is the caller's clock, as in [`crate::tcpnet::exchange`].
// Eight arguments because the caller owns eight decisions - the link, the connection, the plan,
// the workspace, the handshake (verifier, name, key, random), what to say, where to put the answer
// and the clock - and folding any of them into another would hide who decides it.
#[allow(clippy::too_many_arguments)]
pub fn exchange<L: Ipv4Link, V: PeerVerifier>(
    link: &L,
    conn: &mut Connection,
    plan: Plan,
    pump: &mut TlsPump,
    hs: &mut Handshake<V>,
    request: &[u8],
    reply: &mut [u8],
    now: &mut dyn FnMut() -> u64,
) -> Result<TlsOutcome, TlsRefusal> {
    pump.begin();
    let mut s = Session {
        hs,
        pump,
        keyed: false,
        request,
        reply,
        out: TlsOutcome::default(),
        hello_sent: false,
        done: false,
        peer_closed: false,
        close_requested: false,
        closed: false,
    };

    conn.open(plan.iss, now())
        .map_err(|_| TlsRefusal::Link(LinkError::ConnectionEnded))?;

    for turn in 0..plan.budget {
        s.out.turns = turn + 1;

        if !s.hello_sent && conn.state() == TcpState::Established {
            let mut message = [0u8; 512];
            let n =
                s.hs.client_hello(&mut message)
                    .map_err(TlsRefusal::Handshake)?;
            s.queue_plaintext_handshake(&message[..n])?;
            s.hello_sent = true;
        }

        s.flush_pending(conn);
        if s.close_requested && !s.closed && s.pump.work.pending_len == 0 {
            s.closed = true;
            let _ = conn.close();
        }

        while let Some(n) = conn.poll_transmit(now(), &mut s.pump.work.tx) {
            link.send_ipv4(&s.pump.work.tx[..n])
                .map_err(TlsRefusal::Link)?;
            s.out.sent_segments += 1;
        }

        match conn.state() {
            TcpState::Closed | TcpState::TimeWait => return s.finish(),
            _ => {}
        }

        let n = link
            .recv_ipv4(plan.spins_per_turn, PROTOCOL_TCP, &mut s.pump.work.rx)
            .map_err(TlsRefusal::Link)?;
        if n > 0 {
            let event = {
                let w = &*s.pump.work;
                match parse_ipv4(&w.rx[..n]) {
                    Ok(ip) if ip.dst == link.local_ip() => match parse_tcp(&ip) {
                        Ok(view) => Some(conn.on_segment(&view, now())),
                        Err(_) => None,
                    },
                    _ => None,
                }
            };
            if let Some(event) = event {
                if !matches!(event, TcpEvent::Refused(_)) {
                    s.out.recv_segments += 1;
                }
                s.drain_connection(conn)?;
                s.process_records()?;
                if matches!(event, TcpEvent::PeerClosed) {
                    s.peer_closed = true;
                    s.close_requested = true;
                }
                if matches!(event, TcpEvent::Ended(_)) {
                    return s.finish();
                }
            }
        }
    }
    if s.done && s.out.received > 0 {
        return Ok(s.out);
    }
    Err(TlsRefusal::Link(LinkError::BudgetSpent))
}

/// The join's contract, proved on every CPU at boot over a link that is a test double with a
/// STAND-IN TLS SERVER behind it. The stand-in encrypts with this tree's own primitives — that
/// part proves only agreement with itself — but its CertificateVerify is the fixture an
/// independent signer produced with a key this kernel does not have (ADR-149), so the one
/// signature the client accepts here is one it could not have forged. What is proved is the JOIN:
/// that records, messages and segments arrive at the right layer in the right order, that the
/// request goes out protected only after the peer is verified, and that every deviation is a
/// refusal with a name.
pub fn tlsclient_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::crypto::sha256;
    use crate::hkdf::{traffic_keys, KeySchedule, TrafficKeys};
    use crate::tcp::{build_segment, Segment, ACK, FIN, PSH, SYN};
    use crate::tlshandshake::{
        certificate_verify_message, finished_mac, fixture_server_hello, CERTIFICATE,
        ENCRYPTED_EXTENSIONS, FINISHED, FIXTURE_CLIENT_PRIVATE, FIXTURE_CLIENT_RANDOM,
        FIXTURE_SERVER_PRIVATE,
    };
    use crate::trust::{
        certificate_message, PinnedRoot, FIXTURE_CERTIFICATE_VERIFY, FIXTURE_NAME, FIXTURE_TIME,
        LEAF_FIXTURE, LEAF_KEY_FIXTURE, ROOT_KEY_FIXTURE,
    };
    use crate::x25519::{public_key, x25519};
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
    const SPORT: u16 = 443;
    const CHUNK: usize = 512;
    const QUEUE: usize = 4;

    /// How the stand-in misbehaves this run. `Honest` is a server that does everything right.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Behaviour {
        Honest,
        /// Sends a CertificateVerify whose signature is one bit off.
        WrongSignature,
        /// Flips one ciphertext byte of its Finished record.
        CorruptFinished,
        /// Sends application data before its Finished.
        DataBeforeFinished,
        /// Answers nothing at all.
        Deaf,
        /// Answers the ClientHello with a plaintext handshake_failure alert.
        PlaintextAlert,
        /// An honest server that never sends the compatibility ChangeCipherSpec.
        NoChangeCipherSpec,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Phase {
        WaitHello,
        WaitClientFinished,
        WaitRequest,
        Answered,
        Over,
    }

    struct PeerState {
        seq: u32,
        ack: u32,
        queue: [[u8; TX_BUF_MIN]; QUEUE],
        queue_len: [usize; QUEUE],
        queued: usize,
        client_closed: bool,
        fin_sent: bool,
        rx: [u8; 2048],
        rx_len: usize,
        tx: [u8; 4096],
        tx_len: usize,
        tx_sent: usize,
        phase: Phase,
        transcript: [u8; 2048],
        transcript_len: usize,
        records: Option<RecordLayer>,
        schedule: KeySchedule,
        client_hs_secret: [u8; 32],
        app_keys: Option<(TrafficKeys, TrafficKeys)>,
        expected_client_finished: [u8; 32],
        client_finished_ok: bool,
        client_records_before_finished: u32,
        request_seen: [u8; 64],
        request_len: usize,
    }

    struct Peer {
        st: RefCell<PeerState>,
        behaviour: Behaviour,
    }

    fn append(buf: &mut [u8], len: &mut usize, bytes: &[u8]) -> bool {
        if *len + bytes.len() > buf.len() {
            return false;
        }
        buf[*len..*len + bytes.len()].copy_from_slice(bytes);
        *len += bytes.len();
        true
    }

    fn handshake_message(ty: u8, body: &[u8], out: &mut [u8]) -> usize {
        out[0] = ty;
        out[1..4].copy_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        out[4..4 + body.len()].copy_from_slice(body);
        4 + body.len()
    }

    fn plaintext_record(ty: u8, body: &[u8], out: &mut [u8]) -> usize {
        out[0] = ty;
        out[1..3].copy_from_slice(&LEGACY_VERSION);
        out[3..5].copy_from_slice(&(body.len() as u16).to_be_bytes());
        out[5..5 + body.len()].copy_from_slice(body);
        5 + body.len()
    }

    impl Peer {
        /// Serve every whole record in the rx stream, producing tx bytes.
        fn serve(&self) {
            let mut st = self.st.borrow_mut();
            let st = &mut *st;
            loop {
                if st.rx_len < HEADER_LEN {
                    return;
                }
                let len = u16::from_be_bytes([st.rx[3], st.rx[4]]) as usize;
                let total = HEADER_LEN + len;
                if st.rx_len < total {
                    return;
                }
                let ty = st.rx[0];
                match (st.phase, ty) {
                    (Phase::WaitHello, RECORD_HANDSHAKE) => {
                        let hello_len = total - HEADER_LEN;
                        let mut hello = [0u8; 512];
                        hello[..hello_len].copy_from_slice(&st.rx[HEADER_LEN..total]);
                        self.answer_hello(st, &hello[..hello_len]);
                    }
                    (Phase::WaitClientFinished, RECORD_APPLICATION_DATA)
                    | (Phase::WaitRequest, RECORD_APPLICATION_DATA) => {
                        let mut record = [0u8; 1024];
                        record[..total].copy_from_slice(&st.rx[..total]);
                        let mut plain = [0u8; 1024];
                        let opened = st
                            .records
                            .as_mut()
                            .map(|r| r.open(&record[..total], &mut plain));
                        match opened {
                            Some(Ok((ContentType::Handshake, m, _)))
                                if st.phase == Phase::WaitClientFinished =>
                            {
                                st.client_finished_ok = m == 36
                                    && plain[0] == FINISHED
                                    && plain[4..36] == st.expected_client_finished;
                                if let (Some(r), Some((c, s))) = (st.records.as_mut(), st.app_keys)
                                {
                                    r.reset(s, c);
                                }
                                st.phase = Phase::WaitRequest;
                            }
                            Some(Ok((ContentType::ApplicationData, m, _)))
                                if st.phase == Phase::WaitRequest =>
                            {
                                let take = m.min(st.request_seen.len());
                                st.request_seen[..take].copy_from_slice(&plain[..take]);
                                st.request_len = take;
                                let mut answer = [0u8; 128];
                                let a = 9 + take;
                                answer[..9].copy_from_slice(b"peer-saw:");
                                answer[9..a].copy_from_slice(&plain[..take]);
                                let mut sealed = [0u8; 256];
                                if let Some(r) = st.records.as_mut() {
                                    if let Ok(k) = r.seal(
                                        ContentType::ApplicationData,
                                        &answer[..a],
                                        0,
                                        &mut sealed,
                                    ) {
                                        append(&mut st.tx, &mut st.tx_len, &sealed[..k]);
                                    }
                                    if let Ok(k) = r.seal(
                                        ContentType::Alert,
                                        &[1, ALERT_CLOSE_NOTIFY],
                                        0,
                                        &mut sealed,
                                    ) {
                                        append(&mut st.tx, &mut st.tx_len, &sealed[..k]);
                                    }
                                }
                                st.phase = Phase::Answered;
                            }
                            Some(Ok((ContentType::ApplicationData, _, _))) => {
                                st.client_records_before_finished += 1;
                            }
                            _ => st.phase = Phase::Over,
                        }
                    }
                    _ => st.phase = Phase::Over,
                }
                st.rx.copy_within(total..st.rx_len, 0);
                st.rx_len -= total;
            }
        }

        fn answer_hello(&self, st: &mut PeerState, hello: &[u8]) {
            if self.behaviour == Behaviour::PlaintextAlert {
                // handshake_failure (40), fatal (2).
                append(
                    &mut st.tx,
                    &mut st.tx_len,
                    &[RECORD_ALERT, 3, 3, 0, 2, 2, 40],
                );
                st.phase = Phase::Over;
                return;
            }
            append(&mut st.transcript, &mut st.transcript_len, hello);
            let mut sh = [0u8; 256];
            let sh_len = fixture_server_hello(&[0u8; 32], &FIXTURE_SERVER_PRIVATE, &mut sh);
            append(&mut st.transcript, &mut st.transcript_len, &sh[..sh_len]);
            let mut rec = [0u8; 512];
            let k = plaintext_record(RECORD_HANDSHAKE, &sh[..sh_len], &mut rec);
            append(&mut st.tx, &mut st.tx_len, &rec[..k]);

            // The stand-in knows the client's share because the suite chose the client's key.
            let client_public = public_key(&FIXTURE_CLIENT_PRIVATE).unwrap_or([0u8; 32]);
            let shared = x25519(&FIXTURE_SERVER_PRIVATE, &client_public).unwrap_or([0u8; 32]);
            let _ = st.schedule.mix_shared_secret(&shared);
            let th = sha256(&st.transcript[..st.transcript_len]);
            let (c_hs, s_hs) = st
                .schedule
                .handshake_traffic_secrets(&th)
                .unwrap_or(([0u8; 32], [0u8; 32]));
            st.client_hs_secret = c_hs;
            let (ck, sk) = (
                traffic_keys(&c_hs).unwrap_or(TrafficKeys {
                    key: [0; 32],
                    iv: [0; 12],
                }),
                traffic_keys(&s_hs).unwrap_or(TrafficKeys {
                    key: [0; 32],
                    iv: [0; 12],
                }),
            );
            if let Some(r) = st.records.as_mut() {
                r.reset(sk, ck);
            }

            if self.behaviour != Behaviour::NoChangeCipherSpec {
                append(
                    &mut st.tx,
                    &mut st.tx_len,
                    &[RECORD_CHANGE_CIPHER_SPEC, 3, 3, 0, 1, 1],
                );
            }
            let mut sealed = [0u8; 1024];
            if self.behaviour == Behaviour::DataBeforeFinished {
                if let Some(Ok(k)) = st
                    .records
                    .as_mut()
                    .map(|r| r.seal(ContentType::ApplicationData, b"too early", 0, &mut sealed))
                {
                    append(&mut st.tx, &mut st.tx_len, &sealed[..k]);
                }
            }

            // EncryptedExtensions, Certificate and CertificateVerify, coalesced in ONE record: a
            // real server does exactly this, and the client must split them by message.
            let mut flight = [0u8; 1024];
            let mut flight_len = 0usize;
            let ee = [ENCRYPTED_EXTENSIONS, 0, 0, 2, 0, 0];
            append(&mut flight, &mut flight_len, &ee);
            append(&mut st.transcript, &mut st.transcript_len, &ee);
            let mut cert_body = [0u8; 512];
            let cb = certificate_message(&[&LEAF_FIXTURE], &mut cert_body).unwrap_or(0);
            let mut cert = [0u8; 512];
            let cl = handshake_message(CERTIFICATE, &cert_body[..cb], &mut cert);
            append(&mut flight, &mut flight_len, &cert[..cl]);
            append(&mut st.transcript, &mut st.transcript_len, &cert[..cl]);
            let mut signature = FIXTURE_CERTIFICATE_VERIFY;
            if self.behaviour == Behaviour::WrongSignature {
                signature[0] ^= 0x01;
            }
            let cv = certificate_verify_message(&signature);
            append(&mut flight, &mut flight_len, &cv);
            append(&mut st.transcript, &mut st.transcript_len, &cv);
            if let Some(Ok(k)) = st.records.as_mut().map(|r| {
                r.seal(
                    ContentType::Handshake,
                    &flight[..flight_len],
                    0,
                    &mut sealed,
                )
            }) {
                append(&mut st.tx, &mut st.tx_len, &sealed[..k]);
            }

            // Finished, in its own record.
            let fin_val = finished_mac(&s_hs, &sha256(&st.transcript[..st.transcript_len]));
            let mut fin = [0u8; 36];
            handshake_message(FINISHED, &fin_val, &mut fin);
            append(&mut st.transcript, &mut st.transcript_len, &fin);
            if let Some(Ok(k)) = st
                .records
                .as_mut()
                .map(|r| r.seal(ContentType::Handshake, &fin, 0, &mut sealed))
            {
                if self.behaviour == Behaviour::CorruptFinished {
                    sealed[HEADER_LEN + 3] ^= 0x01;
                }
                append(&mut st.tx, &mut st.tx_len, &sealed[..k]);
            }

            let _ = st.schedule.finish_handshake();
            let th_fin = sha256(&st.transcript[..st.transcript_len]);
            if let Ok((c_ap, s_ap)) = st.schedule.application_traffic_secrets(&th_fin) {
                if let (Ok(ck), Ok(sk)) = (traffic_keys(&c_ap), traffic_keys(&s_ap)) {
                    st.app_keys = Some((ck, sk));
                }
            }
            st.expected_client_finished = finished_mac(&st.client_hs_secret, &th_fin);
            st.phase = Phase::WaitClientFinished;
        }
    }

    impl Ipv4Link for Peer {
        fn send_ipv4(&self, datagram: &[u8]) -> Result<(), LinkError> {
            if self.behaviour == Behaviour::Deaf {
                return Ok(());
            }
            let Ok(ip) = parse_ipv4(datagram) else {
                return Err(LinkError::Device);
            };
            let Ok(v) = parse_tcp(&ip) else {
                return Err(LinkError::Device);
            };
            let mut flags = ACK;
            let mut speak = false;
            {
                let mut st = self.st.borrow_mut();
                let st = &mut *st;
                if v.has(SYN) {
                    st.ack = v.seq.wrapping_add(1);
                    flags |= SYN;
                    speak = true;
                } else if v.seq == st.ack {
                    if !v.payload.is_empty() {
                        let ok = append(&mut st.rx, &mut st.rx_len, v.payload);
                        if !ok {
                            return Err(LinkError::TooLong);
                        }
                        speak = true;
                    }
                    st.ack = st.ack.wrapping_add(v.seq_len());
                    if v.has(FIN) {
                        st.client_closed = true;
                        speak = true;
                    }
                }
            }
            self.serve();
            let mut st = self.st.borrow_mut();
            let st = &mut *st;
            let remaining = st.tx_len - st.tx_sent;
            let payload_len = remaining.min(CHUNK);
            let all_sent = payload_len == remaining;
            let want_fin = !st.fin_sent
                && all_sent
                && (st.client_closed || matches!(st.phase, Phase::Answered | Phase::Over));
            if payload_len > 0 || want_fin {
                speak = true;
            }
            if !speak || st.queued == QUEUE {
                return Ok(());
            }
            let mut body = [0u8; CHUNK];
            body[..payload_len].copy_from_slice(&st.tx[st.tx_sent..st.tx_sent + payload_len]);
            st.tx_sent += payload_len;
            if payload_len > 0 {
                flags |= PSH;
            }
            if want_fin {
                flags |= FIN;
                st.fin_sent = true;
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
            let slot = st.queued;
            st.queue[slot][..k].copy_from_slice(&wire[..k]);
            st.queue_len[slot] = k;
            st.queued += 1;
            Ok(())
        }

        fn recv_ipv4(
            &self,
            _spins: u64,
            _protocol: u8,
            out: &mut [u8],
        ) -> Result<usize, LinkError> {
            let mut st = self.st.borrow_mut();
            if st.queued == 0 {
                return Ok(0);
            }
            let k = st.queue_len[0];
            if out.len() < k {
                return Err(LinkError::TooLong);
            }
            out[..k].copy_from_slice(&st.queue[0][..k]);
            for i in 1..st.queued {
                let len = st.queue_len[i];
                let moved = st.queue[i];
                st.queue[i - 1][..len].copy_from_slice(&moved[..len]);
                st.queue_len[i - 1] = len;
            }
            st.queued -= 1;
            Ok(k)
        }

        fn local_ip(&self) -> [u8; 4] {
            CLIENT
        }
    }

    fn peer(behaviour: Behaviour, records: Option<RecordLayer>) -> Peer {
        Peer {
            st: RefCell::new(PeerState {
                seq: 0x7000,
                ack: 0,
                queue: [[0u8; TX_BUF_MIN]; QUEUE],
                queue_len: [0; QUEUE],
                queued: 0,
                client_closed: false,
                fin_sent: false,
                rx: [0; 2048],
                rx_len: 0,
                tx: [0; 4096],
                tx_len: 0,
                tx_sent: 0,
                phase: Phase::WaitHello,
                transcript: [0; 2048],
                transcript_len: 0,
                records,
                schedule: KeySchedule::new(None),
                client_hs_secret: [0; 32],
                app_keys: None,
                expected_client_finished: [0; 32],
                client_finished_ok: false,
                client_records_before_finished: 0,
                request_seen: [0; 64],
                request_len: 0,
            }),
            behaviour,
        }
    }

    /// Everything the suite allocates, allocated ONCE: the client's pump and handshake, and the
    /// stand-in's record layer. Eight conversations that each built their own would spend over a
    /// megabyte on a heap that never frees - and did, before this struct existed: the desktop that
    /// boots after this suite found the heap gone ("memory allocation of 2600 bytes failed").
    struct Fixtures {
        pump: TlsPump,
        hs: Handshake<PinnedRoot>,
        peer_records: Option<RecordLayer>,
    }

    /// One conversation against a stand-in of the given behaviour.
    fn converse(
        f: &mut Fixtures,
        behaviour: Behaviour,
        budget: u64,
        reply: &mut [u8],
    ) -> (Result<TlsOutcome, TlsRefusal>, bool, u32, [u8; 64], usize) {
        let l = peer(behaviour, f.peer_records.take());
        let mut c = Connection::new(CLIENT, CPORT, SERVER, SPORT, 4);
        if f.hs
            .restart(FIXTURE_CLIENT_PRIVATE, FIXTURE_CLIENT_RANDOM)
            .is_err()
        {
            return (Err(TlsRefusal::NoRoom), false, 0, [0; 64], 0);
        }
        let mut tick = 0u64;
        let plan = Plan {
            iss: 0x1000,
            budget,
            spins_per_turn: 1,
        };
        let got = exchange(
            &l,
            &mut c,
            plan,
            &mut f.pump,
            &mut f.hs,
            b"hello-over-tls",
            reply,
            &mut || {
                tick += 1;
                tick
            },
        );
        let st = l.st.into_inner();
        f.peer_records = st.records;
        (
            got,
            st.client_finished_ok,
            st.client_records_before_finished,
            st.request_seen,
            st.request_len,
        )
    }

    let verifier = match PinnedRoot::new(ROOT_KEY_FIXTURE, FIXTURE_TIME) {
        Ok(v) => v,
        Err(_) => return Err((1, "tlsclient: the pinned verifier could not be built")),
    };
    let hs = match Handshake::new(
        verifier,
        FIXTURE_NAME,
        FIXTURE_CLIENT_PRIVATE,
        FIXTURE_CLIENT_RANDOM,
    ) {
        Ok(h) => h,
        Err(_) => return Err((1, "tlsclient: a handshake could not be constructed")),
    };
    let zero = TrafficKeys {
        key: [0u8; 32],
        iv: [0u8; 12],
    };
    let mut f = Fixtures {
        pump: TlsPump::new(),
        hs,
        peer_records: Some(RecordLayer::new(zero, zero)),
    };

    // 1 — the whole conversation: a connection opened, a ClientHello on the wire, the peer's
    //     flight split into its messages, its certificate accepted under the pin, its signature
    //     over THIS transcript verified, our Finished accepted by the peer, the request sent
    //     protected, and the protected answer decrypted into the caller's buffer.
    {
        let mut reply = [0u8; 64];
        let (got, fin_ok, early, seen, seen_len) =
            converse(&mut f, Behaviour::Honest, 96, &mut reply);
        let ok = match got {
            Ok(o) => {
                o.received == 9 + 14
                    && &reply[..o.received] == b"peer-saw:hello-over-tls"
                    && o.peer_key == LEAF_KEY_FIXTURE
                    && o.records_in >= 3
                    && o.records_out >= 3
                    && !o.truncated
            }
            Err(_) => false,
        };
        check!(
            ok && fin_ok && early == 0 && &seen[..seen_len] == b"hello-over-tls",
            "tlsclient: over a link and a stand-in server the handshake completes, the request goes out protected and the answer comes back decrypted"
        );
    }

    // 2 — a peer whose CertificateVerify is one bit off is refused as a bad signature, and this
    //     client sent NOTHING protected: no Finished, no request. The pin was honoured; the
    //     party holding the pinned certificate could not prove it held its key.
    {
        let mut reply = [0u8; 64];
        let (got, fin_ok, early, _, seen_len) =
            converse(&mut f, Behaviour::WrongSignature, 96, &mut reply);
        check!(
            got == Err(TlsRefusal::Handshake(HandshakeRefusal::BadSignature))
                && !fin_ok
                && early == 0
                && seen_len == 0,
            "tlsclient: a peer that cannot prove its key ends the conversation by name and receives nothing protected"
        );
    }

    // 3 — one flipped ciphertext byte in the peer's Finished record is a failed tag, and a failed
    //     tag is fatal: the conversation ends, and the request is never sent.
    {
        let mut reply = [0u8; 64];
        let (got, _, _, _, seen_len) = converse(&mut f, Behaviour::CorruptFinished, 96, &mut reply);
        check!(
            got == Err(TlsRefusal::Record(RecordRefusal::Fatal)) && seen_len == 0,
            "tlsclient: a record that fails authentication ends the conversation and nothing is sent after it"
        );
    }

    // 4 — application data before the peer's Finished is refused by name: a server that talks
    //     before it is verified is not one this client listens to.
    {
        let mut reply = [0u8; 64];
        let (got, _, _, _, seen_len) =
            converse(&mut f, Behaviour::DataBeforeFinished, 96, &mut reply);
        check!(
            got == Err(TlsRefusal::DataBeforeFinished) && seen_len == 0 && reply.iter().all(|&b| b == 0),
            "tlsclient: application data before the handshake finished is refused and never copied out"
        );
    }

    // 5 — a peer that never answers costs the budget and is refused by name.
    {
        let mut reply = [0u8; 16];
        let (got, _, _, _, _) = converse(&mut f, Behaviour::Deaf, 12, &mut reply);
        check!(
            matches!(
                got,
                Err(TlsRefusal::Link(LinkError::BudgetSpent))
                    | Err(TlsRefusal::Link(LinkError::ConnectionEnded))
            ),
            "tlsclient: a deaf peer costs the budget and is refused by name, never waited on forever"
        );
    }

    // 6 — a plaintext fatal alert in place of a ServerHello ends the conversation naming the
    //     alert, and nothing protected is ever produced.
    {
        let mut reply = [0u8; 16];
        let (got, _, _, _, _) = converse(&mut f, Behaviour::PlaintextAlert, 96, &mut reply);
        check!(
            got == Err(TlsRefusal::Alert(40)),
            "tlsclient: a fatal alert from the peer ends the conversation naming the alert"
        );
    }

    // 7 — the compatibility ChangeCipherSpec is skipped whether or not the peer sends one: an
    //     honest server without it completes exactly as one with it.
    {
        let mut reply = [0u8; 64];
        let (got, fin_ok, _, _, _) =
            converse(&mut f, Behaviour::NoChangeCipherSpec, 96, &mut reply);
        check!(
            got.is_ok_and(|o| o.received == 23) && fin_ok,
            "tlsclient: the compatibility ChangeCipherSpec is skipped whether or not the peer sends it"
        );
    }

    // 8 — an answer larger than the caller's buffer is truncated and said to be, never written
    //     past the buffer.
    {
        let mut guarded = [0xAAu8; 16];
        let (got, _, _, _, _) = converse(&mut f, Behaviour::Honest, 96, &mut guarded[..8]);
        check!(
            got.is_ok_and(|o| o.received == 8 && o.truncated)
                && &guarded[..8] == b"peer-saw"
                && guarded[8..].iter().all(|&b| b == 0xAA),
            "tlsclient: an answer larger than the caller's buffer is truncated, said so, never overflowed"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_join_invariant() {
        let mut seen = 0;
        let n = tlsclient_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the tls client suite should hold");
        assert_eq!(n, 8);
        assert_eq!(seen, 8);
    }

    #[test]
    fn ephemeral_material_separates_key_from_random() {
        let (k, r) = ephemeral_material(b"seed");
        let (k2, r2) = ephemeral_material(b"seed");
        let (k3, _) = ephemeral_material(b"seeds");
        assert_ne!(k, r);
        assert_eq!((k, r), (k2, r2));
        assert_ne!(k, k3);
    }
}
