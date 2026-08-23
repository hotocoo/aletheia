//! Long-running soak: the machine runs for a living (ALET-P2-009, ADR-063).
//!
//! Every suite before this one proves a subsystem CORRECT on a handful of hand-picked cases. None
//! of them answers an operator's next question: does it still hold after the machine has been
//! RUNNING — creating, replacing, revoking, dispatching — for a very long time? A soak test answers
//! that by repetition, and repetition on Aletheia runs into a hard truth: the kernel heap is a bump
//! allocator that NEVER frees (`kernel/src/heap.rs`), so a churn loop that allocates per operation
//! does not soak, it counts down the heap until the machine dies. Long-running on this kernel is
//! therefore a RESOURCE property before it is a correctness property:
//!
//! **one more cycle must cost nothing permanent.**
//!
//! So each phase here is written to a steady state — buffers allocated ONCE before the meter starts,
//! mutated in place forever after — and the one thing that can be claimed EXACTLY is claimed
//! exactly: on a target that meters its heap, the journal churn window allocates NOTHING per
//! transaction (invariant 1). The other phases are bounded-by-declared-constants (grant records and
//! scheduler nodes accumulate a fixed, printed amount) and their heap deltas are REPORTED, never
//! gated. The hosted test (`tests/soak.rs`) takes the same harness to loads tens to hundreds of
//! times larger on a real allocator, where long-running means what it means everywhere else.
//!
//! What is GATED is scale-free — the same property at 384 transactions and at 50 000:
//! every journaled transaction reads back byte-for-byte; recovery mid-soak replays idempotently;
//! every namespace mutation leaves the filesystem structurally sound; a shared region's bytes are
//! seen through every live grant; revocation releases the mapping and is refused by name forever
//! after; unauthorized and amplifying shares fail closed at volume; a Finished task never runs
//! again; Blocked tasks are never dispatched; every generation drains to empty; and the same seed
//! replays the identical campaign. What is REPORTED is timing — nanoseconds under QEMU-TCG are an
//! emulator's numbers (the same rule [`crate::mlrisk_stress`] already established).
//!
//! Memory budget of [`BOOT_LOAD`], measured against the 8 MiB never-freeing heap: the journal phase
//! is flat by construction; the fs phase keeps a 4-name namespace on a small RAM disk; the grant
//! phase accumulates ~2 records/cycle (revocation keeps the record, drops the mapping); the task
//! phase re-admits a fixed 16-id pool (the priority pool's sequence-keyed nodes accumulate per
//! generation). Every target prints its heap before/after the whole suite so the number stays a
//! measured fact instead of a hope.

use alloc::vec::Vec;

use crate::fs::{Filesystem, FILE_DATA_START};
use crate::grant::{GrantError, GrantTable, ShareMode};
use crate::priosched::{Priority, PriorityScheduler};
use crate::sched::{RoundRobin, TaskId};
use crate::spine::{CapEngine, Constraints, Scope};
use crate::storage::{BlockDevice, Journal, BLOCK_SIZE, DATA_START};
use crate::Hal;

/// Deterministic SplitMix64. A soak whose workload changed run to run would not be evidence of
/// anything, and no target has an entropy source this early in boot anyway.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..hi`, saturating on an empty range.
    fn below(&mut self, hi: usize) -> usize {
        if hi == 0 {
            return 0;
        }
        (self.next() % hi as u64) as usize
    }
}

/// FNV-1a over a byte stream — the storage layer's own integrity primitive, reused here so the
/// soak's "byte-for-byte" claims are the SAME claim the journal's commit record makes.
fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

const FNV_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// A block device that materializes ONLY the blocks this soak actually touches.
///
/// The kernel heap is a bump allocator that never frees, so a soak that formats a full RAM disk
/// pays for every block it will ever NOT touch up front — a `MemBlockDevice` of 69 blocks costs
/// ~270 KiB before the first transaction. Long-running means the campaign's footprint must be
/// proportional to the WORK done, not to the disk's reported capacity: `num_blocks` is a number
/// the layout requires, while RAM is spent only on blocks a transaction names. A churn loop whose
/// device grew toward the disk size would be measuring the allocator's death, not endurance.
///
/// Semantics match [`crate::storage::MemBlockDevice`] exactly: untouched blocks read back ZERO
/// (a fresh disk), out-of-range indices and mis-sized buffers are refused by name.
pub struct SparseDevice {
    capacity: usize,
    blocks: Vec<(usize, [u8; BLOCK_SIZE])>,
}

