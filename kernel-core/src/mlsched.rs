//! The resident risk advisor: loaded once at boot, consulted for the life of the machine
//! (REQ-ML-003, ADR-056).
//!
//! [`crate::mlrisk`] can verify and evaluate a forest; [`crate::taskfeat`] can derive that forest's
//! feature vector from a live task. Neither of them, by itself, makes the model *resident*: until
//! this module, the blob was loaded inside a boot selftest, asked a few thousand fixture questions,
//! and then dropped on the way to the shell. A model that stops being consulted when the boot log
//! ends is an installed model, not a running one.
//!
//! [`RiskService`] is the difference. It holds the verified forest for the machine's whole uptime,
//! owns the live [`crate::taskfeat::FeatureSource`], and is the path through which a task reaches
//! [`crate::priosched::PriorityScheduler`]. Every admission on that path derives features, asks the
//! forest, and records the answer; every dispatch and every task death is fed back into the history
//! the *next* advice reads.
//!
//! **A real user-mode task reaches it.** The aarch64 and RISC-V targets each run two genuine ring-3
//! / U-mode tasks — own address spaces, own trap frames, real context switches — admitted through
//! [`resident::admit`] and dispatched by [`crate::priosched::PriorityScheduler`], with the dispatch
//! and the exit fed back (`run_advised_scheduler` in each target's `usermode.rs`, gated as three
//! boot invariants per target). The task is described with the memory it actually mapped, not with a
//! plausible-looking constant.
//!
//! All three targets are wired: aarch64, RISC-V and x86-64. The x86-64 one landed last and
//! deliberately so — its ring-3 gate was red on a defect predating this work, and wiring a model into
//! a target whose user-mode gate cannot pass proves nothing about either.
//!
//! **Continuity is measured, not asserted.** [`AdviceStats`] carries the counters a console can
//! print: how many advices have been given, the verdict census, the longest historical gap between
//! two consecutive consultations, and — the one that actually distinguishes *resident* from *ran
//! once at boot* — [`AdviceStats::silence_secs`], how long it has been since the last consultation
//! as of the machine's most recent tick. A historical gap can only close when the next consultation
//! arrives, so an advisor that fell silent an hour ago still reports the small gaps it managed while
//! it was busy; the silence does not, and grows with the machine.
//!
//! **Memory is a boundary the advisor cannot cross (ADR-081).** The forest advises ORDER among
//! admissible tasks; whether a task is admissible at all is decided by the machine's own frame
//! allocator, reported through [`MemoryMeter`] and enforced by [`RiskService::admit_bounded`]: a
//! task asking for more frames than are free RIGHT NOW is refused [`AdmitRefusal::MemoryExhausted`]
//! with both numbers named, BEFORE the model is consulted and without touching the scheduler; a
//! service no allocator has reported to admits nothing through the bounded door
//! ([`AdmitRefusal::Unmetered`] — fail closed, never "unbounded until told"); a meter that is not
//! a meter is refused by name and records nothing. The meter's low-water mark, the last reading,
//! every refusal and every crossing of the pressure watermark are counted in [`AdviceStats`] and
//! printed by `mlstat`. What this rung does NOT do is make memory a forest feature: the 20-column
//! contract is frozen by the trainer and hash-checked by `mlrisk_contract`, so RAM pressure
//! reaches the DECISION as a hard bound, not as an opinion — and the second forest trained on the
//! eviction event (REQ-ML-005) stays unwired, stated in the register.
//!
//! **Still advisory (INV-014).** Residency changes how often the model is asked, not what its answer
//! may do. `admit` calls [`crate::priosched::PriorityScheduler::admit_with_advice`], whose verdict is
//! an equal-priority tiebreak; base priority, capability checks and every invariant are evaluated
//! identically whether the forest is loaded, absent, or wrong. With
//! [`RiskService::without_model`] — or with a blob the kernel refused — the service still runs, still
//! counts, and admits through [`crate::priosched::PriorityScheduler::admit`], so the schedule is
//! bit-identical to the model-free kernel. Absence is *named*: [`RiskService::model_error`] returns
//! the [`crate::mlrisk::ModelError`] that refused the blob, so the console can print why the machine
//! is running without advice instead of implying it is running with it.
use alloc::vec::Vec;

use crate::mlrisk::{Advice, ModelError, RiskAdvisor, Verdict};
use crate::priosched::{Priority, PriorityScheduler};
use crate::sched::TaskId;
use crate::taskfeat::{FeatureSource, JobId, MachineCapacity, Outcome, TaskSubmission, UserId};

