//! Priority-inheritance blocking IPC + priority-aware scheduling (REQ-IPC-009, ADR-020).
//!
//! The round-robin scheduler ([`crate::sched::RoundRobin`]) is fair but priority-blind, so it is prey
//! to **unbounded priority inversion**: a high-priority task H blocks waiting on an endpoint held by a
//! low-priority task L, and a medium task M — needing neither — preempts L indefinitely, so H waits on
//! M through L. A real microkernel breaks this with **priority inheritance**: while H is blocked on an
//! endpoint L holds, L temporarily *inherits* H's priority, runs ahead of M, finishes, and releases —
//! bounding the inversion to L's own critical section.
//!
//! This module is the **arch-independent** reification of that discipline, so all three CPU targets
//! inherit it from one source (ADR-019). It owns the *policy*: task base priorities, an endpoint
//! ownership + wait graph, transitive priority donation across a chain of held endpoints, and a
//! priority-aware "run the highest effective priority Ready task" selection. Acquiring or waiting on an
//! endpoint is authorized by the SAME [`CapEngine`] the deterministic pipeline uses — fail-closed, no
//! ambient access to a kernel endpoint. It owns NO registers and NO assembly: the actual context
//! switch stays each target's [`crate::sched::TaskContext`] seam, exactly as [`crate::sched`] already
//! splits scheduling policy from the arch mechanism.
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::sched::{TaskId, TaskState};
use crate::spine::{CapEngine, CapToken, Decision, Target};

/// A scheduling priority: **higher value = more urgent**. Base priorities are assigned on admission;
/// a task's *effective* priority may rise above its base via inheritance while it holds an endpoint a
/// higher task is blocked on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Priority(pub u8);

/// A kernel endpoint (an IPC server port / a lock). Held by at most one task at a time; others
/// `wait` on it and are donated-to by whoever is blocked.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Endpoint(pub u64);

/// Why a priority-scheduler operation was refused (fail-closed: nothing changes on error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchedError {
    /// The endpoint-acquire capability check did not return `Allow`.
    Unauthorized,
    /// The endpoint is already held by another task (cannot acquire; use `wait`).
    Held,
    /// The endpoint is free (nothing to wait on) or the task does not hold it (cannot release).
    NotHeld,
    /// No such admitted task.
    UnknownTask,
}

/// Priority-inheritance scheduler over an abstract task set + endpoint graph. Pure policy — no
/// registers, no asm, no arch deps; a backend drives it and performs the context switch behind the
/// [`crate::sched::TaskContext`] seam.
#[derive(Default)]
pub struct PriorityScheduler {
    acquire_action: String,
    base: BTreeMap<TaskId, Priority>,
    state: BTreeMap<TaskId, TaskState>,
    order: VecDeque<TaskId>,
    current: Option<TaskId>,
    /// endpoint → the task currently holding it.
    holder: BTreeMap<Endpoint, TaskId>,
    /// endpoint → tasks blocked waiting on it (FIFO).
    waiters: BTreeMap<Endpoint, Vec<TaskId>>,
    /// task → the endpoint it is currently blocked on (for transitive donation).
    blocked_on: BTreeMap<TaskId, Endpoint>,
}

impl PriorityScheduler {
    /// A scheduler whose endpoint acquisition/waiting is gated by capability `acquire_action`.
    pub fn new(acquire_action: &str) -> Self {
        PriorityScheduler {
            acquire_action: acquire_action.to_string(),
            ..Default::default()
        }
    }

    /// Admit a task Ready at `base` priority (at the tail of the FIFO tiebreak order).
    pub fn admit(&mut self, id: TaskId, base: Priority) {
        self.base.insert(id, base);
        self.state.insert(id, TaskState::Ready);
        if !self.order.contains(&id) {
            self.order.push_back(id);
        }
    }

    /// Acquire a free endpoint, authorized by `acquire_action`. Fail-closed: no capability ⇒
    /// `Unauthorized` and nothing is held; a busy endpoint ⇒ `Held` (the caller should `wait`).
    pub fn acquire(
        &mut self,
        engine: &CapEngine,
        ep: Endpoint,
        task: TaskId,
        offered: &[CapToken],
    ) -> Result<(), SchedError> {
        if !self.base.contains_key(&task) {
            return Err(SchedError::UnknownTask);
        }
        if engine.evaluate(&self.acquire_action, &Target::default(), offered) != Decision::Allow {
            return Err(SchedError::Unauthorized);
        }
        if self.holder.contains_key(&ep) {
            return Err(SchedError::Held);
        }
        self.holder.insert(ep, task);
        Ok(())
    }

    /// Block `task` waiting on an endpoint held by another task, authorized by `acquire_action`.
    /// The waiter goes `Blocked` and — this is the inheritance — the holder (transitively) inherits
    /// the waiter's priority for as long as it holds the endpoint. Fail-closed: unauthorized ⇒
    /// `Unauthorized`; a free endpoint ⇒ `NotHeld` (acquire it instead).
    pub fn wait(
        &mut self,
        engine: &CapEngine,
        ep: Endpoint,
        task: TaskId,
        offered: &[CapToken],
    ) -> Result<(), SchedError> {
        if !self.base.contains_key(&task) {
            return Err(SchedError::UnknownTask);
        }
        if engine.evaluate(&self.acquire_action, &Target::default(), offered) != Decision::Allow {
            return Err(SchedError::Unauthorized);
        }
        if !self.holder.contains_key(&ep) {
            return Err(SchedError::NotHeld);
        }
        self.state.insert(task, TaskState::Blocked);
        self.order.retain(|&t| t != task);
        if self.current == Some(task) {
            self.current = None;
        }
        self.waiters.entry(ep).or_default().push(task);
        self.blocked_on.insert(task, ep);
        Ok(())
    }

