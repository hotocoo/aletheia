//! A write-through block cache under the namespace (ADR-232).
//!
//! Every namespace operation reads the directory block, and every write reads the bitmap too
//! (`fs.rs`), so without a cache the same two blocks cross the device on every command. The cache
//! keeps a fixed number of recently read blocks in memory and answers repeat reads from them.
//!
//! * **Write-through.** A write goes to the device first and is acknowledged only when the device
//!   acknowledged it, so the device's state, and therefore every crash and recovery property the
//!   journal proves (`storage.rs`), is exactly what it would be without the cache.
//! * **Read-allocate, write-update.** A block enters the cache only when it is read. A write
//!   refreshes a cached copy but never brings a block in: a journal commit writes up to 64 slots
//!   that are read again only by recovery, and letting them in would push out the blocks every
//!   command reads.
//! * **A failed device call caches nothing,** and a failed write drops any cached copy of that
//!   block, because the device's content is then unknown.
//! * **CLOCK eviction** (one reference bit per slot, a sweeping hand): hot blocks survive a pass of
//!   one-time reads at O(1) cost and no per-access bookkeeping beyond one bit.
//!
//! The cache sits BELOW the capability guard (`device.rs`), so every request is still authorized;
//! only device traffic is saved. Slots are allocated once, on first use, and kept for the cache's
//! life (the kernel heap is grown, not churned).
use core::cell::{Cell, RefCell};

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::storage::{BlockDevice, StorageError, BLOCK_SIZE};

/// Blocks the console's namespace cache holds: 64 KiB.
pub const CONSOLE_BLOCKS: usize = 16;

struct Slot {
    idx: usize,
    referenced: bool,
    data: Box<[u8; BLOCK_SIZE]>,
}

/// What the cache has saved, for the operator and the gates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Reads answered from memory.
    pub hits: u64,
    /// Reads that went to the device.
    pub misses: u64,
    /// Blocks pushed out to make room.
    pub evictions: u64,
}

/// A [`BlockDevice`] that answers repeat reads of `D` from memory.
pub struct BlockCache<D: BlockDevice> {
    dev: D,
    capacity: usize,
    slots: RefCell<Vec<Slot>>,
    hand: Cell<usize>,
    stats: Cell<CacheStats>,
}

impl<D: BlockDevice> BlockCache<D> {
    /// A cache of at most `capacity` blocks over `dev` (0 caches nothing).
    pub fn new(dev: D, capacity: usize) -> Self {
        BlockCache {
            dev,
            capacity,
            slots: RefCell::new(Vec::new()),
            hand: Cell::new(0),
            stats: Cell::new(CacheStats::default()),
        }
    }

    pub fn stats(&self) -> CacheStats {
        self.stats.get()
    }

    /// The device underneath, for callers that must reach it directly.
    pub fn inner(&self) -> &D {
        &self.dev
    }

    fn count(&self, f: impl FnOnce(&mut CacheStats)) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }

    /// Keep `buf` as block `idx`: a free slot while there is one, else the first slot the hand
    /// finds unreferenced (clearing reference bits as it passes).
    fn insert(&self, idx: usize, buf: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        let mut slots = self.slots.borrow_mut();
        if slots.len() < self.capacity {
            let mut data = Box::new([0u8; BLOCK_SIZE]);
            data.copy_from_slice(buf);
            slots.push(Slot {
                idx,
                referenced: false,
                data,
            });
            return;
        }
        let mut hand = self.hand.get();
        while slots[hand].referenced {
            slots[hand].referenced = false;
            hand = (hand + 1) % slots.len();
        }
        let slot = &mut slots[hand];
        slot.idx = idx;
        slot.referenced = false;
        slot.data.copy_from_slice(buf);
        self.hand.set((hand + 1) % slots.len());
        drop(slots);
        self.count(|s| s.evictions += 1);
    }
}

impl<D: BlockDevice> BlockDevice for BlockCache<D> {
    fn num_blocks(&self) -> usize {
        self.dev.num_blocks()
    }

    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        if buf.len() != BLOCK_SIZE {
            return Err(StorageError::BadBlockSize);
        }
        if let Some(slot) = self.slots.borrow_mut().iter_mut().find(|s| s.idx == idx) {
            slot.referenced = true;
            buf.copy_from_slice(&slot.data[..]);
            self.count(|s| s.hits += 1);
            return Ok(());
        }
        self.dev.read_block(idx, buf)?;
        self.count(|s| s.misses += 1);
        self.insert(idx, buf);
        Ok(())
    }

    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        let wrote = self.dev.write_block(idx, buf);
        let mut slots = self.slots.borrow_mut();
        if let Some(at) = slots.iter().position(|s| s.idx == idx) {
            match wrote {
                Ok(()) => slots[at].data.copy_from_slice(buf),
                // The device's copy is unknown now: the next read must ask it. The slot is
                // marked unused (an index no device has) rather than removed, so the vector
                // never shrinks and the hand stays valid.
                Err(_) => {
                    slots[at].idx = usize::MAX;
                    slots[at].referenced = false;
                }
            }
        }
        wrote
    }

    fn flush(&mut self) -> Result<(), StorageError> {
        self.dev.flush()
    }
}
