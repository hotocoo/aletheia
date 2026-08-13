//! Capability lifetime across a reboot (REQ-CAP-008, ADR-048).
//!
//! Until this module existed, [`crate::spine::CapEngine`] was born empty at every boot. That is a
//! safe default and a real limitation, and the durable-store work (REQ-STOR-003) said so in as many
//! words: entities survive a reboot, authority does not. An operating system whose authority
//! evaporates on restart cannot have a durable subject at all — every capability would have to be
//! re-minted by something with more authority still, at a point in the boot where nothing has
//! authenticated anyone.
//!
//! Persisting authority is the dangerous direction, so the rules here are about what a reload must
//! REFUSE, not about what it can restore.
//!
//! # A persisted registry is untrusted input
//!
//! A capability created through [`CapEngine::delegate`](crate::spine::CapEngine::delegate) passed
//! the attenuation check. A capability read back off a disk passed nothing: whoever can write the
//! block can widen a child's scope, drop the revocation list, or point a record at a parent that
//! does not exist. So [`load`] re-runs the whole admission test — the SAME
//! [`crate::capalg::attenuates`] the engine applies at delegation time — over every parent/child
//! edge in the store, and refuses the entire image if any edge fails. Never a partial load: a
//! registry restored without its revocation list is strictly worse than no registry at all.
//!
//! # The clock is part of the capability
//!
//! `Constraints::expires_at` is compared against the engine's logical clock. Persist a capability
//! that expired at 1000 and reload it under a clock that restarts at 0, and it is live again — the
//! expiry did not fail, the frame of reference moved. [`save`] therefore stamps the clock it was
//! taken under and [`load`] REFUSES a clock that has gone backwards ([`CapStoreError::ClockRewound`])
//! rather than silently resurrecting every expired grant in the image. Aletheia has no trusted wall
//! clock; what it has is a value that must never decrease, and this is the check that says so.
//!
//! # An id must never be minted twice
//!
//! Token ids come from `next_id ^ secret`. Persist the registry and lose the counter, and the next
//! boot re-mints ids that are already held — a new capability inheriting an old handle, or worse, a
//! REVOKED id, so a token that was killed authorizes again. Both the counter and the secret are in
//! the image, and [`load`] proves the property directly rather than trusting the pair: every stored
//! id must be one the counter has already passed ([`CapStoreError::IdReusable`]).
//!
//! # Not claimed
//!
//! The legacy image is checksummed, not authenticated: the checksum catches corruption and
//! truncation, but an attacker who can write the block can also write a matching checksum. The
//! [`save_authenticated`] / [`load_authenticated`] pair adds HMAC-SHA256 when a caller supplies a
//! trusted key. Key custody, rotation, and secure boot delivery remain outside this module.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::capalg::{attenuates, Authority};
use crate::fs::{Filesystem, FsError};
use crate::spine::{content_hash, CapEngine, Constraints, EntityType, Scope, StoredCapability};

/// Image magic — `ALCS`, "Aletheia capability store".
const MAGIC: &[u8; 4] = b"ALCS";
/// On-disk format version. A load refuses any other value outright; a capability store is not a
/// place to be lenient about a format it does not recognize.
pub const VERSION: u32 = 1;
/// Filesystem object carrying the capability image. It is replaced atomically, never appended.
pub const STORE_OBJECT: &str = "cap.store";

