//! The OS remembers: a durable, content-verified spine store (REQ-STOR-003, ADR-038).
//!
//! Everything below this module was per-boot. The journal made a multi-block write atomic, the
//! namespace (ADR-035) gave durable objects names, and the drivers (ADR-036/037) put both on real
//! hardware on all three targets — but the **capability-secure spine itself** was built fresh in RAM
//! every boot, so nothing Aletheia knew survived a power cycle. An operating system that forgets
//! everything at reset is a demo.
//!
//! This module makes the store durable, in the way the rest of the system already works:
//!
//! * **One object, one atomic transaction.** The whole store is encoded into a single fs object
//!   (`spine.store`) written with [`crate::fs::Filesystem::replace`], so an update is one journal
//!   transaction: a crash leaves the previous store or the new one, never a half-written mixture and
//!   never *nothing* (which "remove then create" would risk).
//!
//! * **Loading re-verifies the content address.** Each entity carries the `content_hash` the spine
//!   computed when it was created; on load the hash is recomputed from the bytes actually read and
//!   compared. A single flipped bit in an entity's content is therefore a **refusal**
//!   ([`PersistError::ContentHashMismatch`]), not silently-accepted state. That is the point of a
//!   content-addressed store, finally applied to the medium: the journal protects a write from being
//!   torn, and the hash protects a read from being wrong.
//!
//! * **The whole record is checksummed, not just the contents.** The per-entity content hash covers an
//!   entity's *content*; it says nothing about its id, version, chain or provenance. A byte-flip sweep
//!   over the encoded record found exactly that hole — flipping an id produced a store that loaded
//!   successfully with different data. So the record carries a trailing FNV-1a checksum over every
//!   preceding byte, and a load verifies both: the content address per entity, then the record as a
//!   whole. Every byte is load-bearing.
//!
//! * **Ids never repeat across a reboot.** `next_id` is part of the record, so a restored store
//!   continues the sequence instead of reissuing ids a previous boot already handed out.
//!
//! ## Not claimed (ADR-038)
//!
//! Entities only — **capabilities are deliberately NOT persisted** (ALET-P1-026 is the open question of
//! what a capability's lifetime even means across a reboot; minting durable authority by accident is
//! exactly the mistake to avoid). No encryption at rest at this layer (ALET-P1-028/029), no incremental
//! save (the whole store is rewritten, bounded by one transaction — [`PersistError::TooLarge`] above
//! that), no event-log persistence, and no schema migration beyond refusing an unknown version.
use alloc::vec::Vec;

use crate::fs::{Filesystem, FsError};
use crate::spine::{content_hash, Entity, EntityType, Store};
use crate::storage::BlockDevice;

/// The fs object the store lives in.
pub const STORE_OBJECT: &str = "spine.store";
/// Record magic ("AlSt\0\0\0\1") and version.
const REC_MAGIC: u64 = 0x416C_5374_0000_0001;
const REC_VERSION: u64 = 1;
/// Header: magic, version, next_id, count.
const HDR_LEN: usize = 32;
/// Trailing checksum length: FNV-1a over every byte before it.
const CKSUM_LEN: usize = 8;
/// Per-entity fixed part: id, (etype+deleted+pad), version, chain, content_hash, two lengths.
const ENT_FIXED: usize = 8 + 8 + 8 + 8 + 8 + 4 + 4;

/// Why a durable-store operation was refused. Every failure is a refusal, never partial state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistError {
    /// No store object exists yet — a first boot, not a failure the caller must treat as one.
    Absent,
    /// The record's magic or version is not one this build writes.
    BadFormat,
    /// The record ends in the middle of a field or an entity.
    Truncated,
    /// An entity's recomputed content hash does not match the one stored with it: the bytes on the
    /// medium are not the bytes the spine hashed. Refused, never restored.
    ContentHashMismatch,
    /// The record's trailing checksum does not match its bytes. This is what catches damage to an
    /// entity's METADATA (id, version, chain, provenance, type) — fields no content hash covers.
    ChecksumMismatch,
    /// An entity's type byte is not a known variant.
    UnknownEntityType,
    /// Content or provenance bytes are not valid UTF-8.
    NotUtf8,
    /// The encoded store exceeds what one filesystem transaction can carry.
    TooLarge,
    /// The filesystem or device refused the operation.
    Fs(FsError),
}

