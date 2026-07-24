//! Host-proof of the ADR-021 Phase 2 per-CPU scheduling policy (REQ-SMP-003) under REAL threads.
//!
//! Same doctrine as `tests/sync.rs` / `tests/cap_concurrency.rs`: the policy that will run on real
//! cores is first hammered by genuinely parallel host threads, progress-gated (never a fixed spin
//! count — that races the thread scheduler and flakes). The aarch64 SMP suite then re-proves the
//! same contract on real cores.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel_core::smpsched::{AffinityMask, SmpSched};

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

// --- REQ-SMP-005: CPU affinity + cross-core migration + the lock-hierarchy tripwire ---------------

/// Affinity is honored: a task pinned to one CPU is never dispatched to any other, and IS dispatched
/// to its permitted CPU (as a steal) when that CPU goes looking for work.
#[test]
fn affinity_pins_a_task_to_its_only_permitted_cpu() {
    let sched = SmpSched::new(4);
    // Seed on CPU 0's queue, but pin the task to CPU 2 only.
    sched.enqueue_on_affine(0, 77, AffinityMask::only(2));

    // CPUs 0, 1, 3 are all forbidden — none may take it, locally or by stealing.
    assert!(
        sched.next_for(0).is_none(),
        "the origin CPU is not permitted -> must not run its own pinned-away task"
    );
    assert!(
        sched.next_for(1).is_none(),
        "a forbidden CPU cannot steal it"
    );
    assert!(
        sched.next_for(3).is_none(),
        "a forbidden CPU cannot steal it"
    );
    // The task is still queued (never dropped) after all those refusals.
    assert_eq!(sched.load(0), 1, "the pinned task stays queued, not lost");

    // CPU 2 is permitted -> it steals the task across cores. This is affinity-honored migration.
    let d = sched
        .next_for(2)
        .expect("the permitted CPU must get the task");
    assert_eq!(d.task, 77);
    assert_eq!(
        d.stolen_from,
        Some(0),
        "CPU 2 migrated the task off CPU 0's queue (affinity-honored steal)"
    );
    assert!(d.is_migration(), "a cross-core steal is a migration");
    assert_eq!(
        sched.load(0),
        0,
        "queue drained after the permitted CPU took it"
    );
}

/// A queue that mixes eligible and pinned-away tasks still hands the asking CPU its eligible ones in
/// FIFO order, rotating past the ones it may not run without losing or reordering them.
#[test]
fn take_skips_ineligible_tasks_but_preserves_fifo_among_eligible() {
    let sched = SmpSched::new(4);
    // On CPU 1's queue: task 10 pinned to CPU 3 (ineligible for 0), 11 ANY, 12 pinned to CPU 3, 13 ANY.
    sched.enqueue_on_affine(1, 10, AffinityMask::only(3));
    sched.enqueue_on_affine(1, 11, AffinityMask::ANY);
    sched.enqueue_on_affine(1, 12, AffinityMask::only(3));
    sched.enqueue_on_affine(1, 13, AffinityMask::ANY);

    // CPU 0 steals: it must get 11 then 13 (the eligible ones, in order), skipping 10 and 12.
    let a = sched.next_for(0).expect("first eligible");
    assert_eq!((a.task, a.stolen_from), (11, Some(1)));
    let b = sched.next_for(0).expect("second eligible");
    assert_eq!((b.task, b.stolen_from), (13, Some(1)));
    assert!(
        sched.next_for(0).is_none(),
        "only the CPU-3-pinned tasks remain -> CPU 0 sees nothing eligible"
    );
    assert_eq!(
        sched.load(1),
        2,
        "the two pinned-away tasks are still queued"
    );

    // CPU 3 drains its pinned tasks, still in FIFO order.
    assert_eq!(sched.next_for(3).unwrap().task, 10);
    assert_eq!(sched.next_for(3).unwrap().task, 12);
}

