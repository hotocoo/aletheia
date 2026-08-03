//! Crash-consistency invariants for the journaled block store (REQ-STOR-002, ADR-024).
//!
//! The load-bearing proof is the **crash-at-every-prefix sweep**: capture the exact ordered sequence
//! of block writes one transaction issues, then for every prefix length K materialize a device as if
//! only the first K writes landed, run recovery, and assert the logical state is EITHER the
//! pre-transaction state OR the fully-applied state — never a torn mixture. That "for all crash
//! points" quantifier is what earns the phrase *crash-consistent*. Two more prove the checksum is
//! load-bearing: a torn commit record and a torn journal payload are both rolled back, never applied.

use kernel_core::storage::{
    BlockDevice, Journal, StorageError, BLOCK_SIZE, DATA_START, JOURNAL_START,
};

/// A block device that records the ordered write log (when recording) so a test can replay any crash
/// prefix. Backed by an in-`alloc` array; `flush` is a no-op (durability is modelled by the prefix).
struct RecDevice {
    blocks: Vec<[u8; BLOCK_SIZE]>,
    log: Vec<(usize, [u8; BLOCK_SIZE])>,
    recording: bool,
}

impl RecDevice {
    fn new(n: usize) -> Self {
        RecDevice {
            blocks: vec![[0u8; BLOCK_SIZE]; n],
            log: Vec::new(),
            recording: false,
        }
    }
    fn from_blocks(blocks: Vec<[u8; BLOCK_SIZE]>) -> Self {
        RecDevice {
            blocks,
            log: Vec::new(),
            recording: false,
        }
    }
}

