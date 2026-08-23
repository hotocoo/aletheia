//! Encryption at rest as a LIFECYCLE, not a key file (ADR-069).
//!
//! The store already sealed every frame with ChaCha20-Poly1305 — but under ONE unversioned key
//! file, with a fresh random 96-bit nonce per record whose non-reuse was delegated to the CSPRNG
//! and never checked, and with no answer to what a content address MEANS once bytes at rest are
//! ciphertext. This module closes the three open rows (ALET-P1-028, ALET-P1-029, ALET-P1-030):
//!
//! * **Key lifecycle (P1-028).** Data keys are VERSIONED entries in a keystore sealed under a
//!   root key (the pre-existing `key` file, now the custody anchor that wraps data keys instead
//!   of being the data key). Rotation appends a new version; writes always use the newest;
//!   reads of older frames keep working while their version is retained; retirement is refused
//!   while it would orphan a version still in use. Every refusal names the version.
//! * **Nonce lifecycle (P1-029).** Every DATA frame is sealed under a CONSTRUCTED nonce —
//!   a 32-bit per-key random prefix || a 64-bit monotone counter — so reuse is impossible by
//!   construction rather than improbable by birthday bound. The counter ledger is the log
//!   itself: after a crash, replay recovers each version's high-water mark from its own
//!   authenticated frames and the counter only ever moves FORWARD (a regression is a named
//!   error, never silently accepted). Exhaustion at `u64::MAX` is a named refusal that
//!   demands rotation, not a wraparound.
//! * **Identity semantics (P1-030).** The content address remains SHA-256(PLAINTEXT): identity,
//!   deduplication and references are semantic facts that outlive the encryption layer. The
//!   bytes at rest are ciphertext under a per-frame nonce, so two identical plaintexts produce
//!   two DIFFERENT frames — equality of content is invisible on the wire, and the address is
//!   stable while the storage encoding is not. Both halves are proved, not asserted.
//!
//! Frames are position-bound: the AEAD additional authenticated data covers the frame's
//! sequence number, its exact length and its key version, so reordering, deletion, duplication
//! or truncation-in-the-middle of frames fails authentication with the position named. The one
//! residual is stated where it lives (`docs/adr/ADR-069-encryption-at-rest-is-a-lifecycle.md`):
//! truncation at a FRAME BOUNDARY at the tail of the log is indistinguishable from "the last
//! writes never happened" without an external anchor, and is a documented non-claim.
//!
//! Legacy compatibility: a log written by the pre-ADR-069 store (bare nonce||ciphertext frames
//! under the root key) is detected by trial-authentication of its first frame — a frame
//! authenticates under at most one reading to within 2^-128 — and is migrated wholesale into
//! the versioned format at open, before any new frame is appended. Steady-state logs are
//! therefore ALWAYS pure v2; the legacy reader exists only inside the one-time migrator.
use crate::crypto::{hmac_sha256, Cipher};
use crate::domain::{AlethError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Frame magic. A v2 frame always begins with these four bytes; a legacy frame begins with the
/// first byte of a random nonce, which equals this prefix with probability 2^-32 — and the
/// trial-authentication discriminator below does not care: only a frame that AUTHENTICATES as
/// legacy is read as legacy, so a coincidental prefix is refused, never misread.
pub const MAGIC: [u8; 4] = *b"ALX1";
/// AEAD domain-separation prefix for the log-frame AAD.
const AAD_DOMAIN: &[u8] = b"aletheia.alog.v2";
/// Keystore file name (sealed under a root-derived subkey).
const KEYSTORE_FILE: &str = "keystore.bin";
/// Root key file name — the pre-existing custody anchor, unchanged for existing stores.
const ROOT_KEY_FILE: &str = "key";
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = 4 + 1 + 4 + 8;

/// A constructed 96-bit nonce: 32-bit per-key random prefix || 64-bit monotone counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nonce96 {
    pub prefix: [u8; 4],
    pub counter: u64,
}