/// Why a persisted registry was refused. Every variant is a REFUSAL of the whole image — there is
/// no partial load, because the parts a partial load would drop (the revocation list, a parent
/// record) are the parts that make the rest safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapStoreError {
    /// The filesystem object does not exist yet.
    Absent,
    /// The encoded image cannot fit one atomic filesystem transaction.
    TooLarge,
    /// The namespace or device refused access to the image.
    Fs(FsError),
    /// The image ends inside a record, or declares more records than its bytes can hold.
    Truncated,
    /// Not a capability store.
    BadMagic,
    /// A format version this kernel does not implement.
    BadVersion,
    /// The trailing checksum does not match the bytes before it.
    Checksum,
    /// A field held a value the format cannot represent (an unknown scope tag, an unknown entity
    /// type, a non-UTF-8 string).
    BadEncoding,
    /// Two records claim the same token id.
    Duplicate,
    /// A record names a parent that is not in the image. Restoring it would create authority with
    /// no ancestor to revoke it by.
    Orphan,
    /// The parent edges contain a cycle — authority that justifies itself, and a revocation walk
    /// with no end.
    Cycle,
    /// A child claims more authority than its parent. The delegation this record describes could
    /// never have been made through `delegate`.
    Amplified,
    /// The clock this image is being loaded under is earlier than the one it was saved under.
    /// Accepting it would un-expire every expired capability in the image.
    ClockRewound,
    /// A stored id is one the counter can still mint, so a future capability would collide with an
    /// existing (possibly revoked) one.
    IdReusable,
    /// There were bytes after the checksum. The image is not exactly what it claims to be.
    TrailingBytes,
    /// The authenticated envelope's HMAC did not verify under the supplied trusted key.
    Authentication,
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}
fn put_opt_u64(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        Some(x) => {
            put_u8(out, 1);
            put_u64(out, x);
        }
        None => {
            put_u8(out, 0);
            put_u64(out, 0);
        }
    }
}

fn etype_tag(t: EntityType) -> u8 {
    match t {
        EntityType::Document => 0,
        EntityType::Summary => 1,
        EntityType::Agent => 2,
        EntityType::Capability => 3,
        EntityType::Event => 4,
    }
}

fn etype_of(tag: u8) -> Option<EntityType> {
    Some(match tag {
        0 => EntityType::Document,
        1 => EntityType::Summary,
        2 => EntityType::Agent,
        3 => EntityType::Capability,
        4 => EntityType::Event,
        _ => return None,
    })
}

fn put_scope(out: &mut Vec<u8>, scope: &Scope) {
    match scope {
        Scope::All => put_u8(out, 0),
        Scope::None => put_u8(out, 1),
        Scope::Type(t) => {
            put_u8(out, 2);
            put_u8(out, etype_tag(*t));
        }
        Scope::Entities(ids) => {
            put_u8(out, 3);
            put_u32(out, ids.len() as u32);
            for id in ids {
                put_u64(out, *id);
            }
        }
    }
}

/// One capability as the image carries it: an id, its parent edge, and the three authority
/// dimensions. This is the *wire* shape, deliberately public and deliberately assemblable by hand —
/// see [`encode_for_test`].
#[derive(Clone, Debug)]
pub struct Record {
    pub id: u64,
    pub parent: Option<u64>,
    pub subject: String,
    pub action: String,
    pub scope: Scope,
    pub constraints: Constraints,
}

/// The one encoder. [`save`] and [`encode_for_test`] both go through it, so an image assembled by
/// an attacker is byte-compatible with one the kernel wrote — which is the whole point: a refusal
/// has to come from [`load`]'s checks, not from the forgery being malformed by accident.
fn encode(epoch: u64, secret: u64, next_id: u64, records: &[Record], revoked: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, VERSION);
    put_u64(&mut out, epoch);
    put_u64(&mut out, secret);
    put_u64(&mut out, next_id);
    put_u32(&mut out, records.len() as u32);
    put_u32(&mut out, revoked.len() as u32);
    for rec in records {
        put_u64(&mut out, rec.id);
        put_opt_u64(&mut out, rec.parent);
        put_scope(&mut out, &rec.scope);
        put_opt_u64(&mut out, rec.constraints.expires_at);
        put_u8(&mut out, rec.constraints.approval_required as u8);
        put_u8(&mut out, rec.constraints.local_only as u8);
        put_str(&mut out, &rec.subject);
        put_str(&mut out, &rec.action);
    }
    for id in revoked {
        put_u64(&mut out, *id);
    }
    let sum = content_hash(&out);
    put_u64(&mut out, sum);
    out
}

