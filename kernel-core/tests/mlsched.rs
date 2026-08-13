//! Hosted proofs for the **live** advisory path (REQ-ML-003, ADR-056).
//!
//! `tests/mlrisk.rs` proves the forest is the one the trainer fitted. This file proves the thing the
//! forest is asked about is a real task on a real machine, that asking it has moved no authority,
//! and that the counters a console prints about its residency are counting what they claim to.
use kernel_core::mlrisk::{Verdict, BUNDLED_MODEL};
use kernel_core::mlsched::{mlsched_suite, RiskService, SUITE_MACHINE};
use kernel_core::priosched::{Priority, PriorityScheduler};
use kernel_core::sched::TaskId;
use kernel_core::taskfeat::{
    FeatureSource, JobId, MachineCapacity, Outcome, TaskSubmission, UserId, PRESSURE_BIN_SEC,
};

fn task(job: u64, index: u32, user: u64) -> TaskSubmission {
    TaskSubmission {
        sched_class: 2,
        priority: 5,
        cpu_millis: 1_000,
        memory_pages: 131_072,
        disk_pages: Some(419_430),
        diff_machine: false,
        task_index: index,
        job: JobId(job),
        user: UserId(user),
    }
}

/// The boot suite is the contract; running it here means a host failure is caught before a VM gate
/// has to catch it, and that the two are literally the same checks.
#[test]
fn the_boot_suite_passes_on_the_host() {
    match mlsched_suite(|_, _, _| {}) {
        Ok(n) => assert!(
            n >= 12,
            "expected at least 12 live-path invariants, got {n}"
        ),
        Err((i, name)) => panic!("live-path invariant {i} failed: {name}"),
    }
}

/// Every invariant reports exactly once, in order, and all of them pass — a suite that silently
/// skipped a check would still return `Ok`.
#[test]
fn every_invariant_reports_once_and_passes() {
    let mut seen = Vec::new();
    let n = mlsched_suite(|i, passed, name| {
        assert!(passed, "invariant {i} ({name}) reported as failed");
        seen.push(i);
    })
    .expect("suite passes");
    assert_eq!(seen, (1..=n).collect::<Vec<_>>());
}

/// Deriving features must be a *pure question* when it is asked as one: `peek_features` may not
/// advance the history that a later real admission depends on.
#[test]
fn peeking_at_features_does_not_move_history() {
    let mut svc = RiskService::without_model(SUITE_MACHINE);
    let mut sched = PriorityScheduler::default();
    let a = svc.peek_features(0, &task(1, 0, 1));
    let b = svc.peek_features(0, &task(1, 0, 1));
    assert_eq!(a, b, "peeking twice gave two different answers");
    svc.admit(&mut sched, TaskId(1), Priority(5), 0, &task(1, 0, 1));
    let after = svc.peek_features(0, &task(1, 1, 1));
    assert_eq!(
        after[9], 1,
        "the real admission should have moved job_submits"
    );
    assert_eq!(
        svc.stats().advices,
        1,
        "peeking must not count as a consultation"
    );
}

/// The same machine, the same stream, twice: same margins, same verdicts, same schedule. A forest
/// in a kernel that is not reproducible is a forest that cannot be debugged.
#[test]
fn the_live_path_is_deterministic() {
    let run = || {
        let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        let mut margins = Vec::new();
        for i in 0..200u64 {
            let mut t = task(i % 16, i as u32, i % 9);
            t.priority = (i % 12) as u8;
            t.cpu_millis = 100 + (i as u32 % 60) * 100;
            let a = svc.admit(
                &mut sched,
                TaskId(i + 1),
                Priority((i % 6) as u8),
                i * 3,
                &t,
            );
            margins.push(a.map(|a| (a.margin, a.verdict)));
        }
        let mut order = Vec::new();
        while let Some(id) = sched.schedule_next() {
            order.push(id);
            sched.finish(id);
        }
        (margins, order, svc.stats())
    };
    let (m1, o1, s1) = run();
    let (m2, o2, s2) = run();
    assert_eq!(m1, m2, "the same stream produced different advice");
    assert_eq!(o1, o2, "the same stream produced a different schedule");
    assert_eq!(s1, s2, "the same stream produced different counters");
}

/// A model IS resident when the image carries a verifiable blob, and the console can say so with a
/// number rather than a claim.
#[test]
fn the_bundled_blob_takes_up_residence() {
    let svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
    assert!(
        svc.active(),
        "the bundled blob did not load: {:?}",
        svc.model_error()
    );
    assert_eq!(svc.model_error(), None);
    let m = svc.model().expect("a resident model");
    assert!(m.trees() > 0 && m.nodes() > m.trees());
}