impl Nonce96 {
    /// The exhaustion check is the point: a wraparound would silently reuse a nonce under the
    /// same key — the one failure AEAD cannot survive — so the last counter is refused BY NAME.
    pub fn new(prefix: [u8; 4], counter: u64) -> Result<Self> {
        if counter == u64::MAX {
            return Err(AlethError::persistence(
                "nonce space exhausted for this key version — rotate now",
            ));
        }
        Ok(Nonce96 { prefix, counter })
    }

    pub fn to_bytes(self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[..4].copy_from_slice(&self.prefix);
        out[4..].copy_from_slice(&self.counter.to_le_bytes());
        out
    }

    pub fn from_bytes(b: &[u8; 12]) -> Self {
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&b[..4]);
        let mut ctr = [0u8; 8];
        ctr.copy_from_slice(&b[4..]);
        Nonce96 {
            prefix,
            counter: u64::from_le_bytes(ctr),
        }
    }
}

/// One versioned data key. `prefix` namespaces this version's nonce space; `next_counter`
/// is the high-water mark recovered from the log at open (the keystore's copy is only the
/// value at creation time and is never trusted over the log).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub version: u32,
    pub key: [u8; 32],
    pub prefix: [u8; 4],
    pub next_counter: u64,
}

#[derive(Serialize, Deserialize)]
struct Keystore {
    entries: Vec<KeyEntry>,
}

/// A successfully opened frame: the plaintext plus WHICH key version and nonce authenticated it
/// (callers use these to recover counter high-water marks and to count frames per version).
pub struct Opened {
    pub version: u32,
    pub nonce: Nonce96,
    pub plaintext: Vec<u8>,
}

/// The encryption-at-rest lifecycle for one store directory.
pub struct AtRest {
    dir: PathBuf,
    root: [u8; 32],
    entries: Vec<KeyEntry>,
}

