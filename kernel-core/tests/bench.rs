//! Host proofs for the benchmark harness (ALET-P2-010, ADR-064, REQ-PERF-002).
//!
//! The VM gates prove the suite runs on real targets over real frames; THIS file proves the
//! things a host can prove better than an emulator:
//!   * every gated invariant HOLDS on a calibrated clock — and the whole report REFUSES when the
//!     clock is frozen (a benchmark that cannot see time must not report speed);
//!   * the hot loops are allocation-free: a counting global allocator watches a full second
//!     campaign and sees only bounded setup traffic, where any per-operation allocation would add
//!     hundreds of thousands of allocations;
//!   * the GUI half renders THIS report legibly at pixel level, and refuses surfaces too small
//!     to hold it;
//!   * unit honesty: a calibrated clock reports ns, an uncalibrated one says "uncalibrated" and
//!     reports ticks instead of inventing a conversion.
extern crate alloc;

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kernel_core::bench::{
    bench_suite, campaign, format_report, render_suite, BenchLoad, BOOT_LOAD, FB_PAGES,
};
use kernel_core::dma::PAGE;
use kernel_core::Hal;

// -------------------------------------------------------------------------------------------
// Clocks under test.
// -------------------------------------------------------------------------------------------

/// A calibrated monotonic clock that always advances: every reading moves at least one tick, so
/// every measured window has positive elapsed time.
pub struct AdvancingHal;

static NOW: AtomicU64 = AtomicU64::new(1);

impl Hal for AdvancingHal {
    fn arch_name() -> &'static str {
        "hosted test / advancing clock"
    }
    fn timer_ticks() -> u64 {
        NOW.fetch_add(7, Ordering::Relaxed)
    }
    fn timer_freq_hz() -> u64 {
        1_000_000_000 // calibrated: 7 ticks per reading == 7 ns
    }
    fn ticks_to_ns(ticks: u64) -> u64 {
        ticks
    }
    fn current_privilege() -> u64 {
        0
    }
    fn exit(_code: i32) -> ! {
        unreachable!("the hosted bench must never exit the process")
    }
}

/// A FROZEN clock — the adversary: if the harness divided by its elapsed time, every rate would
/// be infinite and every cost zero. The suite must refuse the whole report BY NAME.
pub struct FrozenHal;

impl Hal for FrozenHal {
    fn arch_name() -> &'static str {
        "hosted test / frozen clock"
    }
    fn timer_ticks() -> u64 {
        42
    }
    fn timer_freq_hz() -> u64 {
        1_000_000_000
    }
    fn ticks_to_ns(ticks: u64) -> u64 {
        ticks
    }
    fn current_privilege() -> u64 {
        0
    }
    fn exit(_code: i32) -> ! {
        unreachable!("the hosted bench must never exit the process")
    }
}

/// An UNCALIBRATED clock: advances like real TSC hardware but reports frequency 0, exactly the
/// x86-64 situation. The report must show ticks and the word "uncalibrated", never nanoseconds.
pub struct UncalibratedHal;

impl Hal for UncalibratedHal {
    fn arch_name() -> &'static str {
        "hosted test / uncalibrated clock"
    }
    fn timer_ticks() -> u64 {
        NOW.fetch_add(3, Ordering::Relaxed)
    }
    fn timer_freq_hz() -> u64 {
        0
    }
    fn ticks_to_ns(ticks: u64) -> u64 {
        ticks // the x86-64 passthrough: NOT a conversion
    }
    fn current_privilege() -> u64 {
        0
    }
    fn exit(_code: i32) -> ! {
        unreachable!("the hosted bench must never exit the process")
    }
}

