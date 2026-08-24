//! Authority custody, attacked rather than exercised (ALET-P1-034, ADR-070,
//! docs/INVARIANT-CONTRACTS.md section INV-CAP-CUSTODY).
//!
//! The suite inside capvault.rs proves the promises on their happy paths; this file places the
//! machine failures UNDER them:
//!
//! * an EXHAUSTIVE crash-injection sweep through every device operation of the three-commit
//!   rekey pivot - at EVERY position whatever commits landed must leave SOME complete world:
//!   a vault that opens under its root, an image that authenticates, authority identical to
//!   what was saved - never a half-pivot;
//! * the counter lifecycle at its exact boundaries (the last usable value works; MAX refuses BY
//!   NAME and stores nothing; rotation escapes exhaustion without touching the root);
//! * retirement genuinely DESTROYS: after a rekey the retired key bytes are gone from the
//!   vault, and a replayed pre-pivot image names its dead version;
//! * a refusal is a TOTAL no-op at the device level - the wrong root leaves every stored byte
//!   exactly where it was.

use kernel_core::capstore;
use kernel_core::capvault::*;
use kernel_core::faultdev::{FaultInject, Op};
use kernel_core::fs::Filesystem;
use kernel_core::spine::{CapEngine, Constraints, Decision, EntityType, Scope, Target};
use kernel_core::storage::{BlockDevice, MemBlockDevice, StorageError, BLOCK_SIZE};

const ROOT_A: [u8; 32] = [0x0A; 32];
const ROOT_B: [u8; 32] = [0x0B; 32];

fn doc() -> Target {
    Target {
        id: None,
        etype: Some(EntityType::Document),
    }
}

fn build_engine() -> (CapEngine, kernel_core::spine::CapToken) {
    let mut e = CapEngine::new(0x5EED, 1000);
    let root_cap = e.mint("user", "entity.*", Scope::All, Constraints::none());
    let child = e
        .delegate(
            root_cap,
            "agent",
            "entity.derive",
            Scope::Type(EntityType::Document),
            Constraints::none(),
        )
        .unwrap();
    (e, child)
}

fn image_counter(bytes: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&bytes[12..20]);
    u64::from_le_bytes(a)
}

/// Records the exact op-kind sequence one clean run executes, so the sweep can aim a refusal
/// of the RIGHT kind at every protocol position.
struct Recorder {
    inner: MemBlockDevice,
    log: std::cell::RefCell<Vec<Op>>,
}

impl BlockDevice for Recorder {
    fn num_blocks(&self) -> usize {
        self.inner.num_blocks()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Op::ReadOk);
        self.inner.read_block(idx, buf)
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Op::WriteOk);
        self.inner.write_block(idx, buf)
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        self.log.borrow_mut().push(Op::FlushOk);
        self.inner.flush()
    }
}

fn fail_script(seq: &[Op], at: usize) -> Vec<Op> {
    seq.iter()
        .enumerate()
        .map(|(i, op)| {
            if i == at {
                match op {
                    Op::ReadOk => Op::ReadFail,
                    Op::WriteOk => Op::WriteFail,
                    Op::FlushOk => Op::FlushFail,
                    other => *other,
                }
            } else {
                *op
            }
        })
        .collect()
}
#[test]
fn sealed_round_trip_across_reopen_cycles_keeps_authority_and_counters_monotone() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let mut last = 0u64;
    let doc_t = doc();
    for _cycle in 0..5 {
        let (engine, child) = build_engine();
        v.save_sealed(&mut fs, &mut disk, &engine).unwrap();
        let mut fs2 = Filesystem::mount(&mut disk).unwrap();
        let v2 = CapVault::open(&mut fs2, &mut disk, &ROOT_A).unwrap();
        let img = fs2.read(&disk, VAULT_OBJECT).unwrap();
        let counter = image_counter(&img);
        assert!(
            counter > last,
            "reserved counters must strictly increase across reopen cycles"
        );
        last = counter;
        assert_eq!(
            v2.load_sealed(&fs2, &disk, 1000)
                .unwrap()
                .evaluate("entity.derive", &doc_t, &[child]),
            Decision::Allow
        );
        v = v2;
        fs = fs2;
    }
}

