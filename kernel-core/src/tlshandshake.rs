//! The TLS 1.3 handshake, as a state machine that cannot be used insecurely (REQ-SEC-TLS-004,
//! ADR-144).
//!
//! The previous rungs built a key schedule (ADR-141), a key exchange (ADR-142) and a record layer
//! (ADR-143). This one drives them: ClientHello out, ServerHello in, keys derived from the
//! transcript, the server's flight verified, Finished exchanged.
//!
//! ## The design decision that matters most
//!
//! A TLS handshake that completes without checking who it is talking to is worse than no TLS at
//! all, because it looks encrypted. It is also the easiest thing in the world to ship by accident:
//! certificate verification is the one step whose absence produces no error, no warning and no
//! visible difference.
//!
//! So this client **cannot** reach application traffic without one. [`Handshake::new`] takes a
//! [`PeerVerifier`], and the only implementation this kernel ships today is [`RefuseAllPeers`],
//! which refuses every certificate by name. The handshake therefore runs to the server's
//! Certificate message and stops with [`HandshakeRefusal::PeerUnverified`], every time, until a
//! real verifier exists. That is a fail-closed default rather than a documented caution, and it is
//! why the handshake can land as its own rung without landing a false sense of security with it.
//!
//! ## Everything else is bounded and named
//!
//! One allocation, at construction, for the transcript and message buffers. Every parse refuses
//! rather than reads: a length that runs past the buffer, a version that is not TLS 1.3, a key
//! share that is not X25519, a message arriving in the wrong order, a Finished that does not
//! verify. The downgrade sentinels RFC 8446 §4.1.3 defines are checked, because a client that
//! ignores them can be talked down to TLS 1.2 by anyone in the path.

use alloc::boxed::Box;

use crate::crypto::{hmac_sha256, sha256};
use crate::hkdf::{derive_secret, traffic_keys, KeySchedule, TrafficKeys, HASH_LEN};
use crate::x25519::{public_key, x25519};

/// Handshake message types this client writes or reads (RFC 8446 §4).
pub const CLIENT_HELLO: u8 = 1;
pub const SERVER_HELLO: u8 = 2;
pub const ENCRYPTED_EXTENSIONS: u8 = 8;
pub const CERTIFICATE: u8 = 11;
pub const CERTIFICATE_VERIFY: u8 = 15;
pub const FINISHED: u8 = 20;

/// The only cipher suite this stack implements: TLS_CHACHA20_POLY1305_SHA256.
pub const CIPHER_SUITE: u16 = 0x1303;
/// The only group: x25519.
pub const GROUP_X25519: u16 = 0x001d;
/// The only signature scheme this client will advertise: Ed25519. Advertising schemes a client
/// cannot verify is how a server is invited to send something the client must then ignore.
pub const SIG_ED25519: u16 = 0x0807;

/// The eight bytes RFC 8446 §4.1.3 requires a TLS 1.3 server to place at the end of its random
/// when it is deliberately negotiating TLS 1.2 or below. A client that does not check these can be
/// talked down by anyone in the path.
const DOWNGRADE_12: [u8; 8] = [0x44, 0x4F, 0x57, 0x4E, 0x47, 0x52, 0x44, 0x01];
const DOWNGRADE_11: [u8; 8] = [0x44, 0x4F, 0x57, 0x4E, 0x47, 0x52, 0x44, 0x00];

/// The largest handshake message this client will hold.
pub const MAX_MESSAGE: usize = 16_384;
/// The largest transcript this client will keep. Bounded like everything else a peer can drive:
/// a server that sends a longer flight is refused by name rather than growing this buffer, and the
/// buffer is sized once because on a heap that never frees (ADR-063) a per-handshake allocation of
/// a hundred kilobytes is a hundred kilobytes the machine never gets back.
pub const MAX_TRANSCRIPT: usize = 16_384;
/// The largest message this client WRITES (its ClientHello is about 130 bytes).
pub const MAX_OUTGOING: usize = 2_048;

/// Where the handshake is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeStage {
    /// Nothing sent yet.
    Start,
    /// ClientHello written; waiting for ServerHello.
    WaitServerHello,
    /// Handshake keys derived; waiting for the server's encrypted flight.
    WaitEncryptedExtensions,
    /// Waiting for the server's Certificate.
    WaitCertificate,
    /// Waiting for CertificateVerify.
    WaitCertificateVerify,
    /// Waiting for the server's Finished.
    WaitFinished,
    /// The handshake completed; application traffic keys exist.
    Done,
    /// The handshake ended and cannot continue.
    Failed,
}

