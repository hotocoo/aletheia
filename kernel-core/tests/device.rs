//! Capability-authorized device access invariants (REQ-DRV-002, ADR-023).
//!
//! The point is that the capability gates REAL I/O to a REAL device, not a boolean from an empty
//! registry: no capability ⇒ no bytes move; a read-only capability can read but cannot mutate; a
//! write capability's bytes actually land and read back. Proved over the real `MemBlockDevice`.

use kernel_core::device::{DeviceError, DeviceGuard};
use kernel_core::spine::{CapEngine, Constraints, Scope};
use kernel_core::storage::{MemBlockDevice, BLOCK_SIZE};

const READ: &str = "dev.blk.read";
const WRITE: &str = "dev.blk.write";
const IDX: usize = 3;

fn full(v: u8) -> [u8; BLOCK_SIZE] {
    [v; BLOCK_SIZE]
}

#[test]
fn no_capability_denies_real_io() {
    let engine = CapEngine::new(0xD001, 1000);
    let mut guard = DeviceGuard::new(MemBlockDevice::new(8), READ, WRITE);

    // No offered capability: both directions are refused, and no bytes move.
    let mut buf = [0u8; BLOCK_SIZE];
    assert_eq!(
        guard.read_block(&engine, &[], IDX, &mut buf),
        Err(DeviceError::Denied)
    );
    assert_eq!(
        guard.write_block(&engine, &[], IDX, &full(0xAA)),
        Err(DeviceError::Denied)
    );
    assert_eq!(guard.flush(&engine, &[]), Err(DeviceError::Denied));
}

#[test]
fn read_only_capability_reads_but_cannot_write() {
    let mut engine = CapEngine::new(0xD002, 1000);
    let read_cap = engine.mint("client", READ, Scope::All, Constraints::none());
    let mut guard = DeviceGuard::new(MemBlockDevice::new(8), READ, WRITE);

    // The read capability reads (the block is still zero).
    let mut buf = [0xFFu8; BLOCK_SIZE];
    guard
        .read_block(&engine, &[read_cap], IDX, &mut buf)
        .expect("read allowed");
    assert_eq!(buf, full(0), "read returns the real (zero) block");

    // …but it cannot write — the attenuated authority genuinely blocks mutation.
    assert_eq!(
        guard.write_block(&engine, &[read_cap], IDX, &full(0xAA)),
        Err(DeviceError::Denied)
    );
    // Confirm no bytes moved: an authorized read still sees zero.
    let mut check = [0xFFu8; BLOCK_SIZE];
    guard
        .read_block(&engine, &[read_cap], IDX, &mut check)
        .unwrap();
    assert_eq!(check, full(0), "a denied write left the device unchanged");
}

#[test]
fn write_capability_writes_and_reads_back() {
    let mut engine = CapEngine::new(0xD003, 1000);
    let read_cap = engine.mint("client", READ, Scope::All, Constraints::none());
    let write_cap = engine.mint("client", WRITE, Scope::All, Constraints::none());
    let caps = [read_cap, write_cap];
    let mut guard = DeviceGuard::new(MemBlockDevice::new(8), READ, WRITE);

    guard
        .write_block(&engine, &caps, IDX, &full(0xC5))
        .expect("write allowed");
    guard.flush(&engine, &caps).expect("flush allowed");
    let mut buf = [0u8; BLOCK_SIZE];
    guard
        .read_block(&engine, &caps, IDX, &mut buf)
        .expect("read allowed");
    assert_eq!(
        buf,
        full(0xC5),
        "the authorized write's bytes actually landed"
    );
}

#[test]
fn wildcard_capability_authorizes_both() {
    // A single `dev.blk.*` capability covers both read and write (action-prefix wildcard).
    let mut engine = CapEngine::new(0xD004, 1000);
    let cap = engine.mint("client", "dev.blk.*", Scope::All, Constraints::none());
    let mut guard = DeviceGuard::new(MemBlockDevice::new(8), READ, WRITE);

    guard
        .write_block(&engine, &[cap], IDX, &full(0x42))
        .expect("write via wildcard");
    let mut buf = [0u8; BLOCK_SIZE];
    guard
        .read_block(&engine, &[cap], IDX, &mut buf)
        .expect("read via wildcard");
    assert_eq!(buf, full(0x42));
}
