//! Zero-copy shared-memory grant-table invariants (REQ-IPC-008, ADR-020).
//!
//! These prove the *arch-independent* authority + lifecycle of a shared-memory grant on the host
//! (fast, no QEMU) — the same discipline that proves the IPC substrate in `tests/ipc.rs`. Turning a
//! grant into a real page-table mapping stays each target's `vm.rs` seam; here we prove the layer
//! above it: capability-gated sharing, attenuation, zero-copy backing, bounded access, and
//! revocation-unmaps — all fail-closed.

use kernel_core::grant::{GrantError, GrantTable, ShareMode};
use kernel_core::spine::{CapEngine, Constraints, Scope};

const SHARE: &str = "memory.share";

/// A capability engine granting `subject` the `memory.share` authority, plus the minted token.
fn engine_with_share(subject: &str) -> (CapEngine, kernel_core::spine::CapToken) {
    let mut engine = CapEngine::new(0xBEEF, 1_000);
    let cap = engine.mint(subject, SHARE, Scope::All, Constraints::none());
    (engine, cap)
}

#[test]
fn share_requires_the_memory_share_capability() {
    // Fail-closed: with NO offered capability, an endpoint cannot establish a shared mapping.
    let (engine, _cap) = engine_with_share("owner");
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 64);

    let denied = gt.share(&engine, region, "owner", "peer", ShareMode::ReadWrite, &[]);
    assert_eq!(denied, Err(GrantError::Unauthorized));
    // Nothing was mapped: only the region itself holds the backing.
    assert_eq!(gt.region_refcount(region), 1);
}

#[test]
fn authorized_share_maps_zero_copy_and_reader_sees_writer() {
    // The core zero-copy property: the writer writes, the reader reads the SAME bytes, with no copy
    // through any message queue, and the backing is genuinely shared (refcount rises with each grant).
    let (engine, cap) = engine_with_share("owner");
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 64);
    assert_eq!(gt.region_refcount(region), 1);

    let writer = gt
        .share(&engine, region, "owner", "producer", ShareMode::ReadWrite, &[cap])
        .expect("authorized RW share");
    let reader = gt
        .share(&engine, region, "owner", "consumer", ShareMode::Read, &[cap])
        .expect("authorized RO share");
    // region + writer grant + reader grant all reference one backing store.
    assert_eq!(gt.region_refcount(region), 3);

    gt.write(writer, 8, b"aletheia").expect("RW grant may write");
    // The reader observes the writer's bytes with no intervening copy — zero-copy shared memory.
    assert_eq!(gt.read(reader, 8, 8).unwrap(), b"aletheia");
}

#[test]
fn read_only_grant_cannot_write() {
    // A Read grant confers no write authority — fail-closed.
    let (engine, cap) = engine_with_share("owner");
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 32);
    let ro = gt
        .share(&engine, region, "owner", "consumer", ShareMode::Read, &[cap])
        .unwrap();

    assert_eq!(gt.write(ro, 0, b"x"), Err(GrantError::ReadOnly));
    // The region is untouched (still all zero).
    assert_eq!(gt.read(ro, 0, 4).unwrap(), vec![0u8; 4]);
}

#[test]
fn share_can_attenuate_but_never_amplify() {
    // Owner (RW) shares Read to a peer; that peer, holding only Read on the region, may re-share
    // Read but NEVER ReadWrite — a transfer can only narrow, exactly like capability delegation.
    let mut engine = CapEngine::new(0xBEEF, 1_000);
    let owner_cap = engine.mint("owner", SHARE, Scope::All, Constraints::none());
    let peer_cap = engine.mint("peer", SHARE, Scope::All, Constraints::none());
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 16);

    let ro = gt
        .share(&engine, region, "owner", "peer", ShareMode::Read, &[owner_cap])
        .expect("owner shares Read to peer");
    assert_eq!(gt.grant_mode(ro), Some(ShareMode::Read));

    // Peer re-shares: Read is allowed (attenuation)…
    let ro2 = gt.share(&engine, region, "peer", "third", ShareMode::Read, &[peer_cap]);
    assert!(ro2.is_ok(), "read-only holder may pass on read-only");

    // …but ReadWrite is refused — the peer cannot amplify beyond the read access it holds.
    let amp = gt.share(&engine, region, "peer", "third", ShareMode::ReadWrite, &[peer_cap]);
    assert_eq!(amp, Err(GrantError::Amplify));
}

#[test]
fn cannot_share_a_region_without_holding_access() {
    // An endpoint that holds memory.share authority but no access to THIS region cannot share it.
    let mut engine = CapEngine::new(0xBEEF, 1_000);
    let stranger_cap = engine.mint("stranger", SHARE, Scope::All, Constraints::none());
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 16);

    let denied = gt.share(&engine, region, "stranger", "x", ShareMode::Read, &[stranger_cap]);
    assert_eq!(denied, Err(GrantError::NoAccess));
}

#[test]
fn access_is_bounded_to_the_region() {
    // The model of the MMU refusing access past the shared frame: any read/write leaving [0,len) is
    // refused fail-closed, and an offset+len overflow does not wrap.
    let (engine, cap) = engine_with_share("owner");
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 16);
    let rw = gt
        .share(&engine, region, "owner", "peer", ShareMode::ReadWrite, &[cap])
        .unwrap();

    assert_eq!(gt.read(rw, 12, 8), Err(GrantError::OutOfBounds));
    assert_eq!(gt.write(rw, 12, &[0u8; 8]), Err(GrantError::OutOfBounds));
    assert_eq!(gt.write(rw, usize::MAX, b"x"), Err(GrantError::OutOfBounds));
    // A write fully inside the region still works.
    assert!(gt.write(rw, 8, &[1u8; 8]).is_ok());
}

#[test]
fn revocation_unmaps_and_releases_the_backing() {
    // Revoking a grant tears the mapping down: later access through it is denied fail-closed and the
    // endpoint's share of the backing is released (refcount drops).
    let (engine, cap) = engine_with_share("owner");
    let mut gt = GrantTable::new(SHARE);
    let region = gt.create_region("owner", 0x4000, 16);
    let rw = gt
        .share(&engine, region, "owner", "peer", ShareMode::ReadWrite, &[cap])
        .unwrap();
    assert_eq!(gt.region_refcount(region), 2);

    assert!(gt.revoke(rw), "live grant revokes");
    assert_eq!(gt.region_refcount(region), 1, "backing share released on revoke");
    assert_eq!(gt.read(rw, 0, 4), Err(GrantError::Revoked));
    assert_eq!(gt.write(rw, 0, b"x"), Err(GrantError::Revoked));
    assert!(!gt.revoke(rw), "double-revoke is a no-op");
    assert_eq!(gt.grant_mode(rw), None, "a revoked grant confers no mode");
}

#[test]
fn unknown_grant_and_region_are_fail_closed() {
    let gt = GrantTable::new(SHARE);
    assert_eq!(gt.read(999, 0, 1), Err(GrantError::NotFound));
    assert_eq!(gt.region_refcount(999), 0);
    assert_eq!(gt.grant_mode(999), None);
}