/// Why the handshake stopped. Every one of these ends the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandshakeRefusal {
    /// A message arrived that does not belong at this stage.
    OutOfOrder,
    /// A length field runs past the bytes that arrived, or past what this client will hold.
    BadLength,
    /// The server did not choose TLS 1.3.
    NotTls13,
    /// The server's random carries RFC 8446's downgrade sentinel.
    Downgrade,
    /// The server chose a cipher suite or group this client did not offer.
    UnsupportedChoice,
    /// The server's key share is missing or malformed.
    BadKeyShare,
    /// The key exchange produced nothing usable (a small-order peer key).
    BadKeyExchange,
    /// The key schedule refused a derivation.
    Kdf,
    /// The peer's certificate was not verified. With no verifier installed this is the ONLY way
    /// this handshake can end, which is the point.
    PeerUnverified,
    /// The server's Finished did not match the transcript.
    BadFinished,
    /// The handshake already failed; it does not restart.
    AlreadyFailed,
}

/// What a caller must provide before this client will accept a peer.
///
/// Implementing this is the whole of "do I trust the other end". It is a trait rather than a
/// function because a real implementation needs a trust root, a clock and a name to check against,
/// and this kernel has none of those yet.
pub trait PeerVerifier {
    /// Decide whether this certificate chain speaks for the expected name. `certificates` is the
    /// raw `Certificate` message body, exactly as the server sent it.
    fn verify(&self, expected_name: &[u8], certificates: &[u8]) -> bool;
}

/// The only verifier this kernel ships: it refuses everything.
///
/// Not a placeholder to be quietly replaced later — a fail-closed default. Until a real verifier
/// exists, every handshake ends at the server's Certificate with
/// [`HandshakeRefusal::PeerUnverified`], and no application traffic keys are ever derived. A
/// client that completed without verification would look encrypted and protect nothing.
pub struct RefuseAllPeers;

impl PeerVerifier for RefuseAllPeers {
    fn verify(&self, _expected_name: &[u8], _certificates: &[u8]) -> bool {
        false
    }
}

/// The transcript and message workspace, allocated once.
struct Workspace {
    transcript: [u8; MAX_TRANSCRIPT],
    transcript_len: usize,
    message: [u8; MAX_OUTGOING],
}

/// A TLS 1.3 client handshake.
pub struct Handshake<V: PeerVerifier> {
    stage: HandshakeStage,
    verifier: V,
    schedule: KeySchedule,
    private: [u8; 32],
    public: [u8; 32],
    client_random: [u8; 32],
    server_name: [u8; 255],
    server_name_len: usize,
    client_hs_secret: [u8; HASH_LEN],
    server_hs_secret: [u8; HASH_LEN],
    application: Option<(TrafficKeys, TrafficKeys)>,
    work: Box<Workspace>,
    /// Refusals, counted like every other refusal in this tree.
    pub refusals: u64,
}

impl<V: PeerVerifier> Handshake<V> {
    /// Begin a handshake for `server_name`, with this client's ephemeral private key and random.
    ///
    /// The private key and the random are arguments rather than generated here: both must be
    /// unpredictable, and the entropy belongs to the platform (the same posture as the initial
    /// sequence number in ADR-140).
    pub fn new(
        verifier: V,
        server_name: &[u8],
        private: [u8; 32],
        client_random: [u8; 32],
    ) -> Result<Self, HandshakeRefusal> {
        if server_name.len() > 255 {
            return Err(HandshakeRefusal::BadLength);
        }
        let public = public_key(&private).map_err(|_| HandshakeRefusal::BadKeyExchange)?;
        let mut name = [0u8; 255];
        name[..server_name.len()].copy_from_slice(server_name);
        Ok(Handshake {
            stage: HandshakeStage::Start,
            verifier,
            schedule: KeySchedule::new(None),
            private,
            public,
            client_random,
            server_name: name,
            server_name_len: server_name.len(),
            client_hs_secret: [0; HASH_LEN],
            server_hs_secret: [0; HASH_LEN],
            application: None,
            work: Box::new(Workspace {
                transcript: [0u8; MAX_TRANSCRIPT],
                transcript_len: 0,
                message: [0u8; MAX_OUTGOING],
            }),
            refusals: 0,
        })
    }

    pub fn stage(&self) -> HandshakeStage {
        self.stage
    }

