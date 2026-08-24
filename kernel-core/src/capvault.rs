//! Authority custody is a LIFECYCLE, not a caller-supplied key (ALET-P1-034, ADR-070).
//!
//! Until this module existed, [`crate::capstore`] could authenticate a persisted registry only
//! under a key the CALLER handed in on every call. That moved the problem, not the risk: whoever
//! holds the caller holds authority, custody was nobody, rotation was impossible, and every boot
//! re-asked the question the keystore should have answered. This module closes the custody half
//! of capability lifetime:
//!
//! * **The root is custody, never a working key.** The caller hands the vault 32 root bytes ONCE
//!   at open (in a booted system those arrive from the platform trust boundary - REQ-BOOT-001
//!   stays their delivery path and remains out of scope here). Every working key is DERIVED from
//!   the root behind domain-separated HMAC-SHA256 KDFs, so the root authenticates and wraps but
//!   never seals a capability image itself.
//! * **Data keys are VERSIONED and forward-chained.** The keystore object (one atomically
//!   replaced filesystem object, AEAD-sealed under a root-derived key) stores ONLY the retained
//!   versions. Rotation derives version max+1 as HMAC(current key, ROTATE) - a one-way chain -
//!   so retirement genuinely destroys: once a version leaves the keystore, its key bytes exist
//!   nowhere and images naming it are refused BY NAME ([VaultError::RetiredVersion]).
//! * **Nonces are CONSTRUCTED, never random.** This kernel has no entropy source at boot, so
//!   randomness cannot be the mechanism. Every sealed object carries a 96-bit constructed nonce:
//!   a per-key deterministic prefix (HMAC of the sealing key) concatenated with a monotone
//!   counter persisted IN the keystore. Reuse is impossible BY CONSTRUCTION, not improbable by
//!   birthday bound. The image protocol RESERVES the counter first (keystore commit) and seals
//!   second: a crash between the two commits wastes a number and can never reuse one.
//!   Exhaustion at u64::MAX is a named refusal demanding rotation - never a wraparound.
//! * **Both objects are AEAD-sealed (ChaCha20-Poly1305, RFC 8439)** with additional
//!   authenticated data binding the format version and every cleartext header field, so
//!   transposition or field-editing fails authentication. Authentication precedes parsing: a
//!   failed open releases NO bytes into any parser, and a keystore that fails authentication
//!   under the supplied root is refused WHOLE ([VaultError::KeystoreAuth]) - there is no
//!   partial load of a keystore.
//! * **Rekey is the three-commit pivot.** Rotate (keystore gains max+1, keeps max), then the
//!   image is rewritten under the newest version, then every older version is retired. Each
//!   commit is atomic ([Filesystem::replace]), and every crash position leaves a world where
//!   SOME complete keystore+image pair opens: old image with both versions retained, new image
//!   with both retained, or new image newest-only. Proved exhaustively by fault injection in
//!   kernel-core/tests/capvault.rs.
//!
//! # Named non-claims
//!
//! * Rolling back BOTH objects to a consistent older snapshot is undetectable without an external
//!   anchor - the same residual ADR-069 documents for log tails. Rolling back the keystore ALONE
//!   against a newer image IS detected and named ([VaultError::FutureVersion]); the residual is
//!   pinned by tests in both directions so documentation and behavior cannot drift.
//! * Root CUSTODY is whoever calls open; secure-boot DELIVERY of the root stays REQ-BOOT-001.
//! * Confidentiality of the capability image follows from the seal, but authority tokens are
//!   unforgeable ids: integrity, not secrecy, is the property the system depends on.
//!
//! No pre-existing deployment reads these objects: nothing outside selftests called the
//! caller-keyed capstore paths, so the vault defines its format without a migration cliff.

use alloc::vec;
use alloc::vec::Vec;

use crate::capstore::{self, CapStoreError};
use crate::crypto::{aead_open, aead_seal, hmac_sha256, AeadError};
use crate::fs::{Filesystem, FsError};
use crate::spine::CapEngine;
use crate::storage::BlockDevice;

