//! The TLS 1.3 record layer (REQ-SEC-TLS-003, ADR-143).
//!
//! What a record layer does is small: wrap bytes in a five-byte header, encrypt them under a
//! sequence-numbered nonce, and refuse anything that does not verify. What makes it worth its own
//! module is that almost every way to get it wrong is **silent**:
//!
//! * A sequence number that does not advance reuses a nonce, which destroys the AEAD's guarantees
//!   completely — the two records can be XORed together by anyone watching.
//! * A header taken from the ciphertext rather than authenticated as associated data lets an
//!   attacker rewrite the length in flight.
//! * A content type read from the *outer* header rather than from the decrypted inner plaintext
//!   lets a peer claim a record is whatever it wants (TLS 1.3 puts the real type inside, and the
//!   outer byte always says `application_data`).
//! * A failed tag that returns "no bytes" instead of ending the connection invites an attacker to
//!   try again with the next guess.
//!
//! So this module is a pair of sequence-numbered state machines with named refusals, and a
//! connection that sees one of them is over: [`RecordRefusal::Fatal`] is not a soft error.
//!
//! One allocation happens, at construction, for the workspace; nothing allocates per record, which
//! a heap that never frees (ADR-063) requires of anything a peer can drive.

use alloc::boxed::Box;

use crate::crypto::{aead_open_into, aead_scratch_len, aead_seal_into, AeadError};
use crate::hkdf::{record_nonce, TrafficKeys};

/// The record header this stack writes and reads: content type, legacy version, length.
pub const HEADER_LEN: usize = 5;
/// TLS 1.3's plaintext limit (RFC 8446 §5.1): 2^14 bytes of content per record.
pub const MAX_PLAINTEXT: usize = 16_384;
/// The ciphertext limit: the plaintext limit, the content-type byte, the AEAD tag, and the 255
/// bytes of padding RFC 8446 §5.2 permits.
pub const MAX_CIPHERTEXT: usize = MAX_PLAINTEXT + 1 + 255 + 16;
/// The largest record this layer will read or write, header included.
pub const MAX_RECORD: usize = HEADER_LEN + MAX_CIPHERTEXT;

/// The outer content type every protected TLS 1.3 record carries: `application_data`. The REAL
/// type is the last non-zero byte of the decrypted inner plaintext.
pub const OUTER_TYPE: u8 = 23;
/// The legacy record version field, frozen at TLS 1.2's value by RFC 8446 §5.1.
pub const LEGACY_VERSION: [u8; 2] = [0x03, 0x03];

/// Content types this stack understands, from the INNER plaintext.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentType {
    ChangeCipherSpec,
    Alert,
    Handshake,
    ApplicationData,
}

impl ContentType {
    pub fn from_byte(b: u8) -> Option<ContentType> {
        match b {
            20 => Some(ContentType::ChangeCipherSpec),
            21 => Some(ContentType::Alert),
            22 => Some(ContentType::Handshake),
            23 => Some(ContentType::ApplicationData),
            _ => None,
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            ContentType::ChangeCipherSpec => 20,
            ContentType::Alert => 21,
            ContentType::Handshake => 22,
            ContentType::ApplicationData => 23,
        }
    }
}

/// Why a record was not produced or not accepted. Every one of these ends the connection in TLS
/// 1.3 except [`RecordRefusal::Incomplete`], which means "ask me again when more bytes arrive".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordRefusal {
    /// Fewer bytes than a whole record. Not fatal: the rest may still be in flight.
    Incomplete,
    /// The content does not fit a record, or the caller's buffer does not fit the result.
    TooLong,
    /// The header is not a TLS 1.3 protected record (wrong outer type or a length past the limit).
    BadHeader,
    /// The tag did not verify. Fatal, always: in TLS 1.3 a decryption failure ends the connection
    /// rather than being retried, so an attacker gets exactly one guess.
    Fatal,
    /// The decrypted inner plaintext is all padding, or its content type is one this stack does
    /// not accept. RFC 8446 §5.4 calls the all-padding case a decode error.
    BadInnerType,
    /// The sequence number is exhausted. TLS 1.3 requires rekeying long before this, and a layer
    /// that wraps would reuse every nonce it has already used.
    SequenceExhausted,
}

/// One direction's protection: a key, an IV, and the sequence number that makes every nonce
/// different.
pub struct Direction {
    keys: TrafficKeys,
    sequence: u64,
    /// Records protected or accepted in this direction.
    pub records: u64,
    /// Refusals, counted like every other refusal in this tree.
    pub refusals: u64,
}

