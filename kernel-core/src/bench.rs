//! The machine measures itself (ALET-P2-010, ADR-064, REQ-PERF-002).
//!
//! Every suite before this one either proves CORRECTNESS on hand-picked cases or proves ENDURANCE
//! under repetition ([crate::soak]). None of them answers the operator's first question about a
//! new machine: how fast ARE the load-bearing paths on THIS substrate? The aarch64 backend answered
//! ad hoc for one target (kernel/src/bench.rs, CNTVCT + svc asm, not shared); this module is the
//! arch-independent answer, run by ALL THREE CPU targets inside their VM gates, on each machine's
//! own clock, through the same [crate::Hal] seam soak already proved.
//!
//! **What is REPORTED and what is GATED** - the doctrine this repo established for soak applies
//! double here: throughput and per-operation cost are an emulator's numbers (QEMU TCG) and are
//! REPORTED, never gated. What IS gated is everything that must hold on ANY substrate at ANY
//! speed: the clock really advanced during every measured window; the measured work was really
//! done (counts exact, deliveries verified, commits read back); the metered windows contain STEADY
//! STATE, not first-touch setup; the scheduler dispatched fairly; and the summary a human reads
//! ON THE DISPLAY is pixel-proved to be what this boot computed - the GUI half of the claim, the
//! serial log being the TUI half.
//!
//! **Allocation-free by construction.** Every hot loop runs on buffers allocated before its meter
//! starts: capability tokens pre-minted, payloads rewritten in place, scheduler pool admitted up
//! front, console lines encoded into a fixed stack buffer through a counting sink. The hosted test
//! proves the second campaign performs NO per-operation allocation at all under a counting global
//! allocator - at 400 000 measured operations, even 8 bytes per operation would be unmissable.
//!
//! **Unit honesty.** aarch64 (CNTFRQ) and riscv64 (SBI timebase) clocks are calibrated; x86-64's
//! TSC is not (Hal::timer_freq_hz() == 0). An uncalibrated clock is reported in RAW TICKS with the
//! word "uncalibrated" attached - converting by guessing a frequency would be fabrication, and
//! reporting a bare number without units would be worse.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::fbcon::{Surface, TextConsole, CELL_H, CELL_W};
use crate::font8x8::{glyph, FONT8X8};
use crate::sched::{RoundRobin, TaskId, TaskState};
use crate::soak::SparseDevice;
use crate::spine::{CapEngine, CapToken, Constraints, Decision, Scope, Target};
use crate::storage::{Journal, BLOCK_SIZE, DATA_START};
use crate::Hal;

// -------------------------------------------------------------------------------------------
// Load: how much work one campaign does. Fixed constants - a benchmark whose workload changes
// between boots measures nothing comparable.
// -------------------------------------------------------------------------------------------

/// How much of each path one campaign measures.
#[derive(Clone, Copy, Debug)]
pub struct BenchLoad {
    /// Authority evaluations in the cap window.
    pub cap_iters: usize,
    /// Capability-checked delivery round-trips in the ipc window.
    pub ipc_iters: usize,
    /// Journaled transactions committed in the fs window.
    pub fs_txs: usize,
    /// Scheduler dispatches in the sched window (a multiple of SCHED_POOL).
    pub sched_iters: usize,
    /// Console lines encoded in the con window.
    pub con_iters: usize,
}

/// The boot-time load. Sized so the whole campaign adds well under a second per target under TCG
/// while still being far too many operations for sub-resolution clock error to matter.
pub const BOOT_LOAD: BenchLoad = BenchLoad {
    cap_iters: 100_000,
    ipc_iters: 100_000,
    fs_txs: 256,
    sched_iters: 100_000,
    con_iters: 100_000,
};

/// Tasks in the scheduler pool. 100_000 % 8 == 0, so fairness is EXACTLY equal counts.
const SCHED_POOL: usize = 8;
/// Warm-up iterations discarded before every metered window - the same rule
/// comparative-bench.sh applies to idle sampling and soak applies to first-touch setup.
const WARMUP_ITERS: usize = 10_000;
/// One console line of the con window, fixed length so byte counts are provable by arithmetic.
const CON_LINE_LEN: usize = 12;