/// Filesystem object holding the sealed, versioned data keys. One atomic replace per mutation.
pub const KEYSTORE_OBJECT: &str = "cap.keys";
/// Filesystem object holding the sealed capability image.
pub const VAULT_OBJECT: &str = "cap.vault";

const KS_MAGIC: &[u8; 4] = b"ALKS";
const IMG_MAGIC: &[u8; 4] = b"ALCV";
/// On-disk format version. Any other value is refused outright, in both objects.
pub const FORMAT_VERSION: u32 = 1;
const TAG_LEN: usize = 16;
/// KS_MAGIC | ver u32 | ks_counter u64 | n_entries u32
const KS_HEADER_LEN: usize = 4 + 4 + 8 + 4;
/// IMG_MAGIC | ver u32 | key_version u32 | counter u64 | pt_len u32
const IMG_HEADER_LEN: usize = 4 + 4 + 4 + 8 + 4;

const KS_SEAL_DOMAIN: &[u8] = b"aletheia.capvault/v1/keystore-seal";
const DATA_KEY_DOMAIN: &[u8] = b"aletheia.capvault/v1/data-key/";
const ROTATE_DOMAIN: &[u8] = b"aletheia.capvault/v1/rotate";
const KS_NONCE_DOMAIN: &[u8] = b"aletheia.capvault/v1/keystore-nonce";
const IMG_NONCE_DOMAIN: &[u8] = b"aletheia.capvault/v1/image-nonce";
const IMG_AAD_DOMAIN: &[u8] = b"aletheia.capvault/v1/image-aad";

/// Why the vault refused. Every variant is a WHOLE-OBJECT refusal: there is no partial load of a
/// keystore and no partially-admitted capability image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultError {
    /// The sealed capability image does not exist yet (nothing was ever saved).
    Absent,
    /// An encoded object could not fit one atomic filesystem transaction.
    TooLarge,
    /// The namespace or device refused access.
    Fs(FsError),
    /// The object ends inside its declared structure.
    Truncated,
    /// Not an object this module wrote.
    BadMagic,
    /// A format version this code does not implement.
    BadVersion,
    /// Authenticated but structurally impossible (unsorted or duplicate versions, zero entries,
    /// trailing bytes).
    Malformed,
    /// Custody handed the vault something other than a 32-byte root.
    RootLength(usize),
    /// The keystore did not authenticate under the root-derived sealing key: the wrong root or
    /// a tampered object. Nothing is loaded.
    KeystoreAuth,
    /// The image did not authenticate under ITS OWN named key version.
    ImageAuth(u32),
    /// The image names a key version the keystore no longer retains - retired under the one-way
    /// rotation chain, its key bytes destroyed.
    RetiredVersion(u32),
    /// The image names a version NEWER than the keystore knows: the keystore was rolled back
    /// alone against a newer image.
    FutureVersion { requested: u32, newest: u32 },
    /// The counter space for this key version is exhausted. Rotate; never wrap.
    Exhausted,
    /// The image authenticated and opened, then the INNER capability admission refused. The
    /// capstore refusal is preserved by name - the seal must not launder it into something vague.
    Image(CapStoreError),
}

impl From<FsError> for VaultError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::NotFound => VaultError::Absent,
            FsError::TooLarge => VaultError::TooLarge,
            other => VaultError::Fs(other),
        }
    }
}

/// One versioned data key as the keystore retains it. next_counter is this version RESERVED
/// high-water mark: every sealed image names a counter strictly below it, and gaps are the
/// crash-safety margin (a reserved-but-unused counter is wasted, never reused).
#[derive(Clone, Copy, Debug)]
pub struct KeyEntry {
    pub version: u32,
    pub key: [u8; 32],
    pub next_counter: u64,
}