impl Direction {
    pub fn new(keys: TrafficKeys) -> Self {
        Direction {
            keys,
            sequence: 0,
            records: 0,
            refusals: 0,
        }
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Install a new key and reset the sequence, as a key update does. Resetting is correct ONLY
    /// because the key changed with it: a reset under the same key would reuse every nonce.
    pub fn rekey(&mut self, keys: TrafficKeys) {
        self.keys = keys;
        self.sequence = 0;
    }
}

/// The workspace. One allocation, at construction, reused by every record: a peer can send records
/// as fast as the wire allows, and on a heap that never frees a per-record buffer is a leak with a
/// remote trigger.
struct Workspace {
    inner: [u8; MAX_PLAINTEXT + 1 + 255],
    scratch: [u8; MAX_RECORD + 64],
}

/// Both directions of one connection's record protection.
pub struct RecordLayer {
    pub write: Direction,
    pub read: Direction,
    work: Box<Workspace>,
}

impl RecordLayer {
    /// Build a record layer over one key pair. The workspace is allocated HERE and never again.
    pub fn new(write_keys: TrafficKeys, read_keys: TrafficKeys) -> Self {
        RecordLayer {
            write: Direction::new(write_keys),
            read: Direction::new(read_keys),
            work: Box::new(Workspace {
                inner: [0u8; MAX_PLAINTEXT + 1 + 255],
                scratch: [0u8; MAX_RECORD + 64],
            }),
        }
    }

    /// Put this layer back to a fresh connection's state under new keys, REUSING the workspace.
    ///
    /// The workspace is thirty-three kilobytes; on a heap that never frees (ADR-063) a suite or a
    /// connection pool that built a new layer per use would spend megabytes the machine never gets
    /// back. Resetting is safe precisely because the keys change with the sequence.
    pub fn reset(&mut self, write_keys: TrafficKeys, read_keys: TrafficKeys) {
        self.write = Direction::new(write_keys);
        self.read = Direction::new(read_keys);
    }

    /// Protect one record: `content` of `ty`, with `padding` zero bytes appended inside the
    /// encryption. Writes header and ciphertext into `out` and returns the length.
    ///
    /// The padding is the caller's choice because it is a traffic-analysis decision, not a
    /// correctness one — but it is INSIDE the AEAD, which is the whole reason TLS 1.3 has it.
    pub fn seal(
        &mut self,
        ty: ContentType,
        content: &[u8],
        padding: usize,
        out: &mut [u8],
    ) -> Result<usize, RecordRefusal> {
        if content.len() > MAX_PLAINTEXT || padding > 255 {
            self.write.refusals += 1;
            return Err(RecordRefusal::TooLong);
        }
        let inner_len = content.len() + 1 + padding;
        let total = HEADER_LEN + inner_len + 16;
        if out.len() < total {
            self.write.refusals += 1;
            return Err(RecordRefusal::TooLong);
        }
        if self.write.sequence == u64::MAX {
            self.write.refusals += 1;
            return Err(RecordRefusal::SequenceExhausted);
        }

        // TLSInnerPlaintext: content || content_type || zeros.
        let inner = &mut self.work.inner[..inner_len];
        inner[..content.len()].copy_from_slice(content);
        inner[content.len()] = ty.to_byte();
        inner[content.len() + 1..].fill(0);

        // The header is the associated data, so an attacker cannot rewrite the length in flight
        // without the tag failing.
        out[0] = OUTER_TYPE;
        out[1..3].copy_from_slice(&LEGACY_VERSION);
        out[3..5].copy_from_slice(&((inner_len + 16) as u16).to_be_bytes());
        let header = [out[0], out[1], out[2], out[3], out[4]];

        let nonce = record_nonce(&self.write.keys.iv, self.write.sequence);
        let need = aead_scratch_len(HEADER_LEN, inner_len);
        let written = aead_seal_into(
            &self.write.keys.key,
            &nonce,
            &header,
            inner,
            &mut out[HEADER_LEN..],
            &mut self.work.scratch[..need],
        )
        .map_err(|_| RecordRefusal::TooLong)?;

        self.write.sequence += 1;
        self.write.records += 1;
        Ok(HEADER_LEN + written)
    }

    /// How many bytes the record at the front of `bytes` needs in total, or why it cannot be one.
    /// Separate from [`RecordLayer::open`] so a caller reading a stream can ask "do I have a whole
    /// record yet?" without handing the bytes to the AEAD.
    pub fn record_len(bytes: &[u8]) -> Result<usize, RecordRefusal> {
        if bytes.len() < HEADER_LEN {
            return Err(RecordRefusal::Incomplete);
        }
        if bytes[0] != OUTER_TYPE {
            return Err(RecordRefusal::BadHeader);
        }
        let len = u16::from_be_bytes([bytes[3], bytes[4]]) as usize;
        if !(17..=MAX_CIPHERTEXT).contains(&len) {
            // Below 17 there is not even room for one content-type byte and a tag.
            return Err(RecordRefusal::BadHeader);
        }
        let total = HEADER_LEN + len;
        if bytes.len() < total {
            return Err(RecordRefusal::Incomplete);
        }
        Ok(total)
    }