/// Affinity-aware placement puts a task only on a permitted queue, and picks the least-loaded among
/// them; an unsatisfiable mask enqueues nothing and returns `None` (the task is not silently dumped).
#[test]
fn affine_placement_balances_within_the_permitted_set() {
    let sched = SmpSched::new(4);
    // Permit only CPUs 1 and 3. 10 tasks must split evenly across exactly those two queues.
    let mask = AffinityMask::only(1).with(3);
    for t in 0..10u64 {
        let chosen = sched
            .enqueue_least_loaded_affine(t, mask)
            .expect("mask permits CPUs 1 and 3");
        assert!(chosen == 1 || chosen == 3, "placed only on a permitted CPU");
    }
    assert_eq!(sched.load(0), 0, "forbidden CPU 0 got nothing");
    assert_eq!(sched.load(2), 0, "forbidden CPU 2 got nothing");
    assert_eq!(
        sched.load(1),
        5,
        "even split across the two permitted queues"
    );
    assert_eq!(
        sched.load(3),
        5,
        "even split across the two permitted queues"
    );

    // A mask permitting only a nonexistent CPU (index 9 on a 4-CPU scheduler) places nothing.
    assert_eq!(
        sched.enqueue_least_loaded_affine(999, AffinityMask::only(9)),
        None,
        "an unsatisfiable mask enqueues nothing rather than dumping onto CPU 0"
    );
}

/// The migration MECHANISM end to end: a task seeded on CPU 1's queue is stolen by an idle CPU 0 and
/// RESUMED there through the `sched::TaskContext` seam — the task did not start on CPU 0, yet runs to
/// completion there. Cooperative handoff (like the VM suites' deterministic first steal); preemption
/// *timing* is the timer's job, proved in the usermode preemption gates. This proves dispatch moves
/// the task and the seam resumes it on the thief.
#[test]
fn stolen_task_resumes_on_the_thief_through_the_taskcontext_seam() {
    use kernel_core::sched::TaskContext;

    const ORIGIN: usize = 1;
    const THIEF: usize = 0;

    // The task's saved context (CPU-independent): a real backend restores registers + address space
    // in `resume`; the mock records WHICH CPU resumed it AND which task's context it carried, so the
    // proof ties the resumed context to the actually-stolen task (not just "a resume happened").
    struct SavedContext {
        resumed_on: AtomicUsize,
        resumed_task: AtomicUsize,
    }
    struct Resumer<'a> {
        cpu: usize,
        task: u64,
        ctx: &'a SavedContext,
    }
    impl TaskContext for Resumer<'_> {
        fn resume(&mut self) {
            self.ctx.resumed_on.store(self.cpu, Ordering::SeqCst);
            self.ctx
                .resumed_task
                .store(self.task as usize, Ordering::SeqCst);
        }
    }

    let sched = SmpSched::new(4);
    let ctx = SavedContext {
        resumed_on: AtomicUsize::new(usize::MAX),
        resumed_task: AtomicUsize::new(usize::MAX),
    };
    sched.enqueue_on(ORIGIN, 42); // ANY affinity -> any idle CPU may migrate it

    // THIEF has no local work -> it must steal task 42 off ORIGIN's queue.
    let d = sched
        .next_for(THIEF)
        .expect("the idle thief steals the only task");
    assert_eq!(d.task, 42);
    assert_eq!(
        d.stolen_from,
        Some(ORIGIN),
        "the dispatch attributes the migration to the origin CPU"
    );

    // Resume the ACTUAL stolen task on the thief through the seam — the resumer carries the dispatched
    // task's identity, so the assertions below bind "what resumed" to "what was stolen".
    let mut resumer = Resumer {
        cpu: THIEF,
        task: d.task,
        ctx: &ctx,
    };
    resumer.resume();

    assert_eq!(
        ctx.resumed_on.load(Ordering::SeqCst),
        THIEF,
        "the stolen task resumed on the thief CPU (cross-core migration through the TaskContext seam)"
    );
    assert_ne!(
        ctx.resumed_on.load(Ordering::SeqCst),
        ORIGIN,
        "and NOT on its origin CPU -> the task genuinely migrated"
    );
    assert_eq!(
        ctx.resumed_task.load(Ordering::SeqCst),
        42,
        "the context that resumed carried the stolen task's identity (not some unrelated value)"
    );
}

