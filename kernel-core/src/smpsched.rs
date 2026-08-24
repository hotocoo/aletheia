//! Per-CPU run queues + cross-core work stealing + CPU affinity + cross-core migration — ADR-021
//! Phase 2 scheduling policy (REQ-SMP-003) plus the Phase 4 affinity/migration slice (REQ-SMP-005),
//! arch-independent and defined ONCE (gap-register Issue 1).
//!
//! Phase 1 (REQ-SMP-002) proved the cores exist and the concurrency substrate holds; this module
//! is the scheduler shape that scales past one core. One global run queue serializes every
//! scheduling decision behind one lock; a real SMP kernel gives each CPU its OWN queue and lets idle
//! CPUs STEAL from loaded ones, so the common case (local pop) contends with nobody.
//!
//! # What this brick adds over REQ-SMP-003
//! * **CPU affinity** ([`AffinityMask`]): every queued task carries the set of CPUs allowed to run
//!   it. Placement and stealing both respect it — a task is never dispatched to a CPU outside its
//!   mask. The mask lives INLINE in the queue element (`(TaskId, AffinityMask)`), never in a second
//!   locked side-table, so the load-bearing "never two queue locks at once" discipline is preserved.
//! * **Cross-core migration**: a task seeded on CPU A's queue and dispatched by CPU B (a steal) HAS
//!   migrated — [`Dispatch::stolen_from`] `= Some(A)` is the migration record. The arch context
//!   switch that makes the migrated task actually RESUME on the thief is the backend's
//!   [`crate::sched::TaskContext`] seam (same split as `sched`/`priosched`); the hosted tests pair a
//!   `SmpSched` steal with a `TaskContext` resume to prove the whole path end to end.
//!
//! LOCK DISCIPLINE (load-bearing, deadlock-free by construction, ADR-028): `SmpSched` NEVER holds two
//! queue locks at once. A local pop locks only the local queue; a steal snapshots victim loads via
//! brief single locks, then locks exactly ONE victim to take from it. With at most one queue lock
//! held per CPU at any instant, no lock-order cycle can exist — deliberately stronger than an ordered
//! two-lock hierarchy. In debug builds a per-instance, per-CPU tripwire ([`SmpSched::held`]) ASSERTS
//! this invariant on every dispatch-path lock, so any future edit that nests two queue locks panics
//! immediately (and it is live under the contention suite and the `-smp 4` VM gates, which build
//! debug).
//!
//! AFFINITY + STEALING CAN STARVE (honest caveat, ADR-028): a task pinned to a CPU that stays busy
//! will wait for that CPU — work stealing cannot rescue it, because no other CPU is permitted to run
//! it. This is inherent to affinity, not a defect; callers that need liveness must give tasks a
//! *satisfiable* mask (at least one CPU that makes progress). The tests use satisfiable masks.
//!
//! CONTRACT (proved under real host threads in `tests/smpsched.rs`, then on real cores by the
//! per-target SMP suites):
//! * **exactly-once** — a task enqueued once is dispatched exactly once, never lost, never
//!   duplicated, under arbitrary cross-CPU contention AND arbitrary affinity masks;
//! * **local first** — a CPU with eligible local work never steals;
//! * **stealing is live** — an idle CPU drains eligible work seeded on another CPU's queue;
//! * **affinity is honored** — a task is never dispatched to a CPU outside its mask;
//! * **placement balances** — affinity-aware least-loaded placement spreads tasks across the
//!   permitted queues.
//!
//! This is the scheduling *policy* (ADR-010 seam): what runs where. The arch context switch that
//! makes a stolen task actually resume on the thief CPU stays each target's `TaskContext` seam —
//! the same split as `sched::RoundRobin` and `priosched::PriorityScheduler`.
use alloc::collections::VecDeque;
use alloc::vec::Vec;
#[cfg(debug_assertions)]
use core::sync::atomic::{AtomicU8, Ordering};

use crate::sync::SpinLock;

/// A schedulable unit's identity. The policy moves ids; targets own what an id resumes.
pub type TaskId = u64;

/// The set of CPUs a task is permitted to run on. Bit `i` set ⇒ CPU `i` is allowed. A 64-wide mask,
/// so affinity covers CPUs `0..64` (every real Aletheia target tops out far below that — QEMU virt
/// GICv2 caps at 8). A CPU index `>= 64` can never be represented and is therefore never permitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffinityMask(u64);