/// Everything the console needs to answer "is the model actually working right now?" — and to be
/// caught out if it is not.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct AdviceStats {
    /// Consultations since boot: one per admitted task, model loaded or not.
    pub advices: u64,
    /// Consultations that returned a decisive `Low`.
    pub low: u64,
    /// Consultations that returned a decisive `Elevated`.
    pub elevated: u64,
    /// Consultations that returned `Abstain`, for either reason.
    pub abstain: u64,
    /// Of those, the ones inside the conformal band (the model has an opinion but not a confident
    /// one), as distinct from the ones outside the training box.
    pub band_abstain: u64,
    /// Of those, the ones withheld because the input was DEGENERATE — every feature carrying the
    /// same value (ALET-P3-006). A nonzero count is a feature extractor emitting constants, which
    /// is an operator-visible anomaly and not the model's judgement about anything.
    pub degenerate_abstain: u64,
    /// Consultations whose input fell outside the per-feature box the forest was fitted in.
    pub out_of_range: u64,
    /// Dispatches observed (cell throughput fed back into the features).
    pub schedules: u64,
    /// Task outcomes observed, by kind. These are the feedback edge: they change what the *next*
    /// advice sees.
    pub finished: u64,
    pub failed: u64,
    pub evicted: u64,
    /// Housekeeping ticks: the clock advancing, cell bins rolling over.
    pub ticks: u64,
    /// Wall-clock second of the first consultation.
    pub first_advice_secs: u64,
    /// Wall-clock second of the most recent consultation.
    pub last_advice_secs: u64,
    /// Longest interval, in seconds, between two consecutive consultations.
    pub max_gap_secs: u64,
    /// The machine's clock at the most recent housekeeping tick. Ticks happen whether or not
    /// anything is being admitted, so this is what the machine believes the time to be — and it is
    /// the half of the falsifiability argument that [`Self::max_gap_secs`] cannot supply on its own:
    /// a gap only ever closes when the NEXT consultation arrives, so an advisor that stopped being
    /// consulted an hour ago still reports the small gaps it managed while it was busy. Comparing
    /// this against [`Self::last_advice_secs`] is what exposes that.
    pub last_tick_secs: u64,
    /// Allocator readings taken through [`RiskService::observe_memory`] (0 = unmetered).
    pub memory_samples: u64,
    /// Frames the allocator manages, as of the last reading.
    pub total_pages: u64,
    /// Free frames as of the last reading.
    pub last_free_pages: u64,
    /// The fewest free frames any reading has shown — the low-water mark.
    pub low_free_pages: u64,
    /// Bounded admissions refused `MemoryExhausted` (the task asked for more than was free).
    pub memory_refusals: u64,
    /// Bounded admissions refused `Unmetered` (no allocator had reported yet).
    pub unmetered_refusals: u64,
    /// Readings that ENTERED the pressure band (free below the watermark); staying in it counts
    /// nothing, leaving and re-entering counts again.
    pub pressure_events: u64,
    /// Whether the last reading was inside the pressure band.
    pub in_pressure: bool,
}

impl AdviceStats {
    /// Seconds spanned by the consultation history (last minus first). Zero before the second
    /// consultation.
    pub fn span_secs(&self) -> u64 {
        self.last_advice_secs.saturating_sub(self.first_advice_secs)
    }

    /// Seconds between the last consultation and the machine's most recent tick: **how long the
    /// advisor has been idle, as of now**. This is the number that makes "continuously active"
    /// falsifiable — an advisor consulted in a burst at boot and never since reports a silence that
    /// grows with the machine's uptime, however small its historical gaps were.
    pub fn silence_secs(&self) -> u64 {
        if self.advices == 0 {
            return 0;
        }
        self.last_tick_secs.saturating_sub(self.last_advice_secs)
    }

    /// Decisive verdicts as a share of all consultations, in tenths of a percent (no floating point).
    pub fn decisive_permille(&self) -> u64 {
        if self.advices == 0 {
            return 0;
        }
        ((self.low + self.elevated) * 1000) / self.advices
    }
}

/// The machine-lifetime advisor. Borrows the blob it was verified from (`include_bytes!`, or a
/// capability-scoped file read) and allocates nothing per advice.
/// What the machine's frame allocator says about itself at one instant (ADR-081): the frames it
/// manages and how many are free. The advisor never estimates memory — it is TOLD, by the
/// allocator, and refuses to admit past what it was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryMeter {
    pub total_pages: u64,
    pub free_pages: u64,
}

/// Below this share of free frames (permille of total) the machine is UNDER PRESSURE.
pub const LOW_WATERMARK_PERMILLE: u64 = 100;

impl MemoryMeter {
    /// A meter a boundary can be built on: at least one frame, and no more free than exist.
    pub fn valid(&self) -> bool {
        self.total_pages > 0 && self.free_pages <= self.total_pages
    }
    /// Free frames below the watermark share of the total.
    pub fn under_pressure(&self) -> bool {
        self.free_pages.saturating_mul(1000)
            < self.total_pages.saturating_mul(LOW_WATERMARK_PERMILLE)
    }
}

/// Why a bounded admission was refused (ADR-081). Every variant names the numbers it was judged on,
/// so a refusal on the boot log or the console is an explanation, not a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmitRefusal {
    /// No allocator has reported: the boundary cannot be enforced, so nothing passes through it.
    Unmetered,
    /// The reading offered was not a meter (zero total, or more free than exist); nothing recorded.
    MeterInvalid { total_pages: u64, free_pages: u64 },
    /// The task asks for more frames than the allocator has free right now.
    MemoryExhausted {
        requested_pages: u64,
        free_pages: u64,
    },
}

pub struct RiskService<'a> {
    model: Option<RiskAdvisor<'a>>,
    error: Option<ModelError>,
    source: FeatureSource,
    stats: AdviceStats,
    started: bool,
}

impl<'a> RiskService<'a> {
    /// Verify `bytes` and take up residence. A refused blob is **not** an error for the caller: the
    /// machine runs, model-free and bit-identically to a kernel built without one, and
    /// [`Self::model_error`] names the refusal so the console can print it.
    pub fn load(bytes: &'a [u8], capacity: MachineCapacity) -> Self {
        match RiskAdvisor::load(bytes) {
            Ok(m) => RiskService {
                model: Some(m),
                error: None,
                source: FeatureSource::new(capacity),
                stats: AdviceStats::default(),
                started: false,
            },
            Err(e) => RiskService {
                model: None,
                error: Some(e),
                source: FeatureSource::new(capacity),
                stats: AdviceStats::default(),
                started: false,
            },
        }
    }

    /// A service with no model at all — the control arm. Everything else about the machine is
    /// unchanged, which is what makes the A/B comparison in `tests/mlsched.rs` meaningful.
    pub fn without_model(capacity: MachineCapacity) -> Self {
        RiskService {
            model: None,
            error: None,
            source: FeatureSource::new(capacity),
            stats: AdviceStats::default(),
            started: false,
        }
    }