// -------------------------------------------------------------------------------------------
// Reports: counters plus elapsed ticks. Timing is data, never a verdict.
// -------------------------------------------------------------------------------------------

fn elapsed_ticks<H: Hal>(t0: u64) -> u64 {
    H::timer_ticks().wrapping_sub(t0)
}

/// Per-operation cost in the target's honest unit: nanoseconds when the clock is calibrated, raw
/// ticks when it is not. The LABEL travels with the number everywhere it is printed.
fn unit_cost<H: Hal>(ticks_total: u64, iters: usize) -> (u64, &'static str) {
    let per = ticks_total / iters.max(1) as u64;
    if H::timer_freq_hz() > 0 {
        (H::ticks_to_ns(per), "ns")
    } else {
        // The line itself says "uncalibrated"; the unit word stays plain so it fits 80 cells.
        (per, "ticks")
    }
}

/// Operations per second, or None on an uncalibrated clock - no frequency means no seconds, and a
/// rate wearing a fake unit would be worse than no rate.
fn unit_rate<H: Hal>(ticks_total: u64, iters: usize) -> Option<u64> {
    if ticks_total == 0 || H::timer_freq_hz() == 0 {
        return None;
    }
    let ns = H::ticks_to_ns(ticks_total);
    Some((iters as u64).saturating_mul(1_000_000_000) / ns)
}

/// Format one metric line: name, work done, elapsed, rate, unit cost. Kept under 78 columns so
/// the graphical console (80 cells) holds it unwrapped. The unit travels with the number —
/// nanoseconds on a calibrated clock, raw ticks with the word "uncalibrated" otherwise. A cost
/// that rounds DOWN to zero is printed as "<1": an integer clock coarser than one operation is
/// a fact about the CLOCK, and printing a confident 0 would be lying in the other direction.
fn metric_line<H: Hal>(name: &str, work: &str, ticks: u64, iters: usize) -> String {
    let (cost, unit) = unit_cost::<H>(ticks, iters);
    let cost_str = if cost == 0 {
        String::from("<1")
    } else {
        use alloc::string::ToString;
        cost.to_string()
    };
    if H::timer_freq_hz() > 0 {
        if let Some(rate) = unit_rate::<H>(ticks, iters) {
            let ms = ticks * 1000 / H::timer_freq_hz().max(1);
            return format!(
                "[bench] {}: {} | {} ms | {}/s | {} {}/op",
                name, work, ms, rate, cost_str, unit
            );
        }
    }
    format!(
        "[bench] {}: {} | {} tk | uncalibrated | {} {}/op",
        name, work, ticks, cost_str, unit
    )
}

/// How the authority-check window measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct CapBench {
    pub iters: usize,
    pub allows: usize,
    pub denies: usize,
    pub ticks: u64,
}

/// How the delivery round-trip window measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct IpcBench {
    pub iters: usize,
    pub requests_pushed: usize,
    pub replies_popped: usize,
    /// Sum of every popped reply - checked against the closed-form expectation.
    pub reply_sum: u64,
    pub ticks: u64,
}

/// How the journaled-commit window measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsBench {
    pub txs: usize,
    pub commit_errors: usize,
    pub verifies: usize,
    pub mismatches: usize,
    /// Device blocks newly materialized during the SECOND (steady-state proof) window.
    pub steady_new_blocks: isize,
    pub post_mismatches: usize,
    pub ticks: u64,
}

/// How the scheduler-dispatch window measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct SchedBench {
    pub iters: usize,
    pub dispatches: usize,
    /// Dispatches per task - all SCHED_POOL entries must equal iters / SCHED_POOL.
    pub per_task: [usize; SCHED_POOL],
    pub none_dispatches: usize,
    pub ticks: u64,
}

/// How the console-format window measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConBench {
    pub iters: usize,
    pub bytes: usize,
    pub last_counter: usize,
    pub last_line_ok: bool,
    pub ticks: u64,
}

/// Everything one campaign measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct BenchReport {
    pub cap: CapBench,
    pub ipc: IpcBench,
    pub fs: FsBench,
    pub sched: SchedBench,
    pub con: ConBench,
}