    /// Open one record from the front of `bytes`: verify, decrypt, strip the padding, and hand
    /// back the real content type with the content written into `out`.
    ///
    /// Returns the content type, the content length, and how many bytes of `bytes` the record
    /// consumed — so a caller can walk a stream without re-deriving the framing.
    pub fn open(
        &mut self,
        bytes: &[u8],
        out: &mut [u8],
    ) -> Result<(ContentType, usize, usize), RecordRefusal> {
        let total = Self::record_len(bytes).inspect_err(|e| {
            if *e != RecordRefusal::Incomplete {
                self.read.refusals += 1;
            }
        })?;
        if self.read.sequence == u64::MAX {
            self.read.refusals += 1;
            return Err(RecordRefusal::SequenceExhausted);
        }
        let header: [u8; HEADER_LEN] = [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]];
        let sealed = &bytes[HEADER_LEN..total];
        let nonce = record_nonce(&self.read.keys.iv, self.read.sequence);
        let need = aead_scratch_len(HEADER_LEN, sealed.len() - 16);
        let inner_len = match aead_open_into(
            &self.read.keys.key,
            &nonce,
            &header,
            sealed,
            &mut self.work.inner,
            &mut self.work.scratch[..need],
        ) {
            Ok(n) => n,
            // A failed tag is FATAL in TLS 1.3. Returning a soft error here would let an attacker
            // try one guess per record for as long as the connection lives.
            Err(AeadError::Authenticate) => {
                self.read.refusals += 1;
                return Err(RecordRefusal::Fatal);
            }
            Err(AeadError::Truncated) => {
                self.read.refusals += 1;
                return Err(RecordRefusal::TooLong);
            }
        };

        // The real content type is the LAST non-zero byte: everything after it is padding, and the
        // outer header always claims `application_data`.
        let inner = &self.work.inner[..inner_len];
        let Some(type_at) = inner.iter().rposition(|&b| b != 0) else {
            self.read.refusals += 1;
            return Err(RecordRefusal::BadInnerType);
        };
        let Some(ty) = ContentType::from_byte(inner[type_at]) else {
            self.read.refusals += 1;
            return Err(RecordRefusal::BadInnerType);
        };
        if out.len() < type_at {
            self.read.refusals += 1;
            return Err(RecordRefusal::TooLong);
        }
        out[..type_at].copy_from_slice(&inner[..type_at]);

        self.read.sequence += 1;
        self.read.records += 1;
        Ok((ty, type_at, total))
    }
}