    /// The verified forest, if one is resident.
    pub fn model(&self) -> Option<&RiskAdvisor<'a>> {
        self.model.as_ref()
    }

    /// Why the blob was refused, if it was. `None` means either a loaded model or a service
    /// deliberately built without one — the console distinguishes those by [`Self::model`].
    pub fn model_error(&self) -> Option<ModelError> {
        self.error
    }

    /// Whether a model is resident and answering.
    pub fn active(&self) -> bool {
        self.model.is_some()
    }

    pub fn stats(&self) -> AdviceStats {
        self.stats
    }

    /// Read-only view of the live history the next advice will be derived from.
    pub fn source(&self) -> &FeatureSource {
        &self.source
    }

    /// Housekeeping: move the cell census to the bin containing `now_secs`. Called from the
    /// scheduler tick, so cell pressure ages even on a machine that is admitting nothing.
    pub fn tick(&mut self, now_secs: u64) {
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        if now_secs > self.stats.last_tick_secs {
            self.stats.last_tick_secs = now_secs;
        }
        self.source.advance_clock(now_secs);
    }

    /// Admit a task through the resident advisor.
    ///
    /// Derives the feature vector from live state, consults the forest if one is resident, records
    /// the census, and admits into `sched`. Returns the advice given, or `None` when no model is
    /// resident — in which case the admission went through the plain
    /// [`PriorityScheduler::admit`] path and the resulting schedule is the model-free one.
    pub fn admit(
        &mut self,
        sched: &mut PriorityScheduler,
        id: TaskId,
        base: Priority,
        now_secs: u64,
        task: &TaskSubmission,
    ) -> Option<Advice> {
        let x = self.source.observe_submit(now_secs, task);
        self.note_consultation(now_secs);

        match self.model {
            Some(ref m) => {
                let advice = m.advise(&x);
                match advice.verdict {
                    Verdict::Low => self.stats.low += 1,
                    Verdict::Elevated => self.stats.elevated += 1,
                    Verdict::Abstain => {
                        self.stats.abstain += 1;
                        // Outside the box is a *range* abstention; inside the box it is either the
                        // conformal band ("the model is unsure") or a degenerate constant input
                        // ("the extractor gave the model nothing to have an opinion ABOUT") —
                        // ALET-P3-006. Separating the three is what makes the census a diagnosis.
                        if !advice.out_of_range {
                            if advice.degenerate {
                                self.stats.degenerate_abstain += 1;
                            } else {
                                self.stats.band_abstain += 1;
                            }
                        }
                    }
                }
                if advice.out_of_range {
                    self.stats.out_of_range += 1;
                }
                sched.admit_with_advice(id, base, advice);
                Some(advice)
            }
            None => {
                self.stats.abstain += 1;
                sched.admit(id, base);
                None
            }
        }
    }

    /// Report that the scheduler dispatched a task.
    /// Record what the allocator says (ADR-081). An invalid meter is refused by name and changes
    /// nothing; a valid one updates the last reading, the low-water mark, and counts a pressure
    /// event exactly when the reading ENTERS the band below the watermark.
    pub fn observe_memory(&mut self, meter: MemoryMeter) -> Result<(), AdmitRefusal> {
        if !meter.valid() {
            return Err(AdmitRefusal::MeterInvalid {
                total_pages: meter.total_pages,
                free_pages: meter.free_pages,
            });
        }
        let s = &mut self.stats;
        s.total_pages = meter.total_pages;
        s.last_free_pages = meter.free_pages;
        if s.memory_samples == 0 || meter.free_pages < s.low_free_pages {
            s.low_free_pages = meter.free_pages;
        }
        s.memory_samples = s.memory_samples.saturating_add(1);
        let low = meter.under_pressure();
        if low && !s.in_pressure {
            s.pressure_events = s.pressure_events.saturating_add(1);
        }
        s.in_pressure = low;
        Ok(())
    }

    /// The last reading, or `None` while no allocator has reported.
    pub fn meter(&self) -> Option<MemoryMeter> {
        if self.stats.memory_samples == 0 {
            None
        } else {
            Some(MemoryMeter {
                total_pages: self.stats.total_pages,
                free_pages: self.stats.last_free_pages,
            })
        }
    }

    /// Admission through the memory boundary (ADR-081): refused `Unmetered` while no allocator has
    /// reported, refused `MemoryExhausted` when the task asks for more frames than are free — in
    /// both cases BEFORE the model is consulted and without touching the scheduler or the feature
    /// history (a refused task never existed). Otherwise exactly [`RiskService::admit`].
    pub fn admit_bounded(
        &mut self,
        sched: &mut PriorityScheduler,
        id: TaskId,
        base: Priority,
        now_secs: u64,
        task: &TaskSubmission,
    ) -> Result<Option<Advice>, AdmitRefusal> {
        let Some(m) = self.meter() else {
            self.stats.unmetered_refusals = self.stats.unmetered_refusals.saturating_add(1);
            return Err(AdmitRefusal::Unmetered);
        };
        if task.memory_pages > m.free_pages {
            self.stats.memory_refusals = self.stats.memory_refusals.saturating_add(1);
            return Err(AdmitRefusal::MemoryExhausted {
                requested_pages: task.memory_pages,
                free_pages: m.free_pages,
            });
        }
        Ok(self.admit(sched, id, base, now_secs, task))
    }

    pub fn observe_schedule(&mut self) {
        self.stats.schedules = self.stats.schedules.saturating_add(1);
        self.source.observe_schedule();
    }

    /// Report how a task ended, so the history the next advice reads is the history that happened.
    pub fn observe_outcome(&mut self, job: JobId, user: UserId, outcome: Outcome) {
        match outcome {
            Outcome::Finished => self.stats.finished += 1,
            Outcome::Failed => self.stats.failed += 1,
            Outcome::Evicted => self.stats.evicted += 1,
        }
        self.source.observe_outcome(job, user, outcome);
    }

    /// The feature vector the *next* admission of `task` at `now_secs` would be advised on, without
    /// admitting anything or disturbing the history. This is what the console prints: the evidence
    /// behind a verdict, not only the verdict.
    pub fn peek_features(
        &self,
        now_secs: u64,
        task: &TaskSubmission,
    ) -> [i32; crate::mlrisk_contract::N_FEATURES] {
        let mut probe = self.source.clone_for_probe();
        probe.observe_submit(now_secs, task)
    }

    /// Timestamp bookkeeping, including the gap that makes residency falsifiable.
    fn note_consultation(&mut self, now_secs: u64) {
        self.stats.advices = self.stats.advices.saturating_add(1);
        if !self.started {
            self.started = true;
            self.stats.first_advice_secs = now_secs;
        } else {
            let gap = now_secs.saturating_sub(self.stats.last_advice_secs);
            if gap > self.stats.max_gap_secs {
                self.stats.max_gap_secs = gap;
            }
        }
        self.stats.last_advice_secs = now_secs;
        if now_secs > self.stats.last_tick_secs {
            self.stats.last_tick_secs = now_secs;
        }
    }
}

