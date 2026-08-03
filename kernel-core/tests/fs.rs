//! Host property proofs for the filesystem namespace (REQ-FS-001, ADR-035).
//!
//! The VM suite (`kernel_core::fs::selftest_on`) proves the named behaviors on every CPU target and,
//! on aarch64, over the real virtio-blk driver. This file proves what a boot log cannot: the crash
//! sweep at **every** prefix of a mutation, and a deterministic op campaign that re-checks the
//! whole-namespace structural invariants after every single operation.
extern crate alloc;

use kernel_core::fs::{
    DirEntry, FaultDevice, Filesystem, FsError, FILE_DATA_START, MAX_FILES, MAX_FILE_BYTES,
};
use kernel_core::storage::{BlockDevice, MemBlockDevice, BLOCK_SIZE};

/// A formatted device with `blocks` data blocks.
fn fresh(blocks: usize) -> MemBlockDevice {
    let mut dev = MemBlockDevice::new(FILE_DATA_START + blocks);
    Filesystem::format(&mut dev).expect("format");
    dev
}

fn body(seed: u8, len: usize) -> alloc::vec::Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// Every structural invariant of a mounted namespace, checked from the device alone.
fn assert_structurally_sound(dev: &mut MemBlockDevice) -> alloc::vec::Vec<DirEntry> {
    let fs = Filesystem::mount(dev).expect("mount");
    let entries = fs.list(dev).expect("list");

    for (i, a) in entries.iter().enumerate() {
        for b in entries.iter().skip(i + 1) {
            assert_ne!(a.name, b.name, "two entries share a name");
        }
    }
    for (i, a) in entries.iter().enumerate() {
        assert!(a.start >= FILE_DATA_START, "extent below the data area");
        assert!(
            a.start + a.blocks() <= dev.num_blocks(),
            "extent off the device"
        );
        for b in entries.iter().skip(i + 1) {
            let disjoint = a.start + a.blocks() <= b.start || b.start + b.blocks() <= a.start;
            assert!(disjoint, "two objects share a data block");
        }
    }
    let held: usize = entries.iter().map(|e| e.blocks()).sum();
    let capacity = dev.num_blocks() - FILE_DATA_START;
    assert_eq!(
        fs.free_blocks(dev).expect("free"),
        capacity - held,
        "bitmap and directory disagree on allocation"
    );
    entries
}

#[test]
fn a_create_is_all_or_nothing_at_every_crash_point() {
    // For EVERY prefix of the device mutations a create performs, recovery must yield either the
    // namespace as it was or the namespace with the whole object — never anything between.
    for nblocks in [0usize, 1, 2, 5] {
        let mut base = fresh(64);
        let mut fs = Filesystem::mount(&mut base).expect("mount");
        fs.create(&mut base, "keep", &body(7, 100)).expect("keep");
        let pre = assert_structurally_sound(&mut base);
        let snap = base.snapshot();
        let payload = body(3, nblocks * BLOCK_SIZE);

        // journal writes + flush + record + flush + home writes + flush, plus one past the end so the
        // "no fault at all" case is swept too.
        let total = 2 * (nblocks + 2) + 3;
        for allow in 0..=total {
            base.restore(&snap);
            let mut fs_run = Filesystem::mount(&mut base).expect("remount");
            {
                let mut faulty = FaultDevice::new(&mut base, allow);
                let _ = fs_run.create(&mut faulty, "torn", &payload);
            }
            let after = assert_structurally_sound(&mut base);
            let fs_after = Filesystem::mount(&mut base).expect("mount after");
            match after.len() {
                1 => assert_eq!(
                    after, pre,
                    "rolled back to a DIFFERENT namespace (allow={allow})"
                ),
                2 => {
                    let got = fs_after.read(&mut base, "torn").expect("torn readable");
                    assert_eq!(got, payload, "committed a TORN body (allow={allow})");
                }
                n => panic!("crash left {n} entries (allow={allow})"),
            }
            assert_eq!(
                fs_after.read(&mut base, "keep").expect("keep survives"),
                body(7, 100),
                "an unrelated object was collateral damage (allow={allow})"
            );
        }
    }
}

#[test]
fn a_remove_is_all_or_nothing_at_every_crash_point() {
    let mut base = fresh(64);
    let mut fs = Filesystem::mount(&mut base).expect("mount");
    let doomed = body(9, 2 * BLOCK_SIZE + 5);
    fs.create(&mut base, "keep", &body(7, 100)).expect("keep");
    fs.create(&mut base, "doomed", &doomed).expect("doomed");
    let snap = base.snapshot();
    let total = 2 * (3 + 2) + 3;

    for allow in 0..=total {
        base.restore(&snap);
        let mut fs_run = Filesystem::mount(&mut base).expect("remount");
        {
            let mut faulty = FaultDevice::new(&mut base, allow);
            let _ = fs_run.remove(&mut faulty, "doomed");
        }
        let after = assert_structurally_sound(&mut base);
        let fs_after = Filesystem::mount(&mut base).expect("mount after");
        if after.iter().any(|e| e.name == "doomed") {
            // Rolled back: the object is intact, bytes and all.
            assert_eq!(
                fs_after.read(&mut base, "doomed").expect("doomed intact"),
                doomed,
                "rollback left a MUTILATED object (allow={allow})"
            );
        } else {
            assert_eq!(
                fs_after.read(&mut base, "doomed"),
                Err(FsError::NotFound),
                "half-removed (allow={allow})"
            );
        }
        assert_eq!(
            fs_after.read(&mut base, "keep").expect("keep survives"),
            body(7, 100)
        );
    }
}