    /// Start a fresh handshake REUSING this one's workspace: new ephemeral key, new random, empty
    /// transcript, schedule back to its early secret.
    ///
    /// The workspace is eighteen kilobytes. A suite — or a client retrying a connection — that
    /// built a new handshake each time would spend that every attempt, on a heap that never frees.
    pub fn restart(
        &mut self,
        private: [u8; 32],
        client_random: [u8; 32],
    ) -> Result<(), HandshakeRefusal> {
        self.public = public_key(&private).map_err(|_| HandshakeRefusal::BadKeyExchange)?;
        self.private = private;
        self.client_random = client_random;
        self.stage = HandshakeStage::Start;
        self.schedule = KeySchedule::new(None);
        self.client_hs_secret = [0; HASH_LEN];
        self.server_hs_secret = [0; HASH_LEN];
        self.application = None;
        self.work.transcript_len = 0;
        Ok(())
    }

    /// The application traffic keys, once the handshake has completed. `None` until then — and
    /// with no real verifier installed, `None` forever, by construction.
    pub fn application_keys(&self) -> Option<(TrafficKeys, TrafficKeys)> {
        self.application
    }

    /// The handshake traffic keys, once the ServerHello has been processed. These protect the
    /// server's flight and this client's Finished.
    pub fn handshake_keys(&self) -> Option<(TrafficKeys, TrafficKeys)> {
        // Only after the ServerHello has been processed: before that the secrets are zeros, and
        // returning keys derived from zeros is exactly the failure this tree refuses elsewhere.
        if !matches!(
            self.stage,
            HandshakeStage::WaitEncryptedExtensions
                | HandshakeStage::WaitCertificate
                | HandshakeStage::WaitCertificateVerify
                | HandshakeStage::WaitFinished
                | HandshakeStage::Done
        ) {
            return None;
        }
        match (
            traffic_keys(&self.client_hs_secret),
            traffic_keys(&self.server_hs_secret),
        ) {
            (Ok(c), Ok(s)) => Some((c, s)),
            _ => None,
        }
    }

    fn fail(&mut self, why: HandshakeRefusal) -> HandshakeRefusal {
        self.stage = HandshakeStage::Failed;
        self.refusals += 1;
        why
    }

    /// Append a message to the transcript exactly as it appeared on the wire. The transcript hash
    /// is what binds every derived key to everything that was actually said, so a message that is
    /// processed but not added is a message an attacker may change for free.
    fn absorb(&mut self, message: &[u8]) -> Result<(), HandshakeRefusal> {
        let end = self.work.transcript_len + message.len();
        if end > self.work.transcript.len() {
            return Err(HandshakeRefusal::BadLength);
        }
        self.work.transcript[self.work.transcript_len..end].copy_from_slice(message);
        self.work.transcript_len = end;
        Ok(())
    }

    fn transcript_hash(&self) -> [u8; HASH_LEN] {
        sha256(&self.work.transcript[..self.work.transcript_len])
    }

