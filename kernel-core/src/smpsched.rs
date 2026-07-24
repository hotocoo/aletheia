//! Per-CPU run queues + cross-core work stealing — ADR-021 Phase 2 scheduling policy
//! (REQ-SMP-003), arch-independent and defined ONCE (gap-register Issue 1).
//!
//! Phase 1 (REQ-SMP-002) proved the cores exist and the concurrency substrate holds; this module
//! is the next brick: a scheduler shape that scales past one core. One global run queue serializes
//! every scheduling decision behind one lock; a real SMP kernel gives each CPU its OWN queue and
//! lets idle CPUs STEAL from loaded ones, so the common case (local pop) contends with nobody.
//!
//! LOCK DISCIPLINE (load-bearing, deadlock-free by construction): `SmpSched` NEVER holds two queue
//! locks at once. A local pop locks only the local queue; a steal snapshots victim loads via brief
//! single locks, then locks exactly ONE victim to take from it. With at most one queue lock held
//! per CPU at any instant, no lock-order cycle can exist — this is deliberately stronger than an
//! ordered two-lock hierarchy and is the documented Phase 2 discipline.
//!
//! CONTRACT (proved under real host threads in `tests/smpsched.rs`, then on real cores by the
//! aarch64 SMP suite):
//! * **exactly-once** — a task enqueued once is dispatched exactly once, never lost, never
//!   duplicated, under arbitrary cross-CPU contention;
//! * **local first** — a CPU with local work never steals;
//! * **stealing is live** — an idle CPU drains work seeded on another CPU's queue;
//! * **placement balances** — `enqueue_least_loaded` spreads tasks across queues.
//!
//! This is the scheduling *policy* (ADR-010 seam): what runs where. The arch context switch that
//! makes a stolen task actually resume on the thief CPU stays each target's `TaskContext` seam —
//! the same split as `sched::RoundRobin` and `priosched::PriorityScheduler`.
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// A schedulable unit's identity. The policy moves ids; targets own what an id resumes.
pub type TaskId = u64;

/// Where a dispatched task came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dispatch {
    pub task: TaskId,
    /// `None` = popped from the caller's own queue; `Some(cpu)` = stolen from that CPU's queue.
    pub stolen_from: Option<usize>,
}

/// Per-CPU run queues with work stealing. All methods take `&self`; every queue is independently
/// locked, so CPUs schedule concurrently and contend only when they actually touch the same queue.
pub struct SmpSched {
    queues: Vec<SpinLock<VecDeque<TaskId>>>,
}

impl SmpSched {
    /// One run queue per CPU. `ncpus` is clamped to at least 1.
    pub fn new(ncpus: usize) -> Self {
        let n = ncpus.max(1);
        let mut queues = Vec::with_capacity(n);
        for _ in 0..n {
            queues.push(SpinLock::new(VecDeque::new()));
        }
        SmpSched { queues }
    }

    pub fn ncpus(&self) -> usize {
        self.queues.len()
    }

    /// Number of tasks currently queued on `cpu` (0 for an out-of-range cpu).
    pub fn load(&self, cpu: usize) -> usize {
        match self.queues.get(cpu) {
            Some(q) => q.lock().len(),
            None => 0,
        }
    }

    /// Enqueue on a specific CPU's queue (affinity placement). Out-of-range cpus fold onto CPU 0
    /// rather than dropping work — losing a task is the one unforgivable failure here.
    pub fn enqueue_on(&self, cpu: usize, task: TaskId) {
        let idx = if cpu < self.queues.len() { cpu } else { 0 };
        self.queues[idx].lock().push_back(task);
    }

    /// Enqueue on the least-loaded queue (ties -> lowest CPU index); returns the chosen CPU.
    /// The load snapshot takes one brief lock per queue, never two at once.
    pub fn enqueue_least_loaded(&self, task: TaskId) -> usize {
        let mut best = 0usize;
        let mut best_load = usize::MAX;
        for cpu in 0..self.queues.len() {
            let load = self.load(cpu);
            if load < best_load {
                best = cpu;
                best_load = load;
            }
        }
        self.enqueue_on(best, task);
        best
    }

    /// Dispatch the next task for `cpu`: local queue first; if empty, steal from the most-loaded
    /// other queue. Returns `None` only when every queue observation came up empty.
    ///
    /// ALLOC-FREE past construction (load-bearing): kernel CPUs spin on this while waiting for
    /// stragglers, and the bare-metal bump allocators never reclaim — so the steal path is a
    /// one-pass most-loaded scan retried at most `ncpus` times, not a collected/sorted victim
    /// list. A load observation is advisory; the pop re-checks under the victim's own lock, and a
    /// victim that raced to empty is retried against the then-most-loaded remaining queue.
    pub fn next_for(&self, cpu: usize) -> Option<Dispatch> {
        let n = self.queues.len();
        let me = if cpu < n { cpu } else { 0 };

        // Local first — the common, contention-free case.
        if let Some(task) = self.queues[me].lock().pop_front() {
            return Some(Dispatch {
                task,
                stolen_from: None,
            });
        }

        for _ in 0..n {
            let mut victim = None;
            let mut best_load = 0usize;
            for v in 0..n {
                if v != me {
                    let load = self.load(v);
                    if load > best_load {
                        best_load = load;
                        victim = Some(v);
                    }
                }
            }
            let v = victim?; // every other queue observed empty -> nothing to steal
            if let Some(task) = self.queues[v].lock().pop_front() {
                return Some(Dispatch {
                    task,
                    stolen_from: Some(v),
                });
            }
        }
        None
    }
}