/// Serialize a capability engine, stamped with the logical clock it was taken under.
///
/// Records are written in id order so an unchanged registry produces byte-identical output — the
/// same doctrine `scripts/sbom.py` follows, and for the same reason: a store that differs on every
/// save makes every save look like a change and hides the real ones.
pub fn save(engine: &CapEngine) -> Vec<u8> {
    let (registry, revoked, next_id, secret) = engine.parts();
    let records: Vec<Record> = registry
        .iter()
        .map(|(id, cap)| Record {
            id: *id,
            parent: cap.parent,
            subject: cap.subject.clone(),
            action: cap.action.clone(),
            scope: cap.scope.clone(),
            constraints: cap.constraints,
        })
        .collect();
    let revoked: Vec<u64> = revoked.iter().copied().collect();
    encode(engine.now(), secret, next_id, &records, &revoked)
}

/// Serialize and authenticate a capability engine with HMAC-SHA256.
///
/// The authenticated envelope is the legacy checksummed image followed by a 32-byte HMAC tag.
/// Keeping the checksum inside the authenticated bytes preserves existing corruption diagnostics
/// after authentication succeeds while making medium writers unable to alter authority without the
/// key. The key is caller-owned; this function does not pretend to establish its provenance.
pub fn save_authenticated(engine: &CapEngine, key: &[u8; 32]) -> Vec<u8> {
    let mut image = save(engine);
    let tag = crate::crypto::hmac_sha256(key, &image);
    image.extend_from_slice(&tag);
    image
}

/// Load an authenticated capability image. Authentication runs before parsing or admitting any
/// capability record, so a wrong key and a tampered image cannot reach the authority lattice.
pub fn load_authenticated(
    bytes: &[u8],
    key: &[u8; 32],
    now: u64,
) -> Result<CapEngine, CapStoreError> {
    if bytes.len() < 32 {
        return Err(CapStoreError::Truncated);
    }
    let split = bytes.len() - 32;
    let (image, tag_bytes) = bytes.split_at(split);
    let mut supplied = [0u8; 32];
    supplied.copy_from_slice(tag_bytes);
    let expected = crate::crypto::hmac_sha256(key, image);
    if !crate::crypto::ct_eq_32(&expected, &supplied) {
        return Err(CapStoreError::Authentication);
    }
    load(image, now)
}

/// The records and counters an engine would save, so a caller can perturb one and re-encode it.
/// Paired with [`encode_for_test`]; together they are the attacker who owns the disk.
pub fn decompose(engine: &CapEngine) -> (u64, u64, u64, Vec<Record>, Vec<u64>) {
    let (registry, revoked, next_id, secret) = engine.parts();
    let records = registry
        .iter()
        .map(|(id, cap)| Record {
            id: *id,
            parent: cap.parent,
            subject: cap.subject.clone(),
            action: cap.action.clone(),
            scope: cap.scope.clone(),
            constraints: cap.constraints,
        })
        .collect();
    (
        engine.now(),
        secret,
        next_id,
        records,
        revoked.iter().copied().collect(),
    )
}

/// Assemble an image from records that never passed [`CapEngine::delegate`] — a widened child, an
/// orphan, a cycle, a re-mintable id, a thinned revocation list. Named for what it is, exactly as
/// [`crate::spine::CapToken::forge_for_test`] is: not a minting path, an attack surface, so that
/// every refusal [`load`] performs is proved against a WELL-FORMED forgery rather than against a
/// corrupted one that would have been refused for the wrong reason.
pub fn encode_for_test(
    epoch: u64,
    secret: u64,
    next_id: u64,
    records: &[Record],
    revoked: &[u64],
) -> Vec<u8> {
    encode(epoch, secret, next_id, records, revoked)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CapStoreError> {
        let end = self.at.checked_add(n).ok_or(CapStoreError::Truncated)?;
        if end > self.b.len() {
            return Err(CapStoreError::Truncated);
        }
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, CapStoreError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, CapStoreError> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, CapStoreError> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
    fn opt_u64(&mut self) -> Result<Option<u64>, CapStoreError> {
        let present = self.u8()?;
        let v = self.u64()?;
        match present {
            0 => Ok(None),
            1 => Ok(Some(v)),
            _ => Err(CapStoreError::BadEncoding),
        }
    }
    fn boolean(&mut self) -> Result<bool, CapStoreError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CapStoreError::BadEncoding),
        }
    }
    fn string(&mut self) -> Result<String, CapStoreError> {
        let n = self.u32()? as usize;
        // Bound the length by what is actually left before allocating: a corrupt length field is
        // the classic way a decoder is made to reserve gigabytes on behalf of an attacker.
        if self.at + n > self.b.len() {
            return Err(CapStoreError::Truncated);
        }
        let s = self.take(n)?;
        core::str::from_utf8(s)
            .map(|x| x.to_string())
            .map_err(|_| CapStoreError::BadEncoding)
    }
    fn scope(&mut self) -> Result<Scope, CapStoreError> {
        match self.u8()? {
            0 => Ok(Scope::All),
            1 => Ok(Scope::None),
            2 => etype_of(self.u8()?)
                .map(Scope::Type)
                .ok_or(CapStoreError::BadEncoding),
            3 => {
                let n = self.u32()? as usize;
                // Each entry is 8 bytes; refuse a count the remaining image cannot contain.
                if n.saturating_mul(8) > self.b.len() - self.at {
                    return Err(CapStoreError::Truncated);
                }
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    ids.push(self.u64()?);
                }
                Ok(Scope::Entities(ids))
            }
            _ => Err(CapStoreError::BadEncoding),
        }
    }
}