impl BenchReport {
    /// The counters that MUST repeat identically between two campaigns on one machine.
    fn census(&self) -> [u64; 9] {
        [
            self.cap.allows as u64,
            self.cap.denies as u64,
            self.ipc.replies_popped as u64,
            self.ipc.reply_sum,
            self.fs.txs as u64,
            self.fs.commit_errors as u64,
            self.sched.dispatches as u64,
            self.con.bytes as u64,
            self.fs.steady_new_blocks as u64,
        ]
    }
}

// -------------------------------------------------------------------------------------------
// Phase 1 - authority: the cost of the check that guards EVERYTHING else.
// -------------------------------------------------------------------------------------------

fn cap_phase<H: Hal>(iters: usize) -> CapBench {
    let mut engine = CapEngine::new(0x5eed_bec4, 1000);
    let token: CapToken = engine.mint("A", "bench.op", Scope::All, Constraints::none());
    let target = Target::default();
    let offered = [token];

    // Warm-up: prime i-cache and TCG translation blocks OUTSIDE the meter.
    for _ in 0..WARMUP_ITERS {
        let _ = engine.evaluate("bench.op", &target, &offered);
    }

    let mut out = CapBench {
        iters,
        ..Default::default()
    };
    let t0 = H::timer_ticks();
    for _ in 0..iters {
        if engine.evaluate("bench.op", &target, &offered) == Decision::Allow {
            out.allows += 1;
        } else {
            out.denies += 1;
        }
    }
    out.ticks = elapsed_ticks::<H>(t0);
    out
}

// -------------------------------------------------------------------------------------------
// Phase 2 - delivery: authority check + authenticated hand-off, twice per round-trip. The
// arch-neutral port of the aarch64 backend's IPC microbenchmark: fixed-capacity rings, no
// allocation, every reply verified against the closed form AFTER the meter stops.
// -------------------------------------------------------------------------------------------

struct Ring {
    buf: [u64; 8],
    head: usize,
    tail: usize,
}

impl Ring {
    fn new() -> Self {
        Ring {
            buf: [0; 8],
            head: 0,
            tail: 0,
        }
    }
    #[inline]
    fn push(&mut self, v: u64) {
        self.buf[self.tail & 7] = v;
        self.tail += 1;
    }
    #[inline]
    fn pop(&mut self) -> Option<u64> {
        if self.head == self.tail {
            None
        } else {
            let v = self.buf[self.head & 7];
            self.head += 1;
            Some(v)
        }
    }
}

fn ipc_phase<H: Hal>(iters: usize) -> IpcBench {
    let mut engine = CapEngine::new(0x5eed_bec5, 2000);
    let token: CapToken = engine.mint("A", "ipc.send", Scope::All, Constraints::none());
    let target = Target::default();
    let offered = [token];

    let mut a2b = Ring::new();
    let mut b2a = Ring::new();
    for i in 0..WARMUP_ITERS as u64 {
        let _ = engine.evaluate("ipc.send", &target, &offered);
        a2b.push(i);
        let m = a2b.pop().unwrap_or(0);
        let _ = engine.evaluate("ipc.send", &target, &offered);
        b2a.push(m + 1);
        let _ = b2a.pop();
    }

    let mut out = IpcBench {
        iters,
        ..Default::default()
    };
    let t0 = H::timer_ticks();
    for i in 0..iters as u64 {
        if engine.evaluate("ipc.send", &target, &offered) == Decision::Allow {
            a2b.push(i);
        }
        let m = a2b.pop().unwrap_or(0);
        if engine.evaluate("ipc.send", &target, &offered) == Decision::Allow {
            b2a.push(m + 1);
        }
        if let Some(r) = b2a.pop() {
            out.reply_sum = out.reply_sum.wrapping_add(r);
            out.replies_popped += 1;
        }
        // A missing reply leaves replies_popped short of iters - invariant 3 refuses that.
        let _ = m;
    }
    out.ticks = elapsed_ticks::<H>(t0);
    out
}