// -------------------------------------------------------------------------------------------
// A counting global allocator: the witness for the allocation-free claim.
// -------------------------------------------------------------------------------------------

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { std::alloc::System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { std::alloc::System.realloc(ptr, old, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn alloc_count() -> usize {
    ALLOCS.load(Ordering::Relaxed)
}

// -------------------------------------------------------------------------------------------
// Page-backed surface memory (same shape tests/fbcon.rs proves against).
// -------------------------------------------------------------------------------------------

struct Pages {
    #[expect(dead_code)]
    mem: alloc::vec::Vec<u8>,
    addrs: alloc::vec::Vec<usize>,
}

impl Pages {
    fn new(n: usize) -> Self {
        let mut mem = alloc::vec![0u8; n * PAGE + PAGE];
        let base0 = mem.as_mut_ptr() as usize;
        let base = base0.div_ceil(PAGE) * PAGE;
        let addrs = (0..n).map(|i| base + i * PAGE).collect();
        Pages { mem, addrs }
    }
}

// -------------------------------------------------------------------------------------------
// The proofs.
// -------------------------------------------------------------------------------------------

#[test]
fn every_gated_invariant_holds_on_a_calibrated_advancing_clock() {
    let pages = Pages::new(FB_PAGES);
    let mut lines: Vec<String> = Vec::new();
    let mut names: Vec<&'static str> = Vec::new();
    let r = bench_suite::<AdvancingHal>(
        BOOT_LOAD,
        &pages.addrs,
        |l| lines.push(l.to_string()),
        |n, ok, name| {
            assert!(ok, "invariant {} failed: {}", n, name);
            names.push(name);
        },
    );
    let (report, count) = r.expect("the suite must pass on a healthy machine");
    assert_eq!(count, 12, "twelve invariants: eight structural + four GUI");
    assert_eq!(names.len(), 12);

    // The measured work was really done — spot-check the counters behind the gates.
    assert_eq!(report.cap.allows, BOOT_LOAD.cap_iters);
    assert_eq!(report.ipc.replies_popped, BOOT_LOAD.ipc_iters);
    assert_eq!(report.fs.txs, BOOT_LOAD.fs_txs);
    assert_eq!(report.sched.dispatches, BOOT_LOAD.sched_iters);
    assert_eq!(report.con.bytes, BOOT_LOAD.con_iters * 12);

    // The report reached the TUI sink: metric lines plus workload plus honesty notes.
    assert_eq!(lines.len(), 8);
    assert!(lines[0].contains("checks"), "{}", lines[0]);
    assert!(lines.iter().any(|l| l.contains("REPORTED, never gated")));
}

#[test]
fn a_frozen_clock_is_refused_by_name_not_reported_as_infinite_speed() {
    let pages = Pages::new(FB_PAGES);
    let r = bench_suite::<FrozenHal>(BOOT_LOAD, &pages.addrs, |_| {}, |_, _, _| {});
    let (n, name) = r.expect_err("a frozen clock must fail the suite");
    assert_eq!(n, 1, "it is the FIRST gate that refuses");
    assert!(
        name.contains("clock advanced"),
        "refusal must name the clock: {}",
        name
    );
}

#[test]
fn the_second_campaign_allocates_only_setup_never_per_operation_traffic() {
    // A full campaign to settle nothing in particular — the claim below is about ANY campaign,
    // so it is proved on a fresh one rather than a warmed process.
    let _ = campaign::<AdvancingHal>(BOOT_LOAD);

    let before = alloc_count();
    let report = campaign::<AdvancingHal>(BOOT_LOAD);
    let delta = alloc_count() - before;
    let _ = &report; // the claim is about ALLOCATIONS, but the run must really happen

    // Measured operations this campaign performed inside its meters:
    let ops = (BOOT_LOAD.cap_iters
        + BOOT_LOAD.ipc_iters * 2
        + BOOT_LOAD.sched_iters
        + BOOT_LOAD.con_iters) as isize;
    // Setup allocations are bounded (devices, engines, scheduler nodes, report buffers). A single
    // 8-byte allocation PER OPERATION would add more than 300_000 deltas; the bound below holds
    // ONLY for allocation-free hot loops.
    assert!(
        delta < 256,
        "a campaign allocated {} times for {} measured ops — per-op heap traffic would dwarf this",
        delta,
        ops
    );
}

#[test]
fn an_uncalibrated_clock_reports_ticks_and_says_so_never_nanoseconds() {
    let r = campaign::<UncalibratedHal>(BOOT_LOAD);
    let lines = format_report::<UncalibratedHal>(&r, BOOT_LOAD);
    let authority = &lines[0];
    assert!(
        authority.contains("uncalibrated"),
        "the line must say uncalibrated: {}",
        authority
    );
    assert!(
        authority.contains("ticks"),
        "the line must carry its raw unit: {}",
        authority
    );
    assert!(
        !authority.contains("/s"),
        "a rate without a known second is a fake unit: {}",
        authority
    );
}

#[test]
fn every_report_line_fits_the_eighty_column_display_unwrapped() {
    let r = campaign::<AdvancingHal>(BOOT_LOAD);
    let lines = format_report::<AdvancingHal>(&r, BOOT_LOAD);
    for l in &lines {
        assert!(
            l.chars().count() < 79,
            "line would wrap on the 80-cell display: {:?} ({})",
            l,
            l.chars().count()
        );
    }
    // And the GUI half accepts them all — which requires the fit above to be real.
    let pages = Pages::new(FB_PAGES);
    let rendered = render_suite(&lines, &pages.addrs, |n, ok, name| {
        assert!(ok, "render invariant {}: {}", n, name);
    });
    assert_eq!(rendered.expect("render suite"), 4);
}

#[test]
fn a_surface_too_small_for_the_summary_is_refused_not_truncated() {
    let pages = Pages::new(4); // far less than FB_PAGES
    let r = campaign::<AdvancingHal>(BOOT_LOAD);
    let lines = format_report::<AdvancingHal>(&r, BOOT_LOAD);
    let out = render_suite(&lines, &pages.addrs, |_, _, _| {});
    assert!(out.is_err(), "an undersized surface must refuse");
}

#[test]
fn a_smaller_load_passes_the_same_gates_scaling_is_not_a_loophole() {
    // The gates are scale-free by design: quarter the load and every structural property must
    // still hold — except scheduler fairness needs the multiple-of-pool rule respected.
    let small = BenchLoad {
        cap_iters: 4_000,
        ipc_iters: 4_000,
        fs_txs: 32,
        sched_iters: 4_000, // still a multiple of SCHED_POOL (8)
        con_iters: 4_000,
    };
    let pages = Pages::new(FB_PAGES);
    let (_, count) = bench_suite::<AdvancingHal>(
        small,
        &pages.addrs,
        |_| {},
        |n, ok, name| {
            assert!(ok, "invariant {} failed at reduced scale: {}", n, name);
        },
    )
    .expect("reduced load must pass unchanged gates");
    assert_eq!(count, 12);
}