/// Restore a capability engine from a persisted image, under the clock `now`.
///
/// Refuses the whole image on any of [`CapStoreError`]. On success the returned engine authorizes
/// exactly the capabilities the image described, minus everything the revocation cascade kills —
/// and the tokens the caller held before the save still name the same authority, because the ids
/// are what was persisted.
pub fn load(bytes: &[u8], now: u64) -> Result<CapEngine, CapStoreError> {
    if bytes.len() < 8 {
        return Err(CapStoreError::Truncated);
    }
    // Checksum first: nothing else in the image is worth interpreting until the bytes are the bytes
    // that were written.
    let (body, tail) = bytes.split_at(bytes.len() - 8);
    let mut a = [0u8; 8];
    a.copy_from_slice(tail);
    if content_hash(body) != u64::from_le_bytes(a) {
        return Err(CapStoreError::Checksum);
    }

    let mut r = Reader { b: body, at: 0 };
    if r.take(4)? != MAGIC {
        return Err(CapStoreError::BadMagic);
    }
    if r.u32()? != VERSION {
        return Err(CapStoreError::BadVersion);
    }
    let epoch = r.u64()?;
    let secret = r.u64()?;
    let next_id = r.u64()?;
    let n_caps = r.u32()? as usize;
    let n_revoked = r.u32()? as usize;

    // The clock must not have gone backwards (see the module docs) — checked before any record is
    // admitted, so a rewound image is refused for the reason that actually matters rather than for
    // whatever its first bad record happens to be.
    if now < epoch {
        return Err(CapStoreError::ClockRewound);
    }

    let mut registry: BTreeMap<u64, StoredCapability> = BTreeMap::new();
    for _ in 0..n_caps {
        let id = r.u64()?;
        let parent = r.opt_u64()?;
        let scope = r.scope()?;
        let expires_at = r.opt_u64()?;
        let approval_required = r.boolean()?;
        let local_only = r.boolean()?;
        let subject = r.string()?;
        let action = r.string()?;
        let cap = StoredCapability {
            subject,
            action,
            scope,
            constraints: Constraints {
                expires_at,
                approval_required,
                local_only,
            },
            parent,
        };
        if registry.insert(id, cap).is_some() {
            return Err(CapStoreError::Duplicate);
        }
    }

    let mut revoked: BTreeSet<u64> = BTreeSet::new();
    for _ in 0..n_revoked {
        revoked.insert(r.u64()?);
    }
    if r.at != body.len() {
        return Err(CapStoreError::TrailingBytes);
    }

    // Every stored id must already be behind the counter, or the next mint collides with it.
    for id in registry.keys() {
        if (*id ^ secret) >= next_id {
            return Err(CapStoreError::IdReusable);
        }
    }

    // Structure: parents exist, no cycles.
    for cap in registry.values() {
        if let Some(p) = cap.parent {
            if !registry.contains_key(&p) {
                return Err(CapStoreError::Orphan);
            }
        }
    }
    for start in registry.keys() {
        let mut seen = 0usize;
        let mut at = *start;
        while let Some(p) = registry.get(&at).and_then(|c| c.parent) {
            at = p;
            seen += 1;
            // A chain longer than the registry has re-entered itself; there is nowhere else for it
            // to go once every record has been visited once.
            if seen > registry.len() {
                return Err(CapStoreError::Cycle);
            }
        }
    }

    // Authority: every edge must satisfy the SAME attenuation rule `delegate` enforces.
    for cap in registry.values() {
        let Some(pid) = cap.parent else { continue };
        let parent = &registry[&pid];
        let ok = attenuates(
            &Authority {
                action: &parent.action,
                scope: &parent.scope,
                constraints: &parent.constraints,
            },
            &Authority {
                action: &cap.action,
                scope: &cap.scope,
                constraints: &cap.constraints,
            },
        );
        if !ok {
            return Err(CapStoreError::Amplified);
        }
    }

    Ok(CapEngine::from_parts(
        registry, revoked, next_id, secret, now,
    ))
}