#[test]
fn rekey_retires_by_name_and_destroys_the_retired_key() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (e1, _child1) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &e1).unwrap();
    let stale_image = fs.read(&disk, VAULT_OBJECT).unwrap();

    v.rekey_image(&mut fs, &mut disk, &e1).unwrap();
    assert_eq!(v.retained_versions(), [2]);
    assert!(
        v.key_for_test(1).is_none(),
        "retirement must destroy the retired version key inside the vault"
    );

    // A replayed pre-pivot image names its dead version - the smallest edit that would revive
    // the most authority were retirement merely a label.
    fs.replace(&mut disk, VAULT_OBJECT, &stale_image).unwrap();
    assert!(matches!(
        v.load_sealed(&fs, &disk, 1000),
        Err(VaultError::RetiredVersion(1))
    ));
}

#[test]
fn a_wrong_root_refusal_is_a_total_noop_at_the_device_level() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (engine, _child) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &engine).unwrap();

    // A stable, recovered view of the medium.
    let _ = Filesystem::mount(&mut disk).unwrap();

    let mut before = Vec::new();
    for i in 0..disk.num_blocks() {
        let mut b = [0u8; BLOCK_SIZE];
        disk.read_block(i, &mut b).unwrap();
        before.push(b);
    }

    let mut fs3 = Filesystem::mount(&mut disk).unwrap();
    assert!(matches!(
        CapVault::open(&mut fs3, &mut disk, &ROOT_B),
        Err(VaultError::KeystoreAuth)
    ));

    for (i, b0) in before.iter().enumerate() {
        let mut b = [0u8; BLOCK_SIZE];
        disk.read_block(i, &mut b).unwrap();
        assert_eq!(b, *b0, "block {i} moved during a refused open");
    }
}

#[test]
fn counter_exhaustion_is_exact_named_and_escapable_by_rotation() {
    // The boundary is EXACT: MAX-1 seals and reserves MAX; MAX refuses BY NAME, stores nothing,
    // changes nothing; and rotation - which resets the counter space under a NEW key - escapes
    // it without touching the root.
    let entries = vec![KeyEntry {
        version: 1,
        key: [0x22; 32],
        next_counter: u64::MAX,
    }];
    let mut v = CapVault::from_parts_for_test([0x11; 32], entries, 42);
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let (engine, child) = build_engine();

    assert_eq!(
        v.save_sealed(&mut fs, &mut disk, &engine),
        Err(VaultError::Exhausted)
    );
    assert!(fs.read(&disk, KEYSTORE_OBJECT).is_err());
    assert!(fs.read(&disk, VAULT_OBJECT).is_err());
    assert_eq!(v.keystore_nonce_counter(), 42);

    v.rotate(&mut fs, &mut disk).unwrap();
    v.save_sealed(&mut fs, &mut disk, &engine).unwrap();
    let img = fs.read(&disk, VAULT_OBJECT).unwrap();
    assert_eq!(u32::from_le_bytes(img[8..12].try_into().unwrap()), 2);
    assert_eq!(image_counter(&img), 1);
    assert_eq!(
        v.load_sealed(&fs, &disk, 1000)
            .unwrap()
            .evaluate("entity.derive", &doc(), &[child]),
        Decision::Allow
    );
}

/// One freshly-seeded world: a formatted disk whose vault holds ONE saved image under ROOT_A.
fn seeded() -> (MemBlockDevice, Filesystem, CapVault) {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (engine, _child) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &engine).unwrap();
    (disk, fs, v)
}