#[test]
fn a_completed_remove_leaves_no_byte_of_the_object_behind() {
    let mut dev = fresh(64);
    let mut fs = Filesystem::mount(&mut dev).expect("mount");
    let secret = alloc::vec![0xC3u8; 3 * BLOCK_SIZE];
    fs.create(&mut dev, "secret", &secret).expect("create");
    let e = fs.stat(&mut dev, "secret").expect("stat");
    fs.remove(&mut dev, "secret").expect("remove");
    for i in 0..e.blocks() {
        let mut blk = [0u8; BLOCK_SIZE];
        dev.read_block(e.start + i, &mut blk).expect("read");
        assert!(
            blk.iter().all(|&b| b == 0),
            "a freed block still carries the object's bytes"
        );
    }
}

#[test]
fn every_refusal_is_a_no_op() {
    let mut dev = fresh(8);
    let mut fs = Filesystem::mount(&mut dev).expect("mount");
    fs.create(&mut dev, "one", &body(1, 10)).expect("one");
    let before = assert_structurally_sound(&mut dev);

    let long = "x".repeat(41);
    let refusals: alloc::vec::Vec<(&str, FsError)> = alloc::vec![
        ("one", FsError::Exists),
        ("", FsError::BadName),
        ("a/b", FsError::BadName),
        (long.as_str(), FsError::BadName),
    ];
    for (name, want) in refusals {
        assert_eq!(
            fs.create(&mut dev, name, b"data"),
            Err(want),
            "name={name:?}"
        );
        assert_eq!(
            assert_structurally_sound(&mut dev),
            before,
            "refusal mutated the namespace"
        );
    }
    assert_eq!(
        fs.create(&mut dev, "huge", &alloc::vec![0u8; MAX_FILE_BYTES + 1]),
        Err(FsError::TooLarge)
    );
    assert_eq!(
        fs.create(&mut dev, "wide", &alloc::vec![0u8; 8 * BLOCK_SIZE]),
        Err(FsError::NoSpace)
    );
    assert_eq!(fs.remove(&mut dev, "absent"), Err(FsError::NotFound));
    assert_eq!(assert_structurally_sound(&mut dev), before);
}

#[test]
fn the_directory_fills_up_and_refuses_rather_than_overwriting() {
    let mut dev = fresh(MAX_FILES + 4);
    let mut fs = Filesystem::mount(&mut dev).expect("mount");
    for i in 0..MAX_FILES {
        let name = alloc::format!("f{i}");
        fs.create(&mut dev, &name, &body(i as u8, 1))
            .expect("create");
    }
    assert_eq!(fs.list(&mut dev).expect("list").len(), MAX_FILES);
    assert_eq!(fs.create(&mut dev, "one-more", b"x"), Err(FsError::NoSpace));
    for i in 0..MAX_FILES {
        let name = alloc::format!("f{i}");
        assert_eq!(fs.read(&mut dev, &name).expect("read"), body(i as u8, 1));
    }
    assert_structurally_sound(&mut dev);
}

#[test]
fn a_deterministic_op_campaign_never_breaks_a_structural_invariant() {
    // 4 000 create/remove/read operations against a model of what should be there. The structural
    // invariants are re-checked from the device after EVERY op, so the first divergence is caught at
    // the op that caused it rather than at the end.
    let mut dev = fresh(96);
    let mut fs = Filesystem::mount(&mut dev).expect("mount");
    let mut model: alloc::vec::Vec<(alloc::string::String, alloc::vec::Vec<u8>)> = alloc::vec![];
    let mut rng: u64 = 0x243F_6A88_85A3_08D3;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for step in 0..4000u32 {
        let r = next();
        let name = alloc::format!("obj{}", r % 24);
        match r % 10 {
            0..=4 => {
                let len = ((r >> 8) % 5) as usize * BLOCK_SIZE + ((r >> 16) % 900) as usize;
                let data = body((r >> 24) as u8, len);
                match fs.create(&mut dev, &name, &data) {
                    Ok(()) => {
                        assert!(
                            !model.iter().any(|(n, _)| *n == name),
                            "step {step}: created a duplicate the model already had"
                        );
                        model.push((name.clone(), data));
                    }
                    Err(FsError::Exists) => assert!(
                        model.iter().any(|(n, _)| *n == name),
                        "step {step}: refused a name the model does not have"
                    ),
                    Err(FsError::NoSpace) => {} // legitimate: fragmentation or a full directory
                    Err(e) => panic!("step {step}: unexpected create error {e:?}"),
                }
            }
            5..=7 => match fs.remove(&mut dev, &name) {
                Ok(()) => {
                    let had = model.iter().position(|(n, _)| *n == name);
                    assert!(had.is_some(), "step {step}: removed a name the model lacks");
                    model.remove(had.expect("checked"));
                }
                Err(FsError::NotFound) => assert!(
                    !model.iter().any(|(n, _)| *n == name),
                    "step {step}: NotFound for a name the model has"
                ),
                Err(e) => panic!("step {step}: unexpected remove error {e:?}"),
            },
            _ => {
                let got = fs.read(&mut dev, &name);
                match model.iter().find(|(n, _)| *n == name) {
                    Some((_, want)) => assert_eq!(
                        got.as_deref(),
                        Ok(&want[..]),
                        "step {step}: contents diverged from the model"
                    ),
                    None => assert_eq!(got, Err(FsError::NotFound), "step {step}"),
                }
            }
        }

        let entries = assert_structurally_sound(&mut dev);
        assert_eq!(
            entries.len(),
            model.len(),
            "step {step}: entry count diverged from the model"
        );
    }

    // Everything the model still holds survives a final remount, byte for byte.
    let fs2 = Filesystem::mount(&mut dev).expect("final mount");
    for (name, want) in &model {
        assert_eq!(fs2.read(&mut dev, name).expect("read"), *want);
    }
}