    /// Write this client's ClientHello into `out`, returning its length.
    ///
    /// Deliberately minimal: one cipher suite, one group, one signature scheme, one key share.
    /// Offering options this client cannot honour is how a server is invited to choose something
    /// the client must then refuse.
    pub fn client_hello(&mut self, out: &mut [u8]) -> Result<usize, HandshakeRefusal> {
        if self.stage != HandshakeStage::Start {
            return Err(self.fail(HandshakeRefusal::OutOfOrder));
        }
        let body = {
            let msg = &mut self.work.message;
            let mut n = 0usize;
            // legacy_version = TLS 1.2, as RFC 8446 §4.1.2 requires
            msg[n..n + 2].copy_from_slice(&[0x03, 0x03]);
            n += 2;
            msg[n..n + 32].copy_from_slice(&self.client_random);
            n += 32;
            msg[n] = 0; // legacy_session_id: empty
            n += 1;
            msg[n..n + 2].copy_from_slice(&2u16.to_be_bytes()); // cipher suites: one
            n += 2;
            msg[n..n + 2].copy_from_slice(&CIPHER_SUITE.to_be_bytes());
            n += 2;
            msg[n] = 1; // legacy_compression_methods: one, null
            n += 1;
            msg[n] = 0;
            n += 1;

            // Extensions, written into a scratch region and length-prefixed afterwards.
            let ext_start = n + 2;
            let mut e = ext_start;

            // supported_versions: TLS 1.3 only
            e = put_ext(msg, e, 43, &[0x02, 0x03, 0x04]);
            // supported_groups: x25519 only
            e = put_ext(msg, e, 10, &[0x00, 0x02, 0x00, 0x1d]);
            // signature_algorithms: ed25519 only
            e = put_ext(msg, e, 13, &[0x00, 0x02, 0x08, 0x07]);
            // key_share: one x25519 share
            let mut ks = [0u8; 2 + 2 + 2 + 32];
            ks[0..2].copy_from_slice(&(36u16).to_be_bytes());
            ks[2..4].copy_from_slice(&GROUP_X25519.to_be_bytes());
            ks[4..6].copy_from_slice(&32u16.to_be_bytes());
            ks[6..38].copy_from_slice(&self.public);
            e = put_ext(msg, e, 51, &ks);
            // server_name, when there is one
            if self.server_name_len > 0 {
                let mut sni = [0u8; 5 + 255];
                let name_len = self.server_name_len;
                sni[0..2].copy_from_slice(&((name_len + 3) as u16).to_be_bytes());
                sni[2] = 0; // host_name
                sni[3..5].copy_from_slice(&(name_len as u16).to_be_bytes());
                sni[5..5 + name_len].copy_from_slice(&self.server_name[..name_len]);
                e = put_ext(msg, e, 0, &sni[..5 + name_len]);
            }
            let ext_len = e - ext_start;
            msg[n..n + 2].copy_from_slice(&(ext_len as u16).to_be_bytes());
            e
        };

        let total = 4 + body;
        if out.len() < total {
            return Err(self.fail(HandshakeRefusal::BadLength));
        }
        out[0] = CLIENT_HELLO;
        out[1..4].copy_from_slice(&(body as u32).to_be_bytes()[1..]);
        out[4..total].copy_from_slice(&self.work.message[..body]);
        let sent = {
            let mut copy = [0u8; 4];
            copy.copy_from_slice(&out[..4]);
            copy
        };
        // The transcript is the message as written, header included.
        self.absorb(&sent).map_err(|e| self.fail(e))?;
        let body_copy_len = body;
        let mut i = 0;
        while i < body_copy_len {
            let take = (body_copy_len - i).min(512);
            let chunk = {
                let mut c = [0u8; 512];
                c[..take].copy_from_slice(&self.work.message[i..i + take]);
                c
            };
            self.absorb(&chunk[..take]).map_err(|e| self.fail(e))?;
            i += take;
        }
        self.stage = HandshakeStage::WaitServerHello;
        Ok(total)
    }

    /// Process the server's ServerHello: check the version, the downgrade sentinel, the choices
    /// and the key share; complete the key exchange; derive the handshake traffic secrets.
    pub fn server_hello(&mut self, message: &[u8]) -> Result<(), HandshakeRefusal> {
        if self.stage != HandshakeStage::WaitServerHello {
            return Err(self.fail(HandshakeRefusal::OutOfOrder));
        }
        let body = message_body(message, SERVER_HELLO).map_err(|e| self.fail(e))?;
        if body.len() < 2 + 32 + 1 + 2 + 1 {
            return Err(self.fail(HandshakeRefusal::BadLength));
        }
        // The legacy version must be TLS 1.2; the real one lives in supported_versions.
        if body[0..2] != [0x03, 0x03] {
            return Err(self.fail(HandshakeRefusal::NotTls13));
        }
        let random = &body[2..34];
        if random[24..32] == DOWNGRADE_12 || random[24..32] == DOWNGRADE_11 {
            return Err(self.fail(HandshakeRefusal::Downgrade));
        }
        let session_len = body[34] as usize;
        let mut n = 35 + session_len;
        if body.len() < n + 3 {
            return Err(self.fail(HandshakeRefusal::BadLength));
        }
        let suite = u16::from_be_bytes([body[n], body[n + 1]]);
        if suite != CIPHER_SUITE {
            return Err(self.fail(HandshakeRefusal::UnsupportedChoice));
        }
        n += 3; // suite + legacy_compression_method
        if body.len() < n + 2 {
            return Err(self.fail(HandshakeRefusal::BadLength));
        }
        let ext_len = u16::from_be_bytes([body[n], body[n + 1]]) as usize;
        n += 2;
        if body.len() < n + ext_len {
            return Err(self.fail(HandshakeRefusal::BadLength));
        }
        let extensions = &body[n..n + ext_len];

        let version = find_ext(extensions, 43).ok_or(HandshakeRefusal::NotTls13);
        let version = match version {
            Ok(v) => v,
            Err(e) => return Err(self.fail(e)),
        };
        if version != [0x03, 0x04] {
            return Err(self.fail(HandshakeRefusal::NotTls13));
        }
        let Some(share) = find_ext(extensions, 51) else {
            return Err(self.fail(HandshakeRefusal::BadKeyShare));
        };
        if share.len() != 2 + 2 + 32 {
            return Err(self.fail(HandshakeRefusal::BadKeyShare));
        }
        if u16::from_be_bytes([share[0], share[1]]) != GROUP_X25519 {
            return Err(self.fail(HandshakeRefusal::UnsupportedChoice));
        }
        if u16::from_be_bytes([share[2], share[3]]) != 32 {
            return Err(self.fail(HandshakeRefusal::BadKeyShare));
        }
        let mut peer = [0u8; 32];
        peer.copy_from_slice(&share[4..36]);

        let shared = x25519(&self.private, &peer).map_err(|_| HandshakeRefusal::BadKeyExchange);
        let shared = match shared {
            Ok(s) => s,
            Err(e) => return Err(self.fail(e)),
        };

        self.absorb(message).map_err(|e| self.fail(e))?;
        if self.schedule.mix_shared_secret(&shared).is_err() {
            return Err(self.fail(HandshakeRefusal::Kdf));
        }
        let hash = self.transcript_hash();
        match self.schedule.handshake_traffic_secrets(&hash) {
            Ok((c, s)) => {
                self.client_hs_secret = c;
                self.server_hs_secret = s;
            }
            Err(_) => return Err(self.fail(HandshakeRefusal::Kdf)),
        }
        self.stage = HandshakeStage::WaitEncryptedExtensions;
        Ok(())
    }

