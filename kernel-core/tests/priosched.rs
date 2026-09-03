//! Priority-inheritance + priority-aware scheduling invariants (REQ-IPC-009, ADR-020).
//!
//! Proved on the host (no QEMU), the arch-independent policy: capability-gated endpoint access,
//! priority donation from a blocked waiter to its holder (incl. transitively), the resulting
//! avoidance of unbounded priority inversion in `schedule_next`, and withdrawal of donation on
//! release — all fail-closed.

use kernel_core::priosched::{Endpoint, Priority, PriorityScheduler, SchedError};
use kernel_core::sched::{TaskId, TaskState};
use kernel_core::spine::{CapEngine, Constraints, Scope};
use std::vec as alloc_vec;

const ACQ: &str = "endpoint.acquire";
const HIGH: Priority = Priority(10);
const MED: Priority = Priority(5);
const LOW: Priority = Priority(1);

fn t(n: u64) -> TaskId {
    TaskId(n)
}

/// Engine that grants `subject` the endpoint-acquire authority; returns (engine, token).
fn engine() -> (CapEngine, kernel_core::spine::CapToken) {
    let mut e = CapEngine::new(0xACE, 1_000);
    let cap = e.mint("task", ACQ, Scope::All, Constraints::none());
    (e, cap)
}

#[test]
fn effective_priority_is_base_with_no_donors() {
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), MED);
    assert_eq!(s.effective_priority(t(1)), MED);
}

#[test]
fn endpoint_access_is_capability_gated_fail_closed() {
    let (e, _cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW);
    // No capability offered ⇒ cannot acquire a kernel endpoint.
    assert_eq!(
        s.acquire(&e, Endpoint(1), t(1), &[]),
        Err(SchedError::Unauthorized)
    );
    assert_eq!(s.holder_of(Endpoint(1)), None);
}

#[test]
fn acquire_free_then_busy_then_wait_semantics() {
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW);
    s.admit(t(2), HIGH);
    // Free endpoint acquires.
    assert_eq!(s.acquire(&e, Endpoint(1), t(1), &[cap]), Ok(()));
    assert_eq!(s.holder_of(Endpoint(1)), Some(t(1)));
    // A second acquirer of a held endpoint is refused (must wait).
    assert_eq!(
        s.acquire(&e, Endpoint(1), t(2), &[cap]),
        Err(SchedError::Held)
    );
    // Waiting on a FREE endpoint is refused — acquire it instead.
    assert_eq!(
        s.wait(&e, Endpoint(2), t(2), &[cap]),
        Err(SchedError::NotHeld)
    );
}

#[test]
fn holder_inherits_a_blocked_waiters_priority() {
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW); // holder L
    s.admit(t(2), HIGH); // waiter H
    s.acquire(&e, Endpoint(1), t(1), &[cap]).unwrap();
    s.wait(&e, Endpoint(1), t(2), &[cap]).unwrap();

    // The low holder now runs at the high waiter's priority — priority inheritance.
    assert_eq!(s.effective_priority(t(1)), HIGH);
    assert_eq!(s.state(t(2)), Some(TaskState::Blocked));
}

#[test]
fn schedule_next_avoids_priority_inversion() {
    // Classic inversion setup: L holds a lock H needs, and an unrelated M is Ready. A priority-blind
    // or naive-priority scheduler would run M (base 5 > L's base 1), starving H indirectly. With
    // inheritance L is boosted to 10 and runs first.
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW); // L
    s.admit(t(2), MED); // M (unrelated)
    s.admit(t(3), HIGH); // H
    s.acquire(&e, Endpoint(1), t(1), &[cap]).unwrap();
    s.wait(&e, Endpoint(1), t(3), &[cap]).unwrap(); // H blocks on L's endpoint

    // Ready = {L(boosted→10), M(5)}; H is blocked. The boosted holder wins.
    assert_eq!(s.schedule_next(), Some(t(1)));
}

