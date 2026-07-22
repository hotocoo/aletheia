//! Crash-consistency invariants for the journaled block store (REQ-STOR-002, ADR-024).
//!
//! The load-bearing proof is the **crash-at-every-prefix sweep**: capture the exact ordered sequence
//! of block writes one transaction issues, then for every prefix length K materialize a device as if
//! only the first K writes landed, run recovery, and assert the logical state is EITHER the
//! pre-transaction state OR the fully-applied state — never a torn mixture. That "for all crash
//! points" quantifier is what earns the phrase *crash-consistent*. Two more prove the checksum is
//! load-bearing: a torn commit record and a torn journal payload are both rolled back, never applied.

use kernel_core::storage::{BlockDevice, Journal, StorageError, BLOCK_SIZE, DATA_START, JOURNAL_START};

/// A block device that records the ordered write log (when recording) so a test can replay any crash
/// prefix. Backed by an in-`alloc` array; `flush` is a no-op (durability is modelled by the prefix).
struct RecDevice {
    blocks: Vec<[u8; BLOCK_SIZE]>,
    log: Vec<(usize, [u8; BLOCK_SIZE])>,
    recording: bool,
}

impl RecDevice {
    fn new(n: usize) -> Self {
        RecDevice { blocks: vec![[0u8; BLOCK_SIZE]; n], log: Vec::new(), recording: false }
    }
    fn from_blocks(blocks: Vec<[u8; BLOCK_SIZE]>) -> Self {
        RecDevice { blocks, log: Vec::new(), recording: false }
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
    Txn { log: dev.log.clone(), pre, h1, h2, pre1, pre2, post1, post2 }
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
    assert!(replayed, "a committed transaction is recognized and replayed");
    assert_eq!(dev.blocks[t.h1], t.post1);
    assert_eq!(dev.blocks[t.h2], t.post2);
}

#[test]
fn fresh_device_has_no_committed_transaction() {
    let mut dev = RecDevice::new(DATA_START + 2);
    let replayed = Journal::new().recover(&mut dev).expect("recover");
    assert!(!replayed, "a blank device recovers to nothing (fail closed, no magic)");
}

#[test]
fn torn_commit_record_is_rejected() {
    // Crash with the commit record half-written (a flipped byte) BEFORE the home apply. The checksum
    // fails, so recovery treats the transaction as uncommitted — the home blocks keep their pre-state.
    let t = run_txn();
    let block0_pos = t.log.iter().position(|(idx, _)| *idx == 0).expect("record write logged");
    let mut crashed = RecDevice::from_blocks(t.pre.clone());
    for (i, (idx, data)) in t.log.iter().enumerate().take(block0_pos + 1) {
        let mut d = *data;
        if i == block0_pos {
            d[100] ^= 0xFF; // tear the commit record
        }
        crashed.blocks[*idx] = d;
    }
    Journal::new().recover(&mut crashed).expect("recover");
    assert_eq!(crashed.blocks[t.h1], t.pre1, "torn commit record ⇒ home unchanged");
    assert_eq!(crashed.blocks[t.h2], t.pre2, "torn commit record ⇒ home unchanged");
}

#[test]
fn torn_journal_payload_is_rejected() {
    // The commit record is intact, but a journal payload block is corrupt (bit-rot / torn write). The
    // checksum covers the journal payload too, so recovery detects it and rolls back — never applies a
    // corrupt block to a home location (corruption surfaced, not swallowed).
    let t = run_txn();
    let block0_pos = t.log.iter().position(|(idx, _)| *idx == 0).expect("record write logged");
    let mut crashed = RecDevice::from_blocks(t.pre.clone());
    for (idx, data) in t.log.iter().take(block0_pos + 1) {
        let mut d = *data;
        if *idx == JOURNAL_START {
            d[7] ^= 0xFF; // corrupt the first journal payload block
        }
        crashed.blocks[*idx] = d;
    }
    Journal::new().recover(&mut crashed).expect("recover");
    assert_eq!(crashed.blocks[t.h1], t.pre1, "corrupt journal payload ⇒ home unchanged");
    assert_eq!(crashed.blocks[t.h2], t.pre2, "corrupt journal payload ⇒ home unchanged");
}
