//! Capability lifetime across a reboot, attacked rather than exercised (REQ-CAP-008, ADR-048,
//! `docs/INVARIANT-CONTRACTS.md` §INV-CAP-LIFE).
//!
//! A persisted registry is untrusted input, so almost every test here is a REFUSAL. The forgeries
//! are assembled through `capstore::encode_for_test`, the same encoder `save` uses, so each one is
//! a well-formed image whose only defect is the one under test — a forgery refused for being
//! malformed would prove nothing about the check it was aimed at.

use kernel_core::capstore::*;
use kernel_core::fs::Filesystem;
use kernel_core::spine::{CapEngine, Constraints, Decision, EntityType, Scope, Target};
use kernel_core::storage::MemBlockDevice;

fn engine() -> (
    CapEngine,
    kernel_core::spine::CapToken,
    kernel_core::spine::CapToken,
) {
    let mut e = CapEngine::new(0xC0FFEE, 1000);
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
    (e, root, child)
}

/// `CapEngine` is deliberately neither `Debug` nor `PartialEq` — an engine that could be compared
/// or printed wholesale is an engine whose private registry is observable. So a refusal is asserted
/// on the error, not on the `Result`.
fn refused(bytes: &[u8], now: u64) -> Option<CapStoreError> {
    load(bytes, now).err()
}

fn doc() -> Target {
    Target {
        id: None,
        etype: Some(EntityType::Document),
    }
}

#[test]
fn capability_image_round_trips_through_atomic_filesystem_object() {
    let (engine, _root, child) = engine();
    let mut disk = MemBlockDevice::new(256);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();

    save_to_fs(&mut fs, &mut disk, &engine).expect("capability image saves");
    let mounted = Filesystem::mount(&mut disk).unwrap();
    let back = load_from_fs(&mounted, &disk, 1000).expect("capability image loads");
    assert_eq!(
        back.evaluate("entity.derive", &doc(), &[child]),
        Decision::Allow
    );
}

#[test]
fn capability_image_corruption_on_medium_is_refused() {
    // The object at rest is an ACMP1 envelope now; damaging its magic must be refused by name.
    let (engine, _root, _child) = engine();
    let mut disk = MemBlockDevice::new(256);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    save_to_fs(&mut fs, &mut disk, &engine).unwrap();

    let mut image = fs.read(&disk, STORE_OBJECT).unwrap();
    image[0] ^= 1;
    fs.replace(&mut disk, STORE_OBJECT, &image).unwrap();
    let mounted = Filesystem::mount(&mut disk).unwrap();
    assert!(matches!(
        load_from_fs(&mounted, &disk, 1000),
        Err(CapStoreError::Compressed(_)),
    ));
}

#[test]
fn a_raw_image_written_before_compression_still_loads() {
    // Detection, not assumption: an image written by the pre-compression path — or by any older
    // kernel — is a raw record, and every reader must keep accepting it. A flipped byte in THAT
    // form is still the record checksum's catch.
    let (engine, _root, child) = engine();
    let mut disk = MemBlockDevice::new(256);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();
    fs.replace(&mut disk, STORE_OBJECT, &save(&engine)).unwrap();

    let mounted = Filesystem::mount(&mut disk).unwrap();
    let back = load_from_fs(&mounted, &disk, 1000).expect("a raw image must still load");
    assert_eq!(
        back.evaluate("entity.derive", &doc(), &[child]),
        Decision::Allow
    );

    // A flip in the MIDDLE of the raw image (magic intact, so detection passes it through) is
    // precisely the record checksum's catch; a flipped MAGIC byte would instead be named as a
    // broken container by the envelope detector above.
    let mut image = fs.read(&disk, STORE_OBJECT).unwrap();
    let mid = image.len() / 2;
    image[mid] ^= 1;
    fs.replace(&mut disk, STORE_OBJECT, &image).unwrap();
    let mounted = Filesystem::mount(&mut disk).unwrap();
    assert!(matches!(
        load_from_fs(&mounted, &disk, 1000),
        Err(CapStoreError::Checksum),
    ));

    // And a raw image whose MAGIC rotted is named as container damage, not misread as an image.
    let mut image = fs.read(&disk, STORE_OBJECT).unwrap();
    image[0] ^= 1;
    fs.replace(&mut disk, STORE_OBJECT, &image).unwrap();
    let mounted = Filesystem::mount(&mut disk).unwrap();
    assert!(matches!(
        load_from_fs(&mounted, &disk, 1000),
        Err(CapStoreError::Compressed(_)),
    ));
}