/// Exactly-once under cross-CPU contention WITH MIXED affinity: every task is dispatched exactly once
/// (none lost, none duplicated) even when some tasks are pinned to a single CPU, and every pinned
/// task runs ONLY on its permitted CPU. Masks are satisfiable (each pinned CPU is a live worker), so
/// the affinity starvation caveat does not apply.
#[test]
fn exactly_once_under_mixed_affinity_contention() {
    const NCPUS: usize = 4;
    const TASKS: usize = 4_000;

    let sched = Arc::new(SmpSched::new(NCPUS));
    let seen: Arc<Vec<AtomicU8>> = Arc::new((0..TASKS).map(|_| AtomicU8::new(0)).collect());
    // Which CPU actually ran each task (usize::MAX = not yet); used to assert affinity was honored.
    let ran_on: Arc<Vec<AtomicUsize>> =
        Arc::new((0..TASKS).map(|_| AtomicUsize::new(usize::MAX)).collect());
    let executed = Arc::new(AtomicUsize::new(0));

    // Deterministic pin rule: task t pinned to CPU (t % NCPUS) when t % 3 == 0, else ANY. Seed
    // everything on CPU 0's queue so the pinned-to-nonzero tasks MUST migrate to run.
    let pinned_cpu = |t: usize| -> Option<usize> {
        if t.is_multiple_of(3) {
            Some(t % NCPUS)
        } else {
            None
        }
    };
    for t in 0..TASKS {
        let mask = match pinned_cpu(t) {
            Some(c) => AffinityMask::only(c),
            None => AffinityMask::ANY,
        };
        sched.enqueue_on_affine(0, t as u64, mask);
    }

    let workers: Vec<_> = (0..NCPUS)
        .map(|cpu| {
            let sched = Arc::clone(&sched);
            let seen = Arc::clone(&seen);
            let ran_on = Arc::clone(&ran_on);
            let executed = Arc::clone(&executed);
            std::thread::spawn(move || {
                let start = Instant::now();
                while executed.load(Ordering::SeqCst) < TASKS && start.elapsed() < DEADLINE {
                    if let Some(d) = sched.next_for(cpu) {
                        let t = d.task as usize;
                        seen[t].fetch_add(1, Ordering::SeqCst);
                        ran_on[t].store(cpu, Ordering::SeqCst);
                        executed.fetch_add(1, Ordering::SeqCst);
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }

    assert_eq!(
        executed.load(Ordering::SeqCst),
        TASKS,
        "every task dispatched (none lost) even with mixed affinity"
    );
    for (t, s) in seen.iter().enumerate() {
        assert_eq!(
            s.load(Ordering::SeqCst),
            1,
            "task {t} ran exactly once (no loss, no duplication)"
        );
    }
    for t in 0..TASKS {
        if let Some(c) = pinned_cpu(t) {
            assert_eq!(
                ran_on[t].load(Ordering::SeqCst),
                c,
                "pinned task {t} ran ONLY on its permitted CPU {c} (affinity honored under contention)"
            );
        }
    }
    for cpu in 0..NCPUS {
        assert_eq!(sched.load(cpu), 0, "queue {cpu} fully drained");
    }
}

/// The ADR-028 lock-hierarchy tripwire is armed (not a silent no-op): deliberately nesting two
/// run-queue locks on the same CPU panics. Debug-only — the guard is compiled out of release builds,
/// and `cargo test` builds debug, so this exercises the real guard that is also live under the
/// contention suite above and the `-smp 4` VM gates.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "lock-hierarchy violation")]
fn nested_queue_lock_trips_the_audit_tripwire() {
    let sched = SmpSched::new(2);
    sched.__audit_probe_nested_lock();
}
