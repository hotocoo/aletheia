//! Priority-inheritance + priority-aware scheduling invariants (REQ-IPC-009, ADR-020).
//!
//! Proved on the host (no QEMU), the arch-independent policy: capability-gated endpoint access,
//! priority donation from a blocked waiter to its holder (incl. transitively), the resulting
//! avoidance of unbounded priority inversion in `schedule_next`, and withdrawal of donation on
//! release — all fail-closed.

use kernel_core::priosched::{Endpoint, Priority, PriorityScheduler, SchedError};
use kernel_core::sched::{TaskId, TaskState};
use kernel_core::spine::{CapEngine, Constraints, Scope};

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
    assert_eq!(s.acquire(&e, Endpoint(1), t(1), &[]), Err(SchedError::Unauthorized));
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
    assert_eq!(s.acquire(&e, Endpoint(1), t(2), &[cap]), Err(SchedError::Held));
    // Waiting on a FREE endpoint is refused — acquire it instead.
    assert_eq!(s.wait(&e, Endpoint(2), t(2), &[cap]), Err(SchedError::NotHeld));
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
    assert_eq!(s.effective_priority(t(0)), HIGH, "L0 inherits H transitively through L1");
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
    assert_eq!(s.acquire(&e, Endpoint(1), t(42), &[cap]), Err(SchedError::UnknownTask));
}