fn fs_error(error: FsError) -> CapStoreError {
    match error {
        FsError::NotFound => CapStoreError::Absent,
        FsError::TooLarge => CapStoreError::TooLarge,
        other => CapStoreError::Fs(other),
    }
}

/// Save capability state into one filesystem object. [`Filesystem::replace`] makes the image
/// crash-atomic: a torn update leaves old authority image or new one, never half an image.
///
/// This persists a self-consistent, checksummed image. It does NOT authenticate whoever can write
/// the medium, and therefore must not be used as boot trust without an authenticated key policy.
pub fn save_to_fs<D: crate::storage::BlockDevice>(
    fs: &mut Filesystem,
    dev: &mut D,
    engine: &CapEngine,
) -> Result<(), CapStoreError> {
    fs.replace(dev, STORE_OBJECT, &save(engine))
        .map_err(fs_error)
}

/// Save an authenticated image as one atomically replaced filesystem object.
pub fn save_authenticated_to_fs<D: crate::storage::BlockDevice>(
    fs: &mut Filesystem,
    dev: &mut D,
    engine: &CapEngine,
    key: &[u8; 32],
) -> Result<(), CapStoreError> {
    fs.replace(dev, STORE_OBJECT, &save_authenticated(engine, key))
        .map_err(fs_error)
}

/// Load and validate capability state from its filesystem object. Missing state is [`Absent`]; any
/// malformed, tampered, widened, orphaned, cyclic, expired-by-clock, or checksum-invalid image is
/// refused by [`load`] as a whole.
pub fn load_from_fs<D: crate::storage::BlockDevice>(
    fs: &Filesystem,
    dev: &D,
    now: u64,
) -> Result<CapEngine, CapStoreError> {
    let image = fs.read(dev, STORE_OBJECT).map_err(fs_error)?;
    load(&image, now)
}

/// Load and authenticate the capability image stored in one filesystem object.
pub fn load_authenticated_from_fs<D: crate::storage::BlockDevice>(
    fs: &Filesystem,
    dev: &D,
    key: &[u8; 32],
    now: u64,
) -> Result<CapEngine, CapStoreError> {
    let image = fs.read(dev, STORE_OBJECT).map_err(fs_error)?;
    load_authenticated(&image, key, now)
}

// ---------------------------------------------------------------------------
// The in-kernel invariant suite
// ---------------------------------------------------------------------------

