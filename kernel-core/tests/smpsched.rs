//! Host-proof of the ADR-021 Phase 2 per-CPU scheduling policy (REQ-SMP-003) under REAL threads.
//!
//! Same doctrine as `tests/sync.rs` / `tests/cap_concurrency.rs`: the policy that will run on real
//! cores is first hammered by genuinely parallel host threads, progress-gated (never a fixed spin
//! count — that races the thread scheduler and flakes). The aarch64 SMP suite then re-proves the
//! same contract on real cores.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel_core::smpsched::SmpSched;

const DEADLINE: Duration = Duration::from_secs(30);

/// Exactly-once under cross-CPU contention: every enqueued task is dispatched once — never lost,
/// never duplicated — while 4 "CPUs" race local pops against steals.
#[test]
fn exactly_once_under_cross_cpu_contention() {
    const NCPUS: usize = 4;
    const TASKS: usize = 4_000;

    let sched = Arc::new(SmpSched::new(NCPUS));
    let seen: Arc<Vec<AtomicU8>> = Arc::new((0..TASKS).map(|_| AtomicU8::new(0)).collect());
    let executed = Arc::new(AtomicUsize::new(0));

    // Deliberately unbalanced seed: everything lands on CPU 1's queue, so CPUs 0/2/3 can make
    // progress ONLY by stealing.
    for t in 0..TASKS {
        sched.enqueue_on(1, t as u64);
    }

    let workers: Vec<_> = (0..NCPUS)
        .map(|cpu| {
            let sched = Arc::clone(&sched);
            let seen = Arc::clone(&seen);
            let executed = Arc::clone(&executed);
            std::thread::spawn(move || {
                let mut stolen = 0usize;
                let start = Instant::now();
                while executed.load(Ordering::SeqCst) < TASKS && start.elapsed() < DEADLINE {
                    if let Some(d) = sched.next_for(cpu) {
                        seen[d.task as usize].fetch_add(1, Ordering::SeqCst);
                        if d.stolen_from.is_some() {
                            stolen += 1;
                        }
                        executed.fetch_add(1, Ordering::SeqCst);
                    } else {
                        std::hint::spin_loop();
                    }
                }
                stolen
            })
        })
        .collect();

    let stolen_per_cpu: Vec<usize> = workers.into_iter().map(|w| w.join().unwrap()).collect();

    assert_eq!(
        executed.load(Ordering::SeqCst),
        TASKS,
        "every task must be dispatched (none lost)"
    );
    for (t, s) in seen.iter().enumerate() {
        assert_eq!(
            s.load(Ordering::SeqCst),
            1,
            "task {t} must run exactly once (no loss, no duplication)"
        );
    }
    // CPUs other than 1 had no local work — anything they executed was a steal.
    let cross_core_steals: usize = stolen_per_cpu
        .iter()
        .enumerate()
        .filter(|(cpu, _)| *cpu != 1)
        .map(|(_, s)| *s)
        .sum();
    assert!(
        cross_core_steals > 0,
        "with all work seeded on CPU 1, the other CPUs must have stolen to progress"
    );
    for cpu in 0..NCPUS {
        assert_eq!(sched.load(cpu), 0, "queue {cpu} must be drained");
    }
}

/// Local first: a CPU with work on its own queue never steals.
#[test]
fn local_work_is_preferred_over_stealing() {
    let sched = SmpSched::new(2);
    sched.enqueue_on(0, 10);
    sched.enqueue_on(1, 20);

    let d = sched.next_for(0).expect("local task available");
    assert_eq!(d.task, 10, "CPU 0 must take its own task");
    assert_eq!(d.stolen_from, None, "a local pop is not a steal");
    assert_eq!(sched.load(1), 1, "the other queue must be untouched");
}

/// Stealing is live and attributed: an idle CPU drains work seeded on another CPU's queue and the
/// dispatch names the victim.
#[test]
fn idle_cpu_steals_from_loaded_queue() {
    let sched = SmpSched::new(3);
    sched.enqueue_on(2, 7);
    sched.enqueue_on(2, 8);

    let d = sched.next_for(0).expect("steal must find the loaded queue");
    assert_eq!(d.task, 7, "FIFO order within the victim queue");
    assert_eq!(d.stolen_from, Some(2), "the dispatch names the victim CPU");
    assert_eq!(sched.load(2), 1, "one task left behind on the victim");

    assert!(
        sched.next_for(1).is_some(),
        "a second thief drains the remainder"
    );
    assert!(
        sched.next_for(0).is_none(),
        "all queues empty -> None (nothing invented)"
    );
}

/// Placement balances: least-loaded enqueue spreads tasks evenly (ties -> lowest CPU index).
#[test]
fn least_loaded_placement_balances_queues() {
    const NCPUS: usize = 4;
    const TASKS: usize = 100;
    let sched = SmpSched::new(NCPUS);
    for t in 0..TASKS {
        sched.enqueue_least_loaded(t as u64);
    }
    for cpu in 0..NCPUS {
        assert_eq!(
            sched.load(cpu),
            TASKS / NCPUS,
            "least-loaded placement must spread {TASKS} tasks evenly over {NCPUS} queues"
        );
    }
}

/// The steal victim order prefers the most-loaded queue (better balance per steal).
#[test]
fn steal_prefers_most_loaded_victim() {
    let sched = SmpSched::new(3);
    sched.enqueue_on(1, 1);
    sched.enqueue_on(2, 2);
    sched.enqueue_on(2, 3);
    sched.enqueue_on(2, 4);

    let d = sched.next_for(0).expect("work exists");
    assert_eq!(
        d.stolen_from,
        Some(2),
        "the thief must target the most-loaded queue"
    );
}
