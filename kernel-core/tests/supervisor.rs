//! Host proofs for the task supervisor (REQ-REL-002, ADR-042).
//!
//! The live half is proved on x86-64: a ring-3 task faults at an address it never declared, the supervisor
//! terminates it, and the boot continues with a later task still running. This file proves the POLICY over
//! many tasks and orderings — that containment is containment, not luck.
use kernel_core::faultclass::{classify, from_x86_error_code, verdict, FaultKind};
use kernel_core::sched::TaskId;
use kernel_core::supervisor::{Supervisor, SupervisorAction, TerminationReason};

fn user_fault() -> (FaultKind, kernel_core::faultclass::FaultVerdict) {
    let f = from_x86_error_code(0b100); // user, not present
    let k = classify(&f);
    (k, verdict(k))
}

#[test]
fn terminating_many_tasks_never_touches_a_survivor() {
    let mut s = Supervisor::new();
    let (kind, v) = user_fault();
    // Kill every third task out of 60, in a scattered order, and re-check ALL of them after each kill.
    let doomed: alloc_vec::Vec<u64> = (0..60u64).filter(|i| i % 3 == 0).rev().collect();
    for (n, id) in doomed.iter().enumerate() {
        assert_eq!(
            s.on_fault(Some(TaskId(*id)), kind, v),
            SupervisorAction::TaskTerminated(TerminationReason::Fault(kind))
        );
        assert_eq!(s.terminated(), n + 1);
        for other in 0..60u64 {
            let should_run = other % 3 != 0 || !doomed[..=n].contains(&other);
            assert_eq!(
                s.may_run(TaskId(other)),
                should_run,
                "after {} kills, task {other} was wrong",
                n + 1
            );
        }
    }
    assert_eq!(s.escalations(), 0, "no user fault should have escalated");
}

#[test]
fn a_kernel_fault_never_becomes_a_task_death_however_many_arrive() {
    let mut s = Supervisor::new();
    for code in [0b000u64, 0b001, 0b010, 0b011, 0b1000, 0b1011] {
        let f = from_x86_error_code(code);
        let k = classify(&f);
        let before = s.terminated();
        assert!(matches!(
            s.on_fault(Some(TaskId(1)), k, verdict(k)),
            SupervisorAction::Escalate(_)
        ));
        assert_eq!(s.terminated(), before, "a kernel fault killed a task");
    }
    assert!(s.may_run(TaskId(1)));
    assert_eq!(s.escalations(), 6);
}

#[test]
fn repeated_faults_from_one_dead_task_do_not_multiply_its_record() {
    let mut s = Supervisor::new();
    let (kind, v) = user_fault();
    for _ in 0..10 {
        let _ = s.on_fault(Some(TaskId(4)), kind, v);
    }
    assert_eq!(s.terminated(), 1, "one task died, once");
    assert_eq!(s.reason(TaskId(4)), Some(TerminationReason::Fault(kind)));
}

#[test]
fn an_exit_and_a_policy_kill_are_distinguishable_from_a_fault() {
    let mut s = Supervisor::new();
    s.terminate(TaskId(1), TerminationReason::Exited);
    s.terminate(TaskId(2), TerminationReason::Policy);
    let (kind, v) = user_fault();
    let _ = s.on_fault(Some(TaskId(3)), kind, v);
    assert_eq!(s.reason(TaskId(1)), Some(TerminationReason::Exited));
    assert_eq!(s.reason(TaskId(2)), Some(TerminationReason::Policy));
    assert_eq!(s.reason(TaskId(3)), Some(TerminationReason::Fault(kind)));
    assert_eq!(s.terminated(), 3);
}

extern crate alloc as alloc_crate;
use alloc_crate::vec as alloc_vec;
