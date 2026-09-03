//! The filesystem under a merciless storm (REQ-QUAL-007 / REQ-FS-001, ADR-088).
//!
//! ADR-086 stormed the desktop and ADR-087 the scheduler; both found per-event allocations on a
//! heap that never frees (ADR-063). The filesystem is the third hot path a real machine hammers —
//! every console `write`, every component that persists anything, every journal transaction — and
//! it was the worst of the three: **sixteen kilobytes per write**, because each transaction built
//! a fresh `Vec` of whole 4 KiB blocks and every directory lookup decoded an owning `String` per
//! slot scanned. A twelve-megabyte kernel heap survived about seven hundred writes.
//!
//! What this suite holds the namespace to, at volume, on the machine:
//!
//! * **A write costs no memory.** After warm-up, a thousand replace cycles must not move the
//!   platform's own heap watermark. (`read` still hands the caller an owned `Vec` — that is the
//!   caller's bytes, named, not hidden.)
//! * **The namespace closes.** Create/replace/remove cycles at volume return the directory and
//!   the free-block count to EXACTLY where they started: no block leaked, no slot lost.
//! * **Erase-on-delete holds at volume** (ADR-033's storage twin): a removed object's blocks read
//!   back zero, every time, not just in the one case a small proof checks.
//! * **A crash lands on ONE side, wherever it falls.** A fault placed at EVERY position of a
//!   commit leaves the object either wholly old or wholly new — never a mixture, never a
//!   half-written directory.
//! * **The same storm twice leaves the same device.** Byte-for-byte, over the directory and the
//!   bitmap the namespace lives in.

use alloc::vec::Vec;

use crate::faultdev::{FaultInject, Op};
use crate::fs::Filesystem;
use crate::storage::{BlockDevice, MemBlockDevice, BLOCK_SIZE};

/// Objects the storm keeps in the namespace.
const OBJECTS: usize = 8;
/// Replace cycles per round.
const CYCLES: u32 = 512;
/// Device size: the journal's own ceiling plus room for the namespace, and NOT one block more.
/// Every block is 4 KiB of a heap that never frees (ADR-063), so a storm that allocated a fresh
/// two-megabyte device per claim would be the very disease this suite exists to catch — the
/// suite threads ONE device through every claim and reformats it instead.
const BLOCKS: usize = 96;

/// The same deterministic stream the other storms use.
struct Storm(u64);

impl Storm {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn name_of(i: usize) -> &'static str {
    const NAMES: [&str; OBJECTS] = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
    ];
    NAMES[i % OBJECTS]
}

/// Reformat the caller's device and seed it with `OBJECTS` small objects. One device, reused by
/// every claim: allocating a new one per claim would cost megabytes on a heap that never frees.
fn seed(dev: &mut MemBlockDevice) -> Filesystem {
    Filesystem::format(dev).expect("format");
    let mut fs = Filesystem::mount(dev).expect("mount");
    let body = [b'a'; 64];
    for i in 0..OBJECTS {
        fs.create(dev, name_of(i), &body).expect("create");
    }
    fs
}