// -------------------------------------------------------------------------------------------
// Phase 3 - storage: the journal's commit path, the same shape soak proves ENDURES. Buffers are
// allocated once and rewritten in place; a warm-up commit materializes the sparse device BEFORE
// the meter, and a second equal-size window must touch ZERO new blocks - proving the reported
// number is steady state, not first-touch setup (the exact trap soak's invariant 1 caught).
// -------------------------------------------------------------------------------------------

const FS_SEED: u64 = 0x8422_9253_c3d2_ab66;

fn fs_phase<H: Hal>(txs: usize) -> FsBench {
    const VERIFY_EVERY: usize = 16;
    let mut dev = SparseDevice::new(DATA_START + 4);
    let mut j = Journal::new();
    let homes = [DATA_START, DATA_START + 1];
    let mut updates: Vec<(usize, [u8; BLOCK_SIZE])> = Vec::with_capacity(homes.len());
    for &h in homes.iter() {
        updates.push((h, [0u8; BLOCK_SIZE]));
    }
    let mut expect = [0u64; 2];
    fn fill(updates: &mut [(usize, [u8; BLOCK_SIZE])], expect: &mut [u64; 2], step: usize) {
        for (k, (_, data)) in updates.iter_mut().enumerate() {
            for (i, b) in data.iter_mut().enumerate() {
                *b = (step as u8)
                    .wrapping_mul(37)
                    .wrapping_add(i as u8)
                    .wrapping_add((k as u8).wrapping_mul(11))
                    % 251;
            }
            expect[k] = fnv1a(FS_SEED, data);
        }
    }

    // Steady-state warm-up: ONE commit writes journal slots 1..2 and both home blocks; every later
    // commit rewrites exactly those blocks in place. After this, touched can only grow if the
    // harness started measuring SETUP instead of WORK - which invariant 5 refuses.
    fill(&mut updates, &mut expect, 0);
    let _ = j.commit(&mut dev, &updates);

    let mut out = FsBench::default(); // counters count UP from zero; none is pre-seeded
    let t0 = H::timer_ticks();
    for step in 0..txs {
        if step != 0 {
            fill(&mut updates, &mut expect, step);
        }
        if j.commit(&mut dev, &updates).is_err() {
            out.commit_errors += 1;
            continue;
        }
        out.txs += 1;
        if step % VERIFY_EVERY == 0 {
            for (k, &h) in homes.iter().enumerate() {
                out.verifies += 1;
                match j.read(&dev, h) {
                    Ok(blk) => {
                        if fnv1a(FS_SEED, &blk) != expect[k] {
                            out.mismatches += 1;
                        }
                    }
                    Err(_) => out.mismatches += 1,
                }
            }
        }
    }
    out.ticks = elapsed_ticks::<H>(t0);

    // Steady-state proof window: the SAME work again, unmetered except for block materialization.
    let touched_before = dev.touched().len();
    for step in txs..txs * 2 {
        fill(&mut updates, &mut expect, step);
        let _ = j.commit(&mut dev, &updates);
    }
    out.steady_new_blocks = dev.touched().len() as isize - touched_before as isize;

    // What the windows committed must REALLY be on the device: full read-back, byte-exact.
    for (k, &h) in homes.iter().enumerate() {
        match j.read(&dev, h) {
            Ok(blk) => {
                if fnv1a(FS_SEED, &blk) != expect[k] {
                    out.post_mismatches += 1;
                }
            }
            Err(_) => out.post_mismatches += 1,
        }
    }
    out
}

fn fnv1a(seed: u64, data: &[u8]) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

// -------------------------------------------------------------------------------------------
// Phase 4 - scheduling: the shared RoundRobin policy, driven flat-out over a fixed pool. Fairness
// is gated EXACTLY: with iters a multiple of the pool size, every task must be dispatched the
// same number of times - a scheduler that skipped or starved a task cannot pass on any substrate,
// at any speed.
// -------------------------------------------------------------------------------------------

