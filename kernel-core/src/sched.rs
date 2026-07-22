//! Arch-independent task + scheduler abstractions (gap-register Issue 1: shared `kernel-core`
//! substrate — "task abstraction", "scheduler interfaces").
//!
//! Today each target's `usermode.rs` hand-rolls the same round-robin ordering next to its
//! arch-specific context switch (the aarch64 `TrapFrame`/`eret`, the x86-64 `iretq`, the RISC-V
//! `sret`). This module lifts the **policy** — which Ready task runs next, and how blocking /
//! unblocking / finishing change the rotation — into one arch-independent place, so the ordering is
//! defined and proved exactly once (on the host, no QEMU) instead of three times by hand. It owns NO
//! registers, NO assembly, and NO architecture dependency: the backend keeps the **mechanism** (save
//! /restore + address-space switch) behind the [`TaskContext`] seam.
//!
//! This is the same split kernel-core already uses for the spine (shared logic) + `Hal`
//! (arch backend). Wiring each target's `usermode.rs` to drive this scheduler in place of its bespoke
//! rotation is the documented follow-on (the per-target asm context switch is unchanged and already
//! VM-gated); this brick lands the shared abstraction + its hosted invariants.
use alloc::collections::{BTreeMap, VecDeque};

/// A scheduled task's identity. Backend-assigned; the scheduler only ever compares/rotates ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct TaskId(pub u64);

/// Where a task is in its lifecycle. `Running` is the single task the backend is currently executing;
/// `Ready` tasks await their slice; `Blocked` tasks are off the rotation until unblocked; `Finished`
/// tasks never run again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,
    Finished,
}

/// The backend seam: a saved execution context the backend knows how to resume (restore registers,
/// switch address space, and return to the task). kernel-core decides *which* context runs next; the
/// backend performs the *mechanism* of running it. The hosted tests implement this over a mock that
/// merely records that it ran, so the scheduling policy is provable with no real CPU state.
pub trait TaskContext {
    /// Resume execution of this task's saved context (arch-specific in a real backend).
    fn resume(&mut self);
}

/// A round-robin scheduler over an abstract task set. Pure policy — no registers, no asm, no arch
/// deps. A backend drives it: `spawn` tasks, call `schedule_next` to obtain the next Ready task to
/// run (rotating the previously-running one to the tail), and report `block` / `unblock` / `finish`
/// as events occur. Fairness is FIFO over Ready tasks; a lone Ready task keeps running.
#[derive(Default)]
pub struct RoundRobin {
    order: VecDeque<TaskId>,
    state: BTreeMap<TaskId, TaskState>,
    current: Option<TaskId>,
}

impl RoundRobin {
    pub fn new() -> Self {
        RoundRobin {
            order: VecDeque::new(),
            state: BTreeMap::new(),
            current: None,
        }
    }

    fn set(&mut self, id: TaskId, s: TaskState) {
        self.state.insert(id, s);
    }

    /// Admit a new task as Ready at the tail of the rotation. Re-spawning an existing id resets its
    /// state to Ready and ensures it is in the rotation exactly once — the caller owns id uniqueness.
    pub fn spawn(&mut self, id: TaskId) {
        self.state.insert(id, TaskState::Ready);
        if !self.order.contains(&id) {
            self.order.push_back(id);
        }
    }

    /// Pick the next Ready task to run, round-robin. The currently-running task (if still Running) is
    /// returned to the tail of the rotation first, then the next Ready task from the front is promoted
    /// to Running and returned. Stale rotation entries (blocked/finished) are skipped. `None` when no
    /// task is Ready.
    pub fn schedule_next(&mut self) -> Option<TaskId> {
        if let Some(cur) = self.current.take() {
            if self.state.get(&cur) == Some(&TaskState::Running) {
                self.set(cur, TaskState::Ready);
                self.order.push_back(cur);
            }
        }
        while let Some(next) = self.order.pop_front() {
            if self.state.get(&next) == Some(&TaskState::Ready) {
                self.set(next, TaskState::Running);
                self.current = Some(next);
                return Some(next);
            }
            // else: a stale entry for a task that has since blocked/finished — drop it.
        }
        None
    }

    /// Take a task off the rotation until it is unblocked. If it was the running task, the CPU is now
    /// idle (the backend should `schedule_next`).
    pub fn block(&mut self, id: TaskId) {
        if self.state.get(&id) == Some(&TaskState::Finished) {
            return;
        }
        self.set(id, TaskState::Blocked);
        self.order.retain(|&t| t != id);
        if self.current == Some(id) {
            self.current = None;
        }
    }

    /// Return a blocked task to the Ready rotation (at the tail). No-op if it is not currently Blocked.
    pub fn unblock(&mut self, id: TaskId) {
        if self.state.get(&id) == Some(&TaskState::Blocked) {
            self.set(id, TaskState::Ready);
            if !self.order.contains(&id) {
                self.order.push_back(id);
            }
        }
    }

    /// Retire a task permanently; it leaves the rotation and never runs again.
    pub fn finish(&mut self, id: TaskId) {
        self.set(id, TaskState::Finished);
        self.order.retain(|&t| t != id);
        if self.current == Some(id) {
            self.current = None;
        }
    }

    pub fn state(&self, id: TaskId) -> Option<TaskState> {
        self.state.get(&id).copied()
    }

    pub fn current(&self) -> Option<TaskId> {
        self.current
    }

    /// Number of tasks currently eligible to run (Ready + the Running one).
    pub fn runnable_len(&self) -> usize {
        self.state
            .values()
            .filter(|s| matches!(s, TaskState::Ready | TaskState::Running))
            .count()
    }
}