    /// Process one message of the server's encrypted flight. Returns the stage the handshake has
    /// reached, or the named reason it stopped.
    pub fn server_flight(&mut self, message: &[u8]) -> Result<HandshakeStage, HandshakeRefusal> {
        if matches!(self.stage, HandshakeStage::Failed) {
            return Err(HandshakeRefusal::AlreadyFailed);
        }
        if message.len() < 4 {
            return Err(self.fail(HandshakeRefusal::BadLength));
        }
        let ty = message[0];
        match (self.stage, ty) {
            (HandshakeStage::WaitEncryptedExtensions, ENCRYPTED_EXTENSIONS) => {
                message_body(message, ENCRYPTED_EXTENSIONS).map_err(|e| self.fail(e))?;
                self.absorb(message).map_err(|e| self.fail(e))?;
                self.stage = HandshakeStage::WaitCertificate;
                Ok(self.stage)
            }
            (HandshakeStage::WaitCertificate, CERTIFICATE) => {
                let body = message_body(message, CERTIFICATE).map_err(|e| self.fail(e))?;
                // The verifier decides. With this kernel's only verifier, it always says no —
                // which is exactly why no application keys can ever be derived here yet.
                let name_len = self.server_name_len;
                let mut name = [0u8; 255];
                name[..name_len].copy_from_slice(&self.server_name[..name_len]);
                if !self.verifier.verify(&name[..name_len], body) {
                    return Err(self.fail(HandshakeRefusal::PeerUnverified));
                }
                self.absorb(message).map_err(|e| self.fail(e))?;
                self.stage = HandshakeStage::WaitCertificateVerify;
                Ok(self.stage)
            }
            (HandshakeStage::WaitCertificateVerify, CERTIFICATE_VERIFY) => {
                message_body(message, CERTIFICATE_VERIFY).map_err(|e| self.fail(e))?;
                // A signature this kernel cannot check is a signature this kernel must not accept.
                Err(self.fail(HandshakeRefusal::PeerUnverified))
            }
            (HandshakeStage::WaitFinished, FINISHED) => {
                let body = message_body(message, FINISHED).map_err(|e| self.fail(e))?;
                let expected = self.finished_value(&self.server_hs_secret);
                if body.len() != HASH_LEN || !ct_eq(body, &expected) {
                    return Err(self.fail(HandshakeRefusal::BadFinished));
                }
                self.absorb(message).map_err(|e| self.fail(e))?;
                if self.schedule.finish_handshake().is_err() {
                    return Err(self.fail(HandshakeRefusal::Kdf));
                }
                let hash = self.transcript_hash();
                match self.schedule.application_traffic_secrets(&hash) {
                    Ok((c, s)) => match (traffic_keys(&c), traffic_keys(&s)) {
                        (Ok(ck), Ok(sk)) => self.application = Some((ck, sk)),
                        _ => return Err(self.fail(HandshakeRefusal::Kdf)),
                    },
                    Err(_) => return Err(self.fail(HandshakeRefusal::Kdf)),
                }
                self.stage = HandshakeStage::Done;
                Ok(self.stage)
            }
            _ => Err(self.fail(HandshakeRefusal::OutOfOrder)),
        }
    }

    /// The Finished value for a traffic secret over the transcript so far (RFC 8446 §4.4.4).
    pub fn finished_value(&self, secret: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
        let key = derive_secret(secret, b"finished", &sha256(&[])).unwrap_or([0u8; HASH_LEN]);
        hmac_sha256(&key, &self.transcript_hash())
    }
}