#[test]
fn donation_is_transitive_across_a_chain() {
    // H → ep1(L1) → ep0(L0): H blocks on an endpoint L1 holds, and L1 blocks on an endpoint L0 holds.
    // The high priority must propagate all the way down to L0.
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(0), Priority(1)); // L0
    s.admit(t(1), Priority(2)); // L1
    s.admit(t(9), HIGH); // H
    s.acquire(&e, Endpoint(0), t(0), &[cap]).unwrap();
    s.acquire(&e, Endpoint(1), t(1), &[cap]).unwrap();
    s.wait(&e, Endpoint(0), t(1), &[cap]).unwrap(); // L1 blocks on L0's endpoint
    s.wait(&e, Endpoint(1), t(9), &[cap]).unwrap(); // H blocks on L1's endpoint

    assert_eq!(s.effective_priority(t(1)), HIGH, "L1 inherits H");
    assert_eq!(
        s.effective_priority(t(0)),
        HIGH,
        "L0 inherits H transitively through L1"
    );
}

#[test]
fn release_withdraws_donation_and_hands_off_to_highest_waiter() {
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW); // holder L
    s.admit(t(2), MED); // waiter (medium)
    s.admit(t(3), HIGH); // waiter (high)
    s.acquire(&e, Endpoint(1), t(1), &[cap]).unwrap();
    s.wait(&e, Endpoint(1), t(2), &[cap]).unwrap();
    s.wait(&e, Endpoint(1), t(3), &[cap]).unwrap();
    // Holder inherits the MAX of its waiters.
    assert_eq!(s.effective_priority(t(1)), HIGH);

    // Release: the highest-priority waiter (H) wins the endpoint and is unblocked…
    assert_eq!(s.release(Endpoint(1), t(1)), Ok(Some(t(3))));
    assert_eq!(s.holder_of(Endpoint(1)), Some(t(3)));
    assert_eq!(s.state(t(3)), Some(TaskState::Ready));
    // …and the ex-holder's donation is withdrawn — back to its base.
    assert_eq!(s.effective_priority(t(1)), LOW);
    // The medium waiter is still queued behind the new holder.
    assert_eq!(s.state(t(2)), Some(TaskState::Blocked));
}

#[test]
fn release_by_non_holder_is_fail_closed() {
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW);
    s.admit(t(2), MED);
    s.acquire(&e, Endpoint(1), t(1), &[cap]).unwrap();
    assert_eq!(s.release(Endpoint(1), t(2)), Err(SchedError::NotHeld));
    assert_eq!(s.holder_of(Endpoint(1)), Some(t(1)));
}

#[test]
fn unknown_task_cannot_acquire() {
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    assert_eq!(
        s.acquire(&e, Endpoint(1), t(42), &[cap]),
        Err(SchedError::UnknownTask)
    );
}

// ---------------------------------------------------------------------------
// INV-PRIO contract (docs/INVARIANT-CONTRACTS.md) — adversarial cases, ALET-P1-016.
//
// The tests above prove donation happens. These attempt to break the PROPERTY: a holder weaker than
// its waiter, a chain that donates only one hop, donation outliving the release, priority conjured
// from nowhere, a dispatch that inverts anyway, a deadlock cycle, an unauthorized state change.
// ---------------------------------------------------------------------------