impl From<FsError> for PersistError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::NotFound => PersistError::Absent,
            FsError::TooLarge => PersistError::TooLarge,
            other => PersistError::Fs(other),
        }
    }
}

fn etype_code(t: EntityType) -> u8 {
    match t {
        EntityType::Document => 1,
        EntityType::Summary => 2,
        EntityType::Agent => 3,
        EntityType::Capability => 4,
        EntityType::Event => 5,
    }
}

fn etype_from(code: u8) -> Option<EntityType> {
    match code {
        1 => Some(EntityType::Document),
        2 => Some(EntityType::Summary),
        3 => Some(EntityType::Agent),
        4 => Some(EntityType::Capability),
        5 => Some(EntityType::Event),
        _ => None,
    }
}

fn put64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn take64(buf: &[u8], at: &mut usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    if end > buf.len() {
        return None;
    }
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[*at..end]);
    *at = end;
    Some(u64::from_le_bytes(a))
}

fn take32(buf: &[u8], at: &mut usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    if end > buf.len() {
        return None;
    }
    let mut a = [0u8; 4];
    a.copy_from_slice(&buf[*at..end]);
    *at = end;
    Some(u32::from_le_bytes(a))
}

/// Encode a store into the durable record. Pure — no device, so it is trivially testable and cannot
/// leave anything half-written.
pub fn encode(store: &Store) -> Vec<u8> {
    let entities: Vec<&Entity> = store.entities().collect();
    let mut out = Vec::with_capacity(HDR_LEN + entities.len() * (ENT_FIXED + 32));
    put64(&mut out, REC_MAGIC);
    put64(&mut out, REC_VERSION);
    put64(&mut out, store.next_id());
    put64(&mut out, entities.len() as u64);
    for e in entities {
        put64(&mut out, e.id);
        // etype, deleted, then 6 bytes of padding so every following field stays 8-byte aligned within
        // the record (alignment of the FORMAT, not of memory — it keeps hand-decoding readable).
        out.push(etype_code(e.etype));
        out.push(u8::from(e.deleted));
        out.extend_from_slice(&[0u8; 6]);
        put64(&mut out, e.version);
        put64(&mut out, e.chain);
        put64(&mut out, e.content_hash);
        put32(&mut out, e.content.len() as u32);
        put32(&mut out, e.provenance.len() as u32);
        out.extend_from_slice(e.content.as_bytes());
        out.extend_from_slice(e.provenance.as_bytes());
    }
    // Trailing checksum over everything above: the metadata fields (id, version, chain, provenance,
    // type, deleted) are covered by NOTHING else, and a load that accepted a flipped id would be
    // silent corruption.
    let ck = content_hash(&out);
    out.extend_from_slice(&ck.to_le_bytes());
    out
}

/// Decode a durable record, re-verifying every entity's content address. Any defect is a refusal.
pub fn decode(buf: &[u8]) -> Result<Store, PersistError> {
    let mut at = 0usize;
    let magic = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
    let version = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
    if magic != REC_MAGIC || version != REC_VERSION {
        return Err(PersistError::BadFormat);
    }
    let next_id = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
    let count = take64(buf, &mut at).ok_or(PersistError::Truncated)? as usize;

    let mut entities = Vec::with_capacity(core::cmp::min(count, 1024));
    for _ in 0..count {
        let id = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
        if at + 8 > buf.len() {
            return Err(PersistError::Truncated);
        }
        let etype = etype_from(buf[at]).ok_or(PersistError::UnknownEntityType)?;
        let deleted = buf[at + 1] != 0;
        at += 8; // type + deleted + 6 pad
        let version = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
        let chain = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
        let stored_hash = take64(buf, &mut at).ok_or(PersistError::Truncated)?;
        let clen = take32(buf, &mut at).ok_or(PersistError::Truncated)? as usize;
        let plen = take32(buf, &mut at).ok_or(PersistError::Truncated)? as usize;
        let cend = at.checked_add(clen).ok_or(PersistError::Truncated)?;
        let pend = cend.checked_add(plen).ok_or(PersistError::Truncated)?;
        if pend > buf.len() {
            return Err(PersistError::Truncated);
        }
        let content = core::str::from_utf8(&buf[at..cend]).map_err(|_| PersistError::NotUtf8)?;
        let provenance =
            core::str::from_utf8(&buf[cend..pend]).map_err(|_| PersistError::NotUtf8)?;
        at = pend;

        // THE check: the bytes actually read must hash to the address the spine recorded.
        if content_hash(content.as_bytes()) != stored_hash {
            return Err(PersistError::ContentHashMismatch);
        }

        entities.push(Entity {
            id,
            etype,
            content: alloc::string::String::from(content),
            content_hash: stored_hash,
            version,
            chain,
            deleted,
            provenance: alloc::string::String::from(provenance),
        });
    }
    // The record checksum is verified LAST, so a damaged content byte still reports the precise
    // ContentHashMismatch (the content-addressing failure) rather than the coarser record failure.
    if buf.len() < HDR_LEN + CKSUM_LEN {
        return Err(PersistError::Truncated);
    }
    let body = &buf[..buf.len() - CKSUM_LEN];
    let mut stored = [0u8; 8];
    stored.copy_from_slice(&buf[buf.len() - CKSUM_LEN..]);
    if content_hash(body) != u64::from_le_bytes(stored) {
        return Err(PersistError::ChecksumMismatch);
    }
    Ok(Store::restore(entities, next_id))
}

