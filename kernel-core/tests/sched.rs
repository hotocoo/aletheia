//! Hosted tests for the arch-independent scheduler policy (gap-register Issue 1: shared kernel-core
//! task/scheduler abstraction). These prove the round-robin ordering + lifecycle transitions that the
//! three targets' `usermode.rs` currently hand-roll, exactly once, on the host — the same shape the
//! VM gates prove per-target for the real asm context switch.

use std::collections::BTreeMap;

use kernel_core::sched::{RoundRobin, TaskContext, TaskId, TaskState};

/// A mock execution context: `resume()` just records that the backend ran this task. Stands in for a
/// real target's register save/restore so the *policy* is testable with no CPU state.
struct Mock {
    runs: u32,
}
impl TaskContext for Mock {
    fn resume(&mut self) {
        self.runs += 1;
    }
}

const A: TaskId = TaskId(1);
const B: TaskId = TaskId(2);
const C: TaskId = TaskId(3);

#[test]
fn round_robin_two_tasks_interleave_ababab() {
    let mut rr = RoundRobin::new();
    rr.spawn(A);
    rr.spawn(B);
    let seq: Vec<TaskId> = (0..6).map(|_| rr.schedule_next().unwrap()).collect();
    assert_eq!(seq, vec![A, B, A, B, A, B], "two Ready tasks round-robin fairly");
    assert_eq!(rr.runnable_len(), 2);
}

#[test]
fn resume_drives_the_backend_context_each_slice() {
    let mut rr = RoundRobin::new();
    let mut ctx: BTreeMap<TaskId, Mock> = BTreeMap::new();
    for id in [A, B] {
        rr.spawn(id);
        ctx.insert(id, Mock { runs: 0 });
    }
    // Six slices, each resuming whichever task the scheduler picked.
    for _ in 0..6 {
        let t = rr.schedule_next().unwrap();
        ctx.get_mut(&t).unwrap().resume();
    }
    assert_eq!(ctx[&A].runs, 3, "each task ran on half the slices via the TaskContext seam");
    assert_eq!(ctx[&B].runs, 3);
}

#[test]
fn lone_runnable_task_keeps_running() {
    let mut rr = RoundRobin::new();
    rr.spawn(A);
    assert_eq!(rr.schedule_next(), Some(A));
    assert_eq!(rr.schedule_next(), Some(A), "a single Ready task is picked every slice");
    assert_eq!(rr.current(), Some(A));
}

#[test]
fn blocked_task_leaves_rotation_and_returns_on_unblock() {
    let mut rr = RoundRobin::new();
    rr.spawn(A);
    rr.spawn(B);
    assert_eq!(rr.schedule_next(), Some(A));
    rr.block(A); // A was running -> now Blocked and off the rotation
    assert_eq!(rr.state(A), Some(TaskState::Blocked));
    assert_eq!(rr.schedule_next(), Some(B), "the blocked task is skipped");
    assert_eq!(rr.schedule_next(), Some(B), "only B is runnable while A is blocked");
    rr.unblock(A);
    // Rotation resumes fairly between both once A is Ready again.
    assert_eq!(rr.schedule_next(), Some(A));
    assert_eq!(rr.schedule_next(), Some(B));
}

#[test]
fn finished_task_never_runs_again() {
    let mut rr = RoundRobin::new();
    for id in [A, B, C] {
        rr.spawn(id);
    }
    assert_eq!(rr.schedule_next(), Some(A));
    rr.finish(A);
    assert_eq!(rr.state(A), Some(TaskState::Finished));
    // A is gone; B and C round-robin, A never reappears.
    let rest: Vec<TaskId> = (0..4).map(|_| rr.schedule_next().unwrap()).collect();
    assert_eq!(rest, vec![B, C, B, C]);
    assert!(!rest.contains(&A), "a finished task is never scheduled again");
    assert_eq!(rr.runnable_len(), 2);
}

#[test]
fn schedule_next_is_none_when_nothing_is_runnable() {
    let mut rr = RoundRobin::new();
    rr.spawn(A);
    rr.finish(A);
    assert_eq!(rr.schedule_next(), None, "no Ready task => nothing to run");
    assert_eq!(rr.runnable_len(), 0);
}
