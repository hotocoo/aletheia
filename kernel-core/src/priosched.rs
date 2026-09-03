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
//!
//! **The ready pool is an ordered set, not a scanned list (ALET-P3-007).** It was a
//! `VecDeque<TaskId>` that `schedule_next` scanned for the best task and then pruned with
//! `retain` — O(n) per dispatch, so draining n admitted tasks cost O(n²) and a 200 000-task admit
//! never drained at all (349 s of wall time without producing one dispatch; the 8 000-task gate bench
//! finished in 2 s and hid the curve). The pool is now a `BTreeSet` keyed
//! `(effective priority desc, enqueue seq asc)`: picking the winner is a first-element read,
//! removing or re-keying any task is O(log n), and a full drain is O(n log n). Donation — the one
//! thing that changes a *Ready* task's effective priority under this policy — re-keys exactly the
//! holder chain the event touched, preserving each task's FIFO age, so selection answers are
//! bit-identical to the scanned implementation rather than merely similar (asserted by the whole
//! [`tests/priosched.rs`] suite and the advised-drain permutation gates).
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::cmp::Reverse;

use crate::mlrisk::{Advice, Verdict};
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
    /// The Ready pool, ordered by `(effective priority desc, enqueue seq asc, task)`: the first
    /// element is always the task [`Self::schedule_next`] would pick before the advisory tiebreak.
    /// `seq` is handed out monotonically at every (re)enqueue, which reproduces the tail-append FIFO
    /// age the old scanned `VecDeque` encoded — a task that rejoins the pool gets a *fresh* seq and
    /// therefore goes behind its equals, exactly as before.
    ready: BTreeSet<(Reverse<Priority>, u64, TaskId)>,
    /// task → the `(priority, seq)` its ready key carries, so removing or re-keying an arbitrary
    /// task is O(log n) instead of a scan. A task absent here is not in the pool.
    ready_key: BTreeMap<TaskId, (Priority, u64)>,
    /// Monotonic FIFO-age counter for ready-pool keys.
    next_seq: u64,
    current: Option<TaskId>,
    /// endpoint → the task currently holding it.
    holder: BTreeMap<Endpoint, TaskId>,
    /// endpoint → tasks blocked waiting on it (FIFO).
    waiters: BTreeMap<Endpoint, Vec<TaskId>>,
    /// Total number of tasks actually sitting in [`Self::waiters`] lists. The donation fast path in
    /// [`Self::effective_priority`] used to answer "is any list non-empty" by scanning every list;
    /// this counter makes that question O(1) without changing its answer.
    waiter_count: usize,
    /// task → the endpoint it is currently blocked on (for transitive donation).
    blocked_on: BTreeMap<TaskId, Endpoint>,
    /// task → the risk model's *advisory* verdict, when a model is loaded and decisive (ADR-056).
    ///
    /// Absent for every task when no model is loaded, and absent for a task whose verdict was
    /// `Abstain`, so an abstention is genuinely no opinion rather than a middle opinion. This map
    /// affects **tiebreaks only**: it can never change a task's priority, its state, an authorization
    /// outcome, or whether it is admitted at all (INV-014).
    risk: BTreeMap<TaskId, Verdict>,
    /// Priority → the `(seq, task)` of every decisive-`Low` member currently in the pool, oldest
    /// first. The ADR-056 tiebreak asks for "the oldest Low in the top band" — a question the
    /// scanned implementation answered for free inside its O(n) sweep, and which a naive
    /// ordered-pool port answers with a band scan that costs O(band) per dispatch: quadratic again
    /// the moment a model marks most of a large band `Elevated`, which the shipped blob does for
    /// essentially every task. The 200 000-task gate hung exactly there before this census existed;
    /// with it, the question is one first-element read per band. Priorities are `u8`, so the outer
    /// map never exceeds 256 entries no matter how large the pool grows.
    low_seq: BTreeMap<Priority, BTreeSet<(u64, TaskId)>>,
}