/// INV-PRIO-1: over a whole sequence of acquires and waits, a holder's effective priority is NEVER
/// below the effective priority of anyone blocked on an endpoint it holds. The waiter set is tracked
/// in the test (the scheduler exposes holders, not waiter lists), and the property is re-checked after
/// EVERY operation rather than once at the end.
#[test]
fn a_holder_is_never_weaker_than_anyone_waiting_on_it() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    // Ids deliberately inverted against priorities, so id order cannot accidentally satisfy this.
    for (id, p) in [
        (1u64, LOW),
        (2, HIGH),
        (3, MED),
        (4, Priority(7)),
        (5, Priority(3)),
    ] {
        s.admit(t(id), p);
    }
    let eps = [Endpoint(100), Endpoint(200)];
    // model: (endpoint, waiter) pairs this test has created.
    let mut waiting: alloc_vec::Vec<(Endpoint, TaskId)> = alloc_vec::Vec::new();

    let assert_no_inversion =
        |s: &PriorityScheduler, waiting: &[(Endpoint, TaskId)], step: &str| {
            for (ep, waiter) in waiting {
                if s.state(*waiter) != Some(TaskState::Blocked) {
                    continue; // no longer waiting: handed the endpoint, or finished
                }
                if let Some(h) = s.holder_of(*ep) {
                    if h == *waiter {
                        continue; // this waiter has since been handed the endpoint
                    }
                    let hp = s.effective_priority(h);
                    let wp = s.effective_priority(*waiter);
                    assert!(
                        hp >= wp,
                        "{step}: INV-PRIO-1 violated — holder {h:?} at {hp:?} is weaker than \
                     waiter {waiter:?} at {wp:?} on {ep:?}"
                    );
                }
            }
        };

    s.acquire(&engine, eps[0], t(1), &[cap]).expect("acquire");
    s.acquire(&engine, eps[1], t(5), &[cap]).expect("acquire");
    assert_no_inversion(&s, &waiting, "after acquires");

    // 2 (HIGH) and 4 (7) queue behind 1 (LOW); 3 (MED) queues behind 5 (3).
    for (ep, id) in [(eps[0], 2u64), (eps[0], 4), (eps[1], 3)] {
        s.wait(&engine, ep, t(id), &[cap]).expect("wait");
        waiting.push((ep, t(id)));
        assert_no_inversion(&s, &waiting, "after a wait");
    }
    // The inversion the mechanism exists to prevent: a LOW holder with a HIGH waiter.
    assert!(
        s.effective_priority(t(1)) >= HIGH,
        "INV-PRIO-1: the LOW holder did not inherit its HIGH waiter's priority"
    );

    // Releasing hands the endpoint to the STRONGEST waiter (2 = HIGH), not the first in line — and the
    // property must still hold for whoever is left waiting.
    let winner = s.release(eps[0], t(1)).expect("release");
    assert_eq!(
        winner,
        Some(t(2)),
        "INV-PRIO-5: the endpoint went to a weaker waiter than the strongest one"
    );
    assert_no_inversion(&s, &waiting, "after handoff");
    let second = s.release(eps[0], t(2)).expect("release");
    assert_eq!(
        second,
        Some(t(4)),
        "the remaining waiter must be handed the endpoint"
    );
    assert_no_inversion(&s, &waiting, "after second handoff");
}

/// INV-PRIO-2: donation must follow the WHOLE chain. A→B→C: C holds what B waits on, B holds what A
/// waits on, so C must inherit A's priority — not merely B's.
#[test]
fn donation_follows_the_whole_chain_not_just_one_hop() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), HIGH); // A
    s.admit(t(2), MED); // B
    s.admit(t(3), LOW); // C
    let ep_b = Endpoint(1);
    let ep_c = Endpoint(2);
    s.acquire(&engine, ep_b, t(2), &[cap])
        .expect("B holds ep_b");
    s.acquire(&engine, ep_c, t(3), &[cap])
        .expect("C holds ep_c");
    s.wait(&engine, ep_c, t(2), &[cap]).expect("B waits on C");
    s.wait(&engine, ep_b, t(1), &[cap]).expect("A waits on B");

    assert_eq!(
        s.effective_priority(t(3)),
        HIGH,
        "INV-PRIO-2: C inherited only one hop — the chain A->B->C must donate A's priority to C"
    );
    assert_eq!(
        s.effective_priority(t(2)),
        HIGH,
        "B holds what A waits on, so B must also carry A's priority"
    );
}

/// INV-PRIO-3: donation ends at release — the ex-holder returns to exactly its base priority.
#[test]
fn donation_stops_the_moment_the_endpoint_is_released() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW);
    s.admit(t(2), HIGH);
    let ep = Endpoint(5);
    s.acquire(&engine, ep, t(1), &[cap]).expect("acquire");
    s.wait(&engine, ep, t(2), &[cap]).expect("wait");
    assert_eq!(s.effective_priority(t(1)), HIGH, "donation did not apply");
    s.release(ep, t(1)).expect("release");
    assert_eq!(
        s.effective_priority(t(1)),
        LOW,
        "INV-PRIO-3: the ex-holder kept donated priority after releasing — scheduling escalation"
    );
    // And the new holder carries only its own base (nobody waits on it now).
    assert_eq!(s.effective_priority(t(2)), HIGH);
}

/// INV-PRIO-4: donation lends, it never manufactures. No task's effective priority may exceed the
/// highest BASE priority admitted, in any configuration of holds and waits.
#[test]
fn donation_never_manufactures_priority_above_the_highest_base() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    let bases = [(1u64, LOW), (2, MED), (3, Priority(7)), (4, LOW)];
    for (id, p) in bases {
        s.admit(t(id), p);
    }
    let max_base = bases.iter().map(|(_, p)| *p).max().expect("nonempty");
    // A dense tangle: every task holds one endpoint and waits on the next, round-robin.
    for i in 0..4u64 {
        s.acquire(&engine, Endpoint(i), t(i + 1), &[cap])
            .expect("acquire");
    }
    for i in 0..4u64 {
        let holder_of_next = (i + 1) % 4;
        // t(i+1) waits on the endpoint held by the next task.
        let _ = s.wait(&engine, Endpoint(holder_of_next), t(i + 1), &[cap]);
    }
    for id in 1..=4u64 {
        assert!(
            s.effective_priority(t(id)) <= max_base,
            "INV-PRIO-4: task {id} has effective priority above every base priority in the system"
        );
    }
}

