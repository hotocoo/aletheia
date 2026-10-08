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
    /// Every block read, in order: the trace the policies are compared on (ADR-243).
    trace: std::cell::RefCell<Vec<usize>>,
    writes: Vec<(usize, [u8; BLOCK_SIZE])>,
    chaos: Option<Cell<u64>>,
}

impl Probe {
    fn new(n: usize) -> Self {
        Probe {
            blocks: vec![[0u8; BLOCK_SIZE]; n],
            reads: Cell::new(0),
            trace: Default::default(),
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
        self.trace.borrow_mut().push(idx);
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
    // A slot parked by a failed write (index usize::MAX) never answers a read of that index.
    let mut dev = Probe::new(4);
    dev.chaos = Some(Cell::new(1));
    let mut parked = BlockCache::new(dev, 2);
    parked.read_block(0, &mut buf).ok();
    while parked.write_block(0, &[9u8; BLOCK_SIZE]).is_ok() {}
    assert_eq!(
        parked.read_block(usize::MAX, &mut buf),
        Err(StorageError::OutOfRange)
    );
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
    workload_of(dev, 12)
}

/// The namespace workload over `files` objects of growing size.
fn workload_of<D: BlockDevice>(dev: &mut D, files: u64) -> Vec<String> {
    let mut out = Vec::new();
    Filesystem::format(dev).unwrap();
    let mut fs = Filesystem::mount(dev).unwrap();
    for i in 0..files as usize {
        fs.create(dev, &format!("f{i}"), &vec![i as u8; 100 + (i % 12) * 700])
            .unwrap();
    }
    let mut rng = Rng(0xC0FFEE);
    for _ in 0..400 {
        let name = format!("f{}", rng.below(files));
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

/// Misses of the cache's CLOCK policy on a read trace, simulated (read-allocate, one reference bit,
/// a sweeping hand), so it can be checked against the real cache and compared with the others.
fn clock_misses(trace: &[usize], cap: usize) -> u64 {
    let (mut slots, mut hand, mut miss): (Vec<(usize, bool)>, usize, u64) = (Vec::new(), 0, 0);
    for &b in trace {
        if let Some(s) = slots.iter_mut().find(|s| s.0 == b) {
            s.1 = true;
            continue;
        }
        miss += 1;
        if slots.len() < cap {
            slots.push((b, false));
            continue;
        }
        while slots[hand].1 {
            slots[hand].1 = false;
            hand = (hand + 1) % cap;
        }
        slots[hand] = (b, false);
        hand = (hand + 1) % cap;
    }
    miss
}

fn lru_misses(trace: &[usize], cap: usize) -> u64 {
    let (mut order, mut miss): (Vec<usize>, u64) = (Vec::new(), 0);
    for &b in trace {
        if let Some(i) = order.iter().position(|&x| x == b) {
            order.remove(i);
        } else {
            miss += 1;
            if order.len() == cap {
                order.remove(0);
            }
        }
        order.push(b);
    }
    miss
}

/// Belady's optimum with bypass: on a miss, keep whichever of the cached blocks and the new one
/// is needed soonest, evicting (or not admitting) the one needed latest. No policy, learned or
/// not, misses less on this trace.
fn opt_misses(trace: &[usize], cap: usize) -> u64 {
    let next_use = |from: usize, b: usize| {
        trace[from..]
            .iter()
            .position(|&x| x == b)
            .map_or(usize::MAX, |p| from + p)
    };
    let (mut cached, mut miss): (Vec<usize>, u64) = (Vec::new(), 0);
    for (i, &b) in trace.iter().enumerate() {
        if cached.contains(&b) {
            continue;
        }
        miss += 1;
        if cached.len() < cap {
            cached.push(b);
            continue;
        }
        let (far, at) = cached
            .iter()
            .enumerate()
            .map(|(k, &c)| (next_use(i + 1, c), k))
            .max()
            .unwrap();
        if far > next_use(i + 1, b) {
            cached[at] = b;
        }
    }
    miss
}

/// ADR-243: before any learned eviction policy, the measurement that says whether one could pay.
/// CLOCK is compared with LRU and with the optimum no policy can beat, on the namespace trace.
#[test]
fn clock_is_measured_against_the_optimum_on_the_namespace_trace() {
    let mut plain = Probe::new(kernel_core::fs::FILE_DATA_START + 64);
    workload(&mut plain);
    let trace = plain.trace.borrow().clone();

    let mut cached = BlockCache::new(
        Probe::new(kernel_core::fs::FILE_DATA_START + 64),
        CONSOLE_BLOCKS,
    );
    workload(&mut cached);
    // The simulation is the cache: same misses on the same trace.
    assert_eq!(clock_misses(&trace, CONSOLE_BLOCKS), cached.stats().misses);

    for cap in [8, 16, CONSOLE_BLOCKS, 64] {
        let (c, l, o) = (
            clock_misses(&trace, cap),
            lru_misses(&trace, cap),
            opt_misses(&trace, cap),
        );
        eprintln!(
            "[bcache] {} reads, {cap} blocks: CLOCK {c} misses, LRU {l}, optimum {o} (CLOCK within {:.1}% of all reads of the optimum)",
            trace.len(),
            100.0 * (c - o) as f64 / trace.len() as f64
        );
        assert!(o <= c && o <= l);
    }
    // At the shipped size CLOCK IS the optimum: no policy has a miss left to save.
    assert_eq!(
        clock_misses(&trace, CONSOLE_BLOCKS),
        opt_misses(&trace, CONSOLE_BLOCKS)
    );
}

/// The same comparison on a namespace four times larger, whose working set no longer fits: what is
/// left for a smarter policy at the shipped size (ADR-243).
#[test]
fn on_a_larger_namespace_clock_stays_near_the_optimum() {
    let mut plain = Probe::new(kernel_core::fs::FILE_DATA_START + 256);
    workload_of(&mut plain, 48);
    let trace = plain.trace.borrow().clone();
    let (c, l, o) = (
        clock_misses(&trace, CONSOLE_BLOCKS),
        lru_misses(&trace, CONSOLE_BLOCKS),
        opt_misses(&trace, CONSOLE_BLOCKS),
    );
    eprintln!(
        "[bcache] larger namespace: {} reads, {CONSOLE_BLOCKS} blocks: CLOCK {c}, LRU {l}, optimum {o}",
        trace.len()
    );
    assert!(
        o <= c && c * 10 <= trace.len() as u64 * 6,
        "{c} of {}",
        trace.len()
    );
}

/// ADR-248: a run read answers exactly what block reads answer, through the default path, the
/// cache (cached blocks from memory, each stretch of misses as one device run) and the filesystem.
#[test]
fn run_reads_answer_what_block_reads_answer() {
    let mut plain = Probe::new(kernel_core::fs::FILE_DATA_START + 64);
    workload(&mut plain);
    // Fewer blocks than the cache holds, so no warmed block is evicted mid-run.
    let n = 20;
    let start = kernel_core::fs::FILE_DATA_START;
    let mut want = vec![0u8; n * BLOCK_SIZE];
    for i in 0..n {
        plain
            .read_block(start + i, &mut want[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE])
            .unwrap();
    }
    let mut got = vec![0u8; n * BLOCK_SIZE];
    plain.read_run(start, &mut got).unwrap();
    assert_eq!(got, want);

    let cached = BlockCache::new(plain, CONSOLE_BLOCKS);
    // Warm a scattered few, then read the whole run: hits and misses interleave.
    let mut one = [0u8; BLOCK_SIZE];
    for i in [3, 4, 17, 19] {
        cached.read_block(start + i, &mut one).unwrap();
    }
    let before = cached.stats();
    let mut through = vec![0u8; n * BLOCK_SIZE];
    cached.read_run(start, &mut through).unwrap();
    assert_eq!(through, want);
    let after = cached.stats();
    assert_eq!(after.hits - before.hits, 4);
    assert_eq!(after.misses - before.misses, (n - 4) as u64);

    let mut bad = vec![0u8; BLOCK_SIZE + 1];
    assert!(cached.read_run(start, &mut bad).is_err());
}