fn sealing_key(root: &[u8; 32]) -> [u8; 32] {
    hmac_sha256(root, KS_SEAL_DOMAIN)
}

fn derive_data_key(ks_key: &[u8; 32], version: u32) -> [u8; 32] {
    let mut msg = Vec::with_capacity(DATA_KEY_DOMAIN.len() + 5);
    msg.extend_from_slice(DATA_KEY_DOMAIN);
    msg.extend_from_slice(&version.to_le_bytes());
    hmac_sha256(ks_key, &msg)
}

fn rotate_key(current: &[u8; 32]) -> [u8; 32] {
    hmac_sha256(current, ROTATE_DOMAIN)
}

/// Constructed 96-bit nonce for a key: deterministic per-key prefix || monotone counter.
fn constructed_nonce(key: &[u8; 32], domain: &[u8], counter: u64) -> [u8; 12] {
    let prefix = hmac_sha256(key, domain);
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&prefix[..4]);
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    u64::from_le_bytes(a)
}
/// The custody state for one machine: the root-derived wrapping subkey (the root itself is
/// forgotten at open), the retained data-key versions oldest-first, and the keystore object's
/// own monotone nonce counter.
pub struct CapVault {
    ks_key: [u8; 32],
    entries: Vec<KeyEntry>,
    ks_counter: u64,
}

impl CapVault {
    fn encode_keystore_plaintext(entries: &[KeyEntry]) -> Vec<u8> {
        let mut out = Vec::with_capacity(entries.len() * 44);
        for e in entries {
            out.extend_from_slice(&e.version.to_le_bytes());
            out.extend_from_slice(&e.key);
            out.extend_from_slice(&e.next_counter.to_le_bytes());
        }
        out
    }