/// INV-PRIO-5: the scheduler must never dispatch a Blocked task, and never dispatch a Ready task
/// while a strictly stronger Ready task exists. Checked over many dispatches.
#[test]
fn the_scheduler_never_runs_a_weaker_ready_task_over_a_stronger_one() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    for (id, p) in [(1u64, LOW), (2, MED), (3, HIGH), (4, Priority(2))] {
        s.admit(t(id), p);
    }
    // Block 3 (HIGH) on an endpoint held by 1 (LOW) — the classic inversion setup.
    let ep = Endpoint(9);
    s.acquire(&engine, ep, t(1), &[cap]).expect("acquire");
    s.wait(&engine, ep, t(3), &[cap]).expect("wait");

    for round in 0..12 {
        let Some(next) = s.schedule_next() else { break };
        assert_ne!(
            s.state(next),
            Some(TaskState::Blocked),
            "INV-PRIO-5: dispatched a BLOCKED task (round {round})"
        );
        let chosen = s.effective_priority(next);
        for id in 1..=4u64 {
            if s.state(t(id)) == Some(TaskState::Ready) {
                assert!(
                    s.effective_priority(t(id)) <= chosen,
                    "INV-PRIO-5: ran {next:?} (prio {chosen:?}) while Ready task {id} was stronger"
                );
            }
        }
        s.finish(next);
    }
}

/// INV-PRIO-6: a donation CYCLE (mutual blocking — a deadlock) must terminate the computation rather
/// than recurse forever. The scheduler cannot fix the deadlock; it must not crash because of it.
#[test]
fn a_donation_cycle_terminates_instead_of_recursing() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), MED);
    s.admit(t(2), HIGH);
    let ep1 = Endpoint(11);
    let ep2 = Endpoint(22);
    s.acquire(&engine, ep1, t(1), &[cap]).expect("1 holds ep1");
    s.acquire(&engine, ep2, t(2), &[cap]).expect("2 holds ep2");
    // Each waits on the other's endpoint: a cycle.
    s.wait(&engine, ep2, t(1), &[cap]).expect("1 waits on ep2");
    s.wait(&engine, ep1, t(2), &[cap]).expect("2 waits on ep1");
    // If donation recursed through the cycle this would never return (or overflow the stack).
    let p1 = s.effective_priority(t(1));
    let p2 = s.effective_priority(t(2));
    assert!(p1 >= MED && p2 >= HIGH, "cycle handling lost base priority");
    assert!(
        p1 <= HIGH && p2 <= HIGH,
        "INV-PRIO-4/6: a cycle manufactured priority"
    );
    // Both are blocked, so nothing is runnable — a deadlock is visible, not a crash.
    assert_eq!(s.state(t(1)), Some(TaskState::Blocked));
    assert_eq!(s.state(t(2)), Some(TaskState::Blocked));
}

/// INV-PRIO-7: an unauthorized acquire or wait changes NOTHING — no holder, no blocked state, no
/// donation. Scheduling state is authority-relevant: an unauthorized `wait` would let any task force
/// a donation onto a holder.
#[test]
fn an_unauthorized_acquire_or_wait_changes_no_scheduling_state() {
    let (engine, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW);
    s.admit(t(2), HIGH);
    let ep = Endpoint(3);

    // No capability offered: the acquire must be refused and leave the endpoint free.
    assert_eq!(
        s.acquire(&engine, ep, t(1), &[]),
        Err(SchedError::Unauthorized)
    );
    assert_eq!(
        s.holder_of(ep),
        None,
        "INV-PRIO-7: an unauthorized acquire took the endpoint"
    );

    // Legitimate holder, then an unauthorized wait: no blocking, and NO donation.
    s.acquire(&engine, ep, t(1), &[cap]).expect("acquire");
    assert_eq!(
        s.wait(&engine, ep, t(2), &[]),
        Err(SchedError::Unauthorized)
    );
    assert_eq!(
        s.state(t(2)),
        Some(TaskState::Ready),
        "INV-PRIO-7: an unauthorized wait blocked the caller anyway"
    );
    assert_eq!(
        s.effective_priority(t(1)),
        LOW,
        "INV-PRIO-7: an unauthorized wait forced a donation onto the holder"
    );
    // An unknown task is refused too, with the state of known tasks untouched.
    assert_eq!(
        s.wait(&engine, ep, t(99), &[cap]),
        Err(SchedError::UnknownTask)
    );
    assert_eq!(s.effective_priority(t(1)), LOW);
}

