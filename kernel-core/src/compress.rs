//! Bounded, integer-only compression for objects at rest (ADR-024 storage stack).
//!
//! Durable bytes cost device blocks, and this system's durable objects — the serialized store
//! image, capability exports, journal payloads — are structured enough that storing them raw
//! wastes real space. This module is the arch-independent codec every one of those paths can
//! share: an **LZSS** compressor with a 4 KiB sliding window (one block — the transfer unit the
//! journal and filesystem already speak), wrapped in a self-describing [`ACMP1`] envelope that
//! carries everything a reader needs to refuse bad input BY NAME:
//!
//! * the **declared original length**, which bounds decoding before a single output byte is
//!   produced — a corrupt or hostile envelope cannot grow past it ([`MAX_OUTPUT`] caps what may
//!   be declared at all), so decompression is bomb-proof by construction rather than by hope;
//! * a **checksum over the ORIGINAL bytes**, verified after reconstruction, so any decoder bug or
//!   flipped bit is a named refusal instead of silently wrong data;
//! * a **RAW fallback** for incompressible input, so compression never makes anything worse than
//!   storing it — the envelope says which world you are in.
//!
//! The checksum is FNV-1a: it is an INTEGRITY check against corruption, not authentication against
//! an adversary — an attacker who can rewrite bytes can recompute it. Authenticated paths
//! (`capstore::save_authenticated_to_fs`) layer HMAC over their own bytes and stay responsible for
//! that themselves.
//!
//! No floating point, no external tables, allocation only for the output and one fixed-size hash
//! table — the same integer-only discipline as the risk forest (`crate::mlrisk`).

use alloc::vec::Vec;

/// Envelope magic: "ACMP".
const MAGIC: [u8; 4] = *b"ACMP";
/// Algorithm byte: payload is stored verbatim (it did not compress).
const ALG_RAW: u8 = 0;
/// Algorithm byte: payload is LZSS tokens over a 4 KiB window.
const ALG_LZSS: u8 = 1;
/// Byte offsets inside the envelope.
const OFF_ALG: usize = 4;
const OFF_ORIG_LEN: usize = 5;
const OFF_PAYLOAD_LEN: usize = 9;
const OFF_CKSUM: usize = 13;
/// Magic(4) + alg(1) + orig_len(4) + payload_len(4) + cksum(4).
pub const HEADER_LEN: usize = 17;
/// Largest declared original length a decompress call will honour. The durable objects this
/// format exists for are kilobytes; sixteen mebibytes is orders of magnitude beyond any real
/// image while still refusing the 4 GiB declaration a corrupt header could otherwise ask for.
pub const MAX_OUTPUT: usize = 16 << 20;
/// LZSS window size: matches one storage block, so offsets never reach across a natural boundary.
const WINDOW: usize = 4096;
/// Shortest match worth encoding (3 bytes beat two literals only if encoded in 2 bytes, which
/// the token format below achieves).
const MIN_MATCH: usize = 3;
/// Longest match encodable in one token: 4 bits of length minus the minimum.
const MAX_MATCH: usize = MIN_MATCH + 15;
/// Hash-table size for the match finder (power of two). One entry per 3-byte hash: the MOST
/// RECENT position with that hash. No chains — a few percent worse ratio buys a bounded,
/// allocation-light encoder whose worst case is linear in the input.
const HASH_SLOTS: usize = 1 << 12;
const HASH_MASK: u32 = (HASH_SLOTS - 1) as u32;

/// Why compressed bytes were refused. Every variant is a *named* refusal — the caller can print
/// which check failed, and none of them can be reached by input the encoder produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecompressError {
    /// Fewer bytes than the fixed envelope header.
    TooShort,
    /// The leading four bytes are not "ACMP".
    BadMagic,
    /// An algorithm byte this decoder does not implement.
    UnknownAlgorithm(u8),
    /// The declared original length exceeds [`MAX_OUTPUT`] — refused before allocating.
    DeclaredTooLarge,
    /// The payload is shorter than the envelope declares.
    Truncated,
    /// The payload declares more input than its length can hold (an internal inconsistency).
    BadPayloadLength,
    /// Decoding ran past the declared original length — the payload does not reproduce it.
    Overrun,
    /// Decoding produced fewer bytes than declared.
    Underrun,
    /// The reconstructed bytes do not hash to the envelope checksum.
    ChecksumMismatch,
}