/// The record layer's contract, proved on every CPU at boot.
pub fn tlsrecord_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::hkdf::traffic_keys;

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

    let client = traffic_keys(&[0x11u8; 32]).unwrap_or(TrafficKeys {
        key: [0; 32],
        iv: [0; 12],
    });
    let server = traffic_keys(&[0x22u8; 32]).unwrap_or(TrafficKeys {
        key: [0; 32],
        iv: [0; 12],
    });
    // Exactly TWO layers for the whole suite, reset between checks: each one owns a 33 KB
    // workspace, and a heap that never frees would lose a megabyte to a suite that built one per
    // check (the same trap the file panel's suite hit in ADR-137).
    let mut a = RecordLayer::new(client, server);
    let mut b = RecordLayer::new(server, client);
    macro_rules! pair {
        () => {{
            a.reset(client, server);
            b.reset(server, client);
        }};
    }

    // 1 — a record written by one side is read by the other, with its real content type and its
    //     exact bytes. Everything else here is about what must NOT work.
    {
        pair!();
        let mut wire = [0u8; 256];
        let mut got = [0u8; 256];
        let written = a.seal(ContentType::Handshake, b"client hello", 0, &mut wire);
        let read = b.open(&wire, &mut got);
        let ok = match (written, read) {
            (Ok(w), Ok((ty, len, used))) => {
                ty == ContentType::Handshake
                    && &got[..len] == b"client hello"
                    && used == w
                    && wire[0] == OUTER_TYPE
                    && wire[1..3] == LEGACY_VERSION
            }
            _ => false,
        };
        check!(
            ok,
            "tlsrecord: a sealed record opens to the same bytes and the same inner content type"
        );
    }

    // 2 — the outer type is always `application_data`, whatever is inside. A stack that writes the
    //     real type outside has told every observer what stage the handshake is at.
    {
        pair!();
        let mut wire = [0u8; 256];
        let mut got = [0u8; 256];
        a.seal(ContentType::Alert, &[2, 40], 0, &mut wire).ok();
        let read = b.open(&wire, &mut got);
        check!(
            wire[0] == OUTER_TYPE && read.map(|(t, l, _)| (t, l)) == Ok((ContentType::Alert, 2)),
            "tlsrecord: the outer type says application data while the inner type is the truth"
        );
    }

    // 3 — padding is inside the encryption, invisible outside it, and stripped exactly. A padded
    //     record is longer on the wire and identical when opened.
    {
        pair!();
        let mut plain = [0u8; 256];
        let mut padded = [0u8; 512];
        let mut got = [0u8; 256];
        let n_plain = a.seal(ContentType::ApplicationData, b"hi", 0, &mut plain);
        b.reset(server, client);
        let n_padded = {
            let mut a2 = RecordLayer::new(client, server);
            a2.seal(ContentType::ApplicationData, b"hi", 100, &mut padded)
        };
        let read = b.open(&padded, &mut got);
        let ok = match (n_plain, n_padded, read) {
            (Ok(p), Ok(q), Ok((ty, len, _))) => {
                q == p + 100 && ty == ContentType::ApplicationData && &got[..len] == b"hi"
            }
            _ => false,
        };
        check!(
            ok,
            "tlsrecord: padding is inside the AEAD, changes the wire length, and is stripped exactly"
        );
    }

    // 4 — one flipped bit ANYWHERE in the record is fatal. Header, ciphertext or tag: the header is
    //     associated data, so rewriting the length in flight fails the tag rather than resizing the
    //     record.
    {
        // Big enough that a flipped LENGTH byte still has its claimed bytes present: the point of
        // this check is that the tag refuses them, not that the framing ran out of buffer.
        let mut wire = [0u8; 1024];
        let mut got = [0u8; 256];
        a.reset(client, server);
        let total = a
            .seal(ContentType::Handshake, b"finished", 0, &mut wire)
            .unwrap_or(0);
        let mut all_fatal = total > 0;
        // The WHOLE buffer is handed over, not just the record: a flipped length byte must fail
        // its tag, and slicing to the original length would make it "incomplete" instead.
        for i in [0usize, 1, 3, 4, HEADER_LEN, HEADER_LEN + 3, total - 1] {
            b.reset(server, client);
            let saved = wire[i];
            wire[i] ^= 0x01;
            let verdict = b.open(&wire, &mut got);
            all_fatal &= matches!(
                verdict,
                Err(RecordRefusal::Fatal) | Err(RecordRefusal::BadHeader)
            );
            wire[i] = saved;
        }
        check!(
            all_fatal,
            "tlsrecord: a flipped bit in the header, the ciphertext or the tag is refused, never read"
        );
    }

    // 5 — the sequence number advances, so two identical records are DIFFERENT on the wire. A
    //     layer that reuses a nonce lets anyone watching XOR the two records together.
    {
        pair!();
        let mut one = [0u8; 256];
        let mut two = [0u8; 256];
        let mut got = [0u8; 256];
        let n1 = a
            .seal(ContentType::ApplicationData, b"same", 0, &mut one)
            .unwrap_or(0);
        let n2 = a
            .seal(ContentType::ApplicationData, b"same", 0, &mut two)
            .unwrap_or(0);
        let distinct = n1 == n2 && one[..n1] != two[..n2];
        // And they open in order, both to the same content.
        let r1 = b
            .open(&one[..n1], &mut got)
            .map(|(_, l, _)| got[..l].to_vec());
        let r2 = b
            .open(&two[..n2], &mut got)
            .map(|(_, l, _)| got[..l].to_vec());
        check!(
            distinct && r1.is_ok() && r1 == r2 && a.write.sequence() == 2 && b.read.sequence() == 2,
            "tlsrecord: the sequence advances, so identical content is never identical on the wire"
        );
    }

    // 6 — records must be opened IN ORDER. A record accepted out of order would be a record
    //     accepted under the wrong nonce, which is exactly what the sequence exists to prevent.
    {
        pair!();
        let mut one = [0u8; 256];
        let mut two = [0u8; 256];
        let mut got = [0u8; 256];
        let n1 = a
            .seal(ContentType::ApplicationData, b"first", 0, &mut one)
            .unwrap_or(0);
        let n2 = a
            .seal(ContentType::ApplicationData, b"second", 0, &mut two)
            .unwrap_or(0);
        let out_of_order = b.open(&two[..n2], &mut got);
        let in_order = {
            b.reset(server, client);
            b.open(&one[..n1], &mut got).is_ok()
        };
        check!(
            out_of_order == Err(RecordRefusal::Fatal) && in_order,
            "tlsrecord: a record opened out of order fails its tag rather than being accepted"
        );
    }

    // 7 — framing is answerable without the AEAD: a short buffer says "incomplete", a bad header
    //     says so, and neither is confused with a decryption failure.
    {
        a.reset(client, server);
        let mut wire = [0u8; 256];
        let total = a
            .seal(ContentType::Handshake, b"x", 0, &mut wire)
            .unwrap_or(0);
        let partial = RecordLayer::record_len(&wire[..total - 1]);
        let empty = RecordLayer::record_len(&[]);
        let mut bad = wire;
        bad[0] = 22; // an unprotected handshake record: not something this layer reads
        let wrong_type = RecordLayer::record_len(&bad[..total]);
        let mut huge = wire;
        huge[3] = 0xFF;
        huge[4] = 0xFF;
        let too_long = RecordLayer::record_len(&huge[..total]);
        check!(
            partial == Err(RecordRefusal::Incomplete)
                && empty == Err(RecordRefusal::Incomplete)
                && wrong_type == Err(RecordRefusal::BadHeader)
                && too_long == Err(RecordRefusal::BadHeader)
                && RecordLayer::record_len(&wire[..total]) == Ok(total),
            "tlsrecord: framing answers incomplete and bad-header without touching the AEAD"
        );
    }

    // 8 — an inner plaintext that is ALL padding has no content type. RFC 8446 §5.4 calls that a
    //     decode error; a layer that reads the last byte anyway reads a zero and invents a type.
    {
        pair!();
        let mut wire = [0u8; 256];
        let mut got = [0u8; 256];
        // Seal a record whose "content" is nothing and whose type byte is zero: constructed by
        // hand, because the sealing API cannot produce one.
        let inner = [0u8; 8];
        let nonce = record_nonce(&a.write.keys.iv, 0);
        let mut scratch = [0u8; 128];
        wire[0] = OUTER_TYPE;
        wire[1..3].copy_from_slice(&LEGACY_VERSION);
        wire[3..5].copy_from_slice(&((inner.len() + 16) as u16).to_be_bytes());
        let header = [wire[0], wire[1], wire[2], wire[3], wire[4]];
        let need = aead_scratch_len(HEADER_LEN, inner.len());
        let written = crate::crypto::aead_seal_into(
            &a.write.keys.key,
            &nonce,
            &header,
            &inner,
            &mut wire[HEADER_LEN..],
            &mut scratch[..need],
        )
        .unwrap_or(0);
        let verdict = b.open(&wire[..HEADER_LEN + written], &mut got);
        check!(
            verdict == Err(RecordRefusal::BadInnerType) && b.read.refusals == 1,
            "tlsrecord: an all-padding inner plaintext is a decode error, not an invented type"
        );
    }

    // 9 — a rekey resets the sequence, and only a rekey does. Resetting under the SAME key would
    //     reuse every nonce already used, so the two must happen together or not at all.
    {
        a.reset(client, server);
        let mut wire = [0u8; 256];
        a.seal(ContentType::ApplicationData, b"one", 0, &mut wire)
            .ok();
        a.seal(ContentType::ApplicationData, b"two", 0, &mut wire)
            .ok();
        let before = a.write.sequence();
        let fresh = crate::hkdf::traffic_keys(&[0x77u8; 32]).unwrap_or(client);
        a.write.rekey(fresh);
        let n1 = a
            .seal(ContentType::ApplicationData, b"one", 0, &mut wire)
            .unwrap_or(0);
        // The first record under the new key is sequence zero again — and it must not equal the
        // first record under the old key, because the KEY changed with it.
        b.reset(client, server);
        let mut old_wire = [0u8; 256];
        let n2 = b
            .seal(ContentType::ApplicationData, b"one", 0, &mut old_wire)
            .unwrap_or(0);
        check!(
            before == 2 && a.write.sequence() == 1 && n1 == n2 && wire[..n1] != old_wire[..n2],
            "tlsrecord: a rekey resets the sequence and changes the bytes on the wire with it"
        );
    }

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_boot_suite_proves_every_record_invariant() {
        let mut seen = 0;
        let n = tlsrecord_suite(|_, passed, name| {
            assert!(passed, "{name}");
            seen += 1;
        })
        .expect("the record-layer suite should hold");
        assert_eq!(n, 9);
        assert_eq!(seen, 9);
    }
}
