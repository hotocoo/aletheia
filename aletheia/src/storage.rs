//! Semantic store: content-addressed, versioned, encrypted-at-rest, durable (ADR-005, SAD §6;
//! encryption-at-rest LIFECYCLE per ADR-069 / `atrest`). Realized in M1 as an encrypted
//! append-only log replayed into in-memory indices. Access is only via System-Core APIs that
//! check capabilities first — the store exposes no ambient namespace.
use crate::atrest::{self, AtRest};
use crate::capabilities::StoredCapability;
use crate::domain::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Clone because the journal mirror keeps the exact append order for whole-log rewrite (rekey /
// legacy migration); every variant is cheap field-wise to clone.
#[derive(Clone, Serialize, Deserialize)]
enum Record {
    Blob { hash: String, data: Vec<u8> },
    Entity(Entity),
    Relationship(Relationship),
    Event(EventRecord),
    Capability(StoredCapability),
    Revoke { token: String },
}

pub struct Store {
    dir: PathBuf,
    atrest: AtRest,
    log_path: PathBuf,
    entities: HashMap<Id, Entity>,
    latest_by_chain: HashMap<Id, Id>,
    relationships: HashMap<Id, Relationship>,
    events: Vec<EventRecord>,
    blobs: HashMap<String, Vec<u8>>,
    loaded_caps: Vec<StoredCapability>,
    revoked_caps: Vec<String>,
    /// Next frame position — also each frame's authenticated identity in the log (ADR-069 AAD).
    frame_seq: u64,
    /// Exact append-order mirror of every record in the log. The indices above are maps without
    /// order; a whole-log rewrite (legacy migration, rekey) must reproduce record ORDER, so the
    /// journal keeps it. Duplicates memory the hosted reference already pays for in full.
    journal: Vec<Record>,
    /// Frames sealed per key version — the audit surface for rotation/rekey (`encryption_status`).
    frames_per_version: HashMap<u32, u64>,
}

impl Store {
    /// Open (or create) the store at `dir`.
    ///
    /// Encryption at rest is the ADR-069 lifecycle: a root key wraps VERSIONED data keys, every
    /// frame is bound to its log position by AEAD AAD, and per-frame nonces are constructed
    /// prefix||counter pairs whose high-water marks are recovered from the authenticated log
    /// itself. A PRE-ADR-069 log (bare frames under the root key) is detected by
    /// trial-authentication and migrated wholesale BEFORE any new frame is appended, so a live
    /// log is always pure v2.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| AlethError::persistence(&e.to_string()))?;
        let atrest = AtRest::init(&dir)?;
        let log_path = dir.join("store.alog");
        let mut store = Store {
            dir,
            atrest,
            log_path,
            entities: HashMap::new(),
            latest_by_chain: HashMap::new(),
            relationships: HashMap::new(),
            events: Vec::new(),
            blobs: HashMap::new(),
            loaded_caps: Vec::new(),
            revoked_caps: Vec::new(),
            frame_seq: 0,
            journal: Vec::new(),
            frames_per_version: HashMap::new(),
        };
        store.replay_and_migrate()?;
        Ok(store)
    }

    /// Replay the log into the indices; migrate a legacy log to v2 in place if one is found.
    fn replay_and_migrate(&mut self) -> Result<()> {
        let Some(buf) = atrest::read_if_exists(&self.log_path)? else {
            return Ok(()); // fresh store
        };
        if buf.is_empty() {
            return Ok(());
        }
        // Format discrimination by AUTHENTICATION, not by magic alone: a frame authenticates
        // under exactly one reading to within cryptographic probability, so whichever opening
        // SUCCEEDS on the first frame names the format — and neither succeeding means a refusal
        // that says so, never a silent guess.
        let (_first_len, first_frame) = Self::first_frame(&buf)?;
        if self.atrest.open_frame(0, first_frame).is_ok() {
            self.replay_v2(&buf)
        } else if self.atrest.open_legacy_frame(0, first_frame).is_ok() {
            self.replay_legacy(&buf)?;
            self.rewrite_log()?; // wholesale migration; atomic via temp+rename
            Ok(())
        } else {
            Err(AlethError::persistence(
                "store log: first frame fails authentication as BOTH v2 and legacy (wrong key or corruption)",
            ))
        }
    }

    /// Frame-length prefix parse of the first frame only (the discriminator needs no more).
    fn first_frame(buf: &[u8]) -> Result<(usize, &[u8])> {
        if buf.len() < 4 {
            return Err(AlethError::persistence(
                "store log: shorter than one length prefix",
            ));
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if 4 + len > buf.len() {
            return Err(AlethError::persistence("store log: truncated first frame"));
        }
        Ok((len, &buf[4..4 + len]))
    }

    /// Strict v2 replay: every frame must authenticate AT ITS POSITION; trailing bytes that do
    /// not compose a whole frame are a REFUSAL, not silent residue (a torn tail write is
    /// corruption, and swallowing it would silently drop records).
    fn replay_v2(&mut self, buf: &[u8]) -> Result<()> {
        let mut marks: HashMap<u32, u64> = HashMap::new();
        let mut i = 0usize;
        while i < buf.len() {
            if i + 4 > buf.len() {
                return Err(AlethError::persistence(
                    "store log: trailing partial length prefix",
                ));
            }
            let len = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
            i += 4;
            if i + len > buf.len() {
                return Err(AlethError::persistence("truncated log frame"));
            }
            let seq = self.frame_seq;
            let opened = self.atrest.open_frame(seq, &buf[i..i + len])?;
            let rec: Record = serde_json::from_slice(&opened.plaintext).map_err(|e| {
                AlethError::persistence(&format!("frame {seq}: corrupt record ({e})"))
            })?;
            let m = marks.entry(opened.version).or_insert(0);
            if opened.nonce.counter >= *m {
                *m = opened.nonce.counter;
            }
            *self.frames_per_version.entry(opened.version).or_insert(0) += 1;
            self.frame_seq += 1;
            self.journal.push(rec.clone());
            self.apply(rec);
            i += len;
        }
        // Recover nonce counters from the authenticated log — the log IS the counter ledger.
        let collected: Vec<(u32, u64)> = marks.into_iter().collect();
        self.atrest.advance_counters(&collected)
    }

    /// Legacy (pre-ADR-069) replay under the root key; the caller then migrates. The same
    /// strict tail rule applies — a torn tail was already refused by the old reader.
    fn replay_legacy(&mut self, buf: &[u8]) -> Result<()> {
        let mut i = 0usize;
        while i < buf.len() {
            if i + 4 > buf.len() {
                return Err(AlethError::persistence(
                    "store log: trailing partial length prefix",
                ));
            }
            let len = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
            i += 4;
            if i + len > buf.len() {
                return Err(AlethError::persistence("truncated log frame"));
            }
            let seq = self.frame_seq;
            let plain = self.atrest.open_legacy_frame(seq, &buf[i..i + len])?;
            let rec: Record = serde_json::from_slice(&plain).map_err(|e| {
                AlethError::persistence(&format!("frame {seq}: corrupt record ({e})"))
            })?;
            self.frame_seq += 1;
            self.journal.push(rec.clone());
            self.apply(rec);
            i += len;
        }
        Ok(())
    }

    /// Rewrite the WHOLE log under the current key version from the append-order journal:
    /// legacy migration AND post-rotation rekey share this one path. Atomic by temp+rename, so
    /// a crash leaves either the old complete log or the new complete log, never a hybrid.
    fn rewrite_log(&mut self) -> Result<()> {
        let tmp = self.log_path.with_extension("alog.tmp");
        std::fs::remove_file(&tmp).ok();
        self.frames_per_version.clear();
        self.frame_seq = 0;
        for (seq, rec) in self.journal.iter().enumerate() {
            let plain =
                serde_json::to_vec(rec).map_err(|e| AlethError::internal(&e.to_string()))?;
            let frame = self.atrest.seal_frame(seq as u64, &plain)?;
            *self
                .frames_per_version
                .entry(self.atrest.current_version())
                .or_insert(0) += 1;
            atrest::append_frame(&tmp, &frame)?;
        }
        self.frame_seq = self.journal.len() as u64;
        // fsync the temp before it becomes the log, then swap.
        if let Ok(f) = std::fs::File::open(&tmp) {
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, &self.log_path).map_err(|e| AlethError::persistence(&e.to_string()))
    }

    fn apply(&mut self, rec: Record) {
        match rec {
            Record::Blob { hash, data } => {
                self.blobs.insert(hash, data);
            }
            Record::Entity(e) => {
                self.latest_by_chain
                    .insert(e.version_chain.clone(), e.id.clone());
                self.entities.insert(e.id.clone(), e);
            }
            Record::Relationship(r) => {
                self.relationships.insert(r.id.clone(), r);
            }
            Record::Event(ev) => {
                self.events.push(ev);
            }
            Record::Capability(c) => {
                self.loaded_caps.push(c);
            }
            Record::Revoke { token } => {
                self.revoked_caps.push(token);
            }
        }
    }

    fn append(&mut self, rec: &Record) -> Result<()> {
        let plain = serde_json::to_vec(rec).map_err(|e| AlethError::internal(&e.to_string()))?;
        let seq = self.frame_seq;
        let frame = self.atrest.seal_frame(seq, &plain)?;
        *self
            .frames_per_version
            .entry(self.atrest.current_version())
            .or_insert(0) += 1;
        atrest::append_frame(&self.log_path, &frame)?;
        self.frame_seq += 1;
        self.journal.push(rec.clone());
        Ok(())
    }

    // --- write API (atomic: index update mirrors the durably-appended record) ---

    pub fn put_blob(&mut self, content: &[u8]) -> Result<String> {
        let hash = crate::crypto::sha256_hex(content);
        if !self.blobs.contains_key(&hash) {
            self.append(&Record::Blob {
                hash: hash.clone(),
                data: content.to_vec(),
            })?;
            self.blobs.insert(hash.clone(), content.to_vec());
        }
        Ok(hash)
    }

    pub fn put_entity(&mut self, e: &Entity) -> Result<()> {
        self.append(&Record::Entity(e.clone()))?;
        self.latest_by_chain
            .insert(e.version_chain.clone(), e.id.clone());
        self.entities.insert(e.id.clone(), e.clone());
        Ok(())
    }

    pub fn put_relationship(&mut self, r: &Relationship) -> Result<()> {
        self.append(&Record::Relationship(r.clone()))?;
        self.relationships.insert(r.id.clone(), r.clone());
        Ok(())
    }

    pub fn put_event(&mut self, ev: &EventRecord) -> Result<()> {
        self.append(&Record::Event(ev.clone()))?;
        self.events.push(ev.clone());
        Ok(())
    }

    pub fn put_capability(&mut self, c: &StoredCapability) -> Result<()> {
        self.append(&Record::Capability(c.clone()))
    }

    pub fn put_revoke(&mut self, token: &str) -> Result<()> {
        self.append(&Record::Revoke {
            token: token.to_string(),
        })
    }

    // --- encryption-at-rest lifecycle (ADR-069) ---

    /// Mint a new data-key version. Subsequent frames seal under it; frames already in the log
    /// remain readable while their version is retained. The keystore persists atomically.
    pub fn rotate_encryption_keys(&mut self) -> Result<u32> {
        self.atrest.rotate()
    }

    /// Rewrite the whole log under the CURRENT key version and retire every older one:
    /// the end state of a rotation, one key on disk, nothing orphaned. Atomic per
    /// `rewrite_log`; the retirement refuses if it would strand the newest version.
    pub fn rekey_log(&mut self) -> Result<usize> {
        let current = self.atrest.current_version();
        self.rewrite_log()?;
        self.atrest.retire_below(current)
    }

    /// Frames sealed under each key version, ascending by version — the rotation audit line.
    pub fn encryption_status(&self) -> Vec<(u32, u64)> {
        let mut v: Vec<(u32, u64)> = self
            .frames_per_version
            .iter()
            .map(|(k, c)| (*k, *c))
            .collect();
        v.sort_by_key(|(ver, _)| *ver);
        v
    }

    /// Key versions currently HELD by this store's keystore (held ≠ in use: retired versions
    /// disappear here before they disappear from the frame counts of an un-rekeyed log).
    pub fn held_key_versions(&self) -> Vec<u32> {
        self.atrest.versions()
    }

    // --- read API ---

    pub fn get_entity(&self, id: &Id) -> Option<&Entity> {
        self.entities.get(id)
    }
    /// Enumerate every stored entity (unordered — callers needing determinism must sort).
    pub fn entities(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }
    pub fn latest_of_chain(&self, chain: &Id) -> Option<&Entity> {
        self.latest_by_chain
            .get(chain)
            .and_then(|id| self.entities.get(id))
    }
    pub fn versions_of_chain(&self, chain: &Id) -> Vec<&Entity> {
        let mut v: Vec<&Entity> = self
            .entities
            .values()
            .filter(|e| &e.version_chain == chain)
            .collect();
        v.sort_by_key(|e| e.version);
        v
    }
    pub fn get_blob(&self, hash: &str) -> Option<&Vec<u8>> {
        self.blobs.get(hash)
    }
    pub fn get_relationship(&self, id: &Id) -> Option<&Relationship> {
        self.relationships.get(id)
    }
    pub fn relationships(&self) -> impl Iterator<Item = &Relationship> {
        self.relationships.values()
    }
    pub fn events(&self) -> &[EventRecord] {
        &self.events
    }
    pub fn loaded_caps(&self) -> &[StoredCapability] {
        &self.loaded_caps
    }
    pub fn revoked_tokens(&self) -> &[String] {
        &self.revoked_caps
    }
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
