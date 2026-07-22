//! Crash-consistent journaled block store (REQ-STOR-002, ADR-024).
//!
//! A general-purpose OS needs storage that survives a power loss *without corruption*. This module is
//! the arch-independent middle of that stack: a **write-ahead journal** over an abstract
//! [`BlockDevice`], so a multi-block update either lands entirely or not at all — never torn.
//!
//! It is deliberately `alloc`-only (no `std`, no filesystem) so the same journal core runs in a kernel
//! later; the device is a **seam** a real driver (virtio-blk, REQ-DRV-001) will implement, exactly as
//! the in-memory device in the tests does now. What lives here is the *correctness*, not the hardware.
//!
//! ## The commit protocol (the atomic pivot)
//!
//! A transaction is a set of `(home_block, new_contents)` updates. [`Journal::commit`]:
//! 1. writes each update into the **journal area**, then `flush` (the redo data is durable);
//! 2. writes a **checksummed commit record** to block 0, then `flush` — *this flush is the atomic
//!    pivot*: before it, no committed transaction exists; after it, the transaction is committed;
//! 3. applies each update to its **home block**, then `flush`.
//!
//! ## Recovery (binary, never torn)
//!
//! [`Journal::recover`] reads the commit record. If its magic is absent or its checksum (over the
//! record header **and** the journal payload) does not verify, the transaction is treated as
//! **uncommitted** and nothing is applied — the home blocks keep their prior contents. If it verifies,
//! the journal is **replayed** into the home blocks (idempotent, so replaying a partially-applied
//! transaction simply completes it). Therefore, for *every* crash point, recovery yields either the
//! pre-transaction state or the fully-applied state — proven by the crash-at-every-prefix sweep in
//! `tests/storage.rs`. The checksum is load-bearing: a half-written commit record or a torn journal
//! payload fails it and is rolled back, never partially applied.
//!
//! Deferred (REQ-STOR-001 / ADR-024, explicit follow-ons — not claimed here): a real storage driver,
//! an encryption-at-rest layer over the device, per-home-block integrity to catch post-commit bit-rot
//! on read, and building the semantic content-addressed store on top of this journal.
use alloc::vec;
use alloc::vec::Vec;

/// Fixed block size (bytes). One page — the natural transfer unit for a real block device.
pub const BLOCK_SIZE: usize = 4096;

/// Commit-record magic ("AlJnl\0\0\1") — its absence means "no committed transaction here".
const MAGIC: u64 = 0x416C_4A6E_6C00_0001;
/// Byte offsets within the block-0 commit record.
const OFF_MAGIC: usize = 0;
const OFF_SEQ: usize = 8;
const OFF_COUNT: usize = 16;
const OFF_INDICES: usize = 24;
/// The checksum occupies the last 8 bytes of the commit record.
const OFF_CKSUM: usize = BLOCK_SIZE - 8;
/// Max home updates per transaction (bounded so the commit record + journal area are fixed-size).
pub const MAX_ENTRIES: usize = 64;
/// Block 0 is the commit record; blocks `1..=MAX_ENTRIES` are journal slots; home data starts after.
pub const JOURNAL_START: usize = 1;
pub const DATA_START: usize = JOURNAL_START + MAX_ENTRIES;

/// Why a storage operation failed. Every failure is surfaced, never swallowed into silent corruption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    /// A block index is outside the device.
    OutOfRange,
    /// A buffer was not exactly [`BLOCK_SIZE`].
    BadBlockSize,
    /// A transaction exceeds [`MAX_ENTRIES`] updates, or targets a reserved (journal/record) block.
    TooLarge,
    /// The underlying device reported a failure.
    Device,
}

/// The device seam: a fixed array of fixed-size blocks with an explicit durability barrier. A real
/// driver (virtio-blk) implements this; hosted tests use a crash-injectable in-memory implementation.
pub trait BlockDevice {
    /// Number of [`BLOCK_SIZE`] blocks on the device.
    fn num_blocks(&self) -> usize;
    /// Read block `idx` into `buf` (must be [`BLOCK_SIZE`]).
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError>;
    /// Write `buf` (must be [`BLOCK_SIZE`]) to block `idx`.
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError>;
    /// Durability barrier: all prior writes are persistent once this returns.
    fn flush(&mut self) -> Result<(), StorageError>;
}

/// FNV-1a over a byte stream — a small dependency-free integrity check. Sufficient to detect the
/// bit-level tearing a crash causes in the hosted proof; a production build would use CRC32C/SHA.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn le64(buf: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(a)
}

fn put64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// A write-ahead journal over a [`BlockDevice`]. Stateless beyond the device + a monotonic sequence;
/// all durable state lives on the device, so a fresh `Journal` can [`recover`](Journal::recover) any
/// device.
pub struct Journal {
    seq: u64,
}

impl Journal {
    pub fn new() -> Self {
        Journal { seq: 0 }
    }

