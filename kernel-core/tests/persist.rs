//! Host property proofs for the durable spine store (REQ-STOR-003, ADR-038).
//!
//! The boot suite proves the named behaviors on every target. This file proves what a boot log cannot:
//! that **every byte** of the record is load-bearing (flip any one and either the load is refused or
//! the data is still exactly right — never silently different), that a save is atomic at every crash
//! prefix, and that a round-trip preserves the store over a wide, deterministic population rather than
//! two hand-picked entities.
extern crate alloc;

use kernel_core::fs::{FaultDevice, Filesystem, FILE_DATA_START};
use kernel_core::persist::{
    decode, encode, load, open_and_witness, save, PersistError, STORE_OBJECT,
};
use kernel_core::spine::{EntityType, Store};
use kernel_core::storage::{BlockDevice, MemBlockDevice, BLOCK_SIZE};

fn fresh(blocks: usize) -> MemBlockDevice {
    let mut dev = MemBlockDevice::new(FILE_DATA_START + blocks);
    Filesystem::format(&mut dev).expect("format");
    dev
}

/// A store with a deterministic, varied population: every entity type, empty and long contents.
fn populated(n: usize) -> Store {
    let mut store = Store::new();
    let types = [
        EntityType::Document,
        EntityType::Summary,
        EntityType::Agent,
        EntityType::Capability,
        EntityType::Event,
    ];
    for i in 0..n {
        let content = match i % 4 {
            0 => alloc::string::String::new(),
            1 => alloc::format!("entity-{i}"),
            2 => "x".repeat(200),
            _ => alloc::format!("{}::{}", i, "unicode ✓ содержимое"),
        };
        store.put(
            types[i % types.len()],
            &content,
            &alloc::format!("prov-{i}"),
        );
    }
    store
}

#[test]
fn a_round_trip_preserves_every_entity_exactly() {
    let store = populated(40);
    let decoded = decode(&encode(&store)).expect("decodes");
    let before: alloc::vec::Vec<_> = store.entities().collect();
    let after: alloc::vec::Vec<_> = decoded.entities().collect();
    assert_eq!(before.len(), after.len());
    for (a, b) in before.iter().zip(after.iter()) {
        assert_eq!(a.id, b.id);
        assert_eq!(a.content, b.content);
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(a.version, b.version);
        assert_eq!(a.chain, b.chain);
        assert_eq!(a.provenance, b.provenance);
    }
    assert!(
        decoded.next_id() >= store.next_id(),
        "a restored store must never reissue an id"
    );
}

#[test]
fn every_byte_of_the_record_is_load_bearing() {
    // Flip one bit in each byte in turn: the load must either refuse, or produce a store that is still
    // exactly right. What must NEVER happen is a load that succeeds with DIFFERENT data — that is
    // silent corruption, the thing the content address exists to prevent.
    let store = populated(6);
    let good = encode(&store);
    let expected: alloc::vec::Vec<(u64, alloc::string::String)> = store
        .entities()
        .map(|e| (e.id, e.content.clone()))
        .collect();

    let mut refused = 0usize;
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        match decode(&bad) {
            Err(_) => refused += 1,
            Ok(s) => {
                let got: alloc::vec::Vec<(u64, alloc::string::String)> =
                    s.entities().map(|e| (e.id, e.content.clone())).collect();
                assert_eq!(
                    got, expected,
                    "byte {i} flipped, load SUCCEEDED, data differs — silent corruption"
                );
            }
        }
    }
    assert!(
        refused > good.len() / 2,
        "only {refused}/{} flips were refused — the record is under-checked",
        good.len()
    );
}

#[test]
fn a_save_is_atomic_at_every_crash_prefix() {
    // At every prefix of the device mutations a save performs, a later load must yield the PREVIOUS
    // store or the NEW one — never a mixture, and never nothing.
    let mut dev = fresh(96);
    let mut fs = Filesystem::mount(&mut dev).expect("mount");
    let first = populated(4);
    save(&mut fs, &mut dev, &first).expect("initial save");
    let second = populated(9);
    let snap = dev.snapshot();

    let old_blocks = encode(&first).len().div_ceil(BLOCK_SIZE);
    let new_blocks = encode(&second).len().div_ceil(BLOCK_SIZE);
    let total = 2 * (old_blocks + new_blocks + 2) + 4;
    for allow in 0..=total {
        dev.restore(&snap);
        let mut fs_run = Filesystem::mount(&mut dev).expect("remount");
        {
            let mut faulty = FaultDevice::new(&mut dev, allow);
            let _ = save(&mut fs_run, &mut faulty, &second);
        }
        let fs_after = Filesystem::mount(&mut dev).expect("mount after");
        let loaded = load(&fs_after, &dev).expect("a store is always loadable after a crash");
        let count = loaded.entities().count();
        assert!(
            count == first.entities().count() || count == second.entities().count(),
            "allow={allow}: loaded {count} entities — neither the old store nor the new one"
        );
    }
}

#[test]
fn a_corrupt_store_is_refused_rather_than_silently_replaced() {
    // "Your data is damaged" must not become "your data is gone": open_and_witness refuses.
    let mut dev = fresh(64);
    let (boot, _) = open_and_witness(&mut dev).expect("first boot");
    assert_eq!(boot, 1);
    let entry = {
        let fs = Filesystem::mount(&mut dev).expect("mount");
        fs.stat(&dev, STORE_OBJECT).expect("store object exists")
    };
    // Clear the journal's commit record first. With a committed transaction still pending, a mount
    // REPLAYS it and repairs the home block — correct behavior, and it would mask the check under test.
    // What is being modelled here is rot on a quiesced medium: nothing pending, and a byte has changed.
    dev.write_block(0, &[0u8; BLOCK_SIZE])
        .expect("clear journal record");
    let mut blk = [0u8; BLOCK_SIZE];
    dev.read_block(entry.start, &mut blk).expect("read");
    blk[40] ^= 0xFF; // the first entity's type byte, inside its metadata
    dev.write_block(entry.start, &blk).expect("write");
    let err = open_and_witness(&mut dev).expect_err("a damaged store must be refused");
    assert!(
        matches!(
            err,
            PersistError::ContentHashMismatch
                | PersistError::Truncated
                | PersistError::BadFormat
                | PersistError::UnknownEntityType
                | PersistError::NotUtf8
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn the_boot_number_increases_by_exactly_one_per_open() {
    let mut dev = fresh(96);
    for expected in 1..=6u64 {
        let (boot, verified) = open_and_witness(&mut dev).expect("open");
        assert_eq!(boot, expected, "the boot counter skipped or repeated");
        assert_eq!(
            verified as u64,
            expected - 1,
            "each boot must verify exactly the entities the previous boots wrote"
        );
    }
}