fn sched_phase<H: Hal>(iters: usize) -> SchedBench {
    let mut rr = RoundRobin::new();
    for i in 1..=SCHED_POOL as u64 {
        rr.spawn(TaskId(i));
    }
    for _ in 0..WARMUP_ITERS {
        let _ = rr.schedule_next();
    }

    let mut out = SchedBench {
        iters,
        ..Default::default()
    };
    let t0 = H::timer_ticks();
    for _ in 0..iters {
        match rr.schedule_next() {
            Some(id) => {
                out.dispatches += 1;
                out.per_task[((id.0 - 1) % SCHED_POOL as u64) as usize] += 1;
            }
            None => out.none_dispatches += 1,
        }
    }
    out.ticks = elapsed_ticks::<H>(t0);

    // Nobody was lost: the pool is intact, nothing Finished or Blocked itself mid-window.
    debug_assert!(matches!(
        rr.state(TaskId(1)),
        Some(TaskState::Ready) | Some(TaskState::Running)
    ));
    out
}

// -------------------------------------------------------------------------------------------
// Phase 5 - console: the format-and-emit path every operator line takes, measured into a FIXED
// stack buffer through a counting sink. No allocation, and the last line is re-encoded outside
// the window and compared - what was counted is what was really formatted.
// -------------------------------------------------------------------------------------------

struct CountingSink {
    bytes: usize,
}

impl CountingSink {
    fn emit(&mut self, line: &[u8]) {
        self.bytes += line.len();
    }
}