#[test]
fn a_refusal_at_every_rekey_position_leaves_a_complete_world() {
    // First learn the EXACT op-kind sequence one clean pivot executes, so every sweep position
    // aims a refusal of the right kind - a mislabeled fault silently misses (the injector treats
    // an unexpected ok-kind as pass-through).
    let seq: Vec<Op> = {
        let (disk, mut fs, mut v) = seeded();
        let (engine, _) = build_engine();
        let mut rec = Recorder {
            inner: disk,
            log: std::cell::RefCell::new(Vec::new()),
        };
        v.rekey_image(&mut fs, &mut rec, &engine).unwrap();
        let seq = rec.log.borrow().clone();
        seq
    };
    assert!(
        !seq.is_empty(),
        "the recording run must observe the pivot's device operations"
    );

    for at in 0..seq.len() {
        let (disk, mut fs, mut v) = seeded();
        let (engine, child) = build_engine();
        let script = fail_script(&seq, at);
        let f = FaultInject::new(disk, script.clone());
        let mut f = f;
        assert!(
            v.rekey_image(&mut fs, &mut f, &engine).is_err(),
            "pos {at}: the injected refusal must surface as Err"
        );
        assert_eq!(
            f.remaining(),
            seq.len() - (at + 1),
            "pos {at}: the protocol must ABORT at the refusal, consuming nothing after it"
        );
        let mut disk = f.into_inner();

        // Whatever commits landed, the reopened machine must hold SOME complete world.
        let mut fs2 = Filesystem::mount(&mut disk).unwrap();
        let v2 = CapVault::open(&mut fs2, &mut disk, &ROOT_A).unwrap();
        let loaded = v2
            .load_sealed(&fs2, &disk, 1000)
            .unwrap_or_else(|e| panic!("pos {at}: no complete world survived: {:?}", e));
        assert_eq!(
            loaded.evaluate("entity.derive", &doc(), &[child]),
            Decision::Allow,
            "pos {at}: the surviving world lost authority",
        );
        let retained = v2.retained_versions();
        assert!(
            retained == [1] || retained == [1, 2] || retained == [2],
            "pos {at}: retained set {:?} is not a promised pivot stage",
            retained
        );
    }
}
#[test]
fn every_single_bit_flip_of_either_object_is_refused_through_the_filesystem() {
    // The EXHAUSTIVE half of INV-CAP-CUSTODY-5. The boot suite samples by region because the
    // kernel heap never frees; here each flip costs a real rewrite and the full byte-by-bit
    // sweep over BOTH stored objects runs for nothing but the claim.
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (engine, _child) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &engine).unwrap();

    let pristine: [(&str, Vec<u8>); 2] = [
        (KEYSTORE_OBJECT, fs.read(&disk, KEYSTORE_OBJECT).unwrap()),
        (VAULT_OBJECT, fs.read(&disk, VAULT_OBJECT).unwrap()),
    ];

    for (object, bytes) in pristine.iter() {
        for i in 0..bytes.len() {
            for bit in 0..8u32 {
                let mut bad = bytes.clone();
                bad[i] ^= 1 << bit;
                fs.replace(&mut disk, object, &bad).unwrap();
                let opened_ok = if *object == KEYSTORE_OBJECT {
                    CapVault::open(&mut fs, &mut disk, &ROOT_A).is_ok()
                } else {
                    match CapVault::open(&mut fs, &mut disk, &ROOT_A) {
                        Ok(v) => v.load_sealed(&fs, &disk, 1000).is_ok(),
                        Err(_) => false,
                    }
                };
                assert!(
                    !opened_ok,
                    "{object} byte {i} bit {bit}: a flipped bit still opened"
                );
            }
        }
        fs.replace(&mut disk, object, bytes).unwrap();
    }
}

#[test]
fn every_truncation_of_either_object_is_refused() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (engine, _child) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &engine).unwrap();

    let pristine: [(&str, Vec<u8>); 2] = [
        (KEYSTORE_OBJECT, fs.read(&disk, KEYSTORE_OBJECT).unwrap()),
        (VAULT_OBJECT, fs.read(&disk, VAULT_OBJECT).unwrap()),
    ];

    for (object, bytes) in pristine.iter() {
        for k in 0..bytes.len() {
            fs.replace(&mut disk, object, &bytes[..k]).unwrap();
            let opened_ok = if *object == KEYSTORE_OBJECT {
                CapVault::open(&mut fs, &mut disk, &ROOT_A).is_ok()
            } else {
                match CapVault::open(&mut fs, &mut disk, &ROOT_A) {
                    Ok(v) => v.load_sealed(&fs, &disk, 1000).is_ok(),
                    Err(_) => false,
                }
            };
            assert!(!opened_ok, "{object} truncated to {k} bytes still opened");
        }
        fs.replace(&mut disk, object, bytes).unwrap();
    }
}