/// A refused blob is *named*, and the machine keeps running model-free rather than pretending.
#[test]
fn a_refused_blob_is_named_and_the_machine_still_runs() {
    let mut corrupt = BUNDLED_MODEL.to_vec();
    corrupt[0] ^= 0xff; // break the magic
    let mut svc = RiskService::load(&corrupt, SUITE_MACHINE);
    assert!(!svc.active());
    assert!(svc.model_error().is_some(), "a refusal must have a name");

    let mut advised = PriorityScheduler::default();
    let mut plain = PriorityScheduler::default();
    for i in 0..32u64 {
        svc.admit(
            &mut advised,
            TaskId(i + 1),
            Priority((i % 3) as u8),
            i,
            &task(i % 4, i as u32, i % 3),
        );
        plain.admit(TaskId(i + 1), Priority((i % 3) as u8));
    }
    loop {
        let (a, p) = (advised.schedule_next(), plain.schedule_next());
        assert_eq!(a, p, "a refused model changed the schedule");
        match a {
            None => break,
            Some(id) => {
                advised.finish(id);
                plain.finish(id);
            }
        }
    }
}

/// The verdict a task gets must follow the machine it is asked about. The same request is a
/// different fraction of a small machine than of a large one, and the features say so.
#[test]
fn the_same_request_is_a_different_fraction_of_a_different_machine() {
    let small = MachineCapacity {
        cpu_millis: 2_000,
        memory_pages: 262_144,
        disk_pages: 1_048_576,
    };
    let big = SUITE_MACHINE;
    let mut a = FeatureSource::new(small);
    let mut b = FeatureSource::new(big);
    let x = a.observe_submit(0, &task(1, 0, 1));
    let y = b.observe_submit(0, &task(1, 0, 1));
    assert_eq!(
        x[2],
        4 * y[2],
        "cpu_request did not scale with machine size"
    );
    assert_eq!(
        x[3],
        4 * y[3],
        "memory_request did not scale with machine size"
    );
}

/// Cell pressure ages: dispatches and evictions in one bin are visible in the next and gone in the
/// one after.
#[test]
fn cell_pressure_is_visible_for_exactly_one_bin() {
    let mut src = FeatureSource::new(SUITE_MACHINE);
    src.observe_schedule();
    src.observe_schedule();
    src.observe_outcome(JobId(1), UserId(1), Outcome::Evicted);
    let next = src.observe_submit(PRESSURE_BIN_SEC, &task(1, 0, 1));
    assert_eq!(next[17], 2, "dispatches did not carry into the next bin");
    assert_eq!(next[18], 1, "evictions did not carry into the next bin");
    let after = src.observe_submit(2 * PRESSURE_BIN_SEC, &task(2, 0, 2));
    assert_eq!(after[17], 0, "cell pressure outlived its bin");
    assert_eq!(after[18], 0, "cell pressure outlived its bin");
}

/// Ticks are housekeeping, not consultations: an idle machine's clock advancing must not inflate the
/// number of advices the console reports.
#[test]
fn ticks_age_the_census_without_counting_as_advice() {
    let mut svc = RiskService::without_model(SUITE_MACHINE);
    for s in 0..20u64 {
        svc.tick(s * PRESSURE_BIN_SEC);
    }
    let s = svc.stats();
    assert_eq!(s.ticks, 20);
    assert_eq!(s.advices, 0, "a tick is not an advice");
    assert_eq!(svc.source().previous_bin(), (0, 0, 0, 0));
}

/// The whole point, stated as a test: the model may reorder equals and may do nothing else. Here the
/// two tasks are equal in priority and the decisive verdicts differ, so the order *may* change — and
/// whatever it is, both tasks are still scheduled exactly once.
#[test]
fn advice_reorders_equals_and_never_loses_a_task() {
    let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
    let mut sched = PriorityScheduler::default();
    let modest = task(1, 0, 1);
    let mut greedy = task(2, 0, 2);
    greedy.cpu_millis = 7_000;
    greedy.memory_pages = 900_000;
    svc.admit(&mut sched, TaskId(1), Priority(5), 0, &greedy);
    svc.admit(&mut sched, TaskId(2), Priority(5), 1, &modest);

    let mut order = Vec::new();
    while let Some(id) = sched.schedule_next() {
        order.push(id);
        sched.finish(id);
    }
    order.sort();
    assert_eq!(
        order,
        vec![TaskId(1), TaskId(2)],
        "a task was invented or dropped"
    );

    let s = svc.stats();
    assert_eq!(s.advices, 2);
    assert_eq!(s.low + s.elevated + s.abstain, 2);
}