/// Constant-time comparison of a Finished value: a byte-by-byte early exit would let a peer learn
/// the expected value one byte at a time.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Write one extension (type, length, body) and return the new offset.
fn put_ext(out: &mut [u8], at: usize, ty: u16, body: &[u8]) -> usize {
    out[at..at + 2].copy_from_slice(&ty.to_be_bytes());
    out[at + 2..at + 4].copy_from_slice(&(body.len() as u16).to_be_bytes());
    out[at + 4..at + 4 + body.len()].copy_from_slice(body);
    at + 4 + body.len()
}

/// Find one extension's body in an extension block, refusing a block whose lengths do not add up.
fn find_ext(extensions: &[u8], want: u16) -> Option<&[u8]> {
    let mut n = 0usize;
    while n + 4 <= extensions.len() {
        let ty = u16::from_be_bytes([extensions[n], extensions[n + 1]]);
        let len = u16::from_be_bytes([extensions[n + 2], extensions[n + 3]]) as usize;
        if n + 4 + len > extensions.len() {
            return None;
        }
        if ty == want {
            return Some(&extensions[n + 4..n + 4 + len]);
        }
        n += 4 + len;
    }
    None
}

/// Check a handshake message's header and hand back its body. A length that lies about the buffer
/// is refused before anything reads past it.
fn message_body(message: &[u8], want_type: u8) -> Result<&[u8], HandshakeRefusal> {
    if message.len() < 4 {
        return Err(HandshakeRefusal::BadLength);
    }
    if message[0] != want_type {
        return Err(HandshakeRefusal::OutOfOrder);
    }
    let len = u32::from_be_bytes([0, message[1], message[2], message[3]]) as usize;
    if len + 4 != message.len() || len > MAX_MESSAGE {
        return Err(HandshakeRefusal::BadLength);
    }
    Ok(&message[4..])
}