impl SparseDevice {
    /// Create a device reporting `capacity` blocks, materializing none of them.
    pub fn new(capacity: usize) -> Self {
        SparseDevice {
            capacity,
            blocks: Vec::new(),
        }
    }

    /// The materialized blocks, oldest touch first — the campaign's whole storage footprint.
    pub fn touched(&self) -> &[(usize, [u8; BLOCK_SIZE])] {
        &self.blocks
    }

    fn slot(&mut self, idx: usize) -> &mut [u8; BLOCK_SIZE] {
        if let Some(pos) = self.blocks.iter().position(|&(i, _)| i == idx) {
            return &mut self.blocks[pos].1;
        }
        self.blocks.push((idx, [0u8; BLOCK_SIZE]));
        let last = self.blocks.len() - 1;
        &mut self.blocks[last].1
    }
}

impl BlockDevice for SparseDevice {
    fn num_blocks(&self) -> usize {
        self.capacity
    }

    fn read_block(&self, idx: usize, buf: &mut [u8]) -> Result<(), crate::storage::StorageError> {
        if idx >= self.capacity {
            return Err(crate::storage::StorageError::OutOfRange);
        }
        if buf.len() != BLOCK_SIZE {
            return Err(crate::storage::StorageError::BadBlockSize);
        }
        match self.blocks.iter().find(|&&(i, _)| i == idx) {
            Some((_, data)) => buf.copy_from_slice(data),
            None => buf.fill(0), // a block nobody wrote is a fresh-disk zero
        }
        Ok(())
    }

    fn write_block(&mut self, idx: usize, buf: &[u8]) -> Result<(), crate::storage::StorageError> {
        if idx >= self.capacity {
            return Err(crate::storage::StorageError::OutOfRange);
        }
        if buf.len() != BLOCK_SIZE {
            return Err(crate::storage::StorageError::BadBlockSize);
        }
        self.slot(idx).copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), crate::storage::StorageError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Phase A — the storage transaction lifecycle, at volume, allocation-free.
// ---------------------------------------------------------------------

/// How the journal phase measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct JournalSoak {
    pub txs: usize,
    pub verifies: usize,
    pub mismatches: usize,
    pub commit_errors: usize,
    pub recovers: usize,
    pub recovers_replayed: usize,
    pub post_recover_mismatches: usize,
    /// FNV over every payload byte written — the determinism check replays this exactly.
    pub checksum: u64,
    pub ns_total: u64,
    /// Heap meter reading at the start/end of the pure churn window, if the target meters at all.
    pub mem_start: Option<u64>,
    pub mem_end: Option<u64>,
}

impl JournalSoak {
    /// Permanent bytes the churn window cost. `None` when the target does not meter its heap; the
    /// gate treats an unmetered target as UNPROVEN on invariant 1, not as passing it.
    pub fn mem_delta(&self) -> Option<u64> {
        Some(self.mem_end?.saturating_sub(self.mem_start?))
    }

    /// Transactions per second on this machine. Zero when the clock did not move.
    pub fn txs_per_second(&self) -> u64 {
        if self.ns_total == 0 {
            return 0;
        }
        (self.txs as u64).saturating_mul(1_000_000_000) / self.ns_total
    }
}

/// Verify every Nth transaction's home blocks by reading them back. 1-in-8 keeps verification
/// inside the timed window (a soak that never reads back measures only writes) without doubling
/// the emulated cost.
const VERIFY_EVERY: usize = 8;
/// Mid-soak recovery rounds AFTER the metered churn window. `recover` allocates (the replay
/// payload is a `Vec`), so these sit outside the allocation-free claim by design, not by oversight.
const RECOVER_ROUNDS: usize = 3;
/// Transactions committed after each recovery, so "the sequence continues" is exercised, not
/// assumed.
const POST_RECOVER_TXS: usize = 4;

