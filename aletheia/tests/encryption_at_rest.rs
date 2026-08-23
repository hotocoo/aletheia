//! Host proofs for encryption at rest as a LIFECYCLE (ADR-069). Closes ALET-P1-028
//! (key management), ALET-P1-029 (nonce lifecycle proven per object) and ALET-P1-030
//! (encrypted content-addressing identity semantics).
//!
//! Every claim here is an attack the store must survive; every refusal is checked BY NAME
//! where it matters; and the one residual — tail truncation at an exact frame boundary —
//! is proved as the documented non-claim it is, so doc and behavior cannot drift apart.
use aletheia::atrest::{AtRest, Nonce96, MAGIC};
use aletheia::crypto::{random_token, sha256_hex, Cipher};
use aletheia::domain::{new_id, now, Entity, EntityType, EventRecord, Provenance};
use aletheia::storage::Store;
use std::io::Write as _;
use std::path::{Path, PathBuf};

fn dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("aletheia-atrest-{tag}-{}", random_token()))
}

fn entity(name: &str) -> Entity {
    let id = new_id();
    Entity {
        id: id.clone(),
        etype: EntityType::Document,
        content_ref: None,
        version: 1,
        version_chain: id.clone(),
        metadata: serde_json::json!({ "name": name }),
        provenance: Provenance::of("test"),
        created_at: now(),
        updated_at: now(),
        deleted: false,
    }
}

fn event(payload: &str) -> EventRecord {
    EventRecord {
        id: new_id(),
        etype: "TestEvent".into(),
        at: now(),
        correlation_id: new_id(),
        actor: "test".into(),
        payload: serde_json::json!({ "line": payload }),
    }
}

/// Length-prefixed frame parse of a whole log image.
fn frames(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        let len = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        out.push(buf[i..i + len].to_vec());
        i += len;
    }
    out
}

fn reassemble(fs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in fs {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
        out.extend_from_slice(f);
    }
    out
}

fn log_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("store.alog")).unwrap()
}

/// Store does not implement Debug, so unwrap_err() cannot be used on its Result — this names
/// the expectation instead: an OPEN must fail, and we want the error itself.
fn open_err(dir: &Path) -> aletheia::domain::AlethError {
    match Store::open(dir) {
        Ok(_) => panic!("store opened; a refusal was required"),
        Err(e) => e,
    }
}

