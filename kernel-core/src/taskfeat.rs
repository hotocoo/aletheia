//! Deriving the risk model's 20-feature vector from a **live task** (REQ-ML-003, ADR-056).
//!
//! `mlrisk` verifies and evaluates a frozen forest; `mlrisk_stress` measures it under load. Both
//! were fed fixture rows, and `docs/MATURITY.md` said so in as many words: *"NOTHING in the kernel
//! derives the 20-feature vector from a live task yet, so on a running machine it currently advises
//! about no one."* This module is that missing derivation, and it is the only reason the advisor can
//! be described as running rather than as installed.
//!
//! The hard part is not arithmetic, it is **meaning**. A forest fitted on the Borg trace learned
//! what `user_fails` means *there*: the number of this user's tasks that had already failed at the
//! instant this task was submitted, counted exclusively of the task itself. Hand it a kernel number
//! that is merely called `user_fails` and the margins are still produced, still deterministic, still
//! within the compare bound — and meaningless. So this module reproduces the trainer's own
//! accounting rules rather than inventing kernel-flavoured approximations of them:
//!
//! * **Exclusive history.** Every counter is read *before* the submission it describes is folded in
//!   ([`FeatureSource::observe_submit`] returns the vector, then bumps), which is the trainer's
//!   `_exclusive_prefix` discipline in `aletheia-ml/src/aletheia_ml/etl.py`.
//! * **Previous bin, never the current one.** Cell pressure is read from the *completed* five-minute
//!   bin, so it can never contain an event that happens after the advice it informed.
//! * **The trainer's fixed point.** Requests are fractions of this machine's capacity in 16.16, the
//!   user fail rate is scaled by 10 000, and `cpu_x_mem` is the product in the same 16.16 — the
//!   scales in [`crate::mlrisk_contract::FEATURE_SCALES`], applied here so the compare thresholds in
//!   the blob mean what they meant during fitting.
//! * **`missing_info` is honest.** When the kernel cannot supply a field it says so in the feature
//!   the trainer reserved for exactly that, instead of substituting a zero that reads as a fact.
//!
//! No floating point anywhere: this is a kernel, and the whole design of the advisor is that it
//! needs `i32` compares and one `i64` accumulator and nothing else.
//!
//! **Still advisory (INV-014).** Deriving real features changes *who* the model advises about, not
//! *what its advice can do*. The output is a `[i32; N_FEATURES]` handed to
//! [`crate::mlrisk::RiskAdvisor::advise`], whose verdict reaches the scheduler only through
//! [`crate::priosched::PriorityScheduler::admit_with_advice`] — an equal-priority tiebreak, never an
//! admission verdict, never a capability, never a plan.
use alloc::collections::BTreeMap;

use crate::mlrisk_contract::N_FEATURES;

/// Fixed-point denominator for the three request fractions and for `cpu_x_mem` (the trainer's
/// `1 << 16`).
pub const REQ_FRAC_BITS: u32 = 16;
/// `1.0` as a request fraction: a task asking for an entire machine.
pub const REQ_ONE: i64 = 1 << REQ_FRAC_BITS;
/// Fixed-point denominator of `user_fail_rate` (the trainer's `10_000`).
pub const FAIL_RATE_SCALE: i64 = 10_000;
/// Width of a cell-pressure bin, in seconds (the trainer's `PRESSURE_BIN_SEC`).
pub const PRESSURE_BIN_SEC: u64 = 300;
/// Seconds in a day, for `time_of_day`.
pub const SECONDS_PER_DAY: u64 = 86_400;

/// Which principal submitted a task. The kernel's own notion of a user; only equality and ordering
/// matter, so any stable id works.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct UserId(pub u64);

/// A group of tasks submitted together (the trainer's *job*). A single-task workload is a job of
/// one, which is the common kernel case and needs no special handling.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct JobId(pub u64);

/// How a task ended. Fed back so the counters the *next* advice reads describe what actually
/// happened, which is what makes the history live rather than a boot-time constant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Ran to completion.
    Finished,
    /// Died: a fault the supervisor attributed to the task, or a policy termination.
    Failed,
    /// Preempted off this machine by something more important.
    Evicted,
}

impl Outcome {
    /// Whether this outcome is a *terminal* event in the trainer's sense (it ends the task's life on
    /// this machine, whatever the reason).
    pub fn is_terminal(self) -> bool {
        true
    }