    /// Commit a transaction: a set of `(home_block_index, new_contents)` updates, applied all-or-
    /// nothing across a crash. `home_block_index` must be `>= DATA_START` and `< num_blocks`; at most
    /// [`MAX_ENTRIES`] updates. See the module docs for the ordering (journal → flush → commit
    /// record → flush pivot → apply → flush).
    pub fn commit<D: BlockDevice>(
        &mut self,
        dev: &mut D,
        updates: &[(usize, [u8; BLOCK_SIZE])],
    ) -> Result<(), StorageError> {
        if updates.len() > MAX_ENTRIES {
            return Err(StorageError::TooLarge);
        }
        let n = dev.num_blocks();
        for (idx, _) in updates {
            if *idx < DATA_START || *idx >= n {
                return Err(StorageError::TooLarge);
            }
        }
        // 1. Write the redo data into the journal area, then flush (the payload is now durable).
        for (i, (_, data)) in updates.iter().enumerate() {
            dev.write_block(JOURNAL_START + i, data)?;
        }
        dev.flush()?;

        // 2. Build the checksummed commit record and write it to block 0, then flush — the pivot.
        self.seq = self.seq.wrapping_add(1);
        let mut rec = [0u8; BLOCK_SIZE];
        put64(&mut rec, OFF_MAGIC, MAGIC);
        put64(&mut rec, OFF_SEQ, self.seq);
        put64(&mut rec, OFF_COUNT, updates.len() as u64);
        for (i, (idx, _)) in updates.iter().enumerate() {
            put64(&mut rec, OFF_INDICES + i * 8, *idx as u64);
        }
        // Checksum covers the record header (everything but the checksum field) AND the journal
        // payload, so a torn record OR a torn journal block is detected as uncommitted.
        let mut ck = fnv1a(0xcbf2_9ce4_8422_2325, &rec[..OFF_CKSUM]);
        for (_, data) in updates {
            ck = fnv1a(ck, data);
        }
        put64(&mut rec, OFF_CKSUM, ck);
        dev.write_block(0, &rec)?;
        dev.flush()?;

        // 3. Apply each update to its home block, then flush. (Recovery would redo this idempotently.)
        for (idx, data) in updates {
            dev.write_block(*idx, data)?;
        }
        dev.flush()?;
        Ok(())
    }

    /// Recover a device to a consistent state. Reads the commit record; if its magic is absent or its
    /// checksum does not verify (a half-written record or torn journal), the transaction is treated as
    /// uncommitted and NOTHING is applied. Otherwise the journal is replayed into the home blocks
    /// (idempotent). Returns whether a committed transaction was replayed.
    pub fn recover<D: BlockDevice>(&mut self, dev: &mut D) -> Result<bool, StorageError> {
        let n = dev.num_blocks();
        let mut rec = [0u8; BLOCK_SIZE];
        dev.read_block(0, &mut rec)?;
        if le64(&rec, OFF_MAGIC) != MAGIC {
            return Ok(false); // no committed transaction on this device
        }
        let count = le64(&rec, OFF_COUNT) as usize;
        if count > MAX_ENTRIES {
            return Ok(false); // implausible record → treat as uncommitted (fail closed)
        }
        // Read the journal payload and recompute the checksum over header + payload.
        let mut payload: Vec<[u8; BLOCK_SIZE]> = Vec::with_capacity(count);
        for i in 0..count {
            let mut b = [0u8; BLOCK_SIZE];
            dev.read_block(JOURNAL_START + i, &mut b)?;
            payload.push(b);
        }
        let mut ck = fnv1a(0xcbf2_9ce4_8422_2325, &rec[..OFF_CKSUM]);
        for b in &payload {
            ck = fnv1a(ck, b);
        }
        if ck != le64(&rec, OFF_CKSUM) {
            return Ok(false); // torn commit record or torn journal → uncommitted, roll back
        }
        // Valid commit → replay the journal into the home blocks (idempotent).
        for (i, b) in payload.iter().enumerate() {
            let idx = le64(&rec, OFF_INDICES + i * 8) as usize;
            if idx < DATA_START || idx >= n {
                return Err(StorageError::OutOfRange);
            }
            dev.write_block(idx, b)?;
        }
        dev.flush()?;
        self.seq = le64(&rec, OFF_SEQ);
        Ok(true)
    }

    /// Read a home block's current contents (post-recovery, this is the consistent value).
    pub fn read<D: BlockDevice>(
        &self,
        dev: &D,
        home_idx: usize,
    ) -> Result<[u8; BLOCK_SIZE], StorageError> {
        if home_idx < DATA_START || home_idx >= dev.num_blocks() {
            return Err(StorageError::OutOfRange);
        }
        let mut b = [0u8; BLOCK_SIZE];
        dev.read_block(home_idx, &mut b)?;
        Ok(b)
    }
}

impl Default for Journal {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple in-`alloc` block device backing store (no I/O) usable both as a default device and as the
/// substrate a crash-injecting test wraps. Kept here (not just in tests) so a hosted tool or a future
/// RAM-disk can reuse it.
pub struct MemBlockDevice {
    blocks: Vec<[u8; BLOCK_SIZE]>,
}

impl MemBlockDevice {
    pub fn new(num_blocks: usize) -> Self {
        MemBlockDevice {
            blocks: vec![[0u8; BLOCK_SIZE]; num_blocks],
        }
    }

    /// Snapshot every block (for crash-sweep materialization in tests).
    pub fn snapshot(&self) -> Vec<[u8; BLOCK_SIZE]> {
        self.blocks.clone()
    }

    /// Restore from a snapshot.
    pub fn restore(&mut self, snap: &[[u8; BLOCK_SIZE]]) {
        self.blocks = snap.to_vec();
    }

    /// Directly set a block (test setup of pre-state; bypasses the journal).
    pub fn poke(&mut self, idx: usize, data: [u8; BLOCK_SIZE]) {
        self.blocks[idx] = data;
    }
}

impl BlockDevice for MemBlockDevice {
    fn num_blocks(&self) -> usize {
        self.blocks.len()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        if idx >= self.blocks.len() {
            return Err(StorageError::OutOfRange);
        }
        if buf.len() != BLOCK_SIZE {
            return Err(StorageError::BadBlockSize);
        }
        buf.copy_from_slice(&self.blocks[idx]);
        Ok(())
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        if idx >= self.blocks.len() {
            return Err(StorageError::OutOfRange);
        }
        if buf.len() != BLOCK_SIZE {
            return Err(StorageError::BadBlockSize);
        }
        self.blocks[idx].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}