impl PriorityScheduler {
    /// A scheduler whose endpoint acquisition/waiting is gated by capability `acquire_action`.
    pub fn new(acquire_action: &str) -> Self {
        PriorityScheduler {
            acquire_action: acquire_action.to_string(),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // Ready-pool mechanics. Every state transition that touches a task's
    // readiness goes through these three helpers, which is what keeps the
    // ordered set, the reverse key map, and `state` in lockstep.
    // -----------------------------------------------------------------------

    /// Enqueue `id` at effective priority `prio` with a fresh FIFO age (tail of its equals).
    fn ready_enqueue(&mut self, id: TaskId, prio: Priority) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.ready.insert((Reverse(prio), seq, id));
        self.ready_key.insert(id, (prio, seq));
        // The advisory verdict is set before admit (see `admit_with_advice`), so a decisive-Low
        // task joins its band's census at the moment it joins the pool.
        if self.risk.get(&id) == Some(&Verdict::Low) {
            self.low_seq.entry(prio).or_default().insert((seq, id));
        }
    }

    /// Dequeue `id` if it is in the pool. O(log n); no-op when it is not ready.
    fn ready_dequeue(&mut self, id: TaskId) {
        if let Some((prio, seq)) = self.ready_key.remove(&id) {
            self.ready.remove(&(Reverse(prio), seq, id));
            if self.risk.get(&id) == Some(&Verdict::Low) {
                if let Some(band) = self.low_seq.get_mut(&prio) {
                    band.remove(&(seq, id));
                    // The band's census is KEPT when it empties (ADR-087). Dropping it here
                    // freed a whole `BTreeSet` and the next decisive-Low admission at this
                    // priority allocated a fresh one — about sixty bytes per dispatch on a heap
                    // that never frees (ADR-063), which the scheduler storm measured. An empty
                    // census costs one map entry per priority band, bounded by the 256 bands
                    // that exist, and `pick` already reads an empty band as "no Low member".
                }
            }
        }
    }

    /// Re-key `id` after an event may have changed its effective priority, KEEPING its FIFO age:
    /// donation moves a task's urgency, not its place among equals. No-op for a task that is not
    /// Ready (blocked and finished tasks are not in the pool; their priority is recomputed from
    /// scratch whenever they become ready again). An unchanged priority restores the key untouched,
    /// so a re-key that measures no change costs one ordered lookup and nothing else.
    fn ready_rekey(&mut self, id: TaskId) {
        let (old_prio, seq) = match self.ready_key.remove(&id) {
            Some(found) => found,
            None => return,
        };
        let prio = self.effective_priority(id);
        if prio == old_prio {
            self.ready_key.insert(id, (old_prio, seq));
            return;
        }
        // A decisive-Low task whose band changed moves between the two band censuses, keeping its age.
        if self.risk.get(&id) == Some(&Verdict::Low) {
            if let Some(band) = self.low_seq.get_mut(&old_prio) {
                band.remove(&(seq, id));
                if band.is_empty() {
                    self.low_seq.remove(&old_prio);
                }
            }
            self.low_seq.entry(prio).or_default().insert((seq, id));
        }
        self.ready.remove(&(Reverse(old_prio), seq, id));
        self.ready.insert((Reverse(prio), seq, id));
        self.ready_key.insert(id, (prio, seq));
    }

    /// Admit a task Ready at `base` priority (at the tail of the FIFO tiebreak order).
    pub fn admit(&mut self, id: TaskId, base: Priority) {
        self.base.insert(id, base);
        self.state.insert(id, TaskState::Ready);
        if !self.ready_key.contains_key(&id) {
            // A freshly admitted task holds no endpoint, so nobody donates to it yet: its
            // effective priority IS its base, and enqueueing the base is exact.
            self.ready_enqueue(id, base);
        }
    }

    /// Admit a task, recording the risk model's advice alongside it (ADR-056).
    ///
    /// The advice is a **tiebreak hint and nothing else**: `base` is used exactly as
    /// [`Self::admit`] would use it, and an `Abstain` verdict is stored as no verdict at all. Call
    /// [`Self::admit`] when no model is loaded — the two produce identical schedules in that case,
    /// which `tests/mlrisk.rs` asserts rather than assumes.
    pub fn admit_with_advice(&mut self, id: TaskId, base: Priority, advice: Advice) {
        // The verdict is recorded BEFORE the enqueue so a decisive-Low task is born into its
        // band's Low census rather than being missed by it.
        if advice.verdict.is_decisive() {
            self.risk.insert(id, advice.verdict);
        } else {
            self.risk.remove(&id);
        }
        self.admit(id, base);
    }

    /// The advisory verdict recorded for a task, if any.
    pub fn risk_of(&self, id: TaskId) -> Option<Verdict> {
        self.risk.get(&id).copied()
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
        self.ready_dequeue(task);
        if self.current == Some(task) {
            self.current = None;
        }
        self.waiters.entry(ep).or_default().push(task);
        self.waiter_count += 1;
        self.blocked_on.insert(task, ep);
        // The new waiter donates to whoever holds `ep` — and through them, transitively. Re-key
        // exactly the Ready holders on that chain so the ordered pool reflects the raised urgency
        // without a rescan.
        self.propagate_donation(ep);
        Ok(())
    }

    /// After the waiter set of `ep` changed, walk the transitive holder chain (`ep`'s holder, the
    /// endpoint that holder is itself blocked on, and so on) and re-key every Ready holder. A cycle
    /// (a deadlock) terminates the walk exactly as [`Self::effective_inner`]'s visited set does.
    fn propagate_donation(&mut self, ep: Endpoint) {
        let mut visited = BTreeSet::new();
        let mut cur = Some(ep);
        while let Some(e) = cur {
            if !visited.insert(e) {
                break;
            }
            let h = match self.holder.get(&e) {
                Some(&h) => h,
                None => break,
            };
            self.ready_rekey(h);
            cur = self.blocked_on.get(&h).copied();
        }
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
                // Computed AFTER the handover: any waiters still queued on `ep` now donate to `w`,
                // and the winner must enter the pool carrying that inheritance.
                let prio = self.effective_priority(w);
                self.ready_enqueue(w, prio);
                // The ex-holder lost every donation `ep`'s waiters made to it. If it is Ready
                // (the usual case is Running, which is not pooled), its key must come down.
                self.ready_rekey(task);
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
        let w = list.remove(best_idx);
        self.waiter_count -= 1;
        Some(w)
    }

    /// A task's **effective** priority: the max of its base and the effective priorities of every task
    /// (transitively) blocked on an endpoint it holds — priority donation. Cycles (a deadlock) are
    /// broken by a visited set so donation terminates rather than recursing forever.
    ///
    /// # Allocation
    ///
    /// Donation needs a visited set to break cycles, and a `BTreeSet` allocates. This function is
    /// called once per Ready task per scheduling decision in the scanned implementation, which is
    /// what turned a 128-task drain into 7.7 MB of bump-allocator traffic (MODEL-CARD §8). Since
    /// ALET-P3-007 the pool is ordered and the function is called only on donation events and
    /// (re)enqueues, but the same rule still holds: with no task waiting on any endpoint, no task
    /// can be donating to any other and a task's effective priority is exactly its base. That is the
    /// common case by a wide margin, the answer is identical either way, and the
    /// [`Self::waiter_count`] counter makes the test O(1) instead of a scan over every waiter list.
    pub fn effective_priority(&self, task: TaskId) -> Priority {
        if self.waiter_count == 0 {
            return self.base.get(&task).copied().unwrap_or(Priority(0));
        }
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

    /// The Ready task that would run next, computed WITHOUT mutating anything: the highest
    /// effective priority, FIFO age among equals — unless the age-oldest member of that top band is
    /// decisively `Elevated`, in which case the oldest decisively-`Low` member of the band goes
    /// first (ADR-056: the model reorders equals only, and only toward the task it expects to
    /// survive). An abstaining or unmodelled leader is never displaced, because the tiebreak needs a
    /// decisive opinion about BOTH sides.
    ///
    /// With no model loaded, or with an abstaining or `Low` leader, selection is one first-element
    /// read. Only an `Elevated` leader consults the band's Low census — itself one ordered lookup.
    /// An earlier draft answered the census question by SCANNING the band per dispatch, which is
    /// O(band) each time and quadratic over a drain; the 200 000-task gate hung on exactly that
    /// (the shipped blob marks essentially every task `Elevated`, so every dispatch paid the scan)
    /// before this census replaced it. The scan is gone: no path through selection touches more
    /// than O(log n) elements.
    fn pick(&self) -> Option<TaskId> {
        let &(key, _, leader_task) = self.ready.iter().next()?;
        let Reverse(prio) = key;
        if self.risk.get(&leader_task) != Some(&Verdict::Elevated) {
            return Some(leader_task);
        }
        // The oldest decisive-Low member of the band displaces the Elevated leader (ADR-056).
        if let Some(band) = self.low_seq.get(&prio) {
            if let Some(&(_, t)) = band.iter().next() {
                return Some(t);
            }
        }
        Some(leader_task)
    }

    /// Pick the next task to run: the Ready (or currently Running) task with the highest **effective**
    /// priority, breaking ties FIFO. The previously-running task, if still Running, rejoins the Ready
    /// pool first (fresh FIFO age — round-robin among equals). `None` when nothing is runnable. This
    /// is where inheritance pays off — a boosted holder outranks an unrelated medium-priority task.
    ///
    /// One dispatch is O(log n): a first-element read for the winner plus an ordered removal, where
    /// the scanned implementation was O(n) scan + O(n) `retain` and made a drain O(n²)
    /// (ALET-P3-007).
    pub fn schedule_next(&mut self) -> Option<TaskId> {
        if let Some(cur) = self.current.take() {
            if self.state.get(&cur) == Some(&TaskState::Running) {
                self.state.insert(cur, TaskState::Ready);
                // The scanned implementation guarded this requeue with `!order.contains`, which
                // also covered the pathological `admit`-while-running case; the pool-membership
                // test preserves that behaviour exactly rather than assuming it away.
                if !self.ready_key.contains_key(&cur) {
                    let prio = self.effective_priority(cur);
                    self.ready_enqueue(cur, prio);
                }
            }
        }
        let winner = self.pick()?;
        self.ready_dequeue(winner);
        self.state.insert(winner, TaskState::Running);
        self.current = Some(winner);
        Some(winner)
    }

    /// Retire a task; it leaves the rotation and any endpoints it held are released to no one (a real
    /// supervisor would reclaim them — that is REQ-REL-001).
    pub fn finish(&mut self, id: TaskId) {
        self.state.insert(id, TaskState::Finished);
        self.ready_dequeue(id);
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