#[test]
fn authenticated_capability_image_requires_trusted_key() {
    let (engine, _root, child) = engine();
    let key = [0x42u8; 32];
    let wrong = [0x24u8; 32];
    let image = save_authenticated(&engine, &key);

    let back = load_authenticated(&image, &key, 1000).expect("trusted image loads");
    assert_eq!(
        back.evaluate("entity.derive", &doc(), &[child]),
        Decision::Allow
    );
    assert_eq!(
        load_authenticated(&image, &wrong, 1000).err(),
        Some(CapStoreError::Authentication)
    );
}

#[test]
fn authenticated_capability_tamper_is_rejected_before_image_parse() {
    let (engine, _root, _child) = engine();
    let key = [0x11u8; 32];
    let mut image = save_authenticated(&engine, &key);
    image[0] ^= 1;
    assert_eq!(
        load_authenticated(&image, &key, 1000).err(),
        Some(CapStoreError::Authentication)
    );
}

#[test]
fn authenticated_capability_image_round_trips_through_atomic_filesystem_object() {
    let (engine, _root, child) = engine();
    let key = [0xa5u8; 32];
    let mut disk = MemBlockDevice::new(256);
    Filesystem::format(&mut disk).unwrap();
    let mut fs = Filesystem::mount(&mut disk).unwrap();

    save_authenticated_to_fs(&mut fs, &mut disk, &engine, &key).unwrap();
    let mounted = Filesystem::mount(&mut disk).unwrap();
    let back = load_authenticated_from_fs(&mounted, &disk, &key, 1000).unwrap();
    assert_eq!(
        back.evaluate("entity.derive", &doc(), &[child]),
        Decision::Allow
    );
    assert_eq!(
        load_authenticated_from_fs(&mounted, &disk, &[0x5au8; 32], 1000).err(),
        Some(CapStoreError::Authentication)
    );

    // The stored form is an envelope around the authenticated image: damaging the ENVELOPE is a
    // compression-layer refusal; the MAC still guards the image inside it.
    let mut sealed = fs.read(&disk, STORE_OBJECT).unwrap();
    sealed[0] ^= 1;
    fs.replace(&mut disk, STORE_OBJECT, &sealed).unwrap();
    let mounted = Filesystem::mount(&mut disk).unwrap();
    assert!(matches!(
        load_authenticated_from_fs(&mounted, &disk, &key, 1000),
        Err(CapStoreError::Compressed(_)),
    ));
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-1 — a round trip preserves authority exactly.
// ---------------------------------------------------------------------------

#[test]
fn a_round_trip_preserves_every_authority_and_invents_none() {
    let (e, root, child) = engine();
    let back = load(&save(&e), 1000).expect("a store this engine wrote must load");
    assert_eq!(back.live_count(), e.live_count());
    for (action, target, token) in [
        ("entity.derive", doc(), child),
        ("entity.delete", Target::default(), root),
    ] {
        assert_eq!(
            back.evaluate(action, &target, &[token]),
            e.evaluate(action, &target, &[token]),
            "verdict changed across the reboot for {action}"
        );
    }
    // And it invents none: the child still cannot do what it never could.
    assert!(matches!(
        back.evaluate("entity.delete", &Target::default(), &[child]),
        Decision::Deny(_)
    ));
}

#[test]
fn saving_an_unchanged_engine_twice_produces_identical_bytes() {
    let (e, _root, _child) = engine();
    assert_eq!(save(&e), save(&e));
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-2 — revocation survives, and the cascade is re-derived.
// ---------------------------------------------------------------------------

#[test]
fn a_revoked_capability_is_still_revoked_after_the_reboot() {
    let (mut e, root, child) = engine();
    e.revoke(child);
    let back = load(&save(&e), 1000).unwrap();
    assert!(back.is_revoked(child));
    assert!(matches!(
        back.evaluate("entity.derive", &doc(), &[child]),
        Decision::Deny(_)
    ));
    // The sibling authority is undisturbed — a revocation that took the parent with it would be a
    // different bug with the same test result.
    assert_eq!(
        back.evaluate("entity.derive", &doc(), &[root]),
        Decision::Allow
    );
}

#[test]
fn the_cascade_is_recomputed_when_the_image_lists_only_its_root() {
    let mut e = CapEngine::new(0xC0FFEE, 1000);
    let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
    let kid = e
        .delegate(root, "a", "entity.derive", Scope::All, Constraints::none())
        .unwrap();
    let grandkid = e
        .delegate(kid, "b", "entity.derive", Scope::All, Constraints::none())
        .unwrap();
    e.revoke(root);

    let (epoch, secret, next_id, records, revoked) = decompose(&e);
    let root_only: Vec<u64> = records
        .iter()
        .filter(|r| r.parent.is_none() && revoked.contains(&r.id))
        .map(|r| r.id)
        .collect();
    assert_eq!(root_only.len(), 1);
    assert!(revoked.len() > 1, "the save must have carried the cascade");

    let thinned = encode_for_test(epoch, secret, next_id, &records, &root_only);
    let back = load(&thinned, 1000).unwrap();
    assert!(back.is_revoked(kid));
    assert!(back.is_revoked(grandkid));
    assert!(matches!(
        back.evaluate("entity.derive", &Target::default(), &[grandkid]),
        Decision::Deny(_)
    ));
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-3 — the clock cannot go backwards.
// ---------------------------------------------------------------------------

#[test]
fn loading_under_an_earlier_clock_is_refused() {
    let (e, _root, _child) = engine();
    let image = save(&e);
    assert_eq!(refused(&image, 999), Some(CapStoreError::ClockRewound));
    assert_eq!(refused(&image, 0), Some(CapStoreError::ClockRewound));
    assert!(
        load(&image, 1000).is_ok(),
        "the same clock is not backwards"
    );
    assert!(load(&image, 10_000).is_ok());
}

#[test]
fn an_expiry_that_has_passed_stays_passed_across_the_reboot() {
    let mut e = CapEngine::new(0xC0FFEE, 1000);
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
    let image = save(&e);
    // Before the expiry it is live; after it, dead — under the RESTORED clock, which is the whole
    // point: a boot that reset the clock to zero would have made the second case live too.
    assert_eq!(
        load(&image, 1200)
            .unwrap()
            .evaluate("entity.derive", &Target::default(), &[cap]),
        Decision::Allow
    );
    assert!(matches!(
        load(&image, 2000)
            .unwrap()
            .evaluate("entity.derive", &Target::default(), &[cap]),
        Decision::Deny(_)
    ));
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-4 — a widened, orphaned or circular registry is refused whole.
// ---------------------------------------------------------------------------

#[test]
fn a_child_widened_in_the_image_is_refused() {
    // A root that is narrow in every dimension, so widening the child in ANY of them is genuinely
    // an amplification. Against an `All`/`*` root it would not be, and the test would pass for the
    // wrong reason — which is what the first version of it did.
    let mut e = CapEngine::new(0xC0FFEE, 1000);
    let root = e.mint(
        "user",
        "entity.derive.*",
        Scope::Entities(vec![1, 2]),
        Constraints::none(),
    );
    e.delegate(
        root,
        "agent",
        "entity.derive.summary",
        Scope::Entities(vec![1]),
        Constraints::none(),
    )
    .unwrap();
    let (epoch, secret, next_id, records, revoked) = decompose(&e);

    // Each dimension, separately, so the load is proved to re-check all three rather than one.
    for widen in 0..3 {
        let mut r = records.clone();
        let target = r.iter_mut().find(|r| r.parent.is_some()).unwrap();
        match widen {
            0 => target.action = "*".to_string(),
            1 => target.scope = Scope::All,
            _ => {
                // Loosen a constraint: the parent is local-only, the child claims it is not.
                target.constraints = Constraints {
                    expires_at: None,
                    approval_required: false,
                    local_only: false,
                }
            }
        }
        let forged = encode_for_test(epoch, secret, next_id, &r, &revoked);
        assert_eq!(
            refused(&forged, 1000),
            Some(CapStoreError::Amplified),
            "widening dimension {widen} was accepted"
        );
    }
}

#[test]
fn an_orphan_is_refused() {
    let (e, _root, _child) = engine();
    let (epoch, secret, next_id, records, revoked) = decompose(&e);
    let orphans: Vec<Record> = records.into_iter().filter(|r| r.parent.is_some()).collect();
    assert_eq!(orphans.len(), 1);
    let forged = encode_for_test(epoch, secret, next_id, &orphans, &revoked);
    assert_eq!(refused(&forged, 1000), Some(CapStoreError::Orphan));
}

#[test]
fn a_cycle_in_the_parent_edges_is_refused() {
    let (e, _root, _child) = engine();
    let (epoch, secret, next_id, mut records, revoked) = decompose(&e);
    // Point the root at its own child: every record now has a parent, none is amplifying (they are
    // both `entity.*`-or-narrower), and a naive walk would never terminate.
    let child_id = records.iter().find(|r| r.parent.is_some()).unwrap().id;
    let root = records.iter_mut().find(|r| r.parent.is_none()).unwrap();
    root.parent = Some(child_id);
    let forged = encode_for_test(epoch, secret, next_id, &records, &revoked);
    assert_eq!(refused(&forged, 1000), Some(CapStoreError::Cycle));
}

#[test]
fn a_duplicate_id_is_refused() {
    let (e, _root, _child) = engine();
    let (epoch, secret, next_id, mut records, revoked) = decompose(&e);
    let dup = records[0].clone();
    records.push(dup);
    let forged = encode_for_test(epoch, secret, next_id, &records, &revoked);
    assert_eq!(refused(&forged, 1000), Some(CapStoreError::Duplicate));
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-5 — an id must never be mintable twice.
// ---------------------------------------------------------------------------

#[test]
fn an_image_whose_counter_could_re_mint_a_stored_id_is_refused() {
    let (e, _root, _child) = engine();
    let (epoch, secret, next_id, records, revoked) = decompose(&e);
    assert!(next_id > 1);
    for rewound in 1..next_id {
        assert_eq!(
            refused(
                &encode_for_test(epoch, secret, rewound, &records, &revoked),
                1000
            ),
            Some(CapStoreError::IdReusable),
            "counter rewound to {rewound} was accepted"
        );
    }
    // A counter that has moved FORWARD is fine — ids stay unique, which is the actual property.
    assert!(load(
        &encode_for_test(epoch, secret, next_id + 100, &records, &revoked),
        1000
    )
    .is_ok());
}

/// The consequence the counter check exists to prevent, demonstrated end to end: with the counter
/// intact, a mint after the reload cannot collide with anything the image carried — including a
/// revoked id, whose reuse would hand a killed token back to whoever still holds it.
#[test]
fn a_mint_after_the_reload_never_collides_with_a_stored_id() {
    let (mut e, root, child) = engine();
    e.revoke(child);
    let image = save(&e);
    let mut back = load(&image, 1000).unwrap();
    for i in 0..500 {
        let fresh = back.mint("late", "entity.derive", Scope::All, Constraints::none());
        assert!(!back.is_revoked(fresh), "mint {i} reused a revoked id");
        assert_ne!(fresh, root);
        assert_ne!(fresh, child);
        assert_eq!(
            back.evaluate("entity.derive", &Target::default(), &[fresh]),
            Decision::Allow
        );
        // …and the revoked token is still dead, which a colliding id would have undone.
        assert!(matches!(
            back.evaluate("entity.derive", &doc(), &[child]),
            Decision::Deny(_)
        ));
    }
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-6 — corruption and truncation are refused, exhaustively.
// ---------------------------------------------------------------------------

#[test]
fn every_single_bit_flip_of_the_image_is_refused() {
    let (e, _root, _child) = engine();
    let image = save(&e);
    for i in 0..image.len() {
        for bit in 0..8 {
            let mut bad = image.clone();
            bad[i] ^= 1 << bit;
            assert!(
                load(&bad, 1000).is_err(),
                "byte {i} bit {bit} flipped and the image still loaded — that region is outside the checksum"
            );
        }
    }
}

#[test]
fn every_truncation_of_the_image_is_refused() {
    let (e, _root, _child) = engine();
    let image = save(&e);
    for k in 0..image.len() {
        assert!(
            load(&image[..k], 1000).is_err(),
            "a {k}-byte prefix of the image loaded"
        );
    }
    assert!(load(&image, 1000).is_ok());
}

#[test]
fn trailing_bytes_are_refused() {
    let (e, _root, _child) = engine();
    let image = save(&e);
    // Append a byte INSIDE the checksummed body and re-checksum, so the image is internally
    // consistent and wrong only in that it is not exactly what it claims to be.
    let mut body = image[..image.len() - 8].to_vec();
    body.push(0);
    let sum = kernel_core::spine::content_hash(&body);
    body.extend_from_slice(&sum.to_le_bytes());
    assert_eq!(refused(&body, 1000), Some(CapStoreError::TrailingBytes));
}

#[test]
fn a_foreign_image_is_refused_by_magic_and_by_version() {
    let (e, _root, _child) = engine();
    let image = save(&e);
    let mut wrong_magic = image[..image.len() - 8].to_vec();
    wrong_magic[0] = b'X';
    let sum = kernel_core::spine::content_hash(&wrong_magic);
    wrong_magic.extend_from_slice(&sum.to_le_bytes());
    assert_eq!(refused(&wrong_magic, 1000), Some(CapStoreError::BadMagic));

    let mut wrong_version = image[..image.len() - 8].to_vec();
    wrong_version[4..8].copy_from_slice(&(VERSION + 1).to_le_bytes());
    let sum = kernel_core::spine::content_hash(&wrong_version);
    wrong_version.extend_from_slice(&sum.to_le_bytes());
    assert_eq!(
        refused(&wrong_version, 1000),
        Some(CapStoreError::BadVersion)
    );

    assert!(load(b"", 1000).is_err());
    assert!(load(b"ALCS", 1000).is_err());
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-7 — a refused load changes nothing.
// ---------------------------------------------------------------------------

#[test]
fn a_refused_load_leaves_the_running_engine_untouched() {
    let (e, root, child) = engine();
    let before = (
        e.live_count(),
        e.evaluate("entity.derive", &doc(), &[child]),
        e.evaluate("entity.delete", &Target::default(), &[root]),
    );
    let image = save(&e);
    for bad in [
        b"not a store".to_vec(),
        Vec::new(),
        image[..image.len() / 2].to_vec(),
    ] {
        assert!(load(&bad, 1000).is_err());
    }
    assert_eq!(
        (
            e.live_count(),
            e.evaluate("entity.derive", &doc(), &[child]),
            e.evaluate("entity.delete", &Target::default(), &[root])
        ),
        before
    );
    // The image itself is unchanged too — a load that mutated its input would break the next boot.
    assert_eq!(image, save(&e));
}

// ---------------------------------------------------------------------------
// INV-CAP-LIFE-8 — the lifetime survives repeated reboots, not just one.
// ---------------------------------------------------------------------------

/// Ten save/load cycles with a mint, a delegation and a revocation between each. One round trip
/// proves the encoder; the point of a lifetime model is that authority is still exactly right after
/// the tenth, and that the image has not been quietly accumulating anything.
#[test]
fn authority_is_stable_across_ten_reboots() {
    let (mut e, root, child) = engine();
    let mut killed: Vec<kernel_core::spine::CapToken> = Vec::new();
    let mut alive = vec![root, child];
    for boot in 0..10 {
        let fresh = e
            .delegate(
                root,
                "agent",
                "entity.derive",
                Scope::Type(EntityType::Document),
                Constraints::none(),
            )
            .unwrap();
        if boot % 2 == 0 {
            e.revoke(fresh);
            killed.push(fresh);
        } else {
            alive.push(fresh);
        }
        let image = save(&e);
        e = load(&image, 1000 + boot).expect("each reboot must load the previous boot's store");

        for k in &killed {
            assert!(e.is_revoked(*k), "boot {boot}: a revoked token came back");
        }
        for a in &alive {
            assert_eq!(
                e.evaluate("entity.derive", &doc(), &[*a]),
                Decision::Allow,
                "boot {boot}: a live token stopped authorizing"
            );
        }
        // Nothing accumulates: the live set is exactly what has been minted and not revoked.
        assert_eq!(e.live_count(), alive.len());
    }
}

// ---------------------------------------------------------------------------
// The in-kernel suite, run on the host — the same doctrine `tests/invariants.rs`
// applies to the spine suite: the boot gate's checks are proved without QEMU too.
// ---------------------------------------------------------------------------

#[test]
fn the_in_kernel_suite_holds_on_the_host_and_reports_every_check_once() {
    let mut reported: Vec<(u32, bool, &'static str)> = Vec::new();
    let outcome = capstore_suite(|n, passed, name| reported.push((n, passed, name)));
    let count = match outcome {
        Ok(n) => n,
        Err((idx, name)) => panic!("in-kernel capability-lifetime invariant {idx} failed: {name}"),
    };
    assert_eq!(
        reported.len() as u32,
        count,
        "every check reports exactly once"
    );
    for (i, (n, passed, _)) in reported.iter().enumerate() {
        assert_eq!(*n, i as u32 + 1, "indices must be dense and in order");
        assert!(passed);
    }
    // Pinned: the boot gates grep for this number, so a suite that silently shrank would still
    // print a green line.
    assert_eq!(count, 14);
}