/// The capability-lifetime invariants, in the same shape every other kernel suite uses: each check
/// reports through `report(index, passed, name)` and the first failure stops the run. Arch
/// independent — it touches no device — so all three targets and the hosted tests prove the same
/// fourteen behaviors, including authenticated-image admission.
pub fn capstore_suite(
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

    // A registry worth persisting: a root, an attenuated child, and a revoked sibling.
    let build = || {
        let mut e = CapEngine::new(0x5EED, 1000);
        let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
        let child = e
            .delegate(
                root,
                "agent",
                "entity.derive",
                Scope::Type(EntityType::Document),
                Constraints::none(),
            )
            .unwrap();
        let doomed = e
            .delegate(
                root,
                "agent",
                "entity.delete",
                Scope::All,
                Constraints::none(),
            )
            .unwrap();
        e.revoke(doomed);
        (e, root, child, doomed)
    };
    let doc_target = crate::spine::Target {
        id: None,
        etype: Some(EntityType::Document),
    };

    // 1 — authority survives the reboot: the token held before the save still authorizes after it.
    let (e, _root, child, doomed) = build();
    let image = save(&e);
    let reloaded = load(&image, 1000);
    check!(
        reloaded
            .as_ref()
            .map(|r| r.evaluate("entity.derive", &doc_target, &[child])
                == crate::spine::Decision::Allow)
            .unwrap_or(false),
        "capstore: a capability minted before the reboot still authorizes after it"
    );

    // 1a — authentication is checked before the image can become authority. The key is supplied by
    // the caller here; custody and delivery are deliberately separate boot-chain requirements.
    let auth_key = [0xA5u8; 32];
    let authenticated = save_authenticated(&e, &auth_key);
    check!(
        load_authenticated(&authenticated, &auth_key, 1000)
            .map(|r| r.evaluate("entity.derive", &doc_target, &[child])
                == crate::spine::Decision::Allow)
            .unwrap_or(false),
        "capstore: an authenticated image still authorizes with its trusted key"
    );
    check!(
        matches!(
            load_authenticated(&authenticated, &[0x5Au8; 32], 1000),
            Err(CapStoreError::Authentication)
        ),
        "capstore: an authenticated image rejects a wrong key before load"
    );
    let mut tampered = authenticated.clone();
    tampered[0] ^= 1;
    check!(
        matches!(
            load_authenticated(&tampered, &auth_key, 1000),
            Err(CapStoreError::Authentication)
        ),
        "capstore: an authenticated image rejects tamper before load"
    );

    // 2 — revocation survives it too. This is the one that matters: a registry restored without its
    // revocation list hands back authority its holder was already stripped of.
    let r2 = load(&image, 1000);
    check!(
        r2.as_ref()
            .map(|r| r.is_revoked(doomed)
                && matches!(
                    r.evaluate("entity.delete", &crate::spine::Target::default(), &[doomed]),
                    crate::spine::Decision::Deny(_)
                ))
            .unwrap_or(false),
        "capstore: a revoked capability is still revoked after the reboot"
    );

    // 3 — the cascade is re-derived, not replayed. Drop every revoked id but the root of the
    // cascade and the descendants must still come back dead.
    {
        let mut e = CapEngine::new(0x5EED, 1000);
        let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
        let kid = e
            .delegate(root, "a", "entity.derive", Scope::All, Constraints::none())
            .unwrap();
        let grandkid = e
            .delegate(kid, "b", "entity.derive", Scope::All, Constraints::none())
            .unwrap();
        e.revoke(root);
        // Rebuild the image with a revocation list naming ONLY the cascade's root — the shape an
        // attacker who can edit the store would choose, since it is the smallest edit that
        // resurrects the most authority.
        let (epoch, secret, next_id, records, revoked) = decompose(&e);
        let only_root: Vec<u64> = records
            .iter()
            .filter(|r| r.parent.is_none())
            .map(|r| r.id)
            .filter(|id| revoked.contains(id))
            .collect();
        let thinned = encode_for_test(epoch, secret, next_id, &records, &only_root);
        let back = load(&thinned, 1000);
        check!(
            back.as_ref()
                .map(|r| r.is_revoked(kid) && r.is_revoked(grandkid))
                .unwrap_or(false),
            "capstore: the revocation cascade is re-derived, not trusted from the image"
        );
    }

    // 4 — a clock that has gone backwards is refused, because it would un-expire everything.
    check!(
        matches!(load(&image, 999), Err(CapStoreError::ClockRewound)),
        "capstore: loading under an earlier clock is refused (expiry would be undone)"
    );

    // 5 — and an expiry that has genuinely passed is still an expiry after the reboot.
    {
        let mut e = CapEngine::new(0x5EED, 1000);
        let cap = e.mint(
            "u",
            "entity.derive",
            Scope::All,
            Constraints {
                expires_at: Some(1500),
                approval_required: false,
                local_only: true,
            },
        );
        let img = save(&e);
        let live = load(&img, 1200).map(|r| {
            r.evaluate("entity.derive", &crate::spine::Target::default(), &[cap])
                == crate::spine::Decision::Allow
        });
        let dead = load(&img, 2000).map(|r| {
            matches!(
                r.evaluate("entity.derive", &crate::spine::Target::default(), &[cap]),
                crate::spine::Decision::Deny(_)
            )
        });
        check!(
            live == Ok(true) && dead == Ok(true),
            "capstore: expiry is measured against the restored clock, before and after"
        );
    }

    // 6 — a tampered child that claims more than its parent is refused, whole image. The forgery is
    // well formed: correct magic, correct checksum, decodable records. Only the authority is wrong,
    // which is the only interesting kind of wrong.
    {
        let (e, _root, _child, _doomed) = build();
        let (epoch, secret, next_id, mut records, revoked) = decompose(&e);
        let widened = records.iter_mut().find(|r| r.parent.is_some()).map(|r| {
            r.action = "*".to_string();
            r.scope = Scope::All;
        });
        let forged = encode_for_test(epoch, secret, next_id, &records, &revoked);
        check!(
            widened.is_some() && matches!(load(&forged, 1000), Err(CapStoreError::Amplified)),
            "capstore: a child widened in the image is refused (attenuation re-checked on load)"
        );
    }

    // 6b — an orphan is refused: authority whose ancestor is absent can never be revoked by it.
    {
        let (e, _root, _child, _doomed) = build();
        let (epoch, secret, next_id, records, revoked) = decompose(&e);
        let orphans: Vec<Record> = records
            .iter()
            .filter(|r| r.parent.is_some())
            .cloned()
            .collect();
        let forged = encode_for_test(epoch, secret, next_id, &orphans, &revoked);
        check!(
            !orphans.is_empty() && matches!(load(&forged, 1000), Err(CapStoreError::Orphan)),
            "capstore: a record whose parent is absent from the image is refused"
        );
    }

    // 6c — an id the counter can still mint is refused, or a later mint would collide with a token
    // that is already held, and if that token was revoked the collision REVIVES it.
    {
        let (e, _root, _child, _doomed) = build();
        let (epoch, secret, _next_id, records, revoked) = decompose(&e);
        let forged = encode_for_test(epoch, secret, 1, &records, &revoked);
        check!(
            matches!(load(&forged, 1000), Err(CapStoreError::IdReusable)),
            "capstore: an image whose counter could re-mint a stored id is refused"
        );
    }

    // 7 — corruption is detected. EVERY single-bit flip, over every byte including the checksum's
    // own — a region the checksum did not cover would show up here as a load that succeeded.
    {
        let mut all_refused = true;
        'sweep: for i in 0..image.len() {
            for bit in 0..8 {
                let mut bad = image.clone();
                bad[i] ^= 1 << bit;
                if load(&bad, 1000).is_ok() {
                    all_refused = false;
                    break 'sweep;
                }
            }
        }
        check!(
            all_refused,
            "capstore: every single-bit corruption of the image is refused"
        );
    }

    // 8 — truncation is not a shorter store, it is not a store.
    check!(
        (1..image.len()).all(|k| load(&image[..k], 1000).is_err()),
        "capstore: every truncation of the image is refused"
    );

    // 9 — a refused load leaves the caller with nothing to use. Fail-closed means the error path
    // yields no engine at all, which the type states; what is proved here is that the engine the
    // caller already had is untouched by a failed reload beside it.
    {
        let (e, _root, child, _d) = build();
        let before = e.live_count();
        let _ = load(b"not a capability store at all", 1000);
        check!(
            e.live_count() == before
                && e.evaluate("entity.derive", &doc_target, &[child])
                    == crate::spine::Decision::Allow,
            "capstore: a refused load changes nothing about the running engine"
        );
    }

    Ok(n)
}