    /// Release an endpoint the task holds. The endpoint is handed to its highest-effective-priority
    /// waiter (FIFO tiebreak), which is unblocked and becomes the new holder; donation is recomputed
    /// implicitly (it is always derived on read). Returns the newly-unblocked holder, if any.
    pub fn release(&mut self, ep: Endpoint, task: TaskId) -> Result<Option<TaskId>, SchedError> {
        if self.holder.get(&ep) != Some(&task) {
            return Err(SchedError::NotHeld);
        }
        self.holder.remove(&ep);
        let winner = self.take_best_waiter(ep);
        match winner {
            Some(w) => {
                self.blocked_on.remove(&w);
                self.holder.insert(ep, w);
                self.state.insert(w, TaskState::Ready);
                if !self.order.contains(&w) {
                    self.order.push_back(w);
                }
                Ok(Some(w))
            }
            None => Ok(None),
        }
    }

    /// Remove and return the highest-effective-priority waiter on `ep` (FIFO among equals).
    fn take_best_waiter(&mut self, ep: Endpoint) -> Option<TaskId> {
        let list = self.waiters.get(&ep)?;
        if list.is_empty() {
            return None;
        }
        // Choose by effective priority, breaking ties by earliest enqueue (position in the FIFO list).
        let mut best_idx = 0usize;
        let mut best_prio = self.effective_priority(list[0]);
        for (i, &w) in list.iter().enumerate().skip(1) {
            let p = self.effective_priority(w);
            if p > best_prio {
                best_prio = p;
                best_idx = i;
            }
        }
        let list = self.waiters.get_mut(&ep)?;
        Some(list.remove(best_idx))
    }

    /// A task's **effective** priority: the max of its base and the effective priorities of every task
    /// (transitively) blocked on an endpoint it holds — priority donation. Cycles (a deadlock) are
    /// broken by a visited set so donation terminates rather than recursing forever.
    pub fn effective_priority(&self, task: TaskId) -> Priority {
        let mut visited = BTreeSet::new();
        self.effective_inner(task, &mut visited)
    }

    fn effective_inner(&self, task: TaskId, visited: &mut BTreeSet<TaskId>) -> Priority {
        if !visited.insert(task) {
            // Already on the current donation chain — a cycle; contribute only this task's base.
            return self.base.get(&task).copied().unwrap_or(Priority(0));
        }
        let mut best = self.base.get(&task).copied().unwrap_or(Priority(0));
        // Every endpoint this task holds: whoever waits on it donates their effective priority.
        for (ep, h) in self.holder.iter() {
            if *h != task {
                continue;
            }
            if let Some(list) = self.waiters.get(ep) {
                for &w in list {
                    let donated = self.effective_inner(w, visited);
                    if donated > best {
                        best = donated;
                    }
                }
            }
        }
        best
    }

    /// Pick the next task to run: the Ready (or currently Running) task with the highest **effective**
    /// priority, breaking ties FIFO. The previously-running task, if still Running, rejoins the Ready
    /// pool first. `None` when nothing is runnable. This is where inheritance pays off — a boosted
    /// holder outranks an unrelated medium-priority task.
    pub fn schedule_next(&mut self) -> Option<TaskId> {
        if let Some(cur) = self.current.take() {
            if self.state.get(&cur) == Some(&TaskState::Running) {
                self.state.insert(cur, TaskState::Ready);
                if !self.order.contains(&cur) {
                    self.order.push_back(cur);
                }
            }
        }
        // Scan the FIFO order (which encodes tiebreak age) for the max effective priority Ready task.
        let mut best: Option<(TaskId, Priority)> = None;
        for &t in &self.order {
            if self.state.get(&t) != Some(&TaskState::Ready) {
                continue;
            }
            let p = self.effective_priority(t);
            match best {
                Some((_, bp)) if p <= bp => {}
                _ => best = Some((t, p)),
            }
        }
        let (winner, _) = best?;
        self.order.retain(|&t| t != winner);
        self.state.insert(winner, TaskState::Running);
        self.current = Some(winner);
        Some(winner)
    }

    /// Retire a task; it leaves the rotation and any endpoints it held are released to no one (a real
    /// supervisor would reclaim them — that is REQ-REL-001).
    pub fn finish(&mut self, id: TaskId) {
        self.state.insert(id, TaskState::Finished);
        self.order.retain(|&t| t != id);
        if self.current == Some(id) {
            self.current = None;
        }
        let held: Vec<Endpoint> = self
            .holder
            .iter()
            .filter(|(_, h)| **h == id)
            .map(|(e, _)| *e)
            .collect();
        for e in held {
            self.holder.remove(&e);
        }
    }

    pub fn state(&self, id: TaskId) -> Option<TaskState> {
        self.state.get(&id).copied()
    }

    pub fn holder_of(&self, ep: Endpoint) -> Option<TaskId> {
        self.holder.get(&ep).copied()
    }
}
