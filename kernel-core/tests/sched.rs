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
    assert_eq!(
        seq,
        vec![A, B, A, B, A, B],
        "two Ready tasks round-robin fairly"
    );
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
    assert_eq!(
        ctx[&A].runs, 3,
        "each task ran on half the slices via the TaskContext seam"
    );
    assert_eq!(ctx[&B].runs, 3);
}

#[test]
fn lone_runnable_task_keeps_running() {
    let mut rr = RoundRobin::new();
    rr.spawn(A);
    assert_eq!(rr.schedule_next(), Some(A));
    assert_eq!(
        rr.schedule_next(),
        Some(A),
        "a single Ready task is picked every slice"
    );
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
    assert_eq!(
        rr.schedule_next(),
        Some(B),
        "only B is runnable while A is blocked"
    );
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
    assert!(
        !rest.contains(&A),
        "a finished task is never scheduled again"
    );
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

// ---------------------------------------------------------------------------
// INV-TASK contract (docs/INVARIANT-CONTRACTS.md) — task lifecycle, ALET-P1-015.
//
// The tests above prove the scheduler dispatches fairly. These attack the LIFECYCLE: a state machine
// with four states has transitions that must be impossible, and until they were written down nothing
// said which. Each test drives long sequences and checks the property after EVERY step, because a
// lifecycle bug is usually a state that is only briefly wrong.
// ---------------------------------------------------------------------------

/// Every state a task can be in, for the exhaustive sweeps below.
const STATES: [TaskState; 4] = [
    TaskState::Ready,
    TaskState::Running,
    TaskState::Blocked,
    TaskState::Finished,
];

/// Drive `s` into `want` for task `id`, or return false if that state is not reachable that way.
fn drive_to(s: &mut RoundRobin, id: TaskId, want: TaskState) -> bool {
    match want {
        TaskState::Ready => s.state(id) == Some(TaskState::Ready),
        TaskState::Running => s.schedule_next() == Some(id),
        TaskState::Blocked => {
            s.block(id);
            true
        }
        TaskState::Finished => {
            s.finish(id);
            true
        }
    }
}

/// INV-TASK-1: `Finished` is terminal. No event — block, unblock, finish again, or a dispatch round —
/// may bring a finished task back to Ready, Running or Blocked.
#[test]
fn finished_is_terminal_whatever_arrives_afterwards() {
    for interference in STATES {
        let mut s = RoundRobin::new();
        s.spawn(TaskId(1));
        s.spawn(TaskId(2));
        s.finish(TaskId(1));
        assert_eq!(s.state(TaskId(1)), Some(TaskState::Finished));
        // Anything at all happens next; task 1 must still be Finished.
        let _ = drive_to(&mut s, TaskId(1), interference);
        s.unblock(TaskId(1));
        s.block(TaskId(1));
        s.finish(TaskId(1));
        for _ in 0..5 {
            let next = s.schedule_next();
            assert_ne!(
                next,
                Some(TaskId(1)),
                "INV-TASK-1: a finished task was dispatched (after {interference:?})"
            );
        }
        assert_eq!(
            s.state(TaskId(1)),
            Some(TaskState::Finished),
            "INV-TASK-1: a finished task left the Finished state (after {interference:?})"
        );
    }
}

/// INV-TASK-2: a Blocked task is never dispatched, however many rounds run, and unblocking is what
/// makes it eligible again — not the passage of dispatches.
#[test]
fn a_blocked_task_is_never_dispatched_until_it_is_unblocked() {
    let mut s = RoundRobin::new();
    for id in 1..=4u64 {
        s.spawn(TaskId(id));
    }
    s.block(TaskId(3));
    for round in 0..20 {
        if let Some(next) = s.schedule_next() {
            assert_ne!(
                next,
                TaskId(3),
                "INV-TASK-2: dispatched a blocked task at round {round}"
            );
        }
    }
    assert_eq!(s.state(TaskId(3)), Some(TaskState::Blocked));
    s.unblock(TaskId(3));
    assert_eq!(s.state(TaskId(3)), Some(TaskState::Ready));
    let mut seen = false;
    for _ in 0..20 {
        if s.schedule_next() == Some(TaskId(3)) {
            seen = true;
            break;
        }
    }
    assert!(seen, "INV-TASK-2: an unblocked task never became eligible");
}

/// INV-TASK-3: at most ONE task is Running at any time, checked after every event in a long sequence.
/// Two Running tasks on one core means the scheduler believes something impossible.
#[test]
fn at_most_one_task_is_running_after_every_event() {
    let mut s = RoundRobin::new();
    for id in 1..=6u64 {
        s.spawn(TaskId(id));
    }
    let mut rng: u64 = 0x5EED;
    let step = |s: &mut RoundRobin, rng: &mut u64| {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        let id = TaskId(*rng % 6 + 1);
        match *rng % 4 {
            0 => {
                s.schedule_next();
            }
            1 => s.block(id),
            2 => s.unblock(id),
            _ => s.finish(id),
        }
    };
    for i in 0..500 {
        step(&mut s, &mut rng);
        let running = (1..=6u64)
            .filter(|id| s.state(TaskId(*id)) == Some(TaskState::Running))
            .count();
        assert!(
            running <= 1,
            "INV-TASK-3: {running} tasks were Running at once (step {i})"
        );
        // And `current` agrees with the state table: a scheduler whose two views disagree will resume
        // the wrong context.
        match s.current() {
            Some(c) => assert_eq!(
                s.state(c),
                Some(TaskState::Running),
                "INV-TASK-3: current() names a task that is not Running (step {i})"
            ),
            None => assert_eq!(
                running, 0,
                "INV-TASK-3: a task is Running with no current (step {i})"
            ),
        }
    }
}

/// INV-TASK-4: `runnable_len` always equals the ROTATION — the tasks that will be dispatched in some
/// future round. That is Ready **plus** the currently Running one, which is rotated to the tail and runs
/// again if nothing else is eligible (a lone Ready task keeps running). It must never count a Blocked or
/// Finished task: a count that drifts from the queue is how a scheduler comes to believe it has work and
/// spins, or believes it has none and idles with work pending.
#[test]
fn the_runnable_count_never_drifts_from_the_dispatchable_set() {
    let mut s = RoundRobin::new();
    for id in 1..=5u64 {
        s.spawn(TaskId(id));
    }
    let events: [(u64, u8); 12] = [
        (1, 0),
        (2, 1),
        (3, 2),
        (1, 3),
        (4, 1),
        (2, 2),
        (5, 3),
        (3, 1),
        (4, 2),
        (1, 1),
        (2, 3),
        (5, 0),
    ];
    for (i, (id, ev)) in events.iter().enumerate() {
        match ev {
            0 => {
                s.schedule_next();
            }
            1 => s.block(TaskId(*id)),
            2 => s.unblock(TaskId(*id)),
            _ => s.finish(TaskId(*id)),
        }
        let dispatchable = (1..=5u64)
            .filter(|t| {
                matches!(
                    s.state(TaskId(*t)),
                    Some(TaskState::Ready) | Some(TaskState::Running)
                )
            })
            .count();
        assert_eq!(
            s.runnable_len(),
            dispatchable,
            "INV-TASK-4: runnable_len {} != {dispatchable} dispatchable (event {i})",
            s.runnable_len()
        );
    }
}

/// INV-TASK-5: an event naming an UNKNOWN task changes nothing — no state appears for it, and no known
/// task is disturbed. A scheduler that invented a task from a stray id would dispatch a context that
/// does not exist.
#[test]
fn an_event_for_an_unknown_task_changes_nothing() {
    let mut s = RoundRobin::new();
    s.spawn(TaskId(1));
    s.spawn(TaskId(2));
    let before: alloc::vec::Vec<_> = (1..=2u64).map(|id| s.state(TaskId(id))).collect();
    let before_len = s.runnable_len();
    for ghost in [TaskId(99), TaskId(0), TaskId(u64::MAX)] {
        s.block(ghost);
        s.unblock(ghost);
        s.finish(ghost);
        assert_eq!(
            s.state(ghost),
            None,
            "INV-TASK-5: an unknown task acquired a state"
        );
    }
    let after: alloc::vec::Vec<_> = (1..=2u64).map(|id| s.state(TaskId(id))).collect();
    assert_eq!(
        before, after,
        "INV-TASK-5: a ghost event disturbed a real task"
    );
    assert_eq!(s.runnable_len(), before_len);
}

extern crate alloc;