    /// Whether this outcome counts as a failure for the fail counters.
    pub fn is_failure(self) -> bool {
        matches!(self, Outcome::Failed)
    }
}

/// What the kernel knows about a task at the moment it is submitted, in kernel-natural units. The
/// conversion into the trainer's fixed point happens in [`FeatureSource::observe_submit`], in one
/// place, so a unit mistake is a compile-time-visible field rather than a silent factor.
#[derive(Clone, Copy, Debug)]
pub struct TaskSubmission {
    /// Latency-sensitivity band, 0..=3 (3 = most latency-sensitive), matching the trainer's
    /// `sched_class`.
    pub sched_class: u8,
    /// Base scheduling priority, 0..=11 in the trainer's range.
    pub priority: u8,
    /// Requested CPU, in thousandths of one CPU (millicores).
    pub cpu_millis: u32,
    /// Requested memory, in 4 KiB pages.
    pub memory_pages: u64,
    /// Requested local disk, in 4 KiB pages. `None` when this kernel has no disk request to report,
    /// which raises `missing_info` rather than reporting a zero request.
    pub disk_pages: Option<u64>,
    /// True when the task may not share a machine with its siblings (an anti-affinity constraint).
    pub diff_machine: bool,
    /// Index of this task within its job.
    pub task_index: u32,
    pub job: JobId,
    pub user: UserId,
}

/// This machine's capacity, used to normalise requests into the fractions the forest was fitted on.
/// A request is *this much of this machine*, which is what the trainer's normalisation meant.
#[derive(Clone, Copy, Debug)]
pub struct MachineCapacity {
    /// Total CPU, in thousandths of one CPU.
    pub cpu_millis: u32,
    /// Total usable RAM, in 4 KiB pages.
    pub memory_pages: u64,
    /// Total local disk, in 4 KiB pages. Zero on a diskless machine, which makes every disk request
    /// unreportable and therefore `missing_info`.
    pub disk_pages: u64,
}

impl MachineCapacity {
    /// A capacity with no zero divisors: any zero field is clamped to 1 so normalisation cannot
    /// divide by zero. The disk case is still *reported* as missing by
    /// [`FeatureSource::observe_submit`]; this only keeps the arithmetic total.
    fn safe(self) -> (i64, i64, i64) {
        (
            core::cmp::max(1, self.cpu_millis as i64),
            core::cmp::max(1, self.memory_pages as i64),
            core::cmp::max(1, self.disk_pages as i64),
        )
    }
}

/// One five-minute census of what this machine did. The advice for a task submitted in bin *n* reads
/// bin *n − 1*, never bin *n*: a completed bin cannot contain an event that has not happened yet.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Bin {
    submits: i64,
    fails: i64,
    scheds: i64,
    evicts: i64,
}

/// The kernel's live counterpart to the trainer's ETL carry state: per-job and per-user running
/// history plus the rolling cell census, maintained as the machine runs.
///
/// Bounded by construction: [`FeatureSource::retire_job`] and [`FeatureSource::retire_user`] drop
/// history the kernel no longer needs, and [`FeatureSource::HISTORY_CAP`] caps how many principals
/// are tracked at once, so a machine that is up for a month does not accumulate an unbounded map on
/// a never-freeing kernel heap. Reaching the cap is *counted*, not hidden — a vector derived without
/// the history it should have had is a vector whose `missing_info` says so.
pub struct FeatureSource {
    capacity: MachineCapacity,
    job_submits: BTreeMap<JobId, i64>,
    job_fails: BTreeMap<JobId, i64>,
    user_submits: BTreeMap<UserId, i64>,
    user_fails: BTreeMap<UserId, i64>,
    user_terminals: BTreeMap<UserId, i64>,
    /// The bin currently accumulating, and the completed bin advice is read from.
    current_bin: u64,
    accumulating: Bin,
    previous: Bin,
    /// How many submissions were described without their history because the cap was reached.
    capped: u64,
}

impl FeatureSource {
    /// How many distinct jobs and users are tracked at once. Past this, new principals are described
    /// with `missing_info` set rather than evicting a live principal's history or growing without
    /// bound.
    pub const HISTORY_CAP: usize = 1024;