    fn decode_keystore_plaintext(bytes: &[u8]) -> Result<Vec<KeyEntry>, VaultError> {
        let mut out: Vec<KeyEntry> = Vec::new();
        let mut at = 0usize;
        while at < bytes.len() {
            if at + 44 > bytes.len() {
                return Err(VaultError::Truncated);
            }
            let version = le_u32(&bytes[at..at + 4]);
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes[at + 4..at + 36]);
            let next_counter = le_u64(&bytes[at + 36..at + 44]);
            if next_counter == 0 {
                return Err(VaultError::Malformed);
            }
            if let Some(last) = out.last() {
                if version <= last.version {
                    return Err(VaultError::Malformed);
                }
            }
            out.push(KeyEntry {
                version,
                key,
                next_counter,
            });
            at += 44;
        }
        if out.is_empty() {
            return Err(VaultError::Malformed);
        }
        Ok(out)
    }

    /// Authenticate-then-parse the sealed keystore under `ks_key`. Authentication runs BEFORE
    /// parsing: a wrong root or tampered byte yields KeystoreAuth and NOTHING is decoded.
    fn open_sealed_keystore(
        bytes: &[u8],
        ks_key: &[u8; 32],
    ) -> Result<(Vec<KeyEntry>, u64), VaultError> {
        if bytes.len() < KS_HEADER_LEN + TAG_LEN {
            return Err(VaultError::Truncated);
        }
        if bytes[0..4] != *KS_MAGIC {
            return Err(VaultError::BadMagic);
        }
        if le_u32(&bytes[4..8]) != FORMAT_VERSION {
            return Err(VaultError::BadVersion);
        }
        let ks_counter = le_u64(&bytes[8..16]);
        let n_entries = le_u32(&bytes[16..20]) as usize;
        if n_entries == 0 {
            return Err(VaultError::Malformed);
        }
        let mut aad = Vec::with_capacity(KS_SEAL_DOMAIN.len() + 8);
        aad.extend_from_slice(KS_SEAL_DOMAIN);
        aad.extend_from_slice(&ks_counter.to_le_bytes());
        let nonce = constructed_nonce(ks_key, KS_NONCE_DOMAIN, ks_counter);
        let pt = aead_open(ks_key, &nonce, &aad, &bytes[KS_HEADER_LEN..]).map_err(|e| match e {
            AeadError::Truncated => VaultError::Truncated,
            AeadError::Authenticate => VaultError::KeystoreAuth,
        })?;
        let entries = Self::decode_keystore_plaintext(&pt)?;
        if entries.len() != n_entries {
            return Err(VaultError::Malformed);
        }
        Ok((entries, ks_counter))
    }

    fn image_aad(version: u32, key_version: u32, counter: u64, pt_len: usize) -> Vec<u8> {
        let mut aad = Vec::with_capacity(IMG_AAD_DOMAIN.len() + 20);
        aad.extend_from_slice(IMG_AAD_DOMAIN);
        aad.extend_from_slice(&version.to_le_bytes());
        aad.extend_from_slice(&key_version.to_le_bytes());
        aad.extend_from_slice(&counter.to_le_bytes());
        aad.extend_from_slice(&(pt_len as u32).to_le_bytes());
        aad
    }

    /// Load the keystore, or create it at version 1 on first boot. The root is validated and
    /// consumed: only its derived wrapping subkey is retained.
    pub fn open<D: BlockDevice>(
        fs: &mut Filesystem,
        dev: &mut D,
        root: &[u8],
    ) -> Result<Self, VaultError> {
        let root: [u8; 32] = root
            .try_into()
            .map_err(|_| VaultError::RootLength(root.len()))?;
        let ks_key = sealing_key(&root);
        match fs.read(dev, KEYSTORE_OBJECT) {
            Ok(bytes) => {
                let (entries, ks_counter) = Self::open_sealed_keystore(&bytes, &ks_key)?;
                Ok(CapVault {
                    ks_key,
                    entries,
                    ks_counter,
                })
            }
            Err(e) => {
                if !matches!(e, FsError::NotFound) {
                    return Err(VaultError::from(e));
                }
                let entries = vec![KeyEntry {
                    version: 1,
                    key: derive_data_key(&ks_key, 1),
                    next_counter: 1,
                }];
                let mut v = CapVault {
                    ks_key,
                    entries,
                    ks_counter: 0,
                };
                v.write_keystore(fs, dev)?;
                Ok(v)
            }
        }
    }

    /// Atomically replace the sealed keystore, advancing its own nonce counter. The counter is
    /// INSIDE the object being rewritten, so each commit strictly advances it and no external
    /// ordering rule is needed.
    fn write_keystore<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem,
        dev: &mut D,
    ) -> Result<(), VaultError> {
        self.ks_counter = self
            .ks_counter
            .checked_add(1)
            .ok_or(VaultError::Exhausted)?;
        let pt = Self::encode_keystore_plaintext(&self.entries);
        let mut aad = Vec::with_capacity(KS_SEAL_DOMAIN.len() + 8);
        aad.extend_from_slice(KS_SEAL_DOMAIN);
        aad.extend_from_slice(&self.ks_counter.to_le_bytes());
        let nonce = constructed_nonce(&self.ks_key, KS_NONCE_DOMAIN, self.ks_counter);
        let ct = aead_seal(&self.ks_key, &nonce, &aad, &pt);
        let mut out = Vec::with_capacity(KS_HEADER_LEN + ct.len());
        out.extend_from_slice(KS_MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.ks_counter.to_le_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&ct);
        fs.replace(dev, KEYSTORE_OBJECT, &out)?;
        Ok(())
    }

    /// The newest key version: writes always use it, older ones only read.
    pub fn active_version(&self) -> u32 {
        self.entries.last().map(|e| e.version).unwrap_or(1)
    }

    /// Every retained version, oldest first.
    pub fn retained_versions(&self) -> Vec<u32> {
        self.entries.iter().map(|e| e.version).collect()
    }

    /// The keystore object's own constructed-nonce high-water mark (proof surface).
    pub fn keystore_nonce_counter(&self) -> u64 {
        self.ks_counter
    }

    /// Seal the engine under the ACTIVE version. TWO atomic commits, in THIS order:
    ///
    /// 1. RESERVE the next image counter in the keystore and commit it.
    /// 2. Seal and replace the image naming that counter.
    ///
    /// Reserve-first is what makes nonce reuse impossible BY CONSTRUCTION across crashes: a crash
    /// between the commits wastes a number, while write-first could replay one after a crash and
    /// reuse a nonce under the same key - the one failure AEAD cannot survive.
    pub fn save_sealed<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem,
        dev: &mut D,
        engine: &CapEngine,
    ) -> Result<(), VaultError> {
        let (version, key, counter) = {
            let entry = match self.entries.last_mut() {
                Some(e) => e,
                None => return Err(VaultError::Malformed),
            };
            if entry.next_counter == u64::MAX {
                return Err(VaultError::Exhausted);
            }
            let counter = entry.next_counter;
            entry.next_counter = counter + 1;
            (entry.version, entry.key, counter)
        };
        self.write_keystore(fs, dev)?;
        let pt = capstore::save(engine);
        let aad = Self::image_aad(FORMAT_VERSION, version, counter, pt.len());
        let nonce = constructed_nonce(&key, IMG_NONCE_DOMAIN, counter);
        let ct = aead_seal(&key, &nonce, &aad, &pt);
        let mut out = Vec::with_capacity(IMG_HEADER_LEN + ct.len());
        out.extend_from_slice(IMG_MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&counter.to_le_bytes());
        out.extend_from_slice(&(pt.len() as u32).to_le_bytes());
        out.extend_from_slice(&ct);
        fs.replace(dev, VAULT_OBJECT, &out)?;
        Ok(())
    }

    /// Load and admit the sealed capability image under clock `now`. The version NAMED in the
    /// cleartext header resolves against the keystore FIRST (retired and future versions are
    /// refused by name before any bytes are decrypted), authentication precedes parsing, and the
    /// inner admission refusals are preserved through the seal.
    pub fn load_sealed<D: BlockDevice>(
        &self,
        fs: &Filesystem,
        dev: &D,
        now: u64,
    ) -> Result<CapEngine, VaultError> {
        let stored = fs.read(dev, VAULT_OBJECT)?;
        self.admit_image_bytes(&stored, now)
    }

    /// Parse-and-admit an ALREADY-READ image object: everything load_sealed does after the
    /// namespace read. Module-private so the in-kernel suite can sweep corrupted byte slices
    /// without a filesystem rewrite per flip (the boot heap never frees).
    fn admit_image_bytes(&self, stored: &[u8], now: u64) -> Result<CapEngine, VaultError> {
        if stored.len() < IMG_HEADER_LEN + TAG_LEN {
            return Err(VaultError::Truncated);
        }
        if stored[0..4] != *IMG_MAGIC {
            return Err(VaultError::BadMagic);
        }
        if le_u32(&stored[4..8]) != FORMAT_VERSION {
            return Err(VaultError::BadVersion);
        }
        let key_version = le_u32(&stored[8..12]);
        let counter = le_u64(&stored[12..20]);
        let pt_len = le_u32(&stored[20..24]) as usize;
        let entry = match self.entries.iter().find(|e| e.version == key_version) {
            Some(e) => e,
            None => {
                let newest = self.active_version();
                return Err(if key_version > newest {
                    VaultError::FutureVersion {
                        requested: key_version,
                        newest,
                    }
                } else {
                    VaultError::RetiredVersion(key_version)
                });
            }
        };
        if stored.len() != IMG_HEADER_LEN + pt_len + TAG_LEN {
            return Err(VaultError::Malformed);
        }
        let aad = Self::image_aad(FORMAT_VERSION, key_version, counter, pt_len);
        let nonce = constructed_nonce(&entry.key, IMG_NONCE_DOMAIN, counter);
        let pt = aead_open(&entry.key, &nonce, &aad, &stored[IMG_HEADER_LEN..]).map_err(
            |e| match e {
                AeadError::Truncated => VaultError::Truncated,
                AeadError::Authenticate => VaultError::ImageAuth(key_version),
            },
        )?;
        capstore::load(&pt, now).map_err(VaultError::Image)
    }

    /// Mint version max+1 from the one-way rotation chain and retain the previous version so
    /// images still naming it keep opening until a rekey retires them.
    pub fn rotate<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem,
        dev: &mut D,
    ) -> Result<u32, VaultError> {
        let current = match self.entries.last() {
            Some(e) => e.key,
            None => return Err(VaultError::Malformed),
        };
        let next_version = self
            .active_version()
            .checked_add(1)
            .ok_or(VaultError::Exhausted)?;
        self.entries.push(KeyEntry {
            version: next_version,
            key: rotate_key(&current),
            next_counter: 1,
        });
        self.write_keystore(fs, dev)?;
        Ok(next_version)
    }

    /// The full pivot: rotate, rewrite the image under the NEWEST version, retire everything
    /// older. Three atomic commits; every crash position leaves some complete keystore+image
    /// pair openable, and afterwards old copies of the image are refused BY NAME.
    pub fn rekey_image<D: BlockDevice>(
        &mut self,
        fs: &mut Filesystem,
        dev: &mut D,
        engine: &CapEngine,
    ) -> Result<u32, VaultError> {
        self.rotate(fs, dev)?;
        self.save_sealed(fs, dev, engine)?;
        let newest = self.active_version();
        self.entries.retain(|e| e.version == newest);
        self.write_keystore(fs, dev)?;
        Ok(newest)
    }
}