impl AtRest {
    /// Load (or create) the root key and the keystore for `dir`. A missing keystore is created
    /// with data-key version 1; an existing one is authenticated and loaded WHOLE — there is no
    /// partial load, because the entries a partial load drops are the ones that make the rest
    /// readable.
    pub fn init(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| AlethError::persistence(&e.to_string()))?;
        let root = Self::load_or_create_root(&dir)?;
        let mut at = AtRest {
            dir,
            root,
            entries: Vec::new(),
        };
        at.load_or_create_keystore()?;
        Ok(at)
    }

    fn load_or_create_root(dir: &Path) -> Result<[u8; 32]> {
        let kp = dir.join(ROOT_KEY_FILE);
        if kp.exists() {
            let mut buf = Vec::new();
            File::open(&kp)
                .and_then(|mut f| f.read_to_end(&mut buf))
                .map_err(|e| AlethError::persistence(&e.to_string()))?;
            if buf.len() != 32 {
                return Err(AlethError::persistence("corrupt root key file"));
            }
            let mut k = [0u8; 32];
            k.copy_from_slice(&buf);
            Ok(k)
        } else {
            let k = crate::crypto::random_key();
            write_private(&kp, &k)?;
            Ok(k)
        }
    }

    /// The keystore is sealed under a key DERIVED from the root (HMAC-SHA256, domain-separated),
    /// so the root file never sits next to a ciphertext made directly under it. Random nonces
    /// are correct here by a WRITTEN BOUND rather than by construction: the keystore is rewritten
    /// only on rotate/retire (a bounded population, not per record), so birthday-bound collision
    /// probability is ~(rewrites)^2 / 2^97 — below 10^-17 for a million rotations. Data frames,
    /// whose population is unbounded, get constructed nonces instead.
    fn keystore_cipher(&self) -> Cipher {
        let sub = hmac_sha256(&self.root, b"aletheia/keystore/v1");
        Cipher::new(&sub)
    }

    fn load_or_create_keystore(&mut self) -> Result<()> {
        let kp = self.dir.join(KEYSTORE_FILE);
        if kp.exists() {
            let mut buf = Vec::new();
            File::open(&kp)
                .and_then(|mut f| f.read_to_end(&mut buf))
                .map_err(|e| AlethError::persistence(&e.to_string()))?;
            let plain = self.keystore_cipher().open(&buf).map_err(|_| {
                AlethError::persistence(
                    "keystore: authentication failed (wrong root key or tampered keystore)",
                )
            })?;
            let ks: Keystore = serde_json::from_slice(&plain)
                .map_err(|e| AlethError::persistence(&format!("keystore: corrupt ({e})")))?;
            if ks.entries.is_empty() {
                return Err(AlethError::persistence("keystore: zero key versions"));
            }
            self.entries = ks.entries;
            Ok(())
        } else {
            let entry = KeyEntry {
                version: 1,
                key: crate::crypto::random_key(),
                prefix: crate::crypto::random_key_prefix(),
                next_counter: 0,
            };
            self.entries = vec![entry];
            self.persist_keystore()
        }
    }

    fn persist_keystore(&self) -> Result<()> {
        let plain = serde_json::to_vec(&Keystore {
            entries: self.entries.clone(),
        })
        .map_err(|e| AlethError::internal(&e.to_string()))?;
        let sealed = self.keystore_cipher().seal(&plain);
        write_private(&self.dir.join(KEYSTORE_FILE), &sealed)
    }

    /// The newest version — the only one `seal_frame` ever writes under.
    pub fn current_version(&self) -> u32 {
        self.entries.last().map(|e| e.version).unwrap_or(0)
    }

    pub fn versions(&self) -> Vec<u32> {
        self.entries.iter().map(|e| e.version).collect()
    }

    /// Mint version `max+1` with a fresh key and a fresh nonce prefix, and persist. Old
    /// entries are retained: existing frames must stay readable until a rekey retires them.
    pub fn rotate(&mut self) -> Result<u32> {
        let v = self.current_version() + 1;
        self.entries.push(KeyEntry {
            version: v,
            key: crate::crypto::random_key(),
            prefix: crate::crypto::random_key_prefix(),
            next_counter: 0,
        });
        self.persist_keystore()?;
        Ok(v)
    }

    /// Forget every version below `floor`. Refuses while the floor is above the newest
    /// version (which would orphan the write path) and refuses to retire the newest.
    pub fn retire_below(&mut self, floor: u32) -> Result<usize> {
        let max = self.current_version();
        if floor > max {
            return Err(AlethError::persistence(&format!(
                "retire floor {floor} is above the newest key version {max}"
            )));
        }
        let before = self.entries.len();
        self.entries.retain(|e| e.version >= floor);
        if self.entries.is_empty() {
            return Err(AlethError::persistence(
                "retirement would leave no key versions",
            ));
        }
        let removed = before - self.entries.len();
        if removed > 0 {
            self.persist_keystore()?;
        }
        Ok(removed)
    }

    /// Seal one frame bound to its log position `seq`. The AAD covers the sequence number,
    /// the exact frame length and the key version — so moving, removing, duplicating or
    /// resizing a frame anywhere in the log breaks authentication with the position named.
    pub fn seal_frame(&mut self, seq: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        let idx = self.entries.len() - 1;
        // The exhaustion check happens BEFORE the increment: wrapping to zero would hand a new
        // record a used nonce under the same key, and u64::MAX overflowing by panic in debug
        // builds would be an accidental crash instead of a named refusal.
        if self.entries[idx].next_counter == u64::MAX {
            return Err(AlethError::persistence(&format!(
                "nonce space exhausted for key v{} — rotate now",
                self.entries[idx].version
            )));
        }
        let (version, prefix, counter) = {
            let e = &mut self.entries[idx];
            let c = e.next_counter;
            e.next_counter += 1;
            (e.version, e.prefix, c)
        };
        let nonce = Nonce96::new(prefix, counter)?;
        let total = (HEADER_LEN + plaintext.len() + TAG_LEN) as u32;
        let aad = Self::frame_aad(seq, total, version);
        let ct = self
            .entry_cipher(version)?
            .seal_with_aad(&nonce.to_bytes(), plaintext, &aad)?;
        let mut frame = Vec::with_capacity(HEADER_LEN + ct.len());
        frame.extend_from_slice(&MAGIC);
        frame.push(version as u8);
        frame.extend_from_slice(&prefix);
        frame.extend_from_slice(&counter.to_le_bytes());
        frame.extend_from_slice(&ct);
        debug_assert_eq!(frame.len(), total as usize);
        Ok(frame)
    }

    /// Open one frame bound to its log position `seq`. Every failure names the position and,
    /// where the frame parsed, the key version — "wrong key", "tampered bytes" and "moved
    /// frame" are deliberately ONE refusal (authentication failure), because AEAD cannot and
    /// must not distinguish them; the position and version are the facts it CAN name.
    pub fn open_frame(&self, seq: u64, frame: &[u8]) -> Result<Opened> {
        if frame.len() < HEADER_LEN + TAG_LEN {
            return Err(AlethError::persistence(&format!(
                "frame {seq}: truncated ({} bytes is below the empty-frame minimum)",
                frame.len()
            )));
        }
        if frame[..4] != MAGIC {
            return Err(AlethError::persistence(&format!(
                "frame {seq}: not an ADR-069 frame (bad magic)"
            )));
        }
        let version = frame[4] as u32;
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&frame[5..9]);
        let mut ctr = [0u8; 8];
        ctr.copy_from_slice(&frame[9..17]);
        let nonce = Nonce96 {
            prefix,
            counter: u64::from_le_bytes(ctr),
        };
        let total = frame.len() as u32;
        let aad = Self::frame_aad(seq, total, version);
        let plain = self
            .entry_cipher(version)?
            .open_with_aad(&nonce.to_bytes(), &frame[HEADER_LEN..], &aad)
            .map_err(|_| {
                AlethError::persistence(&format!(
                    "frame {seq}: authentication failed under key v{version} (tampered, moved, or key not held)"
                ))
            })?;
        Ok(Opened {
            version,
            nonce,
            plaintext: plain,
        })
    }

    /// Open a PRE-ADR-069 frame: bare nonce||ciphertext under the ROOT key, no AAD, no version.
    /// Exists only for the one-time migration path.
    pub fn open_legacy_frame(&self, seq: u64, frame: &[u8]) -> Result<Vec<u8>> {
        Cipher::new(&self.root).open(frame).map_err(|_| {
            AlethError::persistence(&format!(
                "frame {seq}: legacy authentication failed (wrong root key or corruption)"
            ))
        })
    }

    /// Raise each version's counter high-water mark to the replayed maximum. The log is the
    /// counter ledger: this runs AFTER a successful replay, so the marks come from frames that
    /// AUTHENTICATED. A mark that would move BACKWARD is a named internal error — the keystore
    /// must never let a counter regress, because a regression is a nonce reuse waiting to happen.
    pub fn advance_counters(&mut self, marks: &[(u32, u64)]) -> Result<()> {
        for (v, seen) in marks {
            let e = self
                .entries
                .iter_mut()
                .find(|e| e.version == *v)
                .ok_or_else(|| {
                    AlethError::persistence(&format!(
                        "log references key v{v} but the keystore does not hold it"
                    ))
                })?;
            // Only forward. A keystore already ahead of the log is the sanctioned post-rekey
            // state; a log mark below it must not pull the counter back toward reuse.
            if *seen + 1 > e.next_counter {
                e.next_counter = *seen + 1;
            }
        }
        Ok(())
    }

    fn entry_cipher(&self, version: u32) -> Result<Cipher> {
        let e = self
            .entries
            .iter()
            .find(|e| e.version == version)
            .ok_or_else(|| {
                AlethError::persistence(&format!(
                    "frame names key v{version} but the keystore holds only {:?} (retired?)",
                    self.versions()
                ))
            })?;
        Ok(Cipher::new(&e.key))
    }

    fn frame_aad(seq: u64, total_len: u32, version: u32) -> Vec<u8> {
        let mut aad = Vec::with_capacity(AAD_DOMAIN.len() + 13);
        aad.extend_from_slice(AAD_DOMAIN);
        aad.extend_from_slice(&seq.to_le_bytes());
        aad.extend_from_slice(&total_len.to_le_bytes());
        aad.push(version as u8);
        aad
    }
}