/// FNV-1a over a byte slice: the envelope's integrity check. Constant-time in nothing, fast,
/// and deliberately NOT cryptographic — see the module header.
#[inline]
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[inline]
fn hash3(data: &[u8], at: usize) -> usize {
    let v = ((data[at] as u32) << 16) | ((data[at + 1] as u32) << 8) | data[at + 2] as u32;
    (v.wrapping_mul(0x9E37_79B1) >> 20) as usize & HASH_MASK as usize
}

/// Compress `data` into an [`ACMP1`](self) envelope. Never fails: input that does not compress
/// is stored under the RAW algorithm, so the result is at most a small fixed header longer than
/// the input. Deterministic — the same bytes always produce the same envelope, which is what lets
/// content-addressed layers reason about the output.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let payload = lzss(data);
    let (alg, payload) = if payload.len() < data.len() {
        (ALG_LZSS, payload)
    } else {
        // Incompressible: store verbatim. `compress` must never be the reason an object got bigger.
        (ALG_RAW, data.to_vec())
    };
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(alg);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&fnv1a(data).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Decompress an [`ACMP1`](self) envelope back to exactly the bytes that went in. Allocation is
/// bounded by the DECLARED original length (capped at [`MAX_OUTPUT`]) before decoding starts, and
/// the result must match that length and its checksum exactly — there is no partial success.
pub fn decompress(envelope: &[u8]) -> Result<Vec<u8>, DecompressError> {
    if envelope.len() < HEADER_LEN {
        return Err(DecompressError::TooShort);
    }
    if envelope[0..4] != MAGIC {
        return Err(DecompressError::BadMagic);
    }
    let alg = envelope[OFF_ALG];
    if alg != ALG_RAW && alg != ALG_LZSS {
        return Err(DecompressError::UnknownAlgorithm(alg));
    }
    let orig_len =
        u32::from_le_bytes(envelope[OFF_ORIG_LEN..OFF_ORIG_LEN + 4].try_into().unwrap()) as usize;
    if orig_len > MAX_OUTPUT {
        return Err(DecompressError::DeclaredTooLarge);
    }
    let payload_len = u32::from_le_bytes(
        envelope[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let payload = envelope
        .get(HEADER_LEN..)
        .ok_or(DecompressError::Truncated)?;
    if payload.len() != payload_len {
        return Err(DecompressError::BadPayloadLength);
    }
    let out = match alg {
        ALG_RAW => payload.to_vec(),
        _ => lzss_decode(payload, orig_len)?,
    };
    if out.len() != orig_len {
        return Err(DecompressError::Underrun);
    }
    if fnv1a(&out) != u32::from_le_bytes(envelope[OFF_CKSUM..OFF_CKSUM + 4].try_into().unwrap()) {
        return Err(DecompressError::ChecksumMismatch);
    }
    Ok(out)
}

/// LZSS encode: greedy longest-match search against the most recent position of each 3-byte
/// hash within the window. Tokens are byte-packed 8 flags (LSB first) per group: flag 1 = one
/// literal byte follows; flag 0 = one match token follows, a u16 carrying `(offset-1) << 4 |`
/// `(len - MIN_MATCH)` — offset 1..=WINDOW, length MIN_MATCH..=MAX_MATCH.
fn lzss(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 8 + 16);
    let mut table = alloc::vec![u32::MAX; HASH_SLOTS];
    let mut flags_at = out.len();
    out.push(0); // flag byte placeholder; rewritten when the group closes
    let mut flag_bit = 0u8;
    let mut flag_byte = 0u8;
    let mut i = 0usize;
    while i < data.len() {
        let mut best_len = 0usize;
        let mut best_off = 0usize;
        if i + MIN_MATCH <= data.len() {
            let h = hash3(data, i);
            let cand = table[h] as usize;
            // The candidate is valid only if it is inside the window AND still holds the same
            // 3 bytes (a slot is overwritten, never cleared, so a stale hit must be re-checked).
            if cand != usize::MAX {
                // The match loop below registers positions AHEAD of the cursor, so a candidate
                // can be the cursor's own just-registered slot; a distance of zero (or backwards)
                // is simply a miss, not an underflow to panic on.
                let dist = i.checked_sub(cand);
                if matches!(dist, Some(d) if (1..=WINDOW).contains(&d) && cand + MIN_MATCH <= i) {
                    let max_len = core::cmp::min(MAX_MATCH, data.len() - i);
                    let mut l = 0usize;
                    while l < max_len && data[cand + l] == data[i + l] {
                        l += 1;
                    }
                    if l >= MIN_MATCH {
                        best_len = l;
                        best_off = dist.unwrap_or(0);
                    }
                }
            }
            // Register THIS position before emitting, so repeats within the same run are found.
            table[h] = i as u32;
        }
        if best_len >= MIN_MATCH {
            // Flag 0: a match token. Offsets are stored minus one so WINDOW fits in 12 bits.
            let tok = (((best_off - 1) as u16) << 4) | ((best_len - MIN_MATCH) as u16 & 0xF);
            out.extend_from_slice(&tok.to_le_bytes());
            // Register every position the match covers, or long runs become unfindable mid-way.
            for k in 1..best_len {
                if i + k + MIN_MATCH <= data.len() {
                    let h = hash3(data, i + k);
                    table[h] = (i + k) as u32;
                }
            }
            i += best_len;
        } else {
            // Flag 1: a literal.
            flag_byte |= 1 << flag_bit;
            out.push(data[i]);
            i += 1;
        }
        flag_bit += 1;
        if flag_bit == 8 {
            out[flags_at] = flag_byte;
            flags_at = out.len();
            out.push(0);
            flag_bit = 0;
            flag_byte = 0;
        }
    }
    if flag_bit != 0 {
        out[flags_at] = flag_byte;
    } else {
        // The trailing placeholder flag byte belongs to no group; drop it.
        out.pop();
    }
    out
}

/// LZSS decode, bounded by `orig_len`: the output vector is allocated ONCE at the declared size,
/// every write is checked against it, and finishing short is an error. A corrupt token stream
/// therefore cannot allocate or produce more than the envelope declared.
fn lzss_decode(payload: &[u8], orig_len: usize) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::with_capacity(orig_len);
    let mut p = 0usize;
    while p < payload.len() {
        let flags = payload[p];
        p += 1;
        for bit in 0..8u8 {
            // Out of payload mid-group: the encoder's final flag byte pads its unused bits with
            // zeros, so anything left to read would have been read. The length and checksum
            // checks below catch a stream that genuinely ended too early.
            if p >= payload.len() {
                break;
            }
            if flags & (1 << bit) != 0 {
                // Literal.
                if out.len() >= orig_len {
                    return Err(DecompressError::Overrun);
                }
                out.push(payload[p]);
                p += 1;
            } else {
                // Match token.
                if p + 2 > payload.len() {
                    return Err(DecompressError::Truncated);
                }
                let tok = u16::from_le_bytes([payload[p], payload[p + 1]]);
                p += 2;
                let off = (tok >> 4) as usize + 1;
                let len = (tok & 0xF) as usize + MIN_MATCH;
                if off > out.len() {
                    return Err(DecompressError::Overrun);
                }
                if out.len() + len > orig_len {
                    return Err(DecompressError::Overrun);
                }
                let start = out.len() - off;
                for k in 0..len {
                    // Byte-at-a-time BY DESIGN: a match may overlap its own output (runs).
                    let b = out[start + k];
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}