// ---------------------------------------------------------------------------
// ALET-P3-007 regression pins. The ready pool was a scanned `VecDeque` pruned
// with `retain` — O(n) per dispatch, a quadratic drain, 200 000 admitted tasks
// that never finished dispatching. It is now an ordered set; these tests pin
// that it answers EXACTLY what the scanned pool answered, not merely something
// similar: requeue age, finish-removal, donation re-keying in selection (not
// just in the priority readback), and the ADR-056 advisory tiebreak among equals.
// ---------------------------------------------------------------------------

/// Requeue age: a task that has run rejoins BEHIND its equals — round-robin among equals is FIFO
/// age, and the ordered pool must reproduce the tail-append the VecDeque performed.
#[test]
fn equal_priority_tasks_drain_round_robin_by_requeue_age() {
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), MED);
    s.admit(t(2), MED);
    s.admit(t(3), MED);
    assert_eq!(s.schedule_next(), Some(t(1)));
    assert_eq!(
        s.schedule_next(),
        Some(t(2)),
        "t(1) must requeue behind t(2)"
    );
    assert_eq!(s.schedule_next(), Some(t(3)));
    assert_eq!(
        s.schedule_next(),
        Some(t(1)),
        "the rotation wraps to the oldest"
    );
}

/// Finishing a task that was never dispatched removes it from the rotation entirely.
#[test]
fn finishing_an_undispatched_task_removes_it_from_the_rotation() {
    let mut s = PriorityScheduler::new(ACQ);
    for id in 1..=4 {
        s.admit(t(id), MED);
    }
    s.finish(t(2));
    // Retire every dispatched task, so the run ends when the pool empties rather than when a
    // running task wraps around: exactly three dispatches may come out, and never t(2).
    let mut seen = alloc_vec::Vec::new();
    while let Some(next) = s.schedule_next() {
        seen.push(next);
        s.finish(next);
    }
    assert_eq!(
        seen,
        alloc_vec::Vec::from([t(1), t(3), t(4)]),
        "the finished task must never be dispatched"
    );
}

/// Donation must move SELECTION, not only the `effective_priority` readback: while H is blocked on
/// L's endpoint the boosted L wins over M immediately, and after the release L loses to M again.
/// The first half is the classic inversion test; the second half pins the key coming back DOWN in
/// the ordered pool, which a stale-key implementation would get wrong.
#[test]
fn donation_moves_selection_not_just_the_priority_readback() {
    let (e, cap) = engine();
    let mut s = PriorityScheduler::new(ACQ);
    s.admit(t(1), LOW); // L
    s.admit(t(2), MED); // M
    s.admit(t(3), HIGH); // H
    s.acquire(&e, Endpoint(1), t(1), &[cap]).unwrap();
    assert_eq!(
        s.schedule_next(),
        Some(t(3)),
        "H runs first on base priority"
    );
    s.wait(&e, Endpoint(1), t(3), &[cap]).unwrap(); // H blocks on L's endpoint
    assert_eq!(
        s.schedule_next(),
        Some(t(1)),
        "the boosted holder must outrank the unrelated medium task"
    );
    s.release(Endpoint(1), t(1)).unwrap();
    assert_eq!(
        s.schedule_next(),
        Some(t(3)),
        "H is ready again and highest"
    );
    s.finish(t(3)); // H leaves the rotation so the M-vs-L question is directly visible
    assert_eq!(
        s.schedule_next(),
        Some(t(2)),
        "after the handover L is back at LOW, so M outranks it"
    );
    s.finish(t(2)); // retire M too: a running task would otherwise keep the CPU
    assert_eq!(s.schedule_next(), Some(t(1)), "L runs last of the three");
}