/// Abstention is counted apart from a decisive verdict, and `decisive_permille` reports the share a
/// console would print.
#[test]
fn the_census_separates_opinion_from_abstention() {
    let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
    let mut sched = PriorityScheduler::default();
    for i in 0..64u64 {
        let mut t = task(i % 8, i as u32, i % 5);
        t.priority = (i % 12) as u8;
        t.sched_class = (i % 4) as u8;
        svc.admit(
            &mut sched,
            TaskId(i + 1),
            Priority((i % 4) as u8),
            i * 11,
            &t,
        );
    }
    let s = svc.stats();
    assert_eq!(s.advices, 64);
    assert_eq!(s.low + s.elevated + s.abstain, 64);
    assert_eq!(s.decisive_permille(), ((s.low + s.elevated) * 1000) / 64);
    assert!(s.band_abstain <= s.abstain);
}

/// Outcomes move the history in the direction they mean: a finished task raises the terminal count
/// and *lowers* the fail rate a later task is judged against.
#[test]
fn a_finished_task_lowers_the_users_fail_rate() {
    let mut svc = RiskService::without_model(SUITE_MACHINE);
    let mut sched = PriorityScheduler::default();
    svc.admit(&mut sched, TaskId(1), Priority(5), 0, &task(1, 0, 4));
    svc.observe_outcome(JobId(1), UserId(4), Outcome::Failed);
    let after_fail = svc.peek_features(1, &task(1, 1, 4));
    for _ in 0..3 {
        svc.observe_outcome(JobId(1), UserId(4), Outcome::Finished);
    }
    let after_success = svc.peek_features(2, &task(1, 2, 4));
    assert_eq!(
        after_fail[13], 10_000,
        "one failure out of one terminal is a rate of 1.0"
    );
    assert_eq!(
        after_success[13], 2_500,
        "one failure out of four terminals is a rate of 0.25"
    );
    assert_eq!(svc.stats().finished, 3);
    assert_eq!(svc.stats().failed, 1);
}

/// A verdict is evidence, not an oracle: the margin that produced it is available to the console.
#[test]
fn the_advice_carries_the_margin_it_came_from() {
    let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
    let mut sched = PriorityScheduler::default();
    let a = svc
        .admit(&mut sched, TaskId(1), Priority(5), 0, &task(1, 0, 1))
        .expect("a resident model advises");
    match a.verdict {
        Verdict::Low | Verdict::Elevated => assert!(a.margin != 0 || a.out_of_range),
        Verdict::Abstain => {}
    }
}

/// The falsifiability claim, as a test rather than a sentence: a historical gap cannot grow while the
/// advisor is quiet, and the silence can. An advisor consulted in a burst at boot and never again
/// keeps reporting its small historical gaps forever, so `max_gap_secs` alone would let a machine go
/// on claiming a model it had stopped consulting. `silence_secs` is measured against the machine's
/// own clock and grows with it.
#[test]
fn silence_grows_while_the_historical_gap_cannot() {
    let mut svc = RiskService::without_model(SUITE_MACHINE);
    let mut sched = PriorityScheduler::default();
    svc.admit(&mut sched, TaskId(1), Priority(5), 0, &task(1, 0, 1));
    svc.admit(&mut sched, TaskId(2), Priority(5), 5, &task(1, 1, 1));
    assert_eq!(svc.stats().max_gap_secs, 5);
    assert_eq!(svc.stats().silence_secs(), 0);

    // The machine runs on, admitting nothing. Only one of the two numbers notices.
    for t in 1..=9u64 {
        svc.tick(5 + t * 100);
    }
    let s = svc.stats();
    assert_eq!(
        s.max_gap_secs, 5,
        "a historical gap must not grow while nothing is asked"
    );
    assert_eq!(s.last_tick_secs, 905);
    assert_eq!(
        s.silence_secs(),
        900,
        "silence must grow with the machine's own clock"
    );

    // A fresh consultation closes the silence and updates the history in one step.
    svc.admit(&mut sched, TaskId(3), Priority(5), 905, &task(1, 2, 1));
    let s = svc.stats();
    assert_eq!(s.silence_secs(), 0);
    assert_eq!(
        s.max_gap_secs, 900,
        "the gap that just closed is now the longest one"
    );
}

/// Before the first consultation there is nothing to be silent about, and the counter says zero
/// rather than the machine's uptime — an advisor that has never been asked has not fallen quiet.
#[test]
fn there_is_no_silence_before_the_first_advice() {
    let mut svc = RiskService::without_model(SUITE_MACHINE);
    for t in 1..=5u64 {
        svc.tick(t * 300);
    }
    let s = svc.stats();
    assert_eq!(s.advices, 0);
    assert_eq!(s.last_tick_secs, 1_500);
    assert_eq!(s.silence_secs(), 0);
}