/// Encode counter into line as bench:NNNNNN - fixed width, ASCII only, no allocator.
fn encode_con_line(counter: usize, line: &mut [u8; CON_LINE_LEN]) {
    let prefix = b"bench:";
    line[..prefix.len()].copy_from_slice(prefix);
    let mut v = counter % 1_000_000;
    for i in (prefix.len()..CON_LINE_LEN).rev() {
        line[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
}

fn con_phase<H: Hal>(iters: usize) -> ConBench {
    let mut sink = CountingSink { bytes: 0 };
    let mut line = [0u8; CON_LINE_LEN];
    for i in 0..WARMUP_ITERS {
        encode_con_line(i, &mut line);
        sink.emit(&line);
    }

    let mut out = ConBench {
        iters,
        ..Default::default()
    };
    // The warm-up shared this sink; its bytes are DISCARDED here exactly as the clock discards
    // its elapsed time — the metered window contains only metered work.
    sink.bytes = 0;
    let t0 = H::timer_ticks();
    for i in 0..iters {
        encode_con_line(i, &mut line);
        sink.emit(&line);
    }
    out.ticks = elapsed_ticks::<H>(t0);
    out.bytes = sink.bytes;
    out.last_counter = iters.saturating_sub(1);

    // Outside the meter: the last window line must encode to exactly what arithmetic demands.
    let mut reference = [0u8; CON_LINE_LEN];
    encode_con_line(out.last_counter, &mut reference);
    out.last_line_ok = reference == line;
    out
}

// -------------------------------------------------------------------------------------------
// Campaign + report formatting + suites.
// -------------------------------------------------------------------------------------------

/// Run the full campaign on THIS machine's clock. Allocation happens only in setup; the hot loops
/// run on pre-owned buffers (proved allocation-free by the hosted test's counting allocator).
pub fn campaign<H: Hal>(load: BenchLoad) -> BenchReport {
    BenchReport {
        cap: cap_phase::<H>(load.cap_iters),
        ipc: ipc_phase::<H>(load.ipc_iters),
        fs: fs_phase::<H>(load.fs_txs),
        sched: sched_phase::<H>(load.sched_iters),
        con: con_phase::<H>(load.con_iters),
    }
}

/// Format this boot's numbers for BOTH consoles: the returned lines go to the serial log (TUI)
/// through the caller's sink AND to the graphical surface through render_suite. Numbers carry
/// their unit with them - a bare number without a unit is not a measurement.
pub fn format_report<H: Hal>(r: &BenchReport, load: BenchLoad) -> Vec<String> {
    let mut lines = vec![
        metric_line::<H>(
            "authority",
            &format!("{} checks", r.cap.allows),
            r.cap.ticks,
            r.cap.iters,
        ),
        metric_line::<H>(
            "delivery",
            &format!("{} rt", r.ipc.replies_popped),
            r.ipc.ticks,
            r.ipc.iters,
        ),
        metric_line::<H>(
            "storage",
            &format!("{} txs", r.fs.txs),
            r.fs.ticks,
            r.fs.txs,
        ),
        metric_line::<H>(
            "schedule",
            &format!("{} disp", r.sched.dispatches),
            r.sched.ticks,
            r.sched.iters,
        ),
        metric_line::<H>(
            "console",
            &format!("{} lines", r.con.bytes / CON_LINE_LEN),
            r.con.ticks,
            r.con.iters,
        ),
    ];
    lines.push(format!(
        "[bench] workload : cap={} ipc={} fs-txs={} sched={} con={}",
        load.cap_iters, load.ipc_iters, load.fs_txs, load.sched_iters, load.con_iters,
    ));
    lines.push(String::from(
        "[bench] numbers REPORTED, never gated: emulation timing is an emulator's,",
    ));
    lines.push(String::from(
        "[bench] not hardware truth. GATED: work done, fairness, steady state.",
    ));
    lines
}

/// The framebuffer geometry the GUI half renders on - the same shape as the GPU console's.
pub const FB_WIDTH: u32 = 640;
pub const FB_HEIGHT: u32 = 240;
/// 4 KiB pages needed to back FB_WIDTH x FB_HEIGHT x 4 bytes.
pub const FB_PAGES: usize = (FB_WIDTH as usize * FB_HEIGHT as usize * 4).div_ceil(crate::dma::PAGE);

/// The font's own doubled ink count for one glyph - computed FROM the table so the check cannot
/// drift from the renderer's source of truth (the same rule tests/fbcon.rs follows).
fn font_ink(ch: u8) -> u32 {
    FONT8X8[ch as usize]
        .iter()
        .map(|r| r.count_ones())
        .sum::<u32>()
        * 2
}

fn cell_ink(surf: &Surface, col: u32, row: u32) -> u32 {
    let (x0, y0) = (col * CELL_W, row * CELL_H);
    let mut n = 0;
    for y in y0..y0 + CELL_H {
        for x in x0..x0 + CELL_W {
            if surf.get(x, y) == Ok(true) {
                n += 1;
            }
        }
    }
    n
}

/// Pixel-exact comparison of one cell against the embedded table's own glyph bits.
fn cell_matches_glyph(surf: &Surface, ch: u8, col: u32, row: u32) -> bool {
    let g = match glyph(ch) {
        Some(g) => g,
        None => return false,
    };
    for (gy, bits) in g.iter().enumerate() {
        for gx in 0..8u32 {
            // The font is LSB = leftmost pixel (font8x8 convention), matching fbcon::blit.
            let want = (*bits >> gx) & 1 == 1;
            // The console double-strucks vertically: each font row draws twice.
            let even_ok = surf.get(col * CELL_W + gx, row * CELL_H + (gy as u32) * 2) == Ok(want);
            let odd_ok =
                surf.get(col * CELL_W + gx, row * CELL_H + (gy as u32) * 2 + 1) == Ok(want);
            if !even_ok || !odd_ok {
                return false;
            }
        }
    }
    true
}

/// The GUI half: render text onto a REAL page-backed surface and prove at PIXEL level that what a
/// human sees is what the suite computed - glyph-exact blitting, wrap, scroll, and THIS boot's own
/// summary lines legible on the display. Four invariants; runs on every target inside the gate.
pub fn render_suite(
    lines: &[String],
    pages: &[usize],
    mut log: impl FnMut(u32, bool, &'static str),
) -> Result<usize, (usize, &'static str)> {
    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let ok = $cond;
            log(n, ok, $name);
            if !ok {
                return Err((n as usize, $name));
            }
        }};
    }

    let cols = FB_WIDTH / CELL_W;
    let rows = FB_HEIGHT / CELL_H;

    // Every probe below runs over its OWN fresh surface + console pair. A refused construction
    // (pages too few, geometry impossible) is simply "not proved" — the probe yields None and
    // the check reads false, exactly like every other invariant in this repo.
    fn run_probe(
        pages: &[usize],
        body: &mut dyn FnMut(&mut Surface, &mut TextConsole) -> Option<bool>,
    ) -> Option<bool> {
        let mut surf = match Surface::new(pages, FB_WIDTH, FB_HEIGHT) {
            Ok(s) => s,
            Err(_) => return None,
        };
        let mut con = match TextConsole::new(FB_WIDTH, FB_HEIGHT) {
            Ok(c) => c,
            Err(_) => return None,
        };
        body(&mut surf, &mut con)
    }

    // R1 - glyph-exact rendering against the embedded table itself, over caller-owned pages.
    let r1: Option<bool> = run_probe(pages, &mut |surf, con| {
        con.clear(surf);
        if con.print(surf, b"Aletheia").is_err() {
            return None;
        }
        let got = cell_ink(surf, 0, 0);
        let bg_dark = surf.get(FB_WIDTH - 1, FB_HEIGHT - 1) == Ok(false);
        Some(got == font_ink(b'A') && font_ink(b'A') > 0 && bg_dark)
    });
    check!(
        r1 == Some(true),
        "bench-gui: the summary surface renders GLYPH-EXACT ink against the embedded font table"
    );

    // R2 - the wrap contract: a line longer than the screen continues on the next row.
    let r2: Option<bool> = run_probe(pages, &mut |surf, con| {
        con.clear(surf);
        let s = vec![b'M'; cols as usize + 3];
        if con.print(surf, &s).is_err() {
            return None;
        }
        let (c, r) = con.cursor();
        Some(c == 3 && r == 1)
    });
    check!(
        r2 == Some(true),
        "bench-gui: a line longer than the display WRAPS - the cursor lands past the break"
    );

    // R3 - the scroll contract: overflow moves earlier lines OFF the top, not nowhere. Print two
    // more SINGLE-CHAR rows than the display holds (a full-width row would also auto-wrap and
    // double-advance). THREE rows then scroll off - rows+2 lines, and the trailing newline of the
    // last fires from the bottom row too - so the visible top row is row index 3, a lone '3'.
    let r3: Option<bool> = run_probe(pages, &mut |surf, con| {
        con.clear(surf);
        for i in 0..rows + 2 {
            let ch = [b'0' + (i % 10) as u8];
            let _ = con.print(surf, &ch);
            let _ = con.print(surf, b"\n");
        }
        Some(cell_ink(surf, 0, 0) == font_ink(b'3') && font_ink(b'3') > 0)
    });
    check!(
        r3 == Some(true),
        "bench-gui: overflowing the display SCROLLS - earlier rows leave by the top, measurably"
    );

    // R4 - THIS boot's numbers reach the display: every summary line sits on the surface with real
    // ink, the first character of the first line is pixel-exact against the font table, and a full
    // surface sweep held bounds. The operator looking at the VM window reads what this boot MEASURED.
    let r4: Option<bool> = run_probe(pages, &mut |surf, con| {
        con.clear(surf);
        if lines.len() > rows as usize {
            return None; // keep the check decidable: the summary must fit one screen
        }
        for line in lines {
            if con.print(surf, line.as_bytes()).is_err() {
                return None;
            }
            if con.print(surf, b"\n").is_err() {
                return None;
            }
        }
        for (i, _) in lines.iter().enumerate() {
            let mut row_ink = 0;
            for c in 0..cols {
                row_ink += cell_ink(surf, c, i as u32);
            }
            if row_ink == 0 {
                return None; // a blank row means a line never reached the display
            }
        }
        let first = match lines[0].as_bytes().first() {
            Some(&f) => f,
            None => return None,
        };
        let head_exact = cell_matches_glyph(surf, first, 0, 0);
        for y in 0..FB_HEIGHT {
            for x in 0..FB_WIDTH {
                if surf.get(x, y).is_err() {
                    return None;
                }
            }
        }
        Some(head_exact)
    });
    check!(
        r4 == Some(true),
        "bench-gui: THIS boot's summary is ON THE DISPLAY - every line inked, first glyph exact"
    );

    Ok(n as usize)
}