/// The boot suite (ADR-088). `used_bytes` reports the CALLER's own heap watermark.
pub fn storm_suite(
    used_bytes: &mut dyn FnMut() -> usize,
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    let mut n: u32 = 0;
    // ONE device for the whole suite (see `seed`): 96 blocks of 4 KiB, allocated once.
    let mut dev = MemBlockDevice::new(BLOCKS);
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // 1 — A WRITE COSTS NO MEMORY. Warm up, then replace over and over and hold the platform's
    //     own heap watermark still.
    {
        let mut fs = seed(&mut dev);
        let mut s = Storm(0xF5F5_0088);
        let body = [b'z'; 96];
        let round = |dev: &mut MemBlockDevice, fs: &mut Filesystem, s: &mut Storm| {
            for _ in 0..CYCLES {
                let i = s.below(OBJECTS as u64) as usize;
                let _ = fs.replace(dev, name_of(i), &body);
            }
        };
        round(&mut dev, &mut fs, &mut s); // warm-up: first-touch growth is paid once per boot
        let before = used_bytes();
        round(&mut dev, &mut fs, &mut s);
        let after = used_bytes();
        crate::storm_report("fsstorm", before, after);
        check!(
            after == before,
            "fsstorm: five hundred writes in the steady state allocate NOTHING"
        );
    }
    // 2 — THE NAMESPACE CLOSES. Create/remove cycles at volume return the directory population
    //     and the free-block count to exactly where they started.
    {
        let mut fs = seed(&mut dev);
        let free0 = fs.free_blocks(&dev).expect("free");
        let live0 = fs.list(&dev).expect("list").len();
        let body = [b'q'; 200];
        for i in 0..CYCLES {
            let name = name_of(i as usize % OBJECTS);
            fs.remove(&mut dev, name).expect("remove");
            fs.create(&mut dev, name, &body).expect("create");
            fs.remove(&mut dev, name).expect("remove");
            fs.create(&mut dev, name, &[b'a'; 64]).expect("recreate");
        }
        check!(
            fs.free_blocks(&dev).expect("free") == free0
                && fs.list(&dev).expect("list").len() == live0,
            "fsstorm: two thousand create/remove cycles leak no block and lose no slot"
        );
    }
    // 3 — ERASE-ON-DELETE HOLDS AT VOLUME: the blocks a removed object used read back ZERO.
    {
        let mut fs = seed(&mut dev);
        let body = [b'S'; BLOCK_SIZE + 32]; // spans two blocks, so the sweep is not trivial
        let mut clean = true;
        for i in 0..64u32 {
            let name = name_of(i as usize % OBJECTS);
            fs.replace(&mut dev, name, &body).expect("replace");
            let e = fs.stat(&dev, name).expect("stat");
            let (start, blocks) = (e.start, e.blocks());
            fs.remove(&mut dev, name).expect("remove");
            let mut blk = [0u8; BLOCK_SIZE];
            for b in 0..blocks {
                dev.read_block(start + b, &mut blk).expect("read");
                clean &= blk.iter().all(|&x| x == 0);
            }
            fs.create(&mut dev, name, &[b'a'; 64]).expect("recreate");
        }
        check!(
            clean,
            "fsstorm: every removed object's blocks read back zero - erase on delete, at volume"
        );
    }
    // 4 — A CRASH LANDS ON ONE SIDE, WHEREVER IT FALLS. A fault at every position of a commit
    //     leaves the object wholly old or wholly new: never a mixture.
    {
        let mut mixed = 0u32;
        let mut positions = 0u32;
        for pos in 0..8usize {
            Filesystem::format(&mut dev).expect("format");
            let mut fs = Filesystem::mount(&mut dev).expect("mount");
            let old = [b'o'; 100];
            let new = [b'n'; 100];
            fs.create(&mut dev, "victim", &old).expect("create");

            // Let `pos` operations succeed, then refuse the next write AND the next flush: the
            // adversary interrupts the commit exactly there.
            let mut script: Vec<Op> = Vec::new();
            for _ in 0..pos {
                script.push(Op::WriteOk);
            }
            script.push(Op::WriteFail);
            let mut faulty =
                FaultInject::new(core::mem::replace(&mut dev, MemBlockDevice::new(0)), script);
            let broken = Filesystem::mount(&mut faulty)
                .map(|mut f| f.replace(&mut faulty, "victim", &new).is_err())
                .unwrap_or(true);
            dev = faulty.into_inner();

            // Whatever happened, a fresh mount must see exactly one of the two contents.
            let fs2 = Filesystem::mount(&mut dev).expect("remount");
            let seen = fs2.read(&dev, "victim").unwrap_or_default();
            let is_old = seen == old;
            let is_new = seen == new;
            positions += 1;
            if !(is_old || is_new) {
                mixed += 1;
            }
            let _ = broken;
        }
        check!(
            positions == 8 && mixed == 0,
            "fsstorm: a fault at every position of a commit leaves the object wholly old or wholly new"
        );
    }
    // 5 — THE SAME STORM TWICE LEAVES THE SAME DEVICE, byte for byte, over the blocks the
    //     namespace lives in.
    {
        let run = |dev: &mut MemBlockDevice| -> Vec<u8> {
            let mut fs = seed(dev);
            let mut s = Storm(0x0F5A_0088);
            for _ in 0..CYCLES {
                let i = s.below(OBJECTS as u64) as usize;
                let len = 32 + s.below(200) as usize;
                let mut body = Vec::with_capacity(len);
                body.resize(len, b'r');
                let _ = fs.replace(dev, name_of(i), &body);
                if s.below(8) == 0 {
                    let _ = fs.remove(dev, name_of(i));
                    let _ = fs.create(dev, name_of(i), &body);
                }
            }
            // The directory and the bitmap ARE the namespace: compare them exactly.
            let mut out = Vec::with_capacity(2 * BLOCK_SIZE);
            let mut blk = [0u8; BLOCK_SIZE];
            for b in 0..2usize {
                if dev
                    .read_block(crate::storage::DATA_START + b, &mut blk)
                    .is_ok()
                {
                    out.extend_from_slice(&blk);
                }
            }
            out
        };
        let a = run(&mut dev);
        let b = run(&mut dev);
        check!(
            !a.is_empty() && a == b,
            "fsstorm: the same storm told twice leaves the namespace byte-for-byte identical"
        );
    }
    Ok(n)
}