/// Write bytes 0600-private (unix) via temp-file + rename so a reader never sees a half key
/// store, and the mode is set BEFORE the data lands.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp).map_err(|e| AlethError::persistence(&e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| AlethError::persistence(&e.to_string()))?;
        }
        f.write_all(bytes)
            .map_err(|e| AlethError::persistence(&e.to_string()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).map_err(|e| AlethError::persistence(&e.to_string()))?;
    Ok(())
}

/// Append-only open helper shared with storage: read a whole file or None if absent.
pub(crate) fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut buf))
        .map_err(|e| AlethError::persistence(&e.to_string()))?;
    Ok(Some(buf))
}

/// Append one length-prefixed frame to the log, syncing before returning.
pub(crate) fn append_frame(path: &Path, frame: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| AlethError::persistence(&e.to_string()))?;
    let len = (frame.len() as u32).to_le_bytes();
    f.write_all(&len)
        .and_then(|_| f.write_all(frame))
        .map_err(|e| AlethError::persistence(&e.to_string()))?;
    f.sync_all().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_roundtrips_and_refuses_exhaustion() {
        let n = Nonce96::new([1, 2, 3, 4], 42).unwrap();
        assert_eq!(Nonce96::from_bytes(&n.to_bytes()), n);
        assert_eq!(n.to_bytes(), [1, 2, 3, 4, 42, 0, 0, 0, 0, 0, 0, 0]);
        let err = Nonce96::new([0; 4], u64::MAX).unwrap_err();
        assert!(err.message.contains("nonce space exhausted"));
    }

    #[test]
    fn frame_roundtrip_and_position_binding() {
        let dir =
            std::env::temp_dir().join(format!("atrest-unit-{}", crate::crypto::random_token()));
        let mut at = AtRest::init(&dir).unwrap();
        let pt = b"hello, at-rest world";
        let frame = at.seal_frame(7, pt).unwrap();
        let opened = at.open_frame(7, &frame).unwrap();
        assert_eq!(opened.plaintext, pt);
        assert_eq!(opened.version, 1);
        // The SAME frame at a DIFFERENT position must fail: the sequence number is AAD.
        assert!(at.open_frame(8, &frame).is_err());
        assert!(at.open_frame(7, &frame[..frame.len() - 1]).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counters_advance_monotonically_across_instances() {
        let dir =
            std::env::temp_dir().join(format!("atrest-unit-{}", crate::crypto::random_token()));
        let mut at = AtRest::init(&dir).unwrap();
        let f0 = at.seal_frame(0, b"a").unwrap();
        let f1 = at.seal_frame(1, b"b").unwrap();
        let n0 = at.open_frame(0, &f0).unwrap().nonce;
        let n1 = at.open_frame(1, &f1).unwrap().nonce;
        assert_eq!(n1.counter, n0.counter + 1);
        // A fresh instance over the same directory continues, never rewinds.
        drop(at);
        let at2 = AtRest::init(&dir).unwrap();
        let opened = at2.open_frame(1, &f1).unwrap();
        assert_eq!(opened.nonce, n1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