    pub fn new(capacity: MachineCapacity) -> Self {
        FeatureSource {
            capacity,
            job_submits: BTreeMap::new(),
            job_fails: BTreeMap::new(),
            user_submits: BTreeMap::new(),
            user_fails: BTreeMap::new(),
            user_terminals: BTreeMap::new(),
            current_bin: 0,
            accumulating: Bin::default(),
            previous: Bin::default(),
            capped: 0,
        }
    }

    /// Submissions described without their history because [`Self::HISTORY_CAP`] was reached.
    pub fn capped_submissions(&self) -> u64 {
        self.capped
    }

    /// A copy of this source for a *hypothetical* derivation — what the vector would be — that the
    /// caller then discards. Deriving a vector mutates history (that is the exclusive-prefix rule),
    /// so asking "what would you advise?" must not be the same thing as saying "a task arrived".
    pub fn clone_for_probe(&self) -> FeatureSource {
        FeatureSource {
            capacity: self.capacity,
            job_submits: self.job_submits.clone(),
            job_fails: self.job_fails.clone(),
            user_submits: self.user_submits.clone(),
            user_fails: self.user_fails.clone(),
            user_terminals: self.user_terminals.clone(),
            current_bin: self.current_bin,
            accumulating: self.accumulating,
            previous: self.previous,
            capped: self.capped,
        }
    }

    /// Cell pressure the next advice will read: submits, failures, dispatches and evictions in the
    /// most recently *completed* five-minute bin.
    pub fn previous_bin(&self) -> (i64, i64, i64, i64) {
        (
            self.previous.submits,
            self.previous.fails,
            self.previous.scheds,
            self.previous.evicts,
        )
    }

    /// How many jobs and users currently have tracked history.
    pub fn tracked(&self) -> (usize, usize) {
        (self.job_submits.len(), self.user_submits.len())
    }

    /// Advance the cell census to the bin containing `now_secs`. Crossing a boundary publishes the
    /// accumulating bin as the one advice will read and starts a fresh one; crossing *several*
    /// boundaries (an idle machine) publishes an empty bin, because that is what an idle machine
    /// did. Returns true when a boundary was crossed.
    pub fn advance_clock(&mut self, now_secs: u64) -> bool {
        let bin = now_secs / PRESSURE_BIN_SEC;
        if bin == self.current_bin {
            return false;
        }
        // Exactly one bin ahead: the bin that just closed is the previous one. More than one: the
        // bins in between were empty, so the previous bin is empty too.
        self.previous = if bin == self.current_bin + 1 {
            self.accumulating
        } else {
            Bin::default()
        };
        self.accumulating = Bin::default();
        self.current_bin = bin;
        true
    }

    /// Record that the scheduler dispatched a task (the trainer's *schedule* event, cell throughput).
    pub fn observe_schedule(&mut self) {
        self.accumulating.scheds += 1;
    }

    /// Record how a task ended, updating both the cell census and the per-principal history the next
    /// advice will read. This is the feedback edge: without it the history is a boot-time constant
    /// and the model advises on a machine it is not watching.
    pub fn observe_outcome(&mut self, job: JobId, user: UserId, outcome: Outcome) {
        if outcome.is_terminal() {
            bump(&mut self.user_terminals, user, Self::HISTORY_CAP);
        }
        if outcome.is_failure() {
            self.accumulating.fails += 1;
            bump(&mut self.job_fails, job, Self::HISTORY_CAP);
            bump(&mut self.user_fails, user, Self::HISTORY_CAP);
        }
        if matches!(outcome, Outcome::Evicted) {
            self.accumulating.evicts += 1;
        }
    }

    /// Forget a finished job's history. Called when the last task of a job is gone.
    pub fn retire_job(&mut self, job: JobId) {
        self.job_submits.remove(&job);
        self.job_fails.remove(&job);
    }

    /// Forget a departed principal's history.
    pub fn retire_user(&mut self, user: UserId) {
        self.user_submits.remove(&user);
        self.user_fails.remove(&user);
        self.user_terminals.remove(&user);
    }

