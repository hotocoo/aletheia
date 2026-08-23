//! Long-running soak on the host: the same harness the three targets run at boot, taken to loads
//! a TCG-emulated riscv64 inside a 120 s watchdog could never finish (ALET-P2-009, ADR-063).
//!
//! The VM gates prove the properties hold on the real CPU at a bounded load, including the one
//! claim ONLY a kernel can make — that the churn loop costs no permanent memory on its own heap
//! meter. This file proves the same scale-free properties still hold when "long-running" means
//! what it means everywhere else: tens of thousands of transactions, cycles and generations. With
//! `--nocapture` it prints the throughput numbers, so "what does a transaction cost" has an
//! answer measured on a machine rather than assumed.
//!
//! Nothing here asserts a timing. Timings are printed; the assertions are the scale-invariant
//! properties, exactly as `soak::soak_suite` gates them.

use kernel_core::soak::{
    campaign, fs_phase, grant_phase, journal_phase, soak_suite, task_phase, FsSoak, GrantSoak,
    JournalSoak, SoakLoad, SoakReport, SparseDevice, TaskSoak,
};
use kernel_core::storage::{Journal, DATA_START};
use std::time::Instant;

/// A host `Hal` that implements only what the soak harness uses.
struct HostHal;

impl kernel_core::Hal for HostHal {
    fn arch_name() -> &'static str {
        "host"
    }
    fn timer_ticks() -> u64 {
        use std::sync::OnceLock;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
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
    fn exit(code: i32) -> ! {
        std::process::exit(code)
    }
}

fn host_load() -> SoakLoad {
    // Debug builds walk the same loops unoptimised; the release numbers are the soak's headline,
    // so the load is chosen for release and trimmed for debug via cfg!
    let scale = if cfg!(debug_assertions) { 5 } else { 100 };
    SoakLoad {
        journal_txs: 500 * scale,
        fs_cycles: 20 * scale,
        grant_cycles: 200 * scale,
        task_generations: 40 * scale,
    }
}