/// A reference machine for the suite: eight CPUs, 4 GiB of RAM, 16 GiB of disk, expressed in the
/// units [`MachineCapacity`] takes. Fixed so the expected feature values below are arithmetic a
/// reader can check by hand rather than numbers to be trusted.
pub const SUITE_MACHINE: MachineCapacity = MachineCapacity {
    cpu_millis: 8_000,
    memory_pages: 1_048_576,
    disk_pages: 4_194_304,
};

/// A task asking for one eighth of the CPU and one eighth of the RAM of [`SUITE_MACHINE`].
fn suite_task(job: u64, task_index: u32, user: u64) -> TaskSubmission {
    TaskSubmission {
        sched_class: 2,
        priority: 5,
        cpu_millis: 1_000,
        memory_pages: 131_072,
        disk_pages: Some(419_430),
        diff_machine: false,
        task_index,
        job: JobId(job),
        user: UserId(user),
    }
}

/// Invariants of the **live** advisory path, re-proved on every boot of every target
/// (REQ-ML-003, ADR-056).
///
/// `mlrisk_suite` proves the forest is the one the trainer fitted; this suite proves the thing the
/// forest is now being *asked about* is a real task on this machine, and that asking has not moved
/// any authority. Each check reports through `report(n, passed, name)`; the first failure returns
/// its index and name, because the name is the diagnosis and the index is only an exit code.
pub fn mlsched_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
    use crate::mlrisk::BUNDLED_MODEL;
    use crate::mlrisk_contract::FEATURE_NAMES;

    let mut n: u32 = 0;
    macro_rules! check {
        ($cond:expr, $name:expr) => {{
            n += 1;
            let passed = $cond;
            report(n, passed, $name);
            if !passed {
                return Err((n, $name));
            }
        }};
    }

    // 1 — the derivation lands each value in the contract's own column, at the contract's own scale.
    // One eighth of the machine is 8192/65536 in the trainer's 16.16, and the bin-packing product is
    // (8192 * 8192) >> 16 = 1024. If a scale ever drifts, the forest's thresholds silently stop
    // meaning what they meant during fitting, so this is checked by value and not by shape.
    {
        let mut src = FeatureSource::new(SUITE_MACHINE);
        let x = src.observe_submit(3_600, &suite_task(1, 0, 1));
        let ok = FEATURE_NAMES[2] == "cpu_request"
            && FEATURE_NAMES[19] == "cpu_x_mem"
            && x[0] == 2
            && x[1] == 5
            && x[2] == 8_192
            && x[3] == 8_192
            && x[4] == 6_553
            && x[5] == 0
            && x[6] == 0
            && x[7] == 3_600
            && x[19] == 1_024;
        check!(
            ok,
            "mlsched: a live task derives into the contract's columns at the contract's scale"
        );
    }

    // 2 — exclusive history: a task never sees itself. The trainer counted a job's submissions
    // strictly before the row being described, so the first task of a job sees zero and the second
    // sees one — off by one here would be a model reading a future it cannot have.
    {
        let mut src = FeatureSource::new(SUITE_MACHINE);
        let first = src.observe_submit(0, &suite_task(7, 0, 3));
        let second = src.observe_submit(1, &suite_task(7, 1, 3));
        check!(
            first[9] == 0 && second[9] == 1 && first[11] == 0 && second[11] == 1,
            "mlsched: history is exclusive — a task is never counted in its own features"
        );
    }

    // 3 — cell pressure comes from the COMPLETED bin, never the one still filling. Three tasks
    // submitted inside one bin all see the same (empty) previous bin; only after the bin closes does
    // that pressure become visible.
    {
        let mut src = FeatureSource::new(SUITE_MACHINE);
        let a = src.observe_submit(10, &suite_task(1, 0, 1));
        let b = src.observe_submit(20, &suite_task(2, 0, 2));
        let c = src.observe_submit(30, &suite_task(3, 0, 3));
        let after = src.observe_submit(10 + PRESSURE_BIN_SEC_I64 as u64, &suite_task(4, 0, 4));
        check!(
            a[15] == 0 && b[15] == 0 && c[15] == 0 && after[15] == 3,
            "mlsched: cell pressure is read from the completed bin, never the one still filling"
        );
    }

    // 4 — an idle machine reports an idle bin. Skipping more than one boundary must publish an empty
    // previous bin rather than a stale one: the last busy five minutes of an hour-idle machine are
    // not what a scheduler is facing now.
    {
        let mut src = FeatureSource::new(SUITE_MACHINE);
        src.observe_submit(10, &suite_task(1, 0, 1));
        let far = src.observe_submit(10 + 10 * PRESSURE_BIN_SEC_I64 as u64, &suite_task(2, 0, 2));
        check!(
            far[15] == 0 && far[16] == 0 && far[17] == 0 && far[18] == 0,
            "mlsched: an idle machine's cell pressure ages out instead of going stale"
        );
    }

    // 5 — an unavailable field raises `missing_info` instead of being reported as a zero request. A
    // zero is a claim; the trainer gave provenance its own column so the kernel does not have to make
    // one up.
    {
        let mut src = FeatureSource::new(SUITE_MACHINE);
        let mut t = suite_task(1, 0, 1);
        t.disk_pages = None;
        let x = src.observe_submit(0, &t);
        check!(
            x[4] == 0 && x[6] != 0,
            "mlsched: an unobservable field is reported as missing, never as a zero measurement"
        );
    }

    // 6 — the feedback edge is live: a task that dies changes what the next task is advised on. This
    // is the difference between features derived once at boot and features derived from a machine
    // the advisor is actually watching.
    {
        let mut svc = RiskService::without_model(SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        svc.admit(&mut sched, TaskId(1), Priority(5), 0, &suite_task(1, 0, 9));
        svc.observe_outcome(JobId(1), UserId(9), Outcome::Failed);
        let x = svc.peek_features(1, &suite_task(1, 1, 9));
        // user_fails = 1, user_terminals = 1 => fail rate 1.0 at scale 10 000.
        check!(
            x[12] == 1 && x[14] == 1 && x[13] == 10_000,
            "mlsched: a task's death is fed back into the features the next advice reads"
        );
    }

    // 7 — THE advisory invariant, on the live path: with no model resident, a stream of admissions
    // drains in exactly the order the model-free kernel drains it in. The service is present, the
    // features are derived, the counters move — and the schedule is bit-identical.
    {
        let mut svc = RiskService::without_model(SUITE_MACHINE);
        let mut advised = PriorityScheduler::default();
        let mut plain = PriorityScheduler::default();
        for i in 0..64u64 {
            let t = suite_task(i % 8, i as u32, i % 5);
            svc.admit(&mut advised, TaskId(i + 1), Priority((i % 4) as u8), i, &t);
            plain.admit(TaskId(i + 1), Priority((i % 4) as u8));
        }
        let mut same = true;
        loop {
            let (a, p) = (advised.schedule_next(), plain.schedule_next());
            if a != p {
                same = false;
                break;
            }
            match a {
                None => break,
                Some(id) => {
                    advised.finish(id);
                    plain.finish(id);
                }
            }
        }
        check!(
            same,
            "mlsched: with no model resident the live path schedules bit-identically to the model-free kernel"
        );
    }

    // 8 — priority is never traded for risk, even when the model is loud. An `Elevated` task at a
    // higher base priority still runs before a `Low` task at a lower one: the verdict is a tiebreak
    // between equals and has no other power (INV-014).
    {
        let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        // A modest task and a machine-swallowing one; whatever the forest says about either, the
        // priority ordering below must hold.
        let modest = suite_task(1, 0, 1);
        let mut greedy = suite_task(2, 0, 2);
        greedy.cpu_millis = 7_900;
        greedy.memory_pages = 1_000_000;
        svc.admit(&mut sched, TaskId(1), Priority(9), 0, &greedy);
        svc.admit(&mut sched, TaskId(2), Priority(1), 1, &modest);
        check!(
            sched.schedule_next() == Some(TaskId(1)),
            "mlsched: priority is never traded for risk on the live path"
        );
    }

    // 9 — residency is accounted for: every admission is exactly one consultation, and the verdict
    // census accounts for every one of them. A census that does not sum is a counter that is not
    // watching the same thing the scheduler is.
    {
        let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        for i in 0..128u64 {
            let mut t = suite_task(i % 16, i as u32, i % 7);
            t.priority = (i % 12) as u8;
            t.sched_class = (i % 4) as u8;
            t.cpu_millis = 100 + (i as u32 % 40) * 100;
            svc.admit(
                &mut sched,
                TaskId(i + 1),
                Priority((i % 8) as u8),
                i * 7,
                &t,
            );
        }
        let s = svc.stats();
        check!(
            s.advices == 128 && s.low + s.elevated + s.abstain == s.advices,
            "mlsched: every admission is one consultation and the census accounts for all of them"
        );
    }

    // 10 — continuity is measurable and honest. Over a stream spanning a known span with a known
    // largest gap, the service reports that gap — so a model that answered a burst at boot and
    // nothing since cannot be described as continuously active without the number contradicting it.
    {
        let mut svc = RiskService::without_model(SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        let stamps = [0u64, 5, 11, 900, 905];
        for (i, t) in stamps.iter().enumerate() {
            svc.admit(
                &mut sched,
                TaskId(i as u64 + 1),
                Priority(4),
                *t,
                &suite_task(1, i as u32, 1),
            );
        }
        // Then the machine keeps running without admitting anything: ticks advance the clock, and
        // the SILENCE grows even though the historical gap cannot. That asymmetry is the point --
        // a max gap only ever closes when the next advice arrives, so an advisor that fell quiet
        // would keep reporting the small gaps it managed while it was busy.
        svc.tick(1_805);
        let s = svc.stats();
        check!(
            s.max_gap_secs == 889
                && s.span_secs() == 905
                && s.first_advice_secs == 0
                && s.silence_secs() == 900,
            "mlsched: the gap between consultations and the silence since the last one are both measured"
        );
    }

    // 11 — the live history is BOUNDED. A kernel heap that never frees cannot host a map that grows
    // with every principal a long-lived machine ever sees; past the cap, new principals are described
    // as missing rather than remembered, and the map stops growing.
    {
        let mut src = FeatureSource::new(SUITE_MACHINE);
        let over = FeatureSource::HISTORY_CAP + 64;
        let mut flagged = 0u32;
        for i in 0..over {
            let x = src.observe_submit(i as u64, &suite_task(i as u64, 0, i as u64));
            if x[6] & 2 != 0 {
                flagged += 1;
            }
        }
        let (jobs, users) = src.tracked();
        check!(
            jobs == FeatureSource::HISTORY_CAP
                && users == FeatureSource::HISTORY_CAP
                && flagged == 64
                && src.capped_submissions() == 64,
            "mlsched: live history is bounded, and a submission described without it says so"
        );
    }

    // 12 — a task outside anything the forest was fitted on is abstained on, not guessed at. A
    // request larger than the machine is exactly that case, and it is counted separately from the
    // conformal band so the console can tell "unsure" from "never seen".
    {
        let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        let mut absurd = suite_task(1, 0, 1);
        absurd.cpu_millis = 8_000_000;
        absurd.memory_pages = 1_000_000_000;
        let advice = svc.admit(&mut sched, TaskId(1), Priority(5), 0, &absurd);
        let out_of_box = advice.map(|a| a.out_of_range).unwrap_or(false);
        let abstained = advice
            .map(|a| a.verdict == Verdict::Abstain)
            .unwrap_or(false);
        check!(
            !svc.active() || (out_of_box && abstained && svc.stats().out_of_range == 1),
            "mlsched: a task outside the training box is abstained on, and counted as such"
        );
    }

    // 13 — a reading that is not a meter is refused BY NAME and records nothing: zero frames, or
    //      more free than exist. The boundary is built on the allocator's word, so a word that
    //      cannot be true is not written down.
    {
        let mut svc = RiskService::without_model(SUITE_MACHINE);
        let zero = svc.observe_memory(MemoryMeter {
            total_pages: 0,
            free_pages: 0,
        });
        let over = svc.observe_memory(MemoryMeter {
            total_pages: 100,
            free_pages: 101,
        });
        let s = svc.stats();
        check!(
            matches!(
                zero,
                Err(AdmitRefusal::MeterInvalid {
                    total_pages: 0,
                    free_pages: 0
                })
            ) && matches!(
                over,
                Err(AdmitRefusal::MeterInvalid {
                    total_pages: 100,
                    free_pages: 101
                })
            ) && s.memory_samples == 0
                && svc.meter().is_none(),
            "mlsched: a reading that is not a meter is refused by name and records nothing"
        );
    }

    // 14 — fail closed: before any allocator has reported, the bounded door admits NOTHING. The
    //      refusal is counted, the scheduler has no task, and the model was not consulted.
    {
        let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        let r = svc.admit_bounded(
            &mut sched,
            TaskId(1),
            Priority(5),
            100,
            &suite_task(1, 0, 1),
        );
        let s = svc.stats();
        check!(
            matches!(r, Err(AdmitRefusal::Unmetered))
                && s.unmetered_refusals == 1
                && s.advices == 0
                && sched.state(TaskId(1)).is_none()
                && sched.schedule_next().is_none(),
            "mlsched: with no allocator reading the bounded door admits nothing - fail closed, counted"
        );
    }

    // 15 — the boundary: with F frames free, a task asking for F+1 is refused `MemoryExhausted`
    //      naming both numbers, before the model is consulted (advices unchanged), with the
    //      scheduler untouched and the feature history unmoved (a refused task never existed);
    //      a task asking for exactly F is admitted, advised, and Ready.
    {
        let mut svc = RiskService::load(BUNDLED_MODEL, SUITE_MACHINE);
        let mut sched = PriorityScheduler::default();
        svc.observe_memory(MemoryMeter {
            total_pages: SUITE_MACHINE.memory_pages,
            free_pages: 131_072,
        })
        .unwrap();
        let mut big = suite_task(2, 0, 2);
        big.memory_pages = 131_073;
        let refused = svc.admit_bounded(&mut sched, TaskId(2), Priority(5), 200, &big);
        let after_refusal = svc.stats();
        let history_after_refusal = svc.source().tracked();
        let fits = suite_task(3, 0, 3); // asks for exactly 131_072 pages
        let admitted = svc.admit_bounded(&mut sched, TaskId(3), Priority(5), 201, &fits);
        let s = svc.stats();
        check!(
            matches!(
                refused,
                Err(AdmitRefusal::MemoryExhausted {
                    requested_pages: 131_073,
                    free_pages: 131_072
                })
            ) && after_refusal.memory_refusals == 1
                && after_refusal.advices == 0
                && history_after_refusal == (0, 0)
                && sched.state(TaskId(2)).is_none()
                && admitted.is_ok()
                && s.advices == 1
                && sched.state(TaskId(3)).is_some()
                && sched.schedule_next() == Some(TaskId(3)),
            "mlsched: a task asking for more than is free is refused by name before the model is asked - the scheduler untouched"
        );
    }

    // 16 — the meter is a ledger: last reading, low-water mark and sample count follow the
    //      readings exactly, and a crossing INTO the pressure band counts once - staying counts
    //      nothing, leaving and re-entering counts again.
    {
        let mut svc = RiskService::without_model(SUITE_MACHINE);
        let t = 1_000u64;
        let seq = [500u64, 250, 90, 80, 300, 50, 600];
        let mut ok = true;
        for &f in &seq {
            ok &= svc
                .observe_memory(MemoryMeter {
                    total_pages: t,
                    free_pages: f,
                })
                .is_ok();
        }
        let s = svc.stats();
        check!(
            ok && s.memory_samples == 7
                && s.total_pages == 1_000
                && s.last_free_pages == 600
                && s.low_free_pages == 50
                && s.pressure_events == 2
                && !s.in_pressure
                && svc.meter()
                    == Some(MemoryMeter {
                        total_pages: 1_000,
                        free_pages: 600
                    }),
            "mlsched: the meter is a ledger - low-water exact, a pressure crossing counted once per entry"
        );
    }

    // 17 — the boundary follows the LATEST reading, and refusals are deterministic: the same
    //      readings and the same requests give the same verdicts and the same counters on two
    //      services, and a request that was refused becomes admissible the moment the allocator
    //      reports enough free frames.
    {
        let run = || {
            let mut svc = RiskService::without_model(SUITE_MACHINE);
            let mut sched = PriorityScheduler::default();
            let mut t = suite_task(4, 0, 4);
            t.memory_pages = 1_000;
            svc.observe_memory(MemoryMeter {
                total_pages: 10_000,
                free_pages: 999,
            })
            .unwrap();
            let a = svc
                .admit_bounded(&mut sched, TaskId(4), Priority(5), 300, &t)
                .is_err();
            svc.observe_memory(MemoryMeter {
                total_pages: 10_000,
                free_pages: 1_000,
            })
            .unwrap();
            let b = svc
                .admit_bounded(&mut sched, TaskId(4), Priority(5), 301, &t)
                .is_ok();
            (a, b, svc.stats())
        };
        let (a1, b1, s1) = run();
        let (a2, b2, s2) = run();
        check!(
            a1 && b1 && a2 && b2 && s1 == s2 && s1.memory_refusals == 1 && s1.advices == 1,
            "mlsched: the boundary follows the latest reading and refuses deterministically"
        );
    }

    Ok(n)
}

/// The pressure-bin width, as the `i64` the suite's arithmetic uses.
const PRESSURE_BIN_SEC_I64: i64 = crate::taskfeat::PRESSURE_BIN_SEC as i64;

/// The machine's single resident advisor, and the reason the model can be described as *running*.
///
/// A boot selftest that loads a blob, asks it some questions and drops it proves the blob is good;
/// it does not make the machine one that consults a model. This is the seam that does: every target
/// calls [`resident::install`] once at boot, and from then until the machine stops, one verified
/// forest is held behind one lock, consulted through [`resident::admit`], aged by
/// [`resident::tick`], and interrogable at any moment from the console by [`resident::stats`].
///
/// It lives in `kernel-core` rather than in each target's `main.rs` for the same reason the spine
/// does (ADR-019): three copies of a residency are three chances for one of them to quietly stop.
pub mod resident {
    use super::{AdmitRefusal, AdviceStats, MemoryMeter, RiskService};
    use crate::mlrisk::Advice;
    use crate::priosched::{Priority, PriorityScheduler};
    use crate::sched::TaskId;
    use crate::sync::SpinLock;
    use crate::taskfeat::{JobId, MachineCapacity, Outcome, TaskSubmission, UserId};

    /// `None` means no target has installed an advisor on this machine — which is a *state*, not a
    /// failure, and every accessor below reports it rather than substituting a default answer.
    static RESIDENT: SpinLock<Option<RiskService<'static>>> = SpinLock::new(None);

    /// Take up residence for the rest of the machine's uptime. Verifying the blob is
    /// [`RiskService::load`]'s job; a refusal still installs a service, so the console can say which
    /// check refused the model instead of the machine merely behaving as though there never was one.
    /// Returns whether a model is actually resident.
    pub fn install(bytes: &'static [u8], capacity: MachineCapacity) -> bool {
        let svc = RiskService::load(bytes, capacity);
        let active = svc.active();
        *RESIDENT.lock() = Some(svc);
        active
    }

    /// Install the control arm: a live service with no model at all. Used by a target that wants the
    /// counters and the feature derivation without the forest.
    pub fn install_without_model(capacity: MachineCapacity) {
        *RESIDENT.lock() = Some(RiskService::without_model(capacity));
    }

    /// Whether a verified model is resident and answering right now.
    pub fn active() -> bool {
        RESIDENT.lock().as_ref().is_some_and(|s| s.active())
    }

    /// The forest's shape, for a console that wants to print what it is holding.
    /// `(trees, nodes, worst-case compares per advice)`.
    pub fn shape() -> Option<(usize, usize, usize)> {
        let g = RESIDENT.lock();
        let m = g.as_ref()?.model()?;
        Some((m.trees(), m.nodes(), m.worst_case_compares()))
    }

    /// Why the resident blob was refused, if it was.
    pub fn model_error() -> Option<crate::mlrisk::ModelError> {
        RESIDENT.lock().as_ref().and_then(|s| s.model_error())
    }

    /// Live counters. `None` only when no target ever installed an advisor.
    pub fn stats() -> Option<AdviceStats> {
        RESIDENT.lock().as_ref().map(|s| s.stats())
    }

    /// Admit a task through the resident advisor. This is the kernel's admission path; there is no
    /// second one that bypasses it — and since ADR-081 it is the BOUNDED path: refused by name when
    /// no allocator has reported or when the task asks for more frames than are free.
    pub fn admit(
        sched: &mut PriorityScheduler,
        id: TaskId,
        base: Priority,
        now_secs: u64,
        task: &TaskSubmission,
    ) -> Result<Option<Advice>, AdmitRefusal> {
        match *RESIDENT.lock() {
            Some(ref mut s) => s.admit_bounded(sched, id, base, now_secs, task),
            // No advisor installed: the deterministic policy stands, exactly as it would in a kernel
            // built without one. The boundary is the SERVICE's; a machine that never installed one
            // has said so on its boot log.
            None => {
                sched.admit(id, base);
                Ok(None)
            }
        }
    }

    /// Tell the resident service what the allocator says (ADR-081). `Ok(true)` recorded,
    /// `Ok(false)` no service installed, `Err` the reading was not a meter.
    pub fn observe_memory(meter: MemoryMeter) -> Result<bool, AdmitRefusal> {
        match *RESIDENT.lock() {
            Some(ref mut s) => s.observe_memory(meter).map(|_| true),
            None => Ok(false),
        }
    }

    /// The resident service's last allocator reading, if any.
    pub fn meter() -> Option<MemoryMeter> {
        RESIDENT.lock().as_ref().and_then(|s| s.meter())
    }

    /// Age the cell census. Called from the scheduler tick so pressure ages on a machine that is
    /// admitting nothing — an idle machine is a *fact* about the machine, not an absence of data.
    pub fn tick(now_secs: u64) {
        if let Some(ref mut s) = *RESIDENT.lock() {
            s.tick(now_secs);
        }
    }

    /// Report a dispatch.
    pub fn observe_schedule() {
        if let Some(ref mut s) = *RESIDENT.lock() {
            s.observe_schedule();
        }
    }

    /// Report how a task ended.
    pub fn observe_outcome(job: JobId, user: UserId, outcome: Outcome) {
        if let Some(ref mut s) = *RESIDENT.lock() {
            s.observe_outcome(job, user, outcome);
        }
    }

    /// Drop the resident advisor. Exists for the hosted tests, which must be able to run the
    /// installation invariants from a known-empty state; a booted machine never calls it.
    pub fn uninstall() {
        *RESIDENT.lock() = None;
    }
}

/// What the commissioning workload did, for a target to print.
#[derive(Clone, Copy, Debug)]
pub struct Commissioning {
    /// Tasks admitted through the resident advisor.
    pub admitted: u64,
    /// Simulated seconds of machine time the arrivals were spread over.
    pub span_secs: u64,
    /// Cell-pressure bins the census rolled through.
    pub bins: u64,
    /// Counters after the workload.
    pub stats: AdviceStats,
    /// Whether the advised drain was a permutation of the model-free one — no task invented,
    /// dropped or starved.
    pub permutation: bool,
    /// Arrivals the memory boundary refused (ADR-081). Zero on a machine whose allocator reported
    /// before commissioning; every one of them is a target that forgot to, or a request the
    /// workload sized wrongly — both are boot failures, not statistics.
    pub refused: u64,
}

/// Put the advisor into residence and prove, on this machine, that it is being consulted.
///
/// A boot that only ran `mlsched_suite` would prove the *code* is correct on hardware. This
/// additionally exercises the path the machine will use for the rest of its uptime: real admissions
/// through [`resident::admit`], real dispatches and outcomes fed back, and the census rolled across
/// many five-minute bins so the arrival history the advisor reads is one it built itself.
///
/// The arrival *times* are simulated — a boot lasts under a second and cell pressure is a
/// five-minute quantity, so a workload spread over the real boot clock would exercise exactly one
/// bin and prove nothing about ageing. Everything else is real: real feature derivation, real
/// margins from the image's own blob, the real scheduler. The console's `mlstat` afterwards reports
/// the machine's own clock, so nothing here is mistaken later for wall-clock uptime.
///
/// Returns the summary, plus the permutation check that keeps this from being a demo: whatever the
/// advice did to the order, the same tasks came out.
pub fn commission(tasks: u64, secs_per_task: u64) -> Commissioning {
    let mut sched = PriorityScheduler::default();
    let mut plain = PriorityScheduler::default();
    let mut span = 0u64;
    // The memory ceiling of this workload is the machine's OWN free frames as its allocator last
    // reported them (ADR-081): every arrival below asks for a share of THAT, from a rounding error
    // to all of it, so the bounded door admits each one on a machine that reported and refuses
    // every one on a machine that did not - which `refused` then says on the boot log.
    let ceiling = resident::meter().map(|m| m.free_pages).unwrap_or(0);
    let mut admitted = 0u64;
    let mut refused = 0u64;

    for i in 0..tasks {
        let now = i * secs_per_task;
        span = now;
        let mut t = suite_task(i % 64, i as u32, i % 23);
        // A workload with texture: several priority bands, several latency classes, requests from a
        // rounding error to most of the machine — so the range guard, the conformal band and both
        // decisive verdicts all get exercised rather than one corner of the feature box.
        t.priority = (i % 12) as u8;
        t.sched_class = (i % 4) as u8;
        t.cpu_millis = 50 + (i as u32 % 61) * 20;
        t.memory_pages = 1 + (i % 97) * (ceiling / 128);
        t.diff_machine = i % 7 == 0;
        // **This machine has no per-task disk request to report, and says so.** It is not a
        // convenience: the shipped borg2019 blob's `disk_request` training range is literally
        // `[0, 0]` — the corpus carries no disk signal at all — so a kernel that supplied a real
        // disk fraction would place EVERY task outside the training box and the advisor would
        // correctly abstain about the entire machine. Reporting the field as unobservable is both
        // true of Aletheia today and the only value the shipped model has ever seen. When a corpus
        // with a disk signal is trained, this becomes a real measurement and the range guard starts
        // meaning something for this column.
        t.disk_pages = None;
        // One arrival in eleven is deliberately enormous, so the range guard is exercised on a
        // booted machine rather than only in a test: a workload that never leaves the box would
        // never prove the kernel declines to guess outside it.
        if i % 11 == 0 {
            t.cpu_millis = 7_900;
            t.memory_pages = ceiling; // every free frame the machine has - admissible, and large
        }
        let band = Priority((i % 8) as u8);
        match resident::admit(&mut sched, TaskId(i + 1), band, now, &t) {
            Ok(_) => {
                admitted += 1;
                plain.admit(TaskId(i + 1), band);
            }
            Err(_) => {
                refused += 1;
                resident::tick(now);
                continue;
            }
        }

        // The feedback edge, driven by nothing the model said: a fixed pattern of outcomes, so the
        // history is real without being the model's own opinion echoed back at it.
        resident::observe_schedule();
        let outcome = match i % 5 {
            0 => Outcome::Failed,
            1 => Outcome::Evicted,
            _ => Outcome::Finished,
        };
        resident::observe_outcome(JobId(i % 64), UserId(i % 23), outcome);
        resident::tick(now);
    }

    // Drain both and compare as multisets: the advised order may differ, the *contents* may not.
    let (mut a, mut b) = (Vec::new(), Vec::new());
    while let Some(id) = sched.schedule_next() {
        a.push(id);
        sched.finish(id);
    }
    while let Some(id) = plain.schedule_next() {
        b.push(id);
        plain.finish(id);
    }
    a.sort();
    b.sort();

    Commissioning {
        admitted,
        span_secs: span,
        bins: span / crate::taskfeat::PRESSURE_BIN_SEC,
        stats: resident::stats().unwrap_or_default(),
        permutation: a == b && a.len() as u64 == admitted,
        refused,
    }
}