    /// Derive the feature vector for a task being submitted at `now_secs`, then fold the submission
    /// into the history.
    ///
    /// The read happens **before** the bump, so the vector describes the machine as it was just
    /// before this task existed — the trainer's exclusive-prefix rule. Returns the vector in
    /// [`crate::mlrisk_contract::FEATURE_NAMES`] order, already in the contract's fixed point, ready
    /// for [`crate::mlrisk::RiskAdvisor::advise`].
    pub fn observe_submit(&mut self, now_secs: u64, t: &TaskSubmission) -> [i32; N_FEATURES] {
        self.advance_clock(now_secs);

        let (cap_cpu, cap_mem, cap_disk) = self.capacity.safe();
        let cpu_q = frac_q16(t.cpu_millis as i64, cap_cpu);
        let mem_q = frac_q16(t.memory_pages as i64, cap_mem);
        let (disk_q, disk_missing) = match t.disk_pages {
            Some(_) if self.capacity.disk_pages == 0 => (0, true),
            Some(pages) => (frac_q16(pages as i64, cap_disk), false),
            None => (0, true),
        };

        let history_full = self.job_submits.len() >= Self::HISTORY_CAP
            || self.user_submits.len() >= Self::HISTORY_CAP;
        let untracked = history_full
            && (!self.job_submits.contains_key(&t.job) || !self.user_submits.contains_key(&t.user));
        if untracked {
            self.capped = self.capped.saturating_add(1);
        }

        let job_submits = read(&self.job_submits, &t.job);
        let job_fails = read(&self.job_fails, &t.job);
        let user_submits = read(&self.user_submits, &t.user);
        let user_fails = read(&self.user_fails, &t.user);
        let user_terminals = read(&self.user_terminals, &t.user);
        let fail_rate = (user_fails * FAIL_RATE_SCALE) / core::cmp::max(1, user_terminals);

        // `missing_info` is the trainer's provenance flag: a non-zero value means at least one field
        // of this row was not observed. The kernel uses it for exactly that and nothing else.
        let missing = (disk_missing as i64) | ((untracked as i64) << 1);

        let raw: [i64; N_FEATURES] = [
            t.sched_class as i64,
            t.priority as i64,
            cpu_q,
            mem_q,
            disk_q,
            t.diff_machine as i64,
            missing,
            (now_secs % SECONDS_PER_DAY) as i64,
            t.task_index as i64,
            job_submits,
            job_fails,
            user_submits,
            user_fails,
            fail_rate,
            user_terminals,
            self.previous.submits,
            self.previous.fails,
            self.previous.scheds,
            self.previous.evicts,
            // cpu * mem in the same 16.16 the trainer used: (cpu_q * mem_q) >> 16.
            (cpu_q * mem_q) >> REQ_FRAC_BITS,
        ];

        // Fold the submission in only now, after the vector has been read.
        self.accumulating.submits += 1;
        bump(&mut self.job_submits, t.job, Self::HISTORY_CAP);
        bump(&mut self.user_submits, t.user, Self::HISTORY_CAP);

        let mut out = [0i32; N_FEATURES];
        for (o, v) in out.iter_mut().zip(raw.iter()) {
            *o = clamp_i32(*v);
        }
        out
    }
}

/// `numer / denom` as a 16.16 fraction, saturating at "one whole machine" only when the request
/// really is that large — an over-request is a *real* out-of-box input the advisor should abstain
/// on, so it is passed through rather than clipped to look reasonable.
fn frac_q16(numer: i64, denom: i64) -> i64 {
    numer.saturating_mul(REQ_ONE) / core::cmp::max(1, denom)
}

/// Saturate into the `i32` the forest compares against. A value this large is far outside any
/// training range, so the range guard will abstain on it — which is the correct answer, and better
/// than a wrapped number that lands back inside the box and reads as ordinary.
fn clamp_i32(v: i64) -> i32 {
    if v > i32::MAX as i64 {
        i32::MAX
    } else if v < i32::MIN as i64 {
        i32::MIN
    } else {
        v as i32
    }
}

fn read<K: Ord>(m: &BTreeMap<K, i64>, k: &K) -> i64 {
    *m.get(k).unwrap_or(&0)
}

/// Increment a counter, refusing to create a new key once `cap` distinct keys are tracked. An
/// existing key is always updated: the cap bounds how many principals are remembered, never how
/// accurately a remembered one is.
fn bump<K: Ord>(m: &mut BTreeMap<K, i64>, k: K, cap: usize) {
    if let Some(v) = m.get_mut(&k) {
        *v += 1;
    } else if m.len() < cap {
        m.insert(k, 1);
    }
}