/// The ADR-056 advisory tiebreak pinned directly on the scheduler, among EQUAL priorities:
/// an age-oldest decisive-`Elevated` leader is displaced by the OLDEST decisive-`Low` challenger;
/// a `Low` or abstaining leader is displaced by nobody, because the tiebreak needs a decisive
/// opinion about both sides.
#[test]
fn the_advisory_tiebreak_among_equals_follows_adr_056_exactly() {
    use kernel_core::mlrisk::{Advice, Verdict};

    let adv = |v: Verdict| Advice {
        verdict: v,
        margin: 0,
        out_of_range: false,
        degenerate: false,
    };
    let abstain = Advice {
        verdict: Verdict::Abstain,
        margin: 0,
        out_of_range: false,
        degenerate: false,
    };

    // Elevated leader, one Low challenger: the Low task goes first.
    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), MED, adv(Verdict::Elevated));
    s.admit_with_advice(t(2), MED, adv(Verdict::Low));
    assert_eq!(s.schedule_next(), Some(t(2)));

    // Low leader, Elevated challenger: the leader keeps the CPU.
    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), MED, adv(Verdict::Low));
    s.admit_with_advice(t(2), MED, adv(Verdict::Elevated));
    assert_eq!(s.schedule_next(), Some(t(1)));

    // Abstaining leader, Low challenger: no opinion about the leader ⇒ no displacement.
    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), MED, abstain);
    s.admit_with_advice(t(2), MED, adv(Verdict::Low));
    assert_eq!(s.schedule_next(), Some(t(1)));

    // Elevated, abstain, Low, Low: the FIRST Low in age order wins, not a later one.
    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), MED, adv(Verdict::Elevated));
    s.admit_with_advice(t(2), MED, abstain);
    s.admit_with_advice(t(3), MED, adv(Verdict::Low));
    s.admit_with_advice(t(4), MED, adv(Verdict::Low));
    assert_eq!(s.schedule_next(), Some(t(3)));

    // Two Elevateds then two Lows: still the oldest Low.
    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), MED, adv(Verdict::Elevated));
    s.admit_with_advice(t(2), MED, adv(Verdict::Elevated));
    s.admit_with_advice(t(3), MED, adv(Verdict::Low));
    s.admit_with_advice(t(4), MED, adv(Verdict::Low));
    assert_eq!(s.schedule_next(), Some(t(3)));

    // And none of this crosses a priority band: a Low verdict never lifts a task above a genuinely
    // higher-priority neighbour.
    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), HIGH, adv(Verdict::Elevated));
    s.admit_with_advice(t(2), MED, adv(Verdict::Low));
    assert_eq!(s.schedule_next(), Some(t(1)));
}

/// The band's decisive-`Low` census survives EMPTYING (ADR-087). The census is kept when its last
/// member leaves — deleting it there cost a fresh `BTreeSet` on the next `Low` admission, about
/// sixty bytes per dispatch on a heap that never frees. What must stay true behaviourally is
/// this: after the band empties and a new `Low` arrives, that task still displaces an `Elevated`
/// leader exactly as ADR-056 says. A kept-but-stale census would break this; so would a deleted
/// one that came back wrong.
#[test]
fn a_band_census_that_emptied_still_displaces_an_elevated_leader() {
    use kernel_core::mlrisk::{Advice, Verdict};

    let adv = |v: Verdict| Advice {
        verdict: v,
        margin: 0,
        out_of_range: false,
        degenerate: false,
    };

    let mut s = PriorityScheduler::new(ACQ);
    s.admit_with_advice(t(1), MED, adv(Verdict::Elevated));
    s.admit_with_advice(t(2), MED, adv(Verdict::Low));
    // The Low member displaces the Elevated leader, then leaves: the band's census is now empty.
    assert_eq!(s.schedule_next(), Some(t(2)));
    s.finish(t(2));
    // With nobody Low left, the Elevated leader runs — an empty census is "no Low member".
    assert_eq!(s.schedule_next(), Some(t(1)));
    s.finish(t(1));

    // A new Low arrival at the SAME priority must displace a new Elevated leader, proving the
    // census that emptied is still the census that works.
    s.admit_with_advice(t(3), MED, adv(Verdict::Elevated));
    s.admit_with_advice(t(4), MED, adv(Verdict::Low));
    assert_eq!(s.schedule_next(), Some(t(4)));
    assert_eq!(s.risk_of(t(4)), Some(Verdict::Low));
}