impl BlockDevice for RecDevice {
    fn num_blocks(&self) -> usize {
        self.blocks.len()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        buf.copy_from_slice(&self.blocks[idx]);
        Ok(())
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        let mut b = [0u8; BLOCK_SIZE];
        b.copy_from_slice(buf);
        if self.recording {
            self.log.push((idx, b));
        }
        self.blocks[idx] = b;
        Ok(())
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}

/// A block whose every byte is `v` (distinct patterns make pre/post states unmistakable).
fn blk(v: u8) -> [u8; BLOCK_SIZE] {
    [v; BLOCK_SIZE]
}

/// One recorded two-block transaction: its ordered write log, the pre-state device image, the two
/// home block indices, and the pre/post contents of each.
struct Txn {
    log: Vec<(usize, [u8; BLOCK_SIZE])>,
    pre: Vec<[u8; BLOCK_SIZE]>,
    h1: usize,
    h2: usize,
    pre1: [u8; BLOCK_SIZE],
    pre2: [u8; BLOCK_SIZE],
    post1: [u8; BLOCK_SIZE],
    post2: [u8; BLOCK_SIZE],
}

/// Set up a device with two home blocks in a known pre-state, then run one two-block transaction with
/// recording on.
fn run_txn() -> Txn {
    let (h1, h2) = (DATA_START, DATA_START + 1);
    let (pre1, pre2) = (blk(0x11), blk(0x22));
    let (post1, post2) = (blk(0xAA), blk(0xBB));
    let mut dev = RecDevice::new(DATA_START + 4);
    dev.blocks[h1] = pre1;
    dev.blocks[h2] = pre2;
    let pre = dev.blocks.clone();

    dev.recording = true;
    Journal::new()
        .commit(&mut dev, &[(h1, post1), (h2, post2)])
        .expect("commit");
    Txn {
        log: dev.log.clone(),
        pre,
        h1,
        h2,
        pre1,
        pre2,
        post1,
        post2,
    }
}

#[test]
fn crash_at_every_prefix_is_atomic() {
    // The core proof: for EVERY crash point in the transaction's write sequence, recovery yields the
    // pre-transaction state OR the fully-applied state across BOTH home blocks — never torn.
    let t = run_txn();
    for k in 0..=t.log.len() {
        // Materialize a device as if only the first `k` writes reached the platter.
        let mut crashed = RecDevice::from_blocks(t.pre.clone());
        for (idx, data) in t.log.iter().take(k) {
            crashed.blocks[*idx] = *data;
        }
        Journal::new().recover(&mut crashed).expect("recover");
        let (got1, got2) = (crashed.blocks[t.h1], crashed.blocks[t.h2]);
        let is_pre = got1 == t.pre1 && got2 == t.pre2;
        let is_post = got1 == t.post1 && got2 == t.post2;
        assert!(
            is_pre || is_post,
            "TORN at crash prefix k={k}: h1={:#x} h2={:#x} (neither pre nor post)",
            got1[0],
            got2[0]
        );
    }
}

#[test]
fn recovery_after_full_commit_is_the_applied_state() {
    // A completed commit + recovery (idempotent replay) leaves the fully-applied state.
    let t = run_txn();
    let mut dev = RecDevice::from_blocks(t.pre.clone());
    for (idx, data) in &t.log {
        dev.blocks[*idx] = *data; // all writes landed
    }
    let replayed = Journal::new().recover(&mut dev).expect("recover");
    assert!(
        replayed,
        "a committed transaction is recognized and replayed"
    );
    assert_eq!(dev.blocks[t.h1], t.post1);
    assert_eq!(dev.blocks[t.h2], t.post2);
}

#[test]
fn fresh_device_has_no_committed_transaction() {
    let mut dev = RecDevice::new(DATA_START + 2);
    let replayed = Journal::new().recover(&mut dev).expect("recover");
    assert!(
        !replayed,
        "a blank device recovers to nothing (fail closed, no magic)"
    );
}

#[test]
fn torn_commit_record_is_rejected() {
    // Crash with the commit record half-written (a flipped byte) BEFORE the home apply. The checksum
    // fails, so recovery treats the transaction as uncommitted — the home blocks keep their pre-state.
    let t = run_txn();
    let block0_pos = t
        .log
        .iter()
        .position(|(idx, _)| *idx == 0)
        .expect("record write logged");
    let mut crashed = RecDevice::from_blocks(t.pre.clone());
    for (i, (idx, data)) in t.log.iter().enumerate().take(block0_pos + 1) {
        let mut d = *data;
        if i == block0_pos {
            d[100] ^= 0xFF; // tear the commit record
        }
        crashed.blocks[*idx] = d;
    }
    Journal::new().recover(&mut crashed).expect("recover");
    assert_eq!(
        crashed.blocks[t.h1], t.pre1,
        "torn commit record ⇒ home unchanged"
    );
    assert_eq!(
        crashed.blocks[t.h2], t.pre2,
        "torn commit record ⇒ home unchanged"
    );
}

#[test]
fn torn_journal_payload_is_rejected() {
    // The commit record is intact, but a journal payload block is corrupt (bit-rot / torn write). The
    // checksum covers the journal payload too, so recovery detects it and rolls back — never applies a
    // corrupt block to a home location (corruption surfaced, not swallowed).
    let t = run_txn();
    let block0_pos = t
        .log
        .iter()
        .position(|(idx, _)| *idx == 0)
        .expect("record write logged");
    let mut crashed = RecDevice::from_blocks(t.pre.clone());
    for (idx, data) in t.log.iter().take(block0_pos + 1) {
        let mut d = *data;
        if *idx == JOURNAL_START {
            d[7] ^= 0xFF; // corrupt the first journal payload block
        }
        crashed.blocks[*idx] = d;
    }
    Journal::new().recover(&mut crashed).expect("recover");
    assert_eq!(
        crashed.blocks[t.h1], t.pre1,
        "corrupt journal payload ⇒ home unchanged"
    );
    assert_eq!(
        crashed.blocks[t.h2], t.pre2,
        "corrupt journal payload ⇒ home unchanged"
    );
}

// ---------------------------------------------------------------------------
// INV-STORE-ERR contract (docs/INVARIANT-CONTRACTS.md) — storage error semantics, ALET-P1-020.
//
// A storage stack's error behavior IS its safety story: a swallowed device error becomes silent data
// loss, and an error that cannot be told apart from another one cannot be handled correctly.
// ---------------------------------------------------------------------------

use kernel_core::fs::{Filesystem, FsError, FILE_DATA_START};
use kernel_core::storage::MemBlockDevice;

/// A device that fails one specific operation, so a single error can be traced end to end.
struct FailingDevice {
    inner: MemBlockDevice,
    fail_write_at: Option<usize>,
    fail_flush: bool,
}

impl BlockDevice for FailingDevice {
    fn num_blocks(&self) -> usize {
        self.inner.num_blocks()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        self.inner.read_block(idx, buf)
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        if self.fail_write_at == Some(idx) {
            return Err(StorageError::Device);
        }
        self.inner.write_block(idx, buf)
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        if self.fail_flush {
            return Err(StorageError::Device);
        }
        self.inner.flush()
    }
}

/// INV-STORE-ERR-1: the error KINDS are distinguishable — a caller can tell "you asked for the wrong
/// size" from "that block does not exist" from "the device failed". Collapsing them would make the only
/// possible response to any error the same one.
#[test]
fn every_error_kind_is_distinguishable_at_its_own_boundary() {
    let dev = MemBlockDevice::new(8);
    let mut buf = [0u8; 16]; // deliberately not BLOCK_SIZE
    assert_eq!(
        dev.read_block(0, &mut buf),
        Err(StorageError::BadBlockSize),
        "a wrong-sized buffer must be its own error"
    );
    let mut full = [0u8; BLOCK_SIZE];
    assert_eq!(
        dev.read_block(99, &mut full),
        Err(StorageError::OutOfRange),
        "a block past the device must be its own error"
    );
    // And the three are genuinely different values, not aliases.
    assert_ne!(StorageError::BadBlockSize, StorageError::OutOfRange);
    assert_ne!(StorageError::OutOfRange, StorageError::Device);
    assert_ne!(StorageError::Device, StorageError::TooLarge);
}

/// INV-STORE-ERR-2: a device error is SURFACED, never swallowed. The journal reports it, and the caller
/// therefore knows the transaction did not commit — the difference between a failure and silent loss.
#[test]
fn a_device_error_surfaces_through_the_journal_rather_than_being_swallowed() {
    let mut dev = FailingDevice {
        inner: MemBlockDevice::new(FILE_DATA_START + 32),
        fail_write_at: Some(0), // the commit record itself
        fail_flush: false,
    };
    let mut journal = Journal::new();
    let mut block = [0u8; BLOCK_SIZE];
    block.fill(0x5A);
    let out = journal.commit(&mut dev, &[(DATA_START + 1, block)]);
    assert_eq!(
        out,
        Err(StorageError::Device),
        "INV-STORE-ERR-2: a failed commit-record write did not surface"
    );
    // And the home block was never written: a refused commit leaves the prior state.
    let mut read = [0u8; BLOCK_SIZE];
    dev.read_block(DATA_START + 1, &mut read).expect("read");
    assert!(
        read.iter().all(|&b| b == 0),
        "INV-STORE-ERR-2: a failed commit still wrote its home block"
    );

    // A failing FLUSH is equally load-bearing: it is the durability barrier, so swallowing it would
    // report durability that does not exist.
    let mut dev2 = FailingDevice {
        inner: MemBlockDevice::new(FILE_DATA_START + 32),
        fail_write_at: None,
        fail_flush: true,
    };
    assert_eq!(
        Journal::new().commit(&mut dev2, &[(DATA_START + 1, block)]),
        Err(StorageError::Device),
        "INV-STORE-ERR-2: a failed flush was reported as success"
    );
}

/// INV-STORE-ERR-3: the filesystem PRESERVES the device error rather than flattening it into a generic
/// failure — `FsError::Storage(Device)` keeps the cause, while its own refusals keep their own names.
#[test]
fn the_filesystem_preserves_the_device_error_and_keeps_its_own_refusals_distinct() {
    let mut dev = FailingDevice {
        inner: MemBlockDevice::new(FILE_DATA_START + 32),
        fail_write_at: None,
        fail_flush: false,
    };
    Filesystem::format(&mut dev).expect("format");
    let mut fs = Filesystem::mount(&mut dev).expect("mount");

    // Its own refusals are its own names, not Storage(...).
    assert_eq!(fs.create(&mut dev, "", b"x"), Err(FsError::BadName));
    fs.create(&mut dev, "a", b"x").expect("create");
    assert_eq!(fs.create(&mut dev, "a", b"y"), Err(FsError::Exists));
    assert_eq!(fs.read(&dev, "missing"), Err(FsError::NotFound));

    // A device failure keeps its cause all the way up.
    dev.fail_flush = true;
    assert_eq!(
        fs.create(&mut dev, "b", b"z"),
        Err(FsError::Storage(StorageError::Device)),
        "INV-STORE-ERR-3: the device error was flattened"
    );
}

/// INV-STORE-ERR-4: a refused operation is a NO-OP. Proven by comparing the whole device image before and
/// after every refusal — the strongest form of "nothing happened".
#[test]
fn every_refusal_leaves_the_device_image_byte_identical() {
    let mut dev = MemBlockDevice::new(FILE_DATA_START + 16);
    Filesystem::format(&mut dev).expect("format");
    let mut fs = Filesystem::mount(&mut dev).expect("mount");
    fs.create(&mut dev, "keep", b"payload").expect("create");
    let before = dev.snapshot();

    let long = "y".repeat(64);
    let refusals: [(&str, &[u8]); 4] = [
        ("keep", b"other"),    // Exists
        ("", b"x"),            // BadName
        ("a/b", b"x"),         // BadName
        (long.as_str(), b"x"), // BadName
    ];
    for (name, data) in refusals {
        assert!(fs.create(&mut dev, name, data).is_err());
        assert!(
            dev.snapshot() == before,
            "INV-STORE-ERR-4: a refused create changed the device image (name={name:?})"
        );
    }
    assert!(fs.remove(&mut dev, "absent").is_err());
    assert!(
        dev.snapshot() == before,
        "INV-STORE-ERR-4: a refused remove changed the device image"
    );
    // A journal transaction naming a reserved block is refused with nothing written.
    let mut block = [0u8; BLOCK_SIZE];
    block.fill(0xEE);
    assert_eq!(
        Journal::new().commit(&mut dev, &[(0, block)]),
        Err(StorageError::TooLarge)
    );
    assert!(
        dev.snapshot() == before,
        "INV-STORE-ERR-4: a refused transaction wrote something"
    );
}