/// Gate the benchmark on everything that must hold on ANY machine at ANY speed. Runs the campaign
/// TWICE: the second run is the determinism check (same machine, same load => same work done;
/// timing may differ, counters may not). Prints metric lines through out (serial TUI), proves
/// them on the display through render_suite (GUI), and returns the first run's report.
pub fn bench_suite<H: Hal>(
    load: BenchLoad,
    pages: &[usize],
    mut out: impl FnMut(&str),
    mut log: impl FnMut(u32, bool, &'static str),
) -> Result<(BenchReport, u32), (u32, &'static str)> {
    let r1 = campaign::<H>(load);
    for line in format_report::<H>(&r1, load) {
        out(&line);
    }
    let r2 = campaign::<H>(load);

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

    // 1 - the clock MOVED in every measured window. A frozen clock would divide a real iteration
    // count by zero elapsed time and report infinite speed; this refuses the whole report.
    check!(
        r1.cap.ticks > 0
            && r1.ipc.ticks > 0
            && r1.fs.ticks > 0
            && r1.sched.ticks > 0
            && r1.con.ticks > 0,
        "bench: the clock advanced in every measured window (nothing was divided by zero)"
    );

    // 2 - authority held steady under measurement: every evaluation allowed, because the offered
    // capability was minted for exactly this action. A deny inside the window would mean authority
    // CHANGED while it was being measured, and the numbers would describe two different systems.
    check!(
        r1.cap.allows == r1.cap.iters && r1.cap.denies == 0,
        "bench: every measured authority check ALLOWED - the capability held for the whole window"
    );

    // 3 - delivery really delivered: every round-trip came back, and the reply sum equals the
    // closed form of the arithmetic series. Checked in O(1) - no trust in the loop's own tally.
    let n64 = r1.ipc.iters as u64;
    // Sum of every reply r_i = i+1 for i in 0..n is the arithmetic series n(n+1)/2.
    let want_sum: u64 = n64.wrapping_mul(n64.wrapping_add(1)) / 2;
    check!(
        r1.ipc.replies_popped == r1.ipc.iters && r1.ipc.reply_sum == want_sum,
        "bench: every measured round-trip DELIVERED - replies counted and summed to the closed form"
    );

    // 4 - storage really stored: no commit failed, sampled read-backs matched, and the FULL
    // post-window read-back of both home blocks is byte-exact. Speed without durability is not a
    // storage number.
    check!(
        r1.fs.txs == load.fs_txs
            && r1.fs.commit_errors == 0
            && r1.fs.mismatches == 0
            && r1.fs.post_mismatches == 0,
        "bench: every measured commit READ BACK byte-for-byte after the window"
    );

    // 5 - the reported storage number is STEADY STATE: an equal-size follow-up window materialized
    // ZERO new device blocks, because the warm-up commit already claimed them. First-touch setup
    // inflating the throughput figure is the exact defect soak's invariant 1 caught in the field.
    check!(
        r1.fs.steady_new_blocks == 0,
        "bench: the storage window is steady state - an equal rerun touches no new device blocks"
    );

    // 6 - scheduling was FAIR, exactly: with iters a multiple of the pool, every task ran the same
    // number of times and nobody starved, finished, or vanished mid-window.
    let want_each = r1.sched.iters / SCHED_POOL;
    check!(
        r1.sched.dispatches == r1.sched.iters
            && r1.sched.none_dispatches == 0
            && r1.sched.per_task.iter().all(|&c| c == want_each),
        "bench: dispatches were EXACTLY fair - every pooled task ran its equal share"
    );

    // 7 - the console really formatted: byte count is arithmetic (iters x line length) and the
    // last line encodes independently to the same bytes outside the window.
    check!(
        r1.con.bytes == r1.con.iters * CON_LINE_LEN && r1.con.last_line_ok,
        "bench: the console emitted EXACTLY the bytes the arithmetic demands, last line verified"
    );

    // 8 - determinism: the second campaign did the IDENTICAL work. Same machine, same load, same
    // counters - a benchmark whose own numbers move between back-to-back runs measures noise.
    check!(
        r1.census() == r2.census(),
        "bench: a rerun on the same machine performed the IDENTICAL work (counter census equal)"
    );

    // 9..12 - the GUI half, at pixel level, over the caller's real frames.
    let gui_lines = format_report::<H>(&r1, load);
    let rendered = render_suite(&gui_lines, pages, |i, ok, name| log(n + i, ok, name))
        .map(|m| m as u32)
        .map_err(|(i, name)| (n + i as u32, name))?;
    n += rendered;

    Ok((r1, n))
}