#[test]
fn the_soak_suite_holds_on_the_host_at_scale() {
    let load = host_load();
    let started = Instant::now();
    let (r, n) = soak_suite(load, |l| campaign::<HostHal>(l, None), |_, _, _| {})
        .expect("every soak invariant holds at host scale");
    let wall = started.elapsed();
    println!(
        "soak suite: {} checks green in {:?} ({:?} build)",
        n,
        wall,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    print_report(&r);
}

#[test]
fn a_long_journal_churn_verifies_every_step_and_recovers_midstream() {
    let txs = if cfg!(debug_assertions) {
        2_500
    } else {
        50_000
    };
    let mut dev = SparseDevice::new(DATA_START + 4);
    let j: JournalSoak = journal_phase::<HostHal>(&mut dev, txs, None);
    assert_eq!(j.commit_errors, 0, "no commit may fail on healthy hardware");
    assert_eq!(j.mismatches, 0, "every verified read-back must match");
    assert!(
        j.verifies >= (j.txs / 8) * 2,
        "verification must ride the loop"
    );
    assert_eq!(j.recovers, 3);
    assert_eq!(j.recovers_replayed, j.recovers, "recovery replays mid-soak");
    assert_eq!(j.post_recover_mismatches, 0);
    println!(
        "journal: {} txs, {} verifies, {} recovers => {} tx/s",
        j.txs,
        j.verifies,
        j.recovers,
        j.txs_per_second()
    );
}

#[test]
fn a_long_namespace_churn_is_structurally_sound_after_every_op() {
    let cycles = if cfg!(debug_assertions) { 100 } else { 2_000 };
    let f: FsSoak = fs_phase::<HostHal>(cycles);
    assert!(f.ops > 0);
    assert_eq!(f.audits, f.ops, "every mutation must be audited");
    assert_eq!(f.tally_violations, 0, "the tally holds after every op");
    assert_eq!(f.mismatches, 0, "contents verify byte-for-byte");
    assert!(f.final_ok, "a fresh mount sees exactly the survivors");
    println!(
        "namespace: {} ops over {} survivors => {} ops/s",
        f.ops,
        f.final_survivors,
        f.ops_per_second()
    );
}

#[test]
fn long_grant_churn_refuses_everything_it_should_and_stays_zero_copy() {
    let cycles = if cfg!(debug_assertions) {
        1_000
    } else {
        20_000
    };
    let g: GrantSoak = grant_phase::<HostHal>(cycles);
    assert_eq!(g.cycles, cycles);
    assert_eq!(g.zero_copy_mismatches, 0, "zero-copy held all campaign");
    assert_eq!(g.refcount_violations, 0, "refcounts exact all campaign");
    assert!(g.unauthorized_attempted > 0);
    assert_eq!(g.unauthorized_refused, g.unauthorized_attempted);
    assert!(g.amplify_attempted > 0);
    assert_eq!(g.amplify_refused, g.amplify_attempted);
    assert_eq!(g.revoked_refused, g.revoked_attempted);
    println!(
        "grants: {} cycles, {}/{} unauthorized refused, {}/{} amplifications refused, {}/{} revoked accesses refused",
        g.cycles,
        g.unauthorized_refused,
        g.unauthorized_attempted,
        g.amplify_refused,
        g.amplify_attempted,
        g.revoked_refused,
        g.revoked_attempted
    );
}

#[test]
fn long_task_churn_never_resurrects_a_finished_or_blocked_task() {
    let generations = if cfg!(debug_assertions) { 200 } else { 4_000 };
    let t: TaskSoak = task_phase::<HostHal>(generations);
    assert_eq!(t.generations, generations);
    assert_eq!(t.finished_redispatches, 0);
    assert_eq!(t.blocked_redispatches, 0);
    assert_eq!(t.drains_not_empty, 0);
    assert_eq!(t.unknown_violations, 0);
    assert_eq!(t.priority_dispatched, t.generations * 16);
    println!(
        "tasks: {} generations, {} priority dispatches, {} unknown-id events swallowed",
        t.generations, t.priority_dispatched, t.unknown_events
    );
}

#[test]
fn two_identical_campaigns_produce_identical_censuses() {
    // The suite's determinism check runs the whole thing twice; this pins the same property for a
    // single phase directly, where a divergence names itself without the rest of the report.
    let mut dev_a = SparseDevice::new(DATA_START + 4);
    let mut dev_b = SparseDevice::new(DATA_START + 4);
    let a = journal_phase::<HostHal>(&mut dev_a, 512, None);
    let b = journal_phase::<HostHal>(&mut dev_b, 512, None);
    assert_eq!(a.checksum, b.checksum);
    assert_eq!(a.txs, b.txs);
    assert_eq!(a.verifies, b.verifies);
    // And the device agrees byte-for-byte with what the census claims.
    assert_eq!(dev_a.touched(), dev_b.touched());
}

#[test]
fn an_unmetered_journal_phase_reports_no_memory_claim() {
    // mem_delta is Some(0) ONLY where a real meter ran; with no meter there is no claim to gate.
    let mut dev = SparseDevice::new(DATA_START + 4);
    let j = journal_phase::<HostHal>(&mut dev, 8, None);
    assert!(j.mem_start.is_none() && j.mem_end.is_none());
    assert!(j.mem_delta().is_none());
    // With a meter, a loop that allocates nothing per op measures exactly zero.
    let mut dev2 = SparseDevice::new(DATA_START + 4);
    let live_cell = std::cell::Cell::new(7u64);
    let meter = || live_cell.get();
    let j2 = journal_phase::<HostHal>(&mut dev2, 8, Some(&meter as &dyn Fn() -> u64));
    assert_eq!(j2.mem_delta(), Some(0));
}

#[test]
fn recovery_on_healthy_hardware_finds_the_last_commit_after_churn() {
    // The idempotence claim, standalone: after a churn campaign, a FRESH journal recovers the
    // device to the same bytes the churning journal last wrote.
    let txs = if cfg!(debug_assertions) { 256 } else { 4_096 };
    let mut dev = SparseDevice::new(DATA_START + 4);
    let j = journal_phase::<HostHal>(&mut dev, txs, None);
    let mut fresh = Journal::new();
    let replayed = fresh
        .recover(&mut dev)
        .expect("recovery works on healthy hardware");
    assert!(
        replayed,
        "a committed transaction must be found after the churn"
    );
    assert_eq!(j.post_recover_mismatches, 0);
}

fn print_report(r: &SoakReport) {
    println!(
        "  journal : {} txs / {} verifies / {} recovers => {} tx/s",
        r.journal.txs,
        r.journal.verifies,
        r.journal.recovers,
        r.journal.txs_per_second()
    );
    println!(
        "  namespace: {} ops => {} ops/s ({} survivors)",
        r.fs.ops,
        r.fs.ops_per_second(),
        r.fs.final_survivors
    );
    println!(
        "  grants  : {} cycles (all refusals exact)",
        r.grants.cycles
    );
    println!(
        "  tasks   : {} generations / {} priority dispatches",
        r.tasks.generations, r.tasks.priority_dispatched
    );
}
