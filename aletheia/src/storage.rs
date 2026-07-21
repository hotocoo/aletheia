//! Semantic store: content-addressed, versioned, encrypted-at-rest, durable (ADR-005, SAD §6).
//! Realized in M1 as an encrypted append-only log replayed into in-memory indices. Access is only
//! via System-Core APIs that check capabilities first — the store exposes no ambient namespace.
use crate::capabilities::StoredCapability;
use crate::crypto::Cipher;
use crate::domain::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
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
    cipher: Cipher,
    log_path: PathBuf,
    entities: HashMap<Id, Entity>,
    latest_by_chain: HashMap<Id, Id>,
    relationships: HashMap<Id, Relationship>,
    events: Vec<EventRecord>,
    blobs: HashMap<String, Vec<u8>>,
    loaded_caps: Vec<StoredCapability>,
    revoked_caps: Vec<String>,
}

impl Store {
    /// Open (or create) the store at `dir`. A local 32-byte key file provides encryption at rest.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| AlethError::persistence(&e.to_string()))?;
        let key = Self::load_or_create_key(&dir)?;
        let cipher = Cipher::new(&key);
        let log_path = dir.join("store.alog");
        let mut store = Store {
            dir,
            cipher,
            log_path,
            entities: HashMap::new(),
            latest_by_chain: HashMap::new(),
            relationships: HashMap::new(),
            events: Vec::new(),
            blobs: HashMap::new(),
            loaded_caps: Vec::new(),
            revoked_caps: Vec::new(),
        };
        store.replay()?;
        Ok(store)
    }

    fn load_or_create_key(dir: &Path) -> Result<[u8; 32]> {
        let kp = dir.join("key");
        if kp.exists() {
            let mut buf = Vec::new();
            File::open(&kp)
                .and_then(|mut f| f.read_to_end(&mut buf))
                .map_err(|e| AlethError::persistence(&e.to_string()))?;
            if buf.len() != 32 { return Err(AlethError::persistence("corrupt key file")); }
            let mut k = [0u8; 32];
            k.copy_from_slice(&buf);
            Ok(k)
        } else {
            let k = crate::crypto::random_key();
            let mut f = File::create(&kp).map_err(|e| AlethError::persistence(&e.to_string()))?;
            f.write_all(&k).map_err(|e| AlethError::persistence(&e.to_string()))?;
            f.sync_all().ok();
            Ok(k)
        }
    }

    fn replay(&mut self) -> Result<()> {
        if !self.log_path.exists() { return Ok(()); }
        let mut buf = Vec::new();
        File::open(&self.log_path)
            .and_then(|mut f| f.read_to_end(&mut buf))
            .map_err(|e| AlethError::persistence(&e.to_string()))?;
        let mut i = 0usize;
        while i + 4 <= buf.len() {
            let len = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
            i += 4;
            if i + len > buf.len() { return Err(AlethError::persistence("truncated log frame")); }
            let frame = &buf[i..i + len];
            i += len;
            let plain = self.cipher.open(frame)?;
            let rec: Record = serde_json::from_slice(&plain)
                .map_err(|e| AlethError::persistence(&e.to_string()))?;
            self.apply(rec);
        }
        Ok(())
    }

    fn apply(&mut self, rec: Record) {
        match rec {
            Record::Blob { hash, data } => { self.blobs.insert(hash, data); }
            Record::Entity(e) => {
                self.latest_by_chain.insert(e.version_chain.clone(), e.id.clone());
                self.entities.insert(e.id.clone(), e);
            }
            Record::Relationship(r) => { self.relationships.insert(r.id.clone(), r); }
            Record::Event(ev) => { self.events.push(ev); }
            Record::Capability(c) => { self.loaded_caps.push(c); }
            Record::Revoke { token } => { self.revoked_caps.push(token); }
        }
    }

    fn append(&mut self, rec: &Record) -> Result<()> {
        let plain = serde_json::to_vec(rec).map_err(|e| AlethError::internal(&e.to_string()))?;
        let sealed = self.cipher.seal(&plain);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .map_err(|e| AlethError::persistence(&e.to_string()))?;
        let len = (sealed.len() as u32).to_le_bytes();
        f.write_all(&len).map_err(|e| AlethError::persistence(&e.to_string()))?;
        f.write_all(&sealed).map_err(|e| AlethError::persistence(&e.to_string()))?;
        f.sync_all().ok();
        Ok(())
    }

    // --- write API (atomic: index update mirrors the durably-appended record) ---

    pub fn put_blob(&mut self, content: &[u8]) -> Result<String> {
        let hash = crate::crypto::sha256_hex(content);
        if !self.blobs.contains_key(&hash) {
            self.append(&Record::Blob { hash: hash.clone(), data: content.to_vec() })?;
            self.blobs.insert(hash.clone(), content.to_vec());
        }
        Ok(hash)
    }

    pub fn put_entity(&mut self, e: &Entity) -> Result<()> {
        self.append(&Record::Entity(e.clone()))?;
        self.latest_by_chain.insert(e.version_chain.clone(), e.id.clone());
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
        self.append(&Record::Revoke { token: token.to_string() })
    }

    // --- read API ---

    pub fn get_entity(&self, id: &Id) -> Option<&Entity> { self.entities.get(id) }
    /// Enumerate every stored entity (unordered — callers needing determinism must sort).
    pub fn entities(&self) -> impl Iterator<Item = &Entity> { self.entities.values() }
    pub fn latest_of_chain(&self, chain: &Id) -> Option<&Entity> {
        self.latest_by_chain.get(chain).and_then(|id| self.entities.get(id))
    }
    pub fn versions_of_chain(&self, chain: &Id) -> Vec<&Entity> {
        let mut v: Vec<&Entity> = self.entities.values().filter(|e| &e.version_chain == chain).collect();
        v.sort_by_key(|e| e.version);
        v
    }
    pub fn get_blob(&self, hash: &str) -> Option<&Vec<u8>> { self.blobs.get(hash) }
    pub fn get_relationship(&self, id: &Id) -> Option<&Relationship> { self.relationships.get(id) }
    pub fn relationships(&self) -> impl Iterator<Item = &Relationship> { self.relationships.values() }
    pub fn events(&self) -> &[EventRecord] { &self.events }
    pub fn loaded_caps(&self) -> &[StoredCapability] { &self.loaded_caps }
    pub fn revoked_tokens(&self) -> &[String] { &self.revoked_caps }
    pub fn log_path(&self) -> &Path { &self.log_path }
    pub fn dir(&self) -> &Path { &self.dir }
}