impl AffinityMask {
    /// Runnable on ANY CPU — the default, which preserves the exact pre-affinity behaviour so the
    /// REQ-SMP-003 callers (and their VM gates) are unchanged.
    pub const ANY: AffinityMask = AffinityMask(u64::MAX);

    /// Runnable ONLY on `cpu` (a hard pin). `cpu >= 64` yields the empty mask (unrepresentable pin).
    pub const fn only(cpu: usize) -> AffinityMask {
        if cpu < 64 {
            AffinityMask(1u64 << cpu)
        } else {
            AffinityMask(0)
        }
    }

    /// Build a mask from an explicit bitmask (bit `i` ⇒ CPU `i` allowed).
    pub const fn from_bits(bits: u64) -> AffinityMask {
        AffinityMask(bits)
    }

    /// A copy of this mask with `cpu` added to the permitted set (`cpu >= 64` is ignored).
    pub const fn with(self, cpu: usize) -> AffinityMask {
        if cpu < 64 {
            AffinityMask(self.0 | (1u64 << cpu))
        } else {
            self
        }
    }

    /// Does this mask permit running on `cpu`? CPUs `>= 64` are never permitted (mask is 64-wide).
    pub const fn allows(self, cpu: usize) -> bool {
        cpu < 64 && (self.0 & (1u64 << cpu)) != 0
    }

    /// A mask that permits nothing — no CPU may run the task (a pin to a nonexistent/out-of-range CPU).
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw permitted-CPU bitmask.
    pub const fn bits(self) -> u64 {
        self.0
    }
}

impl Default for AffinityMask {
    fn default() -> Self {
        AffinityMask::ANY
    }
}

/// Where a dispatched task came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dispatch {
    pub task: TaskId,
    /// `None` = popped from the caller's own queue; `Some(cpu)` = **migrated** — stolen from that
    /// CPU's queue and now to be resumed on the caller's CPU via the `TaskContext` seam.
    pub stolen_from: Option<usize>,
}

impl Dispatch {
    /// True when this dispatch is a cross-core migration (the task was enqueued on a CPU other than
    /// the one now dispatching it).
    pub fn is_migration(&self) -> bool {
        self.stolen_from.is_some()
    }
}

/// Per-CPU run queues with work stealing + affinity. All methods take `&self`; every queue is
/// independently locked, so CPUs schedule concurrently and contend only when they actually touch the
/// same queue.
pub struct SmpSched {
    queues: Vec<SpinLock<VecDeque<(TaskId, AffinityMask)>>>,
    /// DEBUG-ONLY lock-hierarchy tripwire (ADR-028): per-CPU count of run-queue locks that CPU holds
    /// on the dispatch path. `with_queue` asserts it never exceeds 1 — the "at most one queue lock
    /// per CPU" discipline. Per-INSTANCE (not a `static`) so parallel host tests, each with their own
    /// `SmpSched`, never alias one another's counters. Absent entirely in release builds.
    #[cfg(debug_assertions)]
    held: Vec<AtomicU8>,
}