/// Write `store` to the device as one atomic filesystem transaction. Returns the record's byte length.
pub fn save<D: BlockDevice>(
    fs: &mut Filesystem,
    dev: &mut D,
    store: &Store,
) -> Result<usize, PersistError> {
    let bytes = encode(store);
    fs.replace(dev, STORE_OBJECT, &bytes)?;
    Ok(bytes.len())
}

/// Read the store back, refusing anything that does not verify. [`PersistError::Absent`] means there is
/// no store yet (a first boot), which a caller normally handles by creating one.
pub fn load<D: BlockDevice>(fs: &Filesystem, dev: &D) -> Result<Store, PersistError> {
    let bytes = fs.read(dev, STORE_OBJECT)?;
    decode(&bytes)
}

/// The content of the boot-witness entity for boot number `n`. Kept in one place so the kernel that
/// writes it and the reader that counts them agree on the wording.
pub fn witness_content(n: u64) -> alloc::string::String {
    alloc::format!("boot-witness::{}", n)
}

/// How many boots this store has witnessed, from the highest witness entity present (0 = none).
pub fn boot_count(store: &Store) -> u64 {
    let mut best = 0u64;
    for e in store.entities() {
        if let Some(rest) = e.content.strip_prefix("boot-witness::") {
            let mut n = 0u64;
            let mut ok = !rest.is_empty();
            for b in rest.bytes() {
                if b.is_ascii_digit() {
                    n = n * 10 + (b - b'0') as u64;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok && n > best {
                best = n;
            }
        }
    }
    best
}

/// Mount (formatting a blank device), load the store if one is there, record that this boot happened,
/// and save it back atomically. Returns `(boot_number, entities_verified_from_the_previous_boot)`.
///
/// This is the whole cross-reboot contract in one function, so the three targets share it: boot 1
/// creates the store, boot 2 on the SAME medium must find and verify boot 1's entities and report
/// `boot_number == 2`. A gate that boots twice therefore proves the OS *remembered*, not merely that it
/// wrote. A corrupt store is a refusal — never silently replaced with a fresh one, because that would
/// turn "your data is damaged" into "your data is gone".
pub fn open_and_witness<D: BlockDevice>(dev: &mut D) -> Result<(u64, usize), PersistError> {
    let mut fs = match Filesystem::mount(dev) {
        Ok(fs) => fs,
        Err(FsError::NotFormatted) => {
            // A blank medium: format it once, then proceed. A device error is NOT swallowed.
            Filesystem::format(dev)?;
            Filesystem::mount(dev)?
        }
        Err(e) => return Err(PersistError::from(e)),
    };

    let mut store = match load(&fs, dev) {
        Ok(s) => s,
        Err(PersistError::Absent) => Store::new(),
        Err(e) => return Err(e),
    };
    let verified = store.entities().count();
    let boot = boot_count(&store) + 1;
    store.put(EntityType::Event, &witness_content(boot), "kernel::persist");
    save(&mut fs, dev, &store)?;
    Ok((boot, verified))
}

/// The durable-store invariant suite (REQ-STOR-003), reported through a caller-supplied logger like
/// every other suite. Runs against any [`BlockDevice`]; **destructive** (it formats).
pub fn selftest_on<D: BlockDevice, F: FnMut(usize, bool, &str)>(
    dev: &mut D,
    mut log: F,
) -> Result<usize, (usize, &'static str)> {
    let mut n = 0usize;
    macro_rules! check {
        ($name:expr, $cond:expr) => {{
            n += 1;
            let ok = $cond;
            log(n, ok, $name);
            if !ok {
                return Err((n, $name));
            }
        }};
    }

    const FIRST: &str = "persist: a blank medium yields an empty store, not a failure";
    if Filesystem::format(dev).is_err() {
        log(1, false, FIRST);
        return Err((1, FIRST));
    }
    let mut fs = match Filesystem::mount(dev) {
        Ok(fs) => fs,
        Err(_) => {
            log(1, false, FIRST);
            return Err((1, FIRST));
        }
    };
    check!(FIRST, matches!(load(&fs, dev), Err(PersistError::Absent)));

    // A saved store reloads with exactly the same entities.
    let mut store = Store::new();
    let doc = store.put(EntityType::Document, "the durable content", "test::a");
    let agent = store.put(EntityType::Agent, "an agent record", "test::b");
    let saved_next = store.next_id();
    check!(
        "persist: saving the store writes one object atomically",
        save(&mut fs, dev, &store).is_ok()
    );
    let back = load(&fs, dev);
    check!(
        "persist: a reloaded store holds the same entities, byte for byte",
        match &back {
            Ok(s) => {
                s.get(doc).map(|e| e.content.as_str()) == Some("the durable content")
                    && s.get(agent).map(|e| e.content.as_str()) == Some("an agent record")
                    && s.entities().count() == 2
            }
            Err(_) => false,
        }
    );
    check!(
        "persist: the id sequence continues across a reload (no id is ever reissued)",
        match &back {
            Ok(s) => s.next_id() >= saved_next,
            Err(_) => false,
        }
    );

    // Tamper with one byte of the stored content: the recomputed content address must refuse it.
    const LOCATE: &str = "persist: the store object is locatable for the tamper check";
    let entry = match fs.stat(dev, STORE_OBJECT) {
        Ok(e) => e,
        Err(_) => {
            log(n + 1, false, LOCATE);
            return Err((n + 1, LOCATE));
        }
    };
    let mut blk = [0u8; crate::storage::BLOCK_SIZE];
    let read_ok = dev.read_block(entry.start, &mut blk).is_ok();
    // The first entity's content bytes sit right after the header + that entity's fixed part.
    blk[HDR_LEN + ENT_FIXED] ^= 0x01;
    let wrote = dev.write_block(entry.start, &blk).is_ok() && dev.flush().is_ok();
    check!(
        "persist: a single flipped content byte is REFUSED, not restored (content address re-verified)",
        read_ok && wrote && matches!(load(&fs, dev), Err(PersistError::ContentHashMismatch))
    );

    // A flipped METADATA byte (an id) is caught by the record checksum, not by any content hash.
    let mut meta_bad = encode(&store);
    meta_bad[HDR_LEN] ^= 0x01; // the first entity's id
    check!(
        "persist: a flipped metadata byte is REFUSED (the whole record is checksummed)",
        matches!(decode(&meta_bad), Err(PersistError::ChecksumMismatch))
    );

    // A record with the wrong magic/version is refused as a format, not read as data.
    check!(
        "persist: an unknown record format is refused",
        matches!(decode(&[0u8; HDR_LEN]), Err(PersistError::BadFormat))
    );
    check!(
        "persist: a truncated record is refused",
        matches!(
            decode(&encode(&store)[..HDR_LEN + 4]),
            Err(PersistError::Truncated)
        )
    );

    // The cross-reboot contract, exercised twice on one medium: the second open must SEE the first.
    const WITNESS: &str = "persist: the witness survives a remount and counts the boot";
    if Filesystem::format(dev).is_err() {
        log(n + 1, false, WITNESS);
        return Err((n + 1, WITNESS));
    }
    let first = open_and_witness(dev);
    let second = open_and_witness(dev);
    check!(WITNESS, first == Ok((1, 0)) && second == Ok((2, 1)));

    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FILE_DATA_START;
    use crate::storage::MemBlockDevice;

    #[test]
    fn the_suite_holds_on_a_ram_disk() {
        let mut dev = MemBlockDevice::new(FILE_DATA_START + 128);
        let n = selftest_on(&mut dev, |_, _, _| {}).expect("every persist invariant holds");
        assert_eq!(n, 9);
    }
}
