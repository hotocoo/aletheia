//! The namespace's block cache (ADR-232): it never answers a read with bytes the device does not
//! hold, it leaves the device's write sequence (and so every crash property) unchanged, and it
//! removes most of a namespace workload's device reads.

use std::cell::Cell;

use kernel_core::bcache::{BlockCache, CONSOLE_BLOCKS};
use kernel_core::fs::Filesystem;
use kernel_core::storage::{BlockDevice, StorageError, BLOCK_SIZE};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A device that counts reads, logs writes in order, and (when `chaos` is on) fails some writes
/// after tearing the block, and fails some reads.
struct Probe {
    blocks: Vec<[u8; BLOCK_SIZE]>,
    reads: Cell<u64>,
    writes: Vec<(usize, [u8; BLOCK_SIZE])>,
    chaos: Option<Cell<u64>>,
}

impl Probe {
    fn new(n: usize) -> Self {
        Probe {
            blocks: vec![[0u8; BLOCK_SIZE]; n],
            reads: Cell::new(0),
            writes: Vec::new(),
            chaos: None,
        }
    }
    fn roll(&self, one_in: u64) -> bool {
        self.chaos.as_ref().is_some_and(|c| {
            let mut r = Rng(c.get());
            let hit = r.below(one_in) == 0;
            c.set(r.0);
            hit
        })
    }
}

impl BlockDevice for Probe {
    fn num_blocks(&self) -> usize {
        self.blocks.len()
    }
    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), StorageError> {
        if self.roll(9) {
            return Err(StorageError::Device);
        }
        self.reads.set(self.reads.get() + 1);
        buf.copy_from_slice(&self.blocks.get(idx).ok_or(StorageError::OutOfRange)?[..]);
        Ok(())
    }
    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), StorageError> {
        if idx >= self.blocks.len() {
            return Err(StorageError::OutOfRange);
        }
        if self.roll(5) {
            // A torn write: half the new bytes land, and the device says it failed.
            self.blocks[idx][..BLOCK_SIZE / 2].copy_from_slice(&buf[..BLOCK_SIZE / 2]);
            return Err(StorageError::Device);
        }
        let mut b = [0u8; BLOCK_SIZE];
        b.copy_from_slice(buf);
        self.writes.push((idx, b));
        self.blocks[idx] = b;
        Ok(())
    }
    fn flush(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}

#[test]
fn a_read_from_the_cache_is_always_what_the_device_holds() {
    for seed in 1..=40u64 {
        let mut dev = Probe::new(32);
        dev.chaos = Some(Cell::new(seed * 7919));
        let mut cache = BlockCache::new(dev, 6);
        let mut rng = Rng(seed);
        let mut buf = [0u8; BLOCK_SIZE];
        for step in 0..3000 {
            // Skewed toward a few hot blocks, as a namespace's directory and bitmap are.
            let idx = if rng.below(3) == 0 {
                rng.below(32) as usize
            } else {
                rng.below(4) as usize
            };
            if rng.below(3) == 0 {
                let fill = [(rng.next() & 0xFF) as u8; BLOCK_SIZE];
                let _ = cache.write_block(idx, &fill);
            } else if cache.read_block(idx, &mut buf).is_ok() {
                assert_eq!(
                    buf,
                    cache.inner().blocks[idx],
                    "seed {seed} step {step}: block {idx} answered from a stale copy"
                );
            }
        }
        let s = cache.stats();
        assert!(
            s.hits > 0 && s.misses > 0 && s.evictions > 0,
            "seed {seed}: {s:?}"
        );
    }
}

#[test]
fn a_bad_buffer_or_a_missing_block_is_refused_and_nothing_is_kept() {
    let cache = BlockCache::new(Probe::new(4), 4);
    assert_eq!(
        cache.read_block(0, &mut [0u8; 16]),
        Err(StorageError::BadBlockSize)
    );
    let mut buf = [0u8; BLOCK_SIZE];
    assert_eq!(cache.read_block(9, &mut buf), Err(StorageError::OutOfRange));
    assert_eq!(cache.stats().hits + cache.stats().misses, 0);
    // Capacity 0 passes every read through.
    let none = BlockCache::new(Probe::new(4), 0);
    for _ in 0..3 {
        none.read_block(1, &mut buf).unwrap();
    }
    assert_eq!(none.inner().reads.get(), 3);
}

/// A namespace workload: seed files, then a mix of reads, stats, listings and replacements, as the
/// console issues them. Returns the device afterwards and every answer the namespace gave.
fn workload<D: BlockDevice>(dev: &mut D) -> Vec<String> {
    let mut out = Vec::new();
    Filesystem::format(dev).unwrap();
    let mut fs = Filesystem::mount(dev).unwrap();
    for i in 0..12 {
        fs.create(dev, &format!("f{i}"), &vec![i as u8; 100 + i * 700])
            .unwrap();
    }
    let mut rng = Rng(0xC0FFEE);
    for _ in 0..400 {
        let name = format!("f{}", rng.below(12));
        match rng.below(10) {
            0..=5 => out.push(format!("{:?}", fs.read(dev, &name).map(|d| d.len()))),
            6 => out.push(format!("{:?}", fs.stat(dev, &name).map(|e| e.len))),
            7 => out.push(format!("{:?}", fs.list(dev).map(|l| l.len()))),
            _ => {
                let len = rng.below(5000) as usize;
                out.push(format!("{:?}", fs.replace(dev, &name, &vec![7u8; len])));
            }
        }
    }
    out
}

#[test]
fn the_cache_changes_no_answer_and_no_write_and_saves_most_reads() {
    let mut plain = Probe::new(kernel_core::fs::FILE_DATA_START + 64);
    let answers = workload(&mut plain);

    let mut cached = BlockCache::new(
        Probe::new(kernel_core::fs::FILE_DATA_START + 64),
        CONSOLE_BLOCKS,
    );
    let cached_answers = workload(&mut cached);
    let s = cached.stats();
    let dev = cached.inner();

    assert_eq!(
        answers, cached_answers,
        "the namespace answered differently"
    );
    assert_eq!(
        plain.writes, dev.writes,
        "the device saw a different write sequence"
    );
    assert_eq!(plain.blocks, dev.blocks, "the device ended differently");
    let (before, after) = (plain.reads.get(), dev.reads.get());
    eprintln!(
        "[bcache] device reads {before} -> {after} ({:.0}% saved), {s:?}",
        100.0 * (before - after) as f64 / before as f64
    );
    assert_eq!(after, s.misses);
    assert!(
        after * 3 <= before,
        "{before} -> {after}: under two thirds saved"
    );
}