#[test]
fn another_stores_objects_refuse_under_ours_by_name() {
    let mut disk_a = MemBlockDevice::new(512);
    Filesystem::format(&mut disk_a).unwrap();
    let mut fs_a = Filesystem::mount(&mut disk_a).unwrap();
    let mut va = CapVault::open(&mut fs_a, &mut disk_a, &ROOT_A).unwrap();
    let (engine_a, _c) = build_engine();
    va.save_sealed(&mut fs_a, &mut disk_a, &engine_a).unwrap();

    let mut disk_b = MemBlockDevice::new(512);
    Filesystem::format(&mut disk_b).unwrap();
    let mut fs_b = Filesystem::mount(&mut disk_b).unwrap();
    let mut vb = CapVault::open(&mut fs_b, &mut disk_b, &ROOT_B).unwrap();
    let (engine_b, _cb) = build_engine();
    vb.save_sealed(&mut fs_b, &mut disk_b, &engine_b).unwrap();

    let img_b = fs_b.read(&disk_b, VAULT_OBJECT).unwrap();
    let ks_b = fs_b.read(&disk_b, KEYSTORE_OBJECT).unwrap();

    fs_a.replace(&mut disk_a, VAULT_OBJECT, &img_b).unwrap();
    let v = CapVault::open(&mut fs_a, &mut disk_a, &ROOT_A).unwrap();
    assert!(matches!(
        v.load_sealed(&fs_a, &disk_a, 1000),
        Err(VaultError::ImageAuth(1))
    ));

    fs_a.replace(&mut disk_a, KEYSTORE_OBJECT, &ks_b).unwrap();
    assert!(matches!(
        CapVault::open(&mut fs_a, &mut disk_a, &ROOT_A),
        Err(VaultError::KeystoreAuth)
    ));
}

#[test]
fn rollback_semantics_are_pinned_in_both_directions() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (e1, child1) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &e1).unwrap();
    let ks_v1 = fs.read(&disk, KEYSTORE_OBJECT).unwrap();
    let img_v1 = fs.read(&disk, VAULT_OBJECT).unwrap();

    v.rotate(&mut fs, &mut disk).unwrap();
    let (e2, _c2) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &e2).unwrap();
    fs.replace(&mut disk, KEYSTORE_OBJECT, &ks_v1).unwrap();
    let vb = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    assert!(matches!(
        vb.load_sealed(&fs, &disk, 1000),
        Err(VaultError::FutureVersion {
            requested: 2,
            newest: 1
        })
    ));

    // Consistent rollback of BOTH objects opens — the documented residual, pinned OPEN.
    fs.replace(&mut disk, VAULT_OBJECT, &img_v1).unwrap();
    let vc = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    assert_eq!(
        vc.load_sealed(&fs, &disk, 1000)
            .unwrap()
            .evaluate("entity.derive", &doc(), &[child1]),
        Decision::Allow
    );
}

#[test]
fn a_widened_registry_sealed_under_the_real_key_is_refused_through_the_vault() {
    let mut disk = MemBlockDevice::new(512);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    let mut v = CapVault::open(&mut fs, &mut disk, &ROOT_A).unwrap();
    let (seeded, _sc) = build_engine();
    v.save_sealed(&mut fs, &mut disk, &seeded).unwrap();

    let legit = build_engine().0;
    let (epoch, secret, next_id, mut records, revoked) = capstore::decompose(&legit);
    for r in records.iter_mut() {
        if r.parent.is_some() {
            r.action = "*".to_string();
            r.scope = Scope::All;
        }
    }
    let forged_pt = capstore::encode_for_test(epoch, secret, next_id, &records, &revoked);

    // Seal the forgery under the REAL retained key via the vault's own envelope format.
    let sealed_inner = v.seal_image_bytes_for_test(1, 999, &forged_pt);
    fs.replace(&mut disk, VAULT_OBJECT, &sealed_inner).unwrap();

    match v.load_sealed(&fs, &disk, 1000) {
        Err(VaultError::Image(kernel_core::capstore::CapStoreError::Amplified)) => {}
        other => panic!("expected Image(Amplified), got {:?}", other.map(|_| ())),
    }
}