// ---------------------------------------------------------------------------
// Proof surfaces
// ---------------------------------------------------------------------------

#[doc(hidden)]
impl CapVault {
    /// The retained data key for a version. Proof surface only: tests seal hand-forged inner
    /// images under it whose admission must STILL be refused through the vault.
    pub fn key_for_test(&self, version: u32) -> Option<[u8; 32]> {
        self.entries
            .iter()
            .find(|e| e.version == version)
            .map(|e| e.key)
    }

    /// Assemble a vault from exact parts, so exhaustion proofs can pin the counter at its
    /// boundary without driving a machine through u64::MAX saves.
    pub fn from_parts_for_test(ks_key: [u8; 32], entries: Vec<KeyEntry>, ks_counter: u64) -> Self {
        CapVault {
            ks_key,
            entries,
            ks_counter,
        }
    }

    /// Admit an ALREADY-READ image object through the full version-resolution and
    /// authentication path. Proof surface only: the custody-delivery suite holds pre-rekey
    /// image BYTES and must show the retired version is refused BY NAME without a filesystem
    /// rewrite per probe.
    #[doc(hidden)]
    pub fn admit_image_bytes_for_test(
        &self,
        stored: &[u8],
        now: u64,
    ) -> Result<crate::spine::CapEngine, VaultError> {
        self.admit_image_bytes(stored, now)
    }

    /// Seal arbitrary plaintext as if it were the vault's own image under `version` — the seam
    /// layering proofs use to hand the admission path a WELL-SEALED forgery.
    pub fn seal_image_bytes_for_test(
        &self,
        version: u32,
        counter: u64,
        plaintext: &[u8],
    ) -> Vec<u8> {
        let key = self.key_for_test(version).expect("version retained");
        let aad = CapVault::image_aad(FORMAT_VERSION, version, counter, plaintext.len());
        let nonce = constructed_nonce(&key, IMG_NONCE_DOMAIN, counter);
        let ct = aead_seal(&key, &nonce, &aad, plaintext);
        let mut out = Vec::with_capacity(IMG_HEADER_LEN + ct.len());
        out.extend_from_slice(IMG_MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&counter.to_le_bytes());
        out.extend_from_slice(&(plaintext.len() as u32).to_le_bytes());
        out.extend_from_slice(&ct);
        out
    }
}