fn write_log_bytes(dir: &Path, bytes: &[u8]) {
    let mut f = std::fs::File::create(dir.join("store.alog")).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

/// A small but complete store: blob dedup, entities, events — enough records that
/// frame-level attacks have real structure to attack. 5 records total.
fn seed(dir: &Path) -> String {
    let mut s = Store::open(dir).unwrap();
    let h = s.put_blob(b"the fountain of truth".as_slice()).unwrap();
    s.put_entity(&entity("doc-one")).unwrap();
    for i in 0..3 {
        s.put_event(&event(&format!("ev-{i}"))).unwrap();
    }
    h
}

// ------------------------------------------------------------ identity semantics (P1-030)

#[test]
fn address_is_plaintext_sha256_and_dedup_survives_encryption() {
    let d = dir("identity");
    let mut s = Store::open(&d).unwrap();
    let content = b"semantic identity does not care about ciphertext";
    let h1 = s.put_blob(content.as_slice()).unwrap();
    let h2 = s.put_blob(content.as_slice()).unwrap(); // second put MUST dedup, not duplicate
    assert_eq!(h1, h2);
    assert_eq!(h1, sha256_hex(content)); // THE semantic fact: address = SHA-256(plaintext)
    assert_eq!(s.get_blob(&h1).unwrap(), &content.to_vec());
    // Exactly ONE frame for both puts: dedup happened above the crypto layer.
    let n = frames(&log_bytes(&d)).len();
    s.put_entity(&entity("x")).unwrap();
    assert_eq!(frames(&log_bytes(&d)).len(), n + 1);
    drop(s);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn identical_plaintexts_produce_different_frames_and_cross_store_ciphertext_differs() {
    let d = dir("equality");
    // Unit level: same plaintext at two positions -> two different frames, both open.
    let unit = d.join("unit");
    std::fs::create_dir_all(&unit).unwrap();
    let mut at = AtRest::init(unit).unwrap();
    let f0 = at.seal_frame(0, b"same").unwrap();
    let f1 = at.seal_frame(1, b"same").unwrap();
    assert_ne!(
        f0, f1,
        "nonce construction makes byte-equal frames impossible"
    );
    assert_eq!(at.open_frame(0, &f0).unwrap().plaintext, b"same");
    assert_eq!(at.open_frame(1, &f1).unwrap().plaintext, b"same");
    drop(at);
    // Store level: independent stores sealing the same record produce different bytes —
    // equality of plaintexts leaks nothing across stores.
    let (d1, d2) = (dir("eq-a"), dir("eq-b"));
    for p in [&d1, &d2] {
        std::fs::create_dir_all(p).unwrap();
        let mut s = Store::open(p).unwrap();
        s.put_event(&event("identical")).unwrap();
    }
    assert_ne!(log_bytes(&d1), log_bytes(&d2));
    for p in [&d1, &d2] {
        std::fs::remove_dir_all(p).ok();
    }
    std::fs::remove_dir_all(&d).ok();
}

// -------------------------------------------------------------- nonce lifecycle (P1-029)

#[test]
fn nonce_lifecycle_prefix_counter_never_repeats_across_reopen_cycles() {
    let d = dir("nonces");
    seed(&d);
    const CYCLES: usize = 6;
    for _ in 0..CYCLES - 1 {
        let mut s = Store::open(&d).unwrap(); // reopen: counters recover from the log
        s.put_event(&event("again")).unwrap();
    }
    let all = frames(&log_bytes(&d));
    assert_eq!(all.len(), 5 + (CYCLES - 1)); // seed records + one event per reopen cycle
    let mut seen: Vec<Nonce96> = Vec::new();
    for (seq, f) in all.iter().enumerate() {
        assert_eq!(&f[..4], &MAGIC, "frame {seq} lost the v2 magic");
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&f[5..9]);
        let mut ctr = [0u8; 8];
        ctr.copy_from_slice(&f[9..17]);
        seen.push(Nonce96 {
            prefix,
            counter: u64::from_le_bytes(ctr),
        });
    }
    // GLOBAL distinctness: no nonce ever repeats under this key version, across reopen cycles.
    let n = seen.len();
    for i in 0..n {
        for j in i + 1..n {
            assert_ne!(seen[i], seen[j], "NONCE REUSE between frames {i} and {j}");
        }
    }
    // Within one prefix the counters strictly increase in log order — recovery moved each
    // high-water mark FORWARD, never backward.
    for i in 1..n {
        if seen[i].prefix == seen[i - 1].prefix {
            assert!(
                seen[i].counter > seen[i - 1].counter,
                "counter regressed at frame {i}"
            );
        }
    }
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn nonce_exhaustion_is_a_named_refusal_not_a_wraparound() {
    let err = Nonce96::new([7; 4], u64::MAX).unwrap_err();
    assert!(err.message.contains("nonce space exhausted"), "{err}");
}

// --------------------------------------------------------- tamper evidence (P1-030 / AAD)

#[test]
fn every_single_bit_flip_of_the_whole_log_image_is_refused() {
    let d = dir("bitflip");
    seed(&d);
    let clean = log_bytes(&d);
    let mut flips = 0usize;
    for byte in 0..clean.len() {
        for bit in 0..8u8 {
            let mut corrupt = clean.clone();
            corrupt[byte] ^= 1 << bit;
            write_log_bytes(&d, &corrupt);
            assert!(
                Store::open(&d).is_err(),
                "log accepted with bit {bit} of byte {byte} flipped"
            );
            flips += 1;
        }
    }
    write_log_bytes(&d, &clean);
    let s = Store::open(&d).unwrap();
    assert_eq!(frames(&log_bytes(&d)).len(), 5);
    assert_eq!(s.events().len(), 3);
    println!(
        "every-single-bit-flip sweep: {flips} mutations over {} bytes, all refused",
        clean.len()
    );
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn transposed_deleted_and_duplicated_frames_are_refused_by_position_binding() {
    let d = dir("structure");
    seed(&d); // 5 frames: blob + entity + 3 events
    let fs = frames(&log_bytes(&d));
    assert_eq!(fs.len(), 5);

    // TRANSPOSE two adjacent frames: their sequence numbers are now wrong -> auth fails.
    let mut m = fs.clone();
    m.swap(2, 3);
    write_log_bytes(&d, &reassemble(&m));
    assert!(Store::open(&d).is_err(), "transposition was accepted");

    // DELETE a middle frame: everything after shifts down a position -> auth fails.
    let mut m = fs.clone();
    m.remove(2);
    write_log_bytes(&d, &reassemble(&m));
    assert!(Store::open(&d).is_err(), "middle deletion was accepted");

    // DUPLICATE a frame: the insertion shifts every later position -> auth fails.
    let mut m = fs.clone();
    m.insert(1, m[1].clone());
    write_log_bytes(&d, &reassemble(&m));
    assert!(Store::open(&d).is_err(), "duplication was accepted");

    // REPLAY an earlier frame into a later slot (a captured-frame resend): refused.
    let mut m = fs.clone();
    m[4] = m[2].clone();
    write_log_bytes(&d, &reassemble(&m));
    assert!(Store::open(&d).is_err(), "frame resend was accepted");

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn torn_tail_refused_but_boundary_truncation_is_the_documented_residual() {
    let d = dir("tail");
    seed(&d);
    let clean = log_bytes(&d);

    // Mid-frame truncation: corruption, refused.
    let cut = clean.len() - 5;
    write_log_bytes(&d, &clean[..cut]);
    let err = open_err(&d);
    assert!(
        err.message.contains("truncated") || err.message.contains("authentication"),
        "unexpected error: {err}"
    );

    // EXACT boundary truncation (drop only the last frame): opens, surviving prefix intact.
    // This is the DOCUMENTED non-claim — without an external anchor, "the last write never
    // happened" and "someone cut it off" are indistinguishable. Pinning the behavior here
    // pins the doc to reality in BOTH directions.
    let fs = frames(&clean);
    write_log_bytes(&d, &reassemble(&fs[..fs.len() - 1]));
    let s = Store::open(&d).unwrap();
    assert_eq!(s.events().len(), 2); // the last event's frame is gone; the rest survived
    assert!(s.get_blob(&sha256_hex(b"the fountain of truth")).is_some());
    std::fs::remove_dir_all(&d).ok();
}

// ------------------------------------------------------------------ key lifecycle (P1-028)

#[test]
fn wrong_root_key_is_a_named_refusal_not_an_empty_store() {
    let d = dir("wrongkey");
    seed(&d);
    // Replace the root key: the keystore can no longer authenticate under its derived subkey.
    std::fs::write(d.join("key"), aletheia::crypto::random_key()).unwrap();
    let err = open_err(&d);
    assert!(err.message.contains("keystore"), "unexpected error: {err}");
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn rotate_write_reopen_reads_every_version_then_rekey_collapses_to_one() {
    let d = dir("rotate");
    {
        let mut s = Store::open(&d).unwrap();
        let h = s.put_blob(b"written under the first key").unwrap();
        s.put_event(&event("v1-era")).unwrap();
        let v2 = s.rotate_encryption_keys().unwrap();
        assert_eq!(v2, 2);
        s.put_event(&event("v2-era")).unwrap();
        assert_eq!(
            s.encryption_status(),
            vec![(1, 2), (2, 1)],
            "two versions live on disk before any rekey"
        );
        assert_eq!(
            s.get_blob(&h).unwrap(),
            &b"written under the first key".to_vec()
        );
    }
    // Reopen: BOTH versions' frames authenticate; nonce counters recover per version.
    {
        let mut s = Store::open(&d).unwrap();
        assert_eq!(s.held_key_versions(), vec![1, 2]);
        assert_eq!(s.events().len(), 2);
        let retired = s.rekey_log().unwrap();
        assert_eq!(retired, 1, "exactly the old version retires");
        assert_eq!(s.encryption_status(), vec![(2, 3)]);
        assert_eq!(s.held_key_versions(), vec![2]);
    }
    // Reopen on the collapsed state: green, single version, nothing orphaned.
    let s = Store::open(&d).unwrap();
    assert_eq!(s.encryption_status(), vec![(2, 3)]);
    let lines: Vec<_> = s
        .events()
        .iter()
        .map(|e| e.payload["line"].as_str().unwrap())
        .collect();
    assert_eq!(lines, ["v1-era", "v2-era"]);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn keystore_tamper_refuses_the_whole_image_with_no_partial_load() {
    let d = dir("kstamper");
    seed(&d);
    let mut ks = std::fs::read(d.join("keystore.bin")).unwrap();
    let mid = ks.len() / 2;
    ks[mid] ^= 0x40;
    std::fs::write(d.join("keystore.bin"), ks).unwrap();
    let err = open_err(&d);
    assert!(err.message.contains("keystore"), "unexpected error: {err}");
    std::fs::remove_dir_all(&d).ok();
}

// ------------------------------------------------------------------- legacy migration

#[test]
fn pre_adr069_legacy_log_is_detected_by_authentication_and_migrated_transparently() {
    let d = dir("legacy");
    std::fs::create_dir_all(&d).unwrap();
    // Hand-write what the OLD store wrote: root key file + [len][12-byte-nonce||ct||tag]
    // frames sealed DIRECTLY under the root key — no magic, no version, no AAD.
    let root = aletheia::crypto::random_key();
    std::fs::write(d.join("key"), root).unwrap();
    let legacy_cipher = Cipher::new(&root);
    let blob_content = b"sealed by the previous generation";
    let hash = sha256_hex(blob_content);
    let recs: Vec<String> = vec![
        serde_json::to_string(&serde_json::json!({
            "Blob": { "hash": hash, "data": blob_content.to_vec() }
        }))
        .unwrap(),
        serde_json::to_string(&serde_json::json!({
            "Entity": entity("legacy-doc")
        }))
        .unwrap(),
    ];
    let mut log = Vec::new();
    for r in &recs {
        let sealed = legacy_cipher.seal(r.as_bytes());
        log.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
        log.extend_from_slice(&sealed);
    }
    write_log_bytes(&d, &log);
    assert_ne!(&log[..4], &MAGIC, "fixture must be genuinely legacy");

    // Open: detected, migrated, fully readable — through the UNCHANGED public API.
    let s = Store::open(&d).unwrap();
    assert_eq!(s.get_blob(&hash).unwrap(), &blob_content.to_vec());
    // The on-disk log is now pure v2 (magic on every frame) under data key v1.
    let migrated = frames(&log_bytes(&d));
    assert_eq!(migrated.len(), recs.len());
    for f in &migrated {
        assert_eq!(&f[..4], &MAGIC);
    }
    assert_eq!(s.encryption_status(), vec![(1, recs.len() as u64)]);
    drop(s);
    // And the migrated store reopens cleanly: steady state never sees legacy again.
    let s = Store::open(&d).unwrap();
    assert_eq!(s.get_blob(&hash).unwrap(), &blob_content.to_vec());
    std::fs::remove_dir_all(&d).ok();
}