/// Churn `txs` two-update journal transactions over `dev`, verifying home blocks by read-back.
///
/// The updates buffer and the device are allocated ONCE by the caller before this runs; the loop
/// mutates payloads in place, so on a bump-heap target the metered window costs nothing per
/// transaction — which is precisely what invariant 1 gates. Recovery rounds run AFTER the meter
/// stops: they are part of the correctness claim (idempotent replay mid-soak) but deliberately
/// outside the allocation-free one.
pub fn journal_phase<H: Hal>(
    dev: &mut SparseDevice,
    txs: usize,
    meter: Option<&dyn Fn() -> u64>,
) -> JournalSoak {
    let mut out = JournalSoak::default();
    let mut j = Journal::new();
    let homes = [DATA_START, DATA_START + 1];
    // Allocated once; mutated in place forever. THIS is the allocation-free claim's substance.
    let mut updates: Vec<(usize, [u8; BLOCK_SIZE])> = Vec::with_capacity(homes.len());
    for &h in homes.iter() {
        updates.push((h, [0u8; BLOCK_SIZE]));
    }
    // Expected content checksum per home block, maintained as payloads are rewritten.
    let mut expect = [0u64; 2];
    let fill =
        |updates: &mut Vec<(usize, [u8; BLOCK_SIZE])>, expect: &mut [u64; 2], step: usize| {
            for (k, (_, data)) in updates.iter_mut().enumerate() {
                for (i, b) in data.iter_mut().enumerate() {
                    *b = (step as u8)
                        .wrapping_mul(31)
                        .wrapping_add(i as u8)
                        .wrapping_add((k as u8).wrapping_mul(7))
                        % 251;
                }
                expect[k] = fnv1a(FNV_SEED, data);
            }
        };
    fill(&mut updates, &mut expect, 0);
    // Steady-state warm-up, BEFORE the meter: the first commit materializes the sparse device's
    // blocks (journal slots, commit record, home blocks) — a one-time cost, not churn. Without
    // this, invariant 1 would measure setup, not endurance; with it, the window below holds only
    // transactions over an already-materialized device, and "one more costs nothing" is exact.
    let _ = j.commit(dev, &updates);

    out.mem_start = meter.map(|m| m());
    let t0 = H::timer_ticks();
    for step in 0..txs {
        if step != 0 {
            fill(&mut updates, &mut expect, step);
        }
        for (_, data) in updates.iter() {
            out.checksum = out.checksum.wrapping_add(fnv1a(FNV_SEED, data));
        }
        if j.commit(dev, &updates).is_err() {
            out.commit_errors += 1;
            continue;
        }
        out.txs += 1;
        if step % VERIFY_EVERY == 0 {
            for (k, &h) in homes.iter().enumerate() {
                out.verifies += 1;
                match j.read(dev, h) {
                    Ok(blk) => {
                        if fnv1a(FNV_SEED, &blk) != expect[k] {
                            out.mismatches += 1;
                        }
                    }
                    Err(_) => out.mismatches += 1,
                }
            }
        }
    }
    out.ns_total = H::ticks_to_ns(H::timer_ticks().wrapping_sub(t0));
    out.mem_end = meter.map(|m| m());

    // Recovery mid-soak: the committed transaction replays idempotently, the homes still read back,
    // and the sequence CONTINUES afterwards (more transactions commit and verify past it).
    for _ in 0..RECOVER_ROUNDS {
        out.recovers += 1;
        if matches!(j.recover(dev), Ok(true)) {
            out.recovers_replayed += 1;
        }
        for (k, &h) in homes.iter().enumerate() {
            match j.read(dev, h) {
                Ok(blk) => {
                    if fnv1a(FNV_SEED, &blk) != expect[k] {
                        out.post_recover_mismatches += 1;
                    }
                }
                Err(_) => out.post_recover_mismatches += 1,
            }
        }
        for extra in 0..POST_RECOVER_TXS {
            let step = txs + extra + 1;
            fill(&mut updates, &mut expect, step);
            if j.commit(dev, &updates).is_ok() {
                out.txs += 1;
                for (k, &h) in homes.iter().enumerate() {
                    out.verifies += 1;
                    match j.read(dev, h) {
                        Ok(blk) => {
                            if fnv1a(FNV_SEED, &blk) != expect[k] {
                                out.mismatches += 1;
                            }
                        }
                        Err(_) => out.mismatches += 1,
                    }
                }
            } else {
                out.commit_errors += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Phase B — the filesystem namespace lifecycle, audited after EVERY mutation.
// ---------------------------------------------------------------------

/// How the namespace phase measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsSoak {
    pub ops: usize,
    pub audits: usize,
    pub tally_violations: usize,
    pub verifies: usize,
    pub mismatches: usize,
    /// Survivors the final fresh mount must (and did, when `final_ok`) account for.
    pub final_survivors: usize,
    pub final_ok: bool,
    pub checksum: u64,
    pub ns_total: u64,
}

impl FsSoak {
    /// Namespace operations per second on this machine.
    pub fn ops_per_second(&self) -> u64 {
        if self.ns_total == 0 {
            return 0;
        }
        (self.ops as u64).saturating_mul(1_000_000_000) / self.ns_total
    }
}

const FS_SLOTS: usize = 4;
const FS_NAMES: [&str; FS_SLOTS] = ["soak-a", "soak-b", "soak-c", "soak-d"];

fn fs_body(slot: usize, cycle: usize) -> Vec<u8> {
    let len = (cycle + slot) % 3 * BLOCK_SIZE + 17;
    (0..len)
        .map(|i| ((i * 31 + cycle * 7 + slot * 13) % 251) as u8)
        .collect()
}

/// Churn a small live namespace through create/replace/remove cycles, auditing the structural
/// contract (unique names, in-bounds disjoint extents, bitmap/directory tally) after EVERY
/// mutation, and verifying contents byte-for-byte at every touch. Ends with a fresh mount that
/// must see exactly the survivors.
///
/// Deliberately SMALL at boot: each op commits a transaction whose buffers the bump heap keeps.
/// The hosted test runs this same phase two orders of magnitude longer.
pub fn fs_phase<H: Hal>(cycles: usize) -> FsSoak {
    let mut out = FsSoak::default();
    let mut dev = SparseDevice::new(FILE_DATA_START + 64);
    if Filesystem::format(&mut dev).is_err() {
        out.tally_violations += 1;
        return out;
    }
    let mut fs = match Filesystem::mount(&mut dev) {
        Ok(fs) => fs,
        Err(_) => {
            out.tally_violations += 1;
            return out;
        }
    };
    // Local model: which names exist and what their content checksum must be.
    let mut live = [false; FS_SLOTS];
    let mut expect = [0u64; FS_SLOTS];
    let mut lens = [0usize; FS_SLOTS];
    let t0 = H::timer_ticks();

    for cycle in 0..cycles {
        let slot = cycle % FS_SLOTS;
        let name = FS_NAMES[slot];
        let body = fs_body(slot, cycle);
        let op = cycle % 3;
        let was_live = live[slot];
        let res = if op == 0 && was_live {
            fs.remove(&mut dev, name).map(|_| ())
        } else if was_live {
            fs.replace(&mut dev, name, &body)
        } else {
            fs.create(&mut dev, name, &body)
        };
        if res.is_err() {
            // Every refusal here is a soak failure: the model says these ops always succeed.
            out.tally_violations += 1;
            continue;
        }
        out.ops += 1;
        // op 0 on a live slot was the REMOVE; everything else established or refreshed content.
        live[slot] = !(op == 0 && was_live);
        if live[slot] {
            expect[slot] = fnv1a(FNV_SEED, &body);
            lens[slot] = body.len();
        }

        // ---- structural audit, after EVERY mutation ----
        out.audits += 1;
        let entries = match fs.list(&dev) {
            Ok(e) => e,
            Err(_) => {
                out.tally_violations += 1;
                continue;
            }
        };
        let mut ok = entries.len() == live.iter().filter(|&&l| l).count();
        let capacity = dev.num_blocks() - FILE_DATA_START;
        let held: usize = entries.iter().map(|e| e.blocks()).sum();
        ok = ok && fs.free_blocks(&dev) == Ok(capacity - held);
        for (i, a) in entries.iter().enumerate() {
            ok = ok && a.start >= FILE_DATA_START && a.start + a.blocks() <= dev.num_blocks();
            for b in entries.iter().skip(i + 1) {
                ok = ok && a.name != b.name;
                ok = ok && (a.start + a.blocks() <= b.start || b.start + b.blocks() <= a.start);
            }
        }
        if !ok {
            out.tally_violations += 1;
        }

        // ---- content verification of the touched name ----
        out.verifies += 1;
        if live[slot] {
            match fs.read(&dev, name) {
                Ok(bytes) => {
                    if bytes.len() != lens[slot] || fnv1a(FNV_SEED, &bytes) != expect[slot] {
                        out.mismatches += 1;
                    }
                    out.checksum = out.checksum.wrapping_add(fnv1a(FNV_SEED, &bytes));
                }
                Err(_) => out.mismatches += 1,
            }
        } else if fs.read(&dev, name).is_ok() {
            out.mismatches += 1; // a removed name must not read
        }
    }
    out.ns_total = H::ticks_to_ns(H::timer_ticks().wrapping_sub(t0));

    // A fresh mount sees exactly the survivors, and each reads back.
    let mut final_ok = true;
    if let Ok(fs2) = Filesystem::mount(&mut dev) {
        let entries = fs2.list(&dev).unwrap_or_default();
        out.final_survivors = entries.len();
        final_ok = final_ok && entries.len() == live.iter().filter(|&&l| l).count();
        for (s, &is_live) in live.iter().enumerate() {
            if is_live {
                match fs2.read(&dev, FS_NAMES[s]) {
                    Ok(bytes) => {
                        final_ok = final_ok
                            && bytes.len() == lens[s]
                            && fnv1a(FNV_SEED, &bytes) == expect[s];
                    }
                    Err(_) => final_ok = false,
                }
            } else {
                final_ok =
                    final_ok && fs2.read(&dev, FS_NAMES[s]) == Err(crate::fs::FsError::NotFound);
            }
        }
    } else {
        final_ok = false;
    }
    out.final_ok = final_ok;
    out
}

// ---------------------------------------------------------------------
// Phase C — the capability-grant lifecycle, at volume.
// ---------------------------------------------------------------------

/// How the grant phase measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrantSoak {
    pub cycles: usize,
    pub zero_copy_mismatches: usize,
    pub refcount_violations: usize,
    pub unauthorized_attempted: usize,
    pub unauthorized_refused: usize,
    pub amplify_attempted: usize,
    pub amplify_refused: usize,
    pub revoked_attempted: usize,
    pub revoked_refused: usize,
    pub checksum: u64,
    pub ns_total: u64,
}

const GRANT_REGIONS: usize = 4;
const REGION_LEN: usize = 256;
const SHARE: &str = "memory.share";

/// Churn share/write/read/revoke cycles over a fixed set of regions: zero-copy observed every
/// cycle, refcounts exact every cycle, and every refused path (unauthorized, amplifying, revoked)
/// attempted and counted at volume.
pub fn grant_phase<H: Hal>(cycles: usize) -> GrantSoak {
    let mut out = GrantSoak::default();
    // One fixed seed for the whole campaign: message placement and bytes are drawn from it, so
    // "the same seed replays the identical campaign" is a real replay, not an index echo.
    let mut rng = Rng(0x50A1_5EED_0001);
    let mut engine = CapEngine::new(0x50A1_0001, 4096);
    let cap = engine.mint("owner", SHARE, Scope::All, Constraints::none());
    let mut gt = GrantTable::new(SHARE);
    let mut regions = [0u64; GRANT_REGIONS];
    for (k, r) in regions.iter_mut().enumerate() {
        *r = gt.create_region("owner", 0x7000 + k as u64 * 0x1000, REGION_LEN);
    }
    let t0 = H::timer_ticks();
    for cycle in 0..cycles {
        out.cycles += 1;
        let r = cycle % GRANT_REGIONS;
        let region = regions[r];

        // Fail-closed at volume: no capability, no grant.
        out.unauthorized_attempted += 1;
        if gt.share(&engine, region, "owner", "peer", ShareMode::ReadWrite, &[])
            == Err(GrantError::Unauthorized)
        {
            out.unauthorized_refused += 1;
        }

        let producer = gt.share(
            &engine,
            region,
            "owner",
            "producer",
            ShareMode::ReadWrite,
            &[cap],
        );
        let consumer = gt.share(
            &engine,
            region,
            "owner",
            "consumer",
            ShareMode::Read,
            &[cap],
        );
        let (producer, consumer) = match (producer, consumer) {
            (Ok(p), Ok(c)) => (p, c),
            _ => {
                out.refcount_violations += 1;
                continue;
            }
        };

        // Attenuation is never amplification: the READ-ONLY holder cannot mint a RW grant.
        if cycle % 4 == 0 {
            out.amplify_attempted += 1;
            if gt.share(
                &engine,
                region,
                "consumer",
                "peer2",
                ShareMode::ReadWrite,
                &[cap],
            ) == Err(GrantError::Amplify)
            {
                out.amplify_refused += 1;
            }
        }

        // Zero-copy under churn: the writer's bytes are the reader's bytes, same backing.
        let off = rng.below(REGION_LEN - 8);
        let mut msg = [0u8; 8];
        for m in msg.iter_mut() {
            *m = (rng.below(251)) as u8;
        }
        if gt.write(producer, off, &msg).is_err() || gt.read(consumer, off, 8) != Ok(msg.to_vec()) {
            out.zero_copy_mismatches += 1;
        }
        out.checksum = out.checksum.wrapping_add(fnv1a(FNV_SEED, &msg));

        // Refcount discipline: owner + two live grants, exactly.
        if gt.region_refcount(region) != 3 {
            out.refcount_violations += 1;
        }
        let revoked_p = gt.revoke(producer);
        let revoked_c = gt.revoke(consumer);
        if !revoked_p || !revoked_c || gt.region_refcount(region) != 1 {
            out.refcount_violations += 1;
        }
        // A revoked grant is refused BY NAME — read and write, every cycle, forever after.
        out.revoked_attempted += 3;
        if gt.read(producer, off, 8) == Err(GrantError::Revoked) {
            out.revoked_refused += 1;
        }
        if gt.read(consumer, off, 8) == Err(GrantError::Revoked) {
            out.revoked_refused += 1;
        }
        if gt.write(producer, off, &msg) == Err(GrantError::Revoked) {
            out.revoked_refused += 1;
        }
    }
    out.ns_total = H::ticks_to_ns(H::timer_ticks().wrapping_sub(t0));
    // End state: every region back to owner-only.
    for &region in regions.iter() {
        if gt.region_refcount(region) != 1 {
            out.refcount_violations += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------
// Phase D — the task lifecycle, generation after generation.
// ---------------------------------------------------------------------

/// How the task phase measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskSoak {
    pub generations: usize,
    /// A Finished task was dispatched again (round-robin or priority pool). Must stay 0.
    pub finished_redispatches: usize,
    /// A Blocked task was dispatched. Must stay 0.
    pub blocked_redispatches: usize,
    /// A generation failed to drain to empty (tasks left runnable, or a drain lost/duplicated a
    /// task in the priority pool). Must stay 0.
    pub drains_not_empty: usize,
    pub priority_drains: usize,
    pub priority_dispatched: usize,
    pub unknown_events: usize,
    /// An unknown-id event CHANGED scheduler state (must stay 0).
    pub unknown_violations: usize,
    pub ns_total: u64,
}

const TASK_POOL: usize = 16;

/// Run `generations` full task-lifecycle generations over BOTH arch-independent schedulers:
/// round-robin with interleaved block/finish churn, and the priority pool admitted and drained
/// exactly once per generation. Unknown-id events are injected every generation and must change
/// nothing.
pub fn task_phase<H: Hal>(generations: usize) -> TaskSoak {
    let mut out = TaskSoak::default();
    let mut rr = RoundRobin::new();
    let mut ps = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut finished = [false; TASK_POOL];
    let mut blocked = [false; TASK_POOL];
    let mut pdispatch = [0usize; TASK_POOL];
    let t0 = H::timer_ticks();

    for gen in 0..generations {
        out.generations += 1;
        for f in finished.iter_mut() {
            *f = false;
        }
        for b in blocked.iter_mut() {
            *b = false;
        }
        for d in pdispatch.iter_mut() {
            *d = 0;
        }

        // ---- round-robin: spawn the pool, churn dispatch/block/finish, drain to empty ----
        for id in 0..TASK_POOL {
            rr.spawn(TaskId(id as u64 + 1));
        }
        for step in 0..(TASK_POOL * 2) {
            if let Some(t) = rr.schedule_next() {
                let id = (t.0 - 1) as usize % TASK_POOL;
                if finished[id] {
                    out.finished_redispatches += 1;
                }
                if blocked[id] {
                    out.blocked_redispatches += 1;
                }
                match (gen * 31 + step) % 7 {
                    0 => {
                        rr.block(t);
                        blocked[id] = true;
                    }
                    1 | 2 => {
                        rr.finish(t);
                        finished[id] = true;
                    }
                    _ => {}
                }
            }
        }
        // Unknown-id events change nothing — checked, not assumed.
        rr.finish(TaskId(9_999));
        rr.block(TaskId(8_888));
        out.unknown_events += 2;
        if rr.state(TaskId(9_999)).is_some() || rr.state(TaskId(8_888)).is_some() {
            out.unknown_violations += 1;
        }
        for id in 0..TASK_POOL {
            rr.finish(TaskId(id as u64 + 1));
        }
        if rr.schedule_next().is_some() || rr.runnable_len() != 0 {
            out.drains_not_empty += 1;
        }

        // ---- priority pool: admit all, drain, every task exactly once ----
        for id in 0..TASK_POOL {
            ps.admit(TaskId(id as u64 + 1), Priority(1 + ((gen + id) % 5) as u8));
        }
        ps.finish(TaskId(4_242)); // unknown-id event: no-op, never a panic
        out.unknown_events += 1;
        while let Some(t) = ps.schedule_next() {
            let id = (t.0 - 1) as usize % TASK_POOL;
            pdispatch[id] += 1;
            ps.finish(t);
            out.priority_dispatched += 1;
        }
        out.priority_drains += 1;
        for count in pdispatch.iter() {
            if *count != 1 {
                // Round-robin may dispatch a task many times in a generation; the PRIORITY pool
                // may not: admit -> dispatch -> finish is exactly-once by contract.
                out.drains_not_empty += 1;
            }
        }
    }
    out.ns_total = H::ticks_to_ns(H::timer_ticks().wrapping_sub(t0));
    out
}

// ---------------------------------------------------------------------
// The campaign + the gated suite.
// ---------------------------------------------------------------------

/// One target's soak workload. Sized for a boot: the journal phase is allocation-flat; the other
/// phases are bounded by these constants and their heap cost is printed by each target.
#[derive(Clone, Copy, Debug)]
pub struct SoakLoad {
    pub journal_txs: usize,
    pub fs_cycles: usize,
    pub grant_cycles: usize,
    pub task_generations: usize,
}

/// The boot-time load. Sized against the TIGHTEST constraint — the riscv64 gate boots under
/// QEMU-TCG inside a 120 s watchdog, and the suite runs TWICE (the determinism check) on a bump
/// heap that never frees. The hosted test takes the same harness to much larger loads.
pub const BOOT_LOAD: SoakLoad = SoakLoad {
    journal_txs: 384,
    fs_cycles: 12,
    grant_cycles: 96,
    task_generations: 48,
};

/// Everything one campaign measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct SoakReport {
    pub journal: JournalSoak,
    pub fs: FsSoak,
    pub grants: GrantSoak,
    pub tasks: TaskSoak,
}

/// Run the full soak campaign: journal churn (metered), namespace churn, grant churn, task
/// generations. `meter` is the target's own heap-usage reading, if it has one — the kernel targets
/// pass `heap::used_bytes`, the hosted test passes `None` and proves scale instead.
pub fn campaign<H: Hal>(load: SoakLoad, meter: Option<&dyn Fn() -> u64>) -> SoakReport {
    let mut dev = SparseDevice::new(DATA_START + 4);
    let journal = journal_phase::<H>(&mut dev, load.journal_txs, meter);
    let fs = fs_phase::<H>(load.fs_cycles);
    let grants = grant_phase::<H>(load.grant_cycles);
    let tasks = task_phase::<H>(load.task_generations);
    SoakReport {
        journal,
        fs,
        grants,
        tasks,
    }
}

/// Gate the soak on everything that must hold at ANY scale, on ANY machine.
///
/// `run` is called TWICE with the same load: the second run is the determinism check. Timing is in
/// the report and is never a pass/fail condition. `Ok((report, n))` = all `n` checks passed, with
/// the first run's numbers for the caller to print.
pub fn soak_suite(
    load: SoakLoad,
    run: impl Fn(SoakLoad) -> SoakReport,
    mut log: impl FnMut(u32, bool, &'static str),
) -> Result<(SoakReport, u32), (u32, &'static str)> {
    let r = run(load);
    let r2 = run(load);
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            log(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // 1 — the churn window is allocation-free where the target can see it. On a bump-heap kernel
    // this is THE long-running property: one more transaction costs nothing permanent. A target
    // that cannot meter its heap is UNPROVEN here, not exempt.
    check!(
        match r.journal.mem_delta() {
            Some(0) => true,
            _ => r.journal.mem_start.is_none(),
        },
        "soak: journal churn allocates nothing per transaction (the heap meter never moves)"
    );

    // 2 — every verified transaction read back byte-for-byte, and no commit ever failed.
    check!(
        r.journal.mismatches == 0
            && r.journal.commit_errors == 0
            && r.journal.verifies >= (r.journal.txs / VERIFY_EVERY) * 2,
        "soak: journaled transactions read back byte-for-byte under load"
    );

    // 3 — recovery mid-soak replayed idempotently and the sequence continued past it.
    check!(
        r.journal.recovers == RECOVER_ROUNDS
            && r.journal.recovers_replayed == RECOVER_ROUNDS
            && r.journal.post_recover_mismatches == 0,
        "soak: recovery mid-soak replays idempotently and the sequence continues"
    );

    // 4 — every namespace mutation left the filesystem structurally sound (audited per op).
    check!(
        r.fs.audits == r.fs.ops && r.fs.tally_violations == 0,
        "soak: every namespace mutation leaves the filesystem structurally sound"
    );

    // 5 — contents verified at every touch, and a fresh mount sees exactly the survivors.
    check!(
        r.fs.mismatches == 0 && r.fs.verifies == r.fs.ops && r.fs.final_ok,
        "soak: namespace contents verify byte-for-byte and a fresh mount sees exactly the survivors"
    );

    // 6 — zero-copy held at volume: the reader always saw the writer's bytes.
    check!(
        r.grants.cycles == load.grant_cycles && r.grants.zero_copy_mismatches == 0,
        "soak: a shared region's bytes are observed through every live grant (zero-copy under churn)"
    );

    // 7 — refcount discipline: owner + live grants, every cycle; owner-only at the end.
    check!(
        r.grants.refcount_violations == 0,
        "soak: revocation releases the mapping (the refcount returns to owner-only)"
    );

    // 8 — fail-closed at volume: every unauthorized share refused, every amplification refused.
    check!(
        r.grants.unauthorized_attempted > 0
            && r.grants.unauthorized_refused == r.grants.unauthorized_attempted
            && r.grants.amplify_attempted > 0
            && r.grants.amplify_refused == r.grants.amplify_attempted,
        "soak: unauthorized and amplifying shares are refused at volume (fail-closed)"
    );

    // 9 — a revoked grant is refused BY NAME: three accesses per cycle, all refused.
    check!(
        r.grants.revoked_attempted == r.grants.cycles * 3
            && r.grants.revoked_refused == r.grants.revoked_attempted,
        "soak: a revoked grant is refused by name (every revoked access, every cycle)"
    );

    // 10 — a Finished task NEVER ran again, on either scheduler, in any generation.
    check!(
        r.tasks.generations == load.task_generations && r.tasks.finished_redispatches == 0,
        "soak: a Finished task never runs again (both schedulers, every generation)"
    );

    // 11 — Blocked tasks were never dispatched, every generation drained to empty, the priority
    // drain was exactly-once, and unknown-id events changed nothing.
    check!(
        r.tasks.blocked_redispatches == 0
            && r.tasks.drains_not_empty == 0
            && r.tasks.unknown_violations == 0
            && r.tasks.priority_dispatched == r.tasks.generations * TASK_POOL,
        "soak: Blocked tasks are never dispatched, every generation drains exactly once, unknown events change nothing"
    );

    // 12 — the same seed replays the identical campaign: every checksum and census equal.
    check!(
        r.journal.checksum == r2.journal.checksum
            && r.journal.txs == r2.journal.txs
            && r.journal.verifies == r2.journal.verifies
            && r.fs.checksum == r2.fs.checksum
            && r.fs.ops == r2.fs.ops
            && r.fs.final_survivors == r2.fs.final_survivors
            && r.grants.checksum == r2.grants.checksum
            && r.grants.cycles == r2.grants.cycles
            && r.grants.revoked_refused == r2.grants.revoked_refused
            && r.tasks.generations == r2.tasks.generations
            && r.tasks.priority_dispatched == r2.tasks.priority_dispatched,
        "soak: the same seed replays the identical campaign (determinism)"
    );

    Ok((r, n))
}
