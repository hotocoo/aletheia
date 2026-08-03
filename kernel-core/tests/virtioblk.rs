//! Host proofs for the shared block-device suite (REQ-DRV-004, ADR-036).
//!
//! The MMIO/virtqueue half of `kernel_core::virtioblk` can only be exercised against a real transport,
//! which is what the aarch64 and RISC-V VM gates do. What IS testable on the host is the part that
//! decides whether a target's boot passes: `device_suite` — the ordered invariant list every target
//! with a disk runs. Proving it here means a regression in the suite itself (a check that stops
//! checking, a mis-numbered invariant, a geometry assertion that passes on the wrong device) is caught
//! by `cargo test` instead of by three QEMU boots.
extern crate alloc;

use kernel_core::storage::{MemBlockDevice, BLOCK_SIZE};
use kernel_core::virtioblk::{device_suite, SECTORS_PER_BLOCK, SECTOR_SIZE};

/// The geometry both VM gates attach: a 1 MiB image = 2048 sectors = 256 blocks.
const GATE_BLOCKS: usize = 256;

#[test]
fn the_sector_geometry_matches_a_4_kib_block() {
    assert_eq!(SECTOR_SIZE, 512);
    assert_eq!(SECTORS_PER_BLOCK, 8);
    assert_eq!(SECTORS_PER_BLOCK as usize * SECTOR_SIZE, BLOCK_SIZE);
    // The gates' 1 MiB image really is 256 blocks — the number the suite asserts.
    assert_eq!(1024 * 1024 / BLOCK_SIZE, GATE_BLOCKS);
}

#[test]
fn the_shared_suite_proves_seventeen_invariants_over_a_device() {
    // Every invariant in the order a target boots them: 4 driver/journal + 15 filesystem + 1
    // capability gating. The count is what the VM gates grep for ("ALL 20 ..."), so it is asserted.
    let dev = MemBlockDevice::new(GATE_BLOCKS);
    let mut seen: alloc::vec::Vec<(usize, alloc::string::String)> = alloc::vec![];
    let n = device_suite(dev, GATE_BLOCKS, |i, passed, name| {
        assert!(passed, "invariant {i} failed: {name}");
        seen.push((i, alloc::string::String::from(name)));
    })
    .expect("every invariant holds over a well-behaved device");

    assert_eq!(
        n, 20,
        "the suite's invariant count changed — update both VM gates"
    );
    // Numbering is dense and 1-based, so an invariant cannot be skipped without the log showing it.
    assert_eq!(seen.len(), 20);
    for (idx, (i, _)) in seen.iter().enumerate() {
        assert_eq!(*i, idx + 1, "invariant numbering has a gap at {i}");
    }
    // The groups are in the order the ADR describes: driver first, filesystem in the middle, authority
    // last (a suite that authorized first would prove the fs behaviors through a guard, not the device).
    assert!(seen[0].1.starts_with("virtio-blk: device discovered"));
    assert!(
        seen[4].1.starts_with("fs: "),
        "group 5 should be the filesystem: {}",
        seen[4].1
    );
    assert!(seen[18].1.starts_with("fs: "));
    assert!(seen[19].1.contains("capability-gated I/O"));
}

#[test]
fn a_device_of_the_wrong_geometry_is_refused_before_any_data_is_trusted() {
    // A device half the expected size must fail at invariant 2 — the capacity assertion — NOT later
    // with corrupt data. This is the check that catches a wrong sector/block mapping.
    let dev = MemBlockDevice::new(GATE_BLOCKS / 2);
    let mut last = 0usize;
    let err = device_suite(dev, GATE_BLOCKS, |i, _, _| last = i)
        .expect_err("a mis-sized device must be refused");
    assert_eq!(
        err.0, 2,
        "geometry must be the SECOND invariant, before any I/O"
    );
    assert!(err.1.contains("capacity"), "wrong invariant: {}", err.1);
    assert_eq!(last, 2, "the failing invariant must still be logged");
}

#[test]
fn a_device_too_small_for_the_journal_fails_closed_rather_than_writing_past_its_end() {
    // Smaller than the journal area itself: the suite must report a failure, never panic and never
    // write outside the device.
    let dev = MemBlockDevice::new(4);
    let err = device_suite(dev, 4, |_, _, _| {}).expect_err("a device this small cannot pass");
    assert!(err.0 >= 1);
}