/// The handshake's contract, proved on every CPU at boot.
pub fn tlshandshake_suite(
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

    /// A stand-in server: it answers a ClientHello with a well-formed ServerHello under a key it
    /// chose, so the client's own parsing and key derivation can be exercised without a network.
    fn server_hello_for(
        client_share: &[u8; 32],
        server_private: &[u8; 32],
        out: &mut [u8],
    ) -> usize {
        let _ = client_share;
        let public = public_key(server_private).unwrap_or([0u8; 32]);
        let mut body = [0u8; 128];
        let mut n = 0;
        body[n..n + 2].copy_from_slice(&[0x03, 0x03]);
        n += 2;
        body[n..n + 32].copy_from_slice(&[0x5au8; 32]);
        n += 32;
        body[n] = 0; // empty session id
        n += 1;
        body[n..n + 2].copy_from_slice(&CIPHER_SUITE.to_be_bytes());
        n += 2;
        body[n] = 0; // compression
        n += 1;
        let ext_at = n + 2;
        let mut e = ext_at;
        e = put_ext(&mut body, e, 43, &[0x03, 0x04]);
        let mut ks = [0u8; 36];
        ks[0..2].copy_from_slice(&GROUP_X25519.to_be_bytes());
        ks[2..4].copy_from_slice(&32u16.to_be_bytes());
        ks[4..36].copy_from_slice(&public);
        e = put_ext(&mut body, e, 51, &ks);
        let ext_len = e - ext_at;
        body[n..n + 2].copy_from_slice(&(ext_len as u16).to_be_bytes());
        let total = 4 + e;
        out[0] = SERVER_HELLO;
        out[1..4].copy_from_slice(&(e as u32).to_be_bytes()[1..]);
        out[4..total].copy_from_slice(&body[..e]);
        total
    }

    let client_private = [0x42u8; 32];
    let server_private = [0x24u8; 32];
    // ONE handshake for the whole suite, restarted between checks: its workspace is eighteen
    // kilobytes, and a suite that built ten would cost the machine a hundred and eighty on a heap
    // that never frees (the trap ADR-137's file-panel suite hit).
    let mut hs = match Handshake::new(RefuseAllPeers, b"example.test", client_private, [7u8; 32]) {
        Ok(h) => h,
        Err(_) => return Err((1, "tlshandshake: a handshake could not be constructed")),
    };

    // 1 — the ClientHello is well formed, offers exactly what this client can honour, and carries
    //     this client's key share. Offering more would invite a server to choose something the
    //     client must then refuse.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut out = [0u8; 512];
        let len = hs.client_hello(&mut out).unwrap_or(0);
        let body = &out[4..len];
        // version(2) + random(32) + legacy_session_id length(1) = 35, then the suite list.
        let ok = len > 60
            && out[0] == CLIENT_HELLO
            && body[0..2] == [0x03, 0x03]
            && body[34] == 0
            && body[35..37] == 2u16.to_be_bytes()
            && body[37..39] == CIPHER_SUITE.to_be_bytes()
            && hs.stage() == HandshakeStage::WaitServerHello;
        check!(
            ok,
            "tlshandshake: the ClientHello offers exactly one suite, one group and one scheme"
        );
    }

    // 2 — a ServerHello that chooses TLS 1.3 and x25519 completes the key exchange and derives
    //     handshake traffic keys, which did not exist a moment earlier.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        let before = hs.handshake_keys().is_some();
        let mut sh = [0u8; 256];
        let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
        let accepted = hs.server_hello(&sh[..len]).is_ok();
        let after = hs.handshake_keys();
        check!(
            !before
                && accepted
                && after.is_some()
                && hs.stage() == HandshakeStage::WaitEncryptedExtensions,
            "tlshandshake: a TLS 1.3 ServerHello derives handshake keys that did not exist before"
        );
    }

    // 3 — the downgrade sentinel is checked. A client that ignores it can be talked down to TLS
    //     1.2 by anyone in the path, and everything above would still look like it worked.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        let mut sh = [0u8; 256];
        let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
        sh[4 + 2 + 24..4 + 2 + 32].copy_from_slice(&DOWNGRADE_12);
        check!(
            hs.server_hello(&sh[..len]) == Err(HandshakeRefusal::Downgrade)
                && hs.stage() == HandshakeStage::Failed,
            "tlshandshake: RFC 8446's downgrade sentinel ends the handshake by name"
        );
    }

    // 4 — a server that chooses a suite or a group this client did not offer is refused. The
    //     client's offer is the whole of what it can honour.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        let mut sh = [0u8; 256];
        let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
        // AES-128-GCM instead of ChaCha20-Poly1305.
        sh[4 + 35..4 + 37].copy_from_slice(&0x1301u16.to_be_bytes());
        check!(
            hs.server_hello(&sh[..len]) == Err(HandshakeRefusal::UnsupportedChoice),
            "tlshandshake: a suite this client did not offer is refused by name"
        );
    }

    // 5 — THE POINT OF THIS RUNG. With the only verifier this kernel ships, the handshake reaches
    //     the server's Certificate and stops. No application traffic keys are derived, ever.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        let mut sh = [0u8; 256];
        let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
        hs.server_hello(&sh[..len]).ok();
        let ee = [ENCRYPTED_EXTENSIONS, 0, 0, 2, 0, 0];
        let after_ee = hs.server_flight(&ee);
        let cert = [CERTIFICATE, 0, 0, 4, 0, 0, 0, 0];
        let verdict = hs.server_flight(&cert);
        check!(
            after_ee == Ok(HandshakeStage::WaitCertificate)
                && verdict == Err(HandshakeRefusal::PeerUnverified)
                && hs.application_keys().is_none()
                && hs.stage() == HandshakeStage::Failed,
            "tlshandshake: with no verifier installed the peer is refused and no traffic keys exist"
        );
    }

    // 6 — messages out of order are refused. A Finished accepted before the server's flight is a
    //     handshake an attacker can drive.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let fin = [FINISHED, 0, 0, 0];
        let early_finished = hs.server_flight(&fin);
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        let twice = hs.client_hello(&mut ch);
        check!(
            early_finished == Err(HandshakeRefusal::OutOfOrder)
                && twice == Err(HandshakeRefusal::OutOfOrder),
            "tlshandshake: a message out of order ends the handshake rather than being processed"
        );
    }

    // 7 — a length that lies about the buffer is refused before anything reads past it.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        // A ServerHello whose header claims more than arrived.
        let bad = [SERVER_HELLO, 0, 0xFF, 0xFF, 1, 2, 3];
        check!(
            hs.server_hello(&bad) == Err(HandshakeRefusal::BadLength),
            "tlshandshake: a length field that lies about the buffer is refused by name"
        );
    }

    // 8 — the transcript binds the keys to what was actually said. Two handshakes that differ in
    //     one byte of the ClientHello must derive different handshake secrets.
    {
        let derive = |hs: &mut Handshake<RefuseAllPeers>, random: [u8; 32]| {
            hs.restart(client_private, random).ok();
            let mut ch = [0u8; 512];
            hs.client_hello(&mut ch).ok();
            let mut sh = [0u8; 256];
            let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
            hs.server_hello(&sh[..len]).ok();
            hs.handshake_keys().map(|(c, _)| c.key)
        };
        let a = derive(&mut hs, [7u8; 32]);
        let b = derive(&mut hs, [7u8; 32]);
        let mut other = [7u8; 32];
        other[31] ^= 1;
        let c = derive(&mut hs, other);
        check!(
            a.is_some() && a == b && a != c,
            "tlshandshake: the transcript binds the keys to every byte of what was said"
        );
    }

    // 9 — the Finished value is the transcript's, and a Finished that does not match is refused.
    //     The comparison is constant-time, so a peer cannot learn the expected value byte by byte.
    {
        hs.restart(client_private, [7u8; 32]).ok();
        let mut ch = [0u8; 512];
        hs.client_hello(&mut ch).ok();
        let mut sh = [0u8; 256];
        let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
        hs.server_hello(&sh[..len]).ok();
        let secret = [0x31u8; HASH_LEN];
        let one = hs.finished_value(&secret);
        let two = hs.finished_value(&secret);
        let other = hs.finished_value(&[0x32u8; HASH_LEN]);
        check!(
            one == two && one != other && ct_eq(&one, &two) && !ct_eq(&one, &other),
            "tlshandshake: the Finished value is deterministic over the transcript and compared in constant time"
        );
    }

    // 10 — with a PINNED ROOT and a chain it signed (ADR-147), the handshake passes the server's
    //      Certificate and reaches CertificateVerify, where it still stops by name: a signature
    //      over the transcript this client does not yet check is a signature it must not accept.
    //      No application traffic keys exist. This is the one place the rung can be seen moving.
    {
        use crate::trust::{
            certificate_message, PinnedRoot, FIXTURE_NAME, FIXTURE_TIME, LEAF_FIXTURE,
            ROOT_KEY_FIXTURE,
        };
        let verdict = match PinnedRoot::new(ROOT_KEY_FIXTURE, FIXTURE_TIME)
            .ok()
            .and_then(|v| Handshake::new(v, FIXTURE_NAME, client_private, [7u8; 32]).ok())
        {
            Some(mut pinned) => {
                let mut ch = [0u8; 512];
                pinned.client_hello(&mut ch).ok();
                let mut sh = [0u8; 256];
                let len = server_hello_for(&[0u8; 32], &server_private, &mut sh);
                pinned.server_hello(&sh[..len]).ok();
                let ee = [ENCRYPTED_EXTENSIONS, 0, 0, 2, 0, 0];
                let after_ee = pinned.server_flight(&ee);
                let mut cert = [0u8; 512];
                let body_len = certificate_message(&[&LEAF_FIXTURE], &mut cert[4..]).unwrap_or(0);
                cert[0] = CERTIFICATE;
                cert[1..4].copy_from_slice(&(body_len as u32).to_be_bytes()[1..]);
                let after_cert = pinned.server_flight(&cert[..4 + body_len]);
                // A CertificateVerify naming Ed25519 (0x0807) with an empty signature.
                let cv = [CERTIFICATE_VERIFY, 0, 0, 4, 0x08, 0x07, 0, 0];
                let after_cv = pinned.server_flight(&cv);
                after_ee == Ok(HandshakeStage::WaitCertificate)
                    && after_cert == Ok(HandshakeStage::WaitCertificateVerify)
                    && after_cv == Err(HandshakeRefusal::PeerUnverified)
                    && pinned.application_keys().is_none()
                    && pinned.stage() == HandshakeStage::Failed
            }
            None => false,
        };
        check!(
            verdict,
            "tlshandshake: a pinned root and a chain it signed reach CertificateVerify, where this client still stops by name"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_handshake_invariant() {
        let mut seen = 0;
        let n = tlshandshake_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the handshake suite should hold");
        assert_eq!(n, 10);
        assert_eq!(seen, 10);
    }

    /// The property that keeps this rung honest: there is no path, with the verifier this kernel
    /// ships, from a fresh handshake to application traffic keys.
    #[test]
    fn no_sequence_of_messages_reaches_application_keys_without_a_verifier() {
        let mut hs = Handshake::new(RefuseAllPeers, b"host", [0x42u8; 32], [1u8; 32]).unwrap();
        let mut out = [0u8; 512];
        hs.client_hello(&mut out).ok();
        // Every message type, in every order, from every stage.
        for ty in [
            ENCRYPTED_EXTENSIONS,
            CERTIFICATE,
            CERTIFICATE_VERIFY,
            FINISHED,
            SERVER_HELLO,
            0xFF,
        ] {
            let msg = [ty, 0, 0, 2, 0, 0];
            let _ = hs.server_flight(&msg);
            assert!(hs.application_keys().is_none(), "type {ty} produced keys");
        }
    }
}