impl SmpSched {
    /// One run queue per CPU. `ncpus` is clamped to at least 1.
    pub fn new(ncpus: usize) -> Self {
        let n = ncpus.max(1);
        let mut queues = Vec::with_capacity(n);
        for _ in 0..n {
            queues.push(SpinLock::new(VecDeque::new()));
        }
        SmpSched {
            queues,
            #[cfg(debug_assertions)]
            held: (0..n).map(|_| AtomicU8::new(0)).collect(),
        }
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

    /// Enqueue on a specific CPU's queue with ANY affinity (the REQ-SMP-003 behaviour). Out-of-range
    /// cpus fold onto CPU 0 rather than dropping work — losing a task is the one unforgivable failure.
    pub fn enqueue_on(&self, cpu: usize, task: TaskId) {
        self.enqueue_on_affine(cpu, task, AffinityMask::ANY);
    }

    /// Enqueue on a specific CPU's queue with an explicit affinity mask. Out-of-range cpus fold onto
    /// CPU 0. NOTE: this does not itself check that `cpu` is inside `affinity` — placing a task on a
    /// queue whose CPU is not permitted is legal (it will only ever be *stolen* by a permitted CPU);
    /// callers that want a local run should pass a mask that includes `cpu`.
    pub fn enqueue_on_affine(&self, cpu: usize, task: TaskId, affinity: AffinityMask) {
        let idx = if cpu < self.queues.len() { cpu } else { 0 };
        self.queues[idx].lock().push_back((task, affinity));
    }

    /// Enqueue on the least-loaded queue (ties → lowest CPU index) with ANY affinity; returns the
    /// chosen CPU. The load snapshot takes one brief lock per queue, never two at once.
    pub fn enqueue_least_loaded(&self, task: TaskId) -> usize {
        // ANY affinity always permits CPU 0, so a placement always exists.
        self.enqueue_least_loaded_affine(task, AffinityMask::ANY)
            .expect("ANY affinity always has an eligible CPU")
    }

    /// Enqueue on the least-loaded *permitted* queue (ties → lowest CPU index); returns the chosen
    /// CPU, or `None` when the mask permits no CPU that exists on this scheduler (nothing is enqueued
    /// — the caller keeps ownership of the task rather than it being silently dropped onto CPU 0).
    pub fn enqueue_least_loaded_affine(
        &self,
        task: TaskId,
        affinity: AffinityMask,
    ) -> Option<usize> {
        let mut best = None;
        let mut best_load = usize::MAX;
        for cpu in 0..self.queues.len() {
            if !affinity.allows(cpu) {
                continue;
            }
            let load = self.load(cpu);
            if load < best_load {
                best = Some(cpu);
                best_load = load;
            }
        }
        let chosen = best?;
        self.enqueue_on_affine(chosen, task, affinity);
        Some(chosen)
    }

    /// Dispatch the next task for `cpu`: eligible local work first; if none, steal the first eligible
    /// task from the most-loaded other queue. Returns `None` only when no queue holds a task this CPU
    /// is permitted to run.
    ///
    /// AFFINITY: a task is taken only if its mask `allows(cpu)`. Within a queue, [`Self::take_eligible`]
    /// rotates past tasks pinned away from `cpu` (bounded by the queue length) and takes the first
    /// eligible one, preserving FIFO among the tasks this CPU may run.
    ///
    /// ALLOC-FREE past construction (load-bearing): kernel CPUs spin on this while waiting for
    /// stragglers, and the bare-metal bump allocators never reclaim. The steal path is a one-pass
    /// most-loaded scan; a victim that yields nothing eligible for `cpu` (raced empty, or all its
    /// tasks pinned away) is retired via a stack `u64` bitmask so a less-loaded victim with eligible
    /// work is still reached and the scan always terminates — no collected/sorted victim list.
    pub fn next_for(&self, cpu: usize) -> Option<Dispatch> {
        let n = self.queues.len();
        let me = if cpu < n { cpu } else { 0 };

        // Local first — the common, contention-free case (now affinity-filtered).
        if let Some(task) = self.with_queue(me, me, |q| Self::take_eligible(q, me)) {
            return Some(Dispatch {
                task,
                stolen_from: None,
            });
        }

        // Steal: repeatedly target the most-loaded OTHER queue that might still hold work eligible for
        // `me`. `exhausted` retires victims that yielded nothing eligible so the scan reaches a
        // less-loaded victim with permitted work and always terminates. 64-wide (affinity is <= 64).
        let mut exhausted: u64 = 0;
        loop {
            let mut victim = None;
            let mut best_load = 0usize;
            for v in 0..n {
                if v == me || (v < 64 && (exhausted & (1u64 << v)) != 0) {
                    continue;
                }
                let load = self.load(v);
                if load > best_load {
                    best_load = load;
                    victim = Some(v);
                }
            }
            let v = victim?; // no remaining victim with (advisory) work -> nothing to steal
            if let Some(task) = self.with_queue(me, v, |q| Self::take_eligible(q, me)) {
                return Some(Dispatch {
                    task,
                    stolen_from: Some(v),
                });
            }
            if v < 64 {
                exhausted |= 1u64 << v;
            } else {
                // Cannot retire victims >= 64 in the bitmask; stop rather than risk a non-terminating
                // scan. No Aletheia target reaches 64 CPUs, so this branch is unreachable in practice.
                return None;
            }
        }
    }

    /// Remove and return the first task in `q` that `cpu` is permitted to run, rotating past
    /// ineligible (pinned-away) tasks. Bounded by the queue length: after one full rotation with no
    /// eligible task, returns `None` with the queue order preserved. Never loses or duplicates a task
    /// (every non-eligible task is `push_back`'d exactly once before it is re-examined).
    fn take_eligible(q: &mut VecDeque<(TaskId, AffinityMask)>, cpu: usize) -> Option<TaskId> {
        let scan = q.len();
        for _ in 0..scan {
            let (task, mask) = q.pop_front().expect("bounded by the observed length");
            if mask.allows(cpu) {
                return Some(task);
            }
            q.push_back((task, mask));
        }
        None
    }

    /// Run `f` under queue `idx`'s lock, tracking (in debug) that `acting_cpu` holds exactly one
    /// run-queue lock for the duration — the ADR-028 tripwire. `acting_cpu` is the CPU *doing the
    /// dispatch* (which may lock a victim's queue during a steal); it is what the "at most one queue
    /// lock per CPU" invariant is stated over.
    #[cfg_attr(not(debug_assertions), allow(unused_variables))] // acting_cpu drives the debug-only tripwire
    fn with_queue<R>(
        &self,
        acting_cpu: usize,
        idx: usize,
        f: impl FnOnce(&mut VecDeque<(TaskId, AffinityMask)>) -> R,
    ) -> R {
        #[cfg(debug_assertions)]
        self.lock_enter(acting_cpu);
        let mut guard = self.queues[idx].lock();
        let out = f(&mut guard);
        drop(guard);
        #[cfg(debug_assertions)]
        self.lock_exit(acting_cpu);
        out
    }

    /// DEBUG tripwire: record that `cpu` is about to hold a run-queue lock; panic if it already holds
    /// one (a second concurrent queue lock on the same CPU violates ADR-028). Runs BEFORE the actual
    /// `.lock()` so a deliberate nesting is caught before it can self-deadlock on the spinlock.
    #[cfg(debug_assertions)]
    fn lock_enter(&self, cpu: usize) {
        if let Some(slot) = self.held.get(cpu) {
            let prev = slot.fetch_add(1, Ordering::AcqRel);
            assert_eq!(
                prev, 0,
                "lock-hierarchy violation (ADR-028): CPU {cpu} tried to hold a 2nd run-queue lock; \
                 the dispatch path must hold at most one queue lock at a time"
            );
        }
    }

    /// DEBUG tripwire: record that `cpu` has released its run-queue lock.
    #[cfg(debug_assertions)]
    fn lock_exit(&self, cpu: usize) {
        if let Some(slot) = self.held.get(cpu) {
            slot.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// TEST-ONLY (debug builds): deliberately nest two run-queue locks on the SAME CPU to prove the
    /// ADR-028 tripwire fires. Never called by real dispatch — it exists so the lock-hierarchy
    /// conformance test can assert the guard actually panics rather than being a silent no-op.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn __audit_probe_nested_lock(&self) {
        let other = if self.queues.len() > 1 { 1 } else { 0 };
        self.with_queue(0, 0, |_| {
            // The inner enter() sees held[0] == 1 and panics before this inner .lock() can run,
            // so no real spinlock self-deadlock occurs.
            self.with_queue(0, other, |_| {});
        });
    }

    /// TEST-ONLY: how many run-queue locks `cpu` currently holds on the dispatch path (always 0
    /// outside a dispatch). The contention suite asserts these return to zero after every storm,
    /// which is what makes the ADR-028 discipline a MEASURED claim under real threads: an
    /// unbalanced enter/exit pair anywhere on the dispatch path shows up here. Release builds
    /// have no counters and report 0.
    #[doc(hidden)]
    pub fn debug_locks_held(&self, cpu: usize) -> u8 {
        #[cfg(debug_assertions)]
        {
            self.held
                .get(cpu)
                .map(|c| c.load(Ordering::SeqCst))
                .unwrap_or(0)
        }
        #[cfg(not(debug_assertions))]
        {
            0
        }
    }
}
