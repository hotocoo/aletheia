//! Reclaim under pressure: the allocator's word triggers, the policy chooses, the forest advises
//! (REQ-ML-005 wired, ADR-082).
//!
//! ADR-081 made memory a boundary at the door: a task asking for more frames than are free is
//! refused before the model is consulted. What it did not answer is the question a machine that
//! is ALREADY under pressure faces — whose frames go? — and the register said so: the second
//! forest, trained on the eviction event specifically (`memrisk`, REQ-ML-005), was measured,
//! exported and never wired. This module wires it the way ADR-056 wired the first one: as an
//! ORDERING, never a verdict with authority.
//!
//! # Three parties, three jobs, none of them shared
//!
//! * **The allocator triggers.** A [`MemoryMeter`] reading (ADR-081) under the watermark is the
//!   only thing that opens a reclaim round; a reading that is not under pressure is refused
//!   [`ReclaimRefusal::NotUnderPressure`] by name and nothing moves. The round's NEED is the
//!   frame count that puts the machine back at twice the watermark — leave the band with room,
//!   not by one frame.
//! * **The policy chooses.** Candidates are ranked deterministically: protected ones are never
//!   chosen (the kernel's own frames, a task the policy shields — skipped and COUNTED, even when
//!   skipping them means the need goes unmet); among the rest, the forest's tier first, then the
//!   largest footprint (free the most with the fewest evictions), then the lowest priority, then
//!   the oldest submission, then the task id — a total order, so two rounds over the same inputs
//!   evict the same tasks in the same sequence.
//! * **The forest advises the tier.** `memrisk` predicts the EVICTION event: a task it marks
//!   `Elevated` is one the trace says would have been evicted anyway, so its work is the cheapest
//!   to lose — tier 0. `Low` is a task likely to complete — taking its frames destroys work it had
//!   already done, the mistake the trainer priced at 4x — tier 2. `Abstain` (inside the conformal
//!   band, outside the training box, degenerate input) and NO MODEL AT ALL are tier 1, so a
//!   machine without the blob, or with one the loader refused, ranks bit-identically to a machine
//!   whose forest abstains about everyone: the model changes the ORDER among candidates and never
//!   whether reclaim happens, how much is needed, or what protection means (INV-014, again).
//!
//! # Execution is a seam
//!
//! [`ReclaimOps::evict`] is what a target does to a chosen task: terminate it through the
//! supervisor (ADR-042) and return its frames through the owner table (ADR-030, ADR-032). The
//! policy asks for it exactly once per chosen task and trusts the number of frames it says came
//! back; the suite drives it with a recording mock, the targets drive it against their REAL
//! allocator under a REAL storm (`storm`), and the two paths share every line above this one.
//!
//! # What this rung does not claim
//!
//! The forest's features are the candidate's SUBMISSION-time vector (the same 20 columns ADR-056
//! derives); no run-time footprint signal reaches it, because the contract is frozen. Protection
//! is a flag the caller sets, not a capability. A round evicts whole tasks; there is no partial
//! reclaim, no swap, no compression. The watermark and the headroom factor are constants.

use alloc::vec::Vec;

use crate::frameown::Owner;
use crate::mlrisk::{Advice, ModelError, RiskAdvisor, Verdict};
use crate::mlrisk_contract::N_FEATURES;
use crate::mlsched::{MemoryMeter, LOW_WATERMARK_PERMILLE};
use crate::priosched::Priority;
use crate::sched::TaskId;

/// The eviction-event forest (`memrisk`, REQ-ML-005): same `ALTM1` format, same 20-column
/// contract, verified at boot by the SAME loader as the risk forest — no second loader, no
/// second contract hash that could drift.
pub const BUNDLED_RECLAIM_MODEL: &[u8] = include_bytes!("../models/aletheia_reclaim.altm");

/// A reclaim round aims to leave the pressure band with this much room: free frames back at
/// `HEADROOM_FACTOR` times the watermark share of the total.
pub const HEADROOM_FACTOR: u64 = 2;

/// A task the reclaimer may take frames from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub task: TaskId,
    pub owner: Owner,
    /// Frames this task holds — what evicting it returns.
    pub footprint_pages: u64,
    pub priority: Priority,
    pub submitted_secs: u64,
    /// Never chosen: the kernel's own frames, or a task the policy shields. Skipped and counted.
    pub protected: bool,
    /// The candidate's submission-time feature vector (ADR-056's 20 columns), what the forest is
    /// asked about.
    pub features: [i32; N_FEATURES],
}

/// Why a round did nothing. Every variant names the numbers it was judged on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimRefusal {
    /// The meter is not under the watermark: nothing to reclaim, nothing reclaimed.
    NotUnderPressure { free_pages: u64, total_pages: u64 },
    /// No candidate may be evicted: none offered, or every one protected.
    NothingEvictable { candidates: usize, protected: usize },
}

/// The forest's opinion of a candidate, folded into the rank: 0 evict first, 2 evict last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// The forest says this task was going to be evicted anyway — its work is the cheapest to lose.
    EvictionLikely = 0,
    /// The forest abstains, the task is outside the box, or there is no forest.
    Unknown = 1,
    /// The forest says this task would complete — taking its frames destroys work.
    CompletionLikely = 2,
}

/// One ranked candidate: what the policy decided and, if a forest was asked, what it said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranked {
    pub candidate: Candidate,
    pub tier: Tier,
    pub advice: Option<Advice>,
}

/// What one round did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimOutcome {
    /// Frames the round set out to free (to `HEADROOM_FACTOR` x the watermark).
    pub need: u64,
    /// Tasks evicted, in the order the policy chose them.
    pub evicted: Vec<TaskId>,
    /// Frames the ops seam reported back.
    pub frames_reclaimed: u64,
    /// Frames still owed when the candidates ran out (0 when the need was met).
    pub shortfall: u64,
    /// Protected candidates that were skipped this round.
    pub protected_skipped: u64,
}

/// The ledger `mlstat` can print: every round, refusal, eviction and frame, since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReclaimLedger {
    /// Rounds that evicted at least one task.
    pub rounds: u64,
    /// Rounds refused by name (not under pressure, nothing evictable).
    pub refusals: u64,
    pub evictions: u64,
    pub frames_reclaimed: u64,
    pub protected_skipped: u64,
    /// Rounds whose candidates ran out before the need was met.
    pub shortfalls: u64,
    /// Candidates the forest gave a decisive tier (Elevated or Low).
    pub advised: u64,
    /// Candidates ranked in the Unknown tier (abstained, out of box, or no forest).
    pub unadvised: u64,
}

/// What a target does to a chosen task: terminate it and return its frames. Returns the frames
/// that came back — the policy trusts this number and counts it.
pub trait ReclaimOps {
    fn evict(&mut self, task: TaskId, owner: Owner) -> u64;
}

/// The reclaimer: an optional, verified forest plus the ledger.
pub struct Reclaimer<'a> {
    model: Option<RiskAdvisor<'a>>,
    error: Option<ModelError>,
    ledger: ReclaimLedger,
}

impl<'a> Reclaimer<'a> {
    /// Verify and hold the forest; a refused blob is NAMED and the reclaimer still works.
    pub fn load(bytes: &'a [u8]) -> Self {
        match RiskAdvisor::load(bytes) {
            Ok(m) => Reclaimer {
                model: Some(m),
                error: None,
                ledger: ReclaimLedger::default(),
            },
            Err(e) => Reclaimer {
                model: None,
                error: Some(e),
                ledger: ReclaimLedger::default(),
            },
        }
    }

    /// The model-free reclaimer: every candidate is `Tier::Unknown`.
    pub fn without_model() -> Self {
        Reclaimer {
            model: None,
            error: None,
            ledger: ReclaimLedger::default(),
        }
    }

    pub fn active(&self) -> bool {
        self.model.is_some()
    }
    pub fn model(&self) -> Option<&RiskAdvisor<'a>> {
        self.model.as_ref()
    }
    pub fn model_error(&self) -> Option<ModelError> {
        self.error
    }
    pub fn ledger(&self) -> ReclaimLedger {
        self.ledger
    }

    /// Frames a round must free to leave the band with headroom: the shortfall from
    /// `HEADROOM_FACTOR` x the watermark share of the total. Zero when not under pressure.
    pub fn need(meter: MemoryMeter) -> u64 {
        if !meter.under_pressure() {
            return 0;
        }
        let target_free = meter
            .total_pages
            .saturating_mul(LOW_WATERMARK_PERMILLE * HEADROOM_FACTOR)
            / 1000;
        target_free.saturating_sub(meter.free_pages)
    }

    /// The forest's tier for one candidate; `Unknown` without a forest or when it abstains.
    pub fn tier_of(&mut self, c: &Candidate) -> (Tier, Option<Advice>) {
        match self.model {
            None => {
                self.ledger.unadvised += 1;
                (Tier::Unknown, None)
            }
            Some(ref m) => {
                let a = m.advise(&c.features);
                let tier = match a.verdict {
                    Verdict::Elevated => Tier::EvictionLikely,
                    Verdict::Low => Tier::CompletionLikely,
                    Verdict::Abstain => Tier::Unknown,
                };
                if tier == Tier::Unknown {
                    self.ledger.unadvised += 1;
                } else {
                    self.ledger.advised += 1;
                }
                (tier, Some(a))
            }
        }
    }

    /// Rank the evictable candidates in the order a round would take them. Protected candidates
    /// are not in the result (they are counted by the round). Total order: tier, footprint
    /// descending, priority ascending, submission ascending, task id ascending.
    pub fn rank(&mut self, candidates: &[Candidate]) -> Vec<Ranked> {
        let mut ranked: Vec<Ranked> = candidates
            .iter()
            .filter(|c| !c.protected)
            .map(|c| {
                let (tier, advice) = self.tier_of(c);
                Ranked {
                    candidate: *c,
                    tier,
                    advice,
                }
            })
            .collect();
        ranked.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then(
                    b.candidate
                        .footprint_pages
                        .cmp(&a.candidate.footprint_pages),
                )
                .then(a.candidate.priority.0.cmp(&b.candidate.priority.0))
                .then(a.candidate.submitted_secs.cmp(&b.candidate.submitted_secs))
                .then(a.candidate.task.0.cmp(&b.candidate.task.0))
        });
        ranked
    }

    /// One reclaim round. Refused by name when the meter is not under pressure or nothing may be
    /// evicted; otherwise evicts in rank order, through `ops`, exactly once per chosen task, until
    /// the need is met or the candidates run out (a SHORTFALL, counted and reported, never hidden).
    pub fn reclaim(
        &mut self,
        meter: MemoryMeter,
        candidates: &[Candidate],
        ops: &mut impl ReclaimOps,
    ) -> Result<ReclaimOutcome, ReclaimRefusal> {
        if !meter.under_pressure() {
            self.ledger.refusals += 1;
            return Err(ReclaimRefusal::NotUnderPressure {
                free_pages: meter.free_pages,
                total_pages: meter.total_pages,
            });
        }
        let protected = candidates.iter().filter(|c| c.protected).count() as u64;
        let ranked = self.rank(candidates);
        if ranked.is_empty() {
            self.ledger.refusals += 1;
            self.ledger.protected_skipped += protected;
            return Err(ReclaimRefusal::NothingEvictable {
                candidates: candidates.len(),
                protected: protected as usize,
            });
        }
        let need = Self::need(meter);
        let mut freed = 0u64;
        let mut evicted = Vec::new();
        for r in ranked.iter() {
            if freed >= need {
                break;
            }
            let got = ops.evict(r.candidate.task, r.candidate.owner);
            freed = freed.saturating_add(got);
            evicted.push(r.candidate.task);
        }
        let shortfall = need.saturating_sub(freed);
        self.ledger.rounds += 1;
        self.ledger.evictions += evicted.len() as u64;
        self.ledger.frames_reclaimed += freed;
        self.ledger.protected_skipped += protected;
        if shortfall > 0 {
            self.ledger.shortfalls += 1;
        }
        Ok(ReclaimOutcome {
            need,
            evicted,
            frames_reclaimed: freed,
            shortfall,
            protected_skipped: protected,
        })
    }
}

/// A recording ops seam for the suites: every eviction is logged, and each candidate's footprint
/// is what "came back".
#[derive(Default)]
pub struct MockOps {
    pub calls: Vec<(TaskId, Owner)>,
    pub footprints: Vec<(TaskId, u64)>,
}

impl MockOps {
    pub fn with(cands: &[Candidate]) -> Self {
        MockOps {
            calls: Vec::new(),
            footprints: cands.iter().map(|c| (c.task, c.footprint_pages)).collect(),
        }
    }
}

impl ReclaimOps for MockOps {
    fn evict(&mut self, task: TaskId, owner: Owner) -> u64 {
        self.calls.push((task, owner));
        self.footprints
            .iter()
            .find(|(t, _)| *t == task)
            .map(|(_, f)| *f)
            .unwrap_or(0)
    }
}

fn cand(id: u64, footprint: u64, prio: u8, at: u64, protected: bool) -> Candidate {
    Candidate {
        task: TaskId(id),
        owner: Owner::address_space(id as u32).unwrap_or(Owner::USER),
        footprint_pages: footprint,
        priority: Priority(prio),
        submitted_secs: at,
        protected,
        features: [0; N_FEATURES],
    }
}

/// A meter under pressure for the suite: 5 % free of 10 000, need = 2000 - 500 = 1500.
pub const SUITE_PRESSURE: MemoryMeter = MemoryMeter {
    total_pages: 10_000,
    free_pages: 500,
};

/// Boot suite: the policy, the tiers, the seam and the ledger, on synthetic candidates. Runs on
/// every target; the live storm (`storm`) then drives the same reclaimer against the REAL
/// allocator.
pub fn reclaim_suite(
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<u32, (u32, &'static str)> {
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

    // 1 — the eviction-event forest is VERIFIED by the same loader as the risk forest: same
    //     format, same 20-column contract. Its shape is the blob's, read back, not asserted.
    {
        let r = Reclaimer::load(BUNDLED_RECLAIM_MODEL);
        let shape = r.model().map(|m| (m.trees(), m.nodes()));
        check!(
            r.active()
                && r.model_error().is_none()
                && matches!(shape, Some((t, nn)) if t > 0 && nn > t),
            "reclaim: the eviction-event forest verifies under the risk forest's own loader and contract"
        );
    }

    // 2 — the allocator triggers: a meter that is not under pressure is refused by name, and the
    //     need of such a meter is zero. Nothing is evicted, the refusal is counted.
    {
        let mut r = Reclaimer::without_model();
        let calm = MemoryMeter {
            total_pages: 10_000,
            free_pages: 5_000,
        };
        let cands = [cand(1, 100, 5, 0, false)];
        let mut ops = MockOps::with(&cands);
        let out = r.reclaim(calm, &cands, &mut ops);
        check!(
            out == Err(ReclaimRefusal::NotUnderPressure {
                free_pages: 5_000,
                total_pages: 10_000
            }) && Reclaimer::need(calm) == 0
                && ops.calls.is_empty()
                && r.ledger().refusals == 1
                && r.ledger().rounds == 0,
            "reclaim: a machine not under pressure reclaims nothing - refused by name, counted"
        );
    }

    // 3 — protection is absolute: with every candidate protected the round is refused
    //     `NothingEvictable` naming the counts, the seam is never called, the skips are counted.
    {
        let mut r = Reclaimer::without_model();
        let cands = [cand(1, 900, 0, 0, true), cand(2, 900, 0, 0, true)];
        let mut ops = MockOps::with(&cands);
        let out = r.reclaim(SUITE_PRESSURE, &cands, &mut ops);
        check!(
            out == Err(ReclaimRefusal::NothingEvictable {
                candidates: 2,
                protected: 2
            }) && ops.calls.is_empty()
                && r.ledger().protected_skipped == 2
                && r.ledger().refusals == 1,
            "reclaim: with nothing evictable the round is refused by name and no frame moves"
        );
    }

    // 4 — a protected candidate is never chosen even when it is the only way to meet the need:
    //     the round evicts what it may, reports the SHORTFALL, and counts it.
    {
        let mut r = Reclaimer::without_model();
        let cands = [cand(1, 5_000, 0, 0, true), cand(2, 600, 5, 0, false)];
        let mut ops = MockOps::with(&cands);
        let out = r.reclaim(SUITE_PRESSURE, &cands, &mut ops).unwrap();
        check!(
            out.need == 1_500
                && out.evicted == [TaskId(2)]
                && out.frames_reclaimed == 600
                && out.shortfall == 900
                && out.protected_skipped == 1
                && ops.calls == [(TaskId(2), Owner::address_space(2).unwrap())]
                && r.ledger().shortfalls == 1,
            "reclaim: a protected task is never taken, and the shortfall it causes is named, not hidden"
        );
    }

    // 5 — the round frees until the need is met and NOT further: three equal-tier candidates,
    //     the largest footprint first, and the round stops the moment the need is covered.
    {
        let mut r = Reclaimer::without_model();
        let cands = [
            cand(1, 400, 5, 0, false),
            cand(2, 1_600, 5, 0, false),
            cand(3, 800, 5, 0, false),
        ];
        let mut ops = MockOps::with(&cands);
        let out = r.reclaim(SUITE_PRESSURE, &cands, &mut ops).unwrap();
        check!(
            out.evicted == [TaskId(2)]
                && out.frames_reclaimed == 1_600
                && out.shortfall == 0
                && ops.calls.len() == 1,
            "reclaim: the largest footprint goes first and the round stops the moment the need is met"
        );
    }

    // 6 — the total order below the tier: footprint descending, then priority ascending (lowest
    //     priority first), then oldest submission, then task id — deterministic on every tie.
    {
        let mut r = Reclaimer::without_model();
        let cands = [
            cand(9, 100, 3, 50, false),
            cand(4, 100, 3, 50, false),
            cand(7, 100, 3, 10, false),
            cand(2, 100, 1, 99, false),
            cand(5, 200, 9, 99, false),
        ];
        let order: Vec<u64> = r.rank(&cands).iter().map(|x| x.candidate.task.0).collect();
        check!(
            order == [5, 2, 7, 4, 9],
            "reclaim: below the tier the order is footprint, then priority, then age, then id - total"
        );
    }

    // 7 — the forest ranks the TIER and nothing else: with the bundled forest, every adjacent
    //     pair in the ranking is tier-ordered, and within a tier the deterministic order holds;
    //     the same candidates ranked by a model-free reclaimer are in the same relative order
    //     within each tier. The vectors come from ADR-056's own derivation so the forest is asked
    //     about tasks shaped like the machine's.
    {
        let mut with = Reclaimer::load(BUNDLED_RECLAIM_MODEL);
        let mut without = Reclaimer::without_model();
        let mut src = crate::taskfeat::FeatureSource::new(crate::mlsched::SUITE_MACHINE);
        let mut cands = Vec::new();
        for i in 0..24u64 {
            let mut t = crate::taskfeat::TaskSubmission {
                sched_class: (i % 4) as u8,
                priority: (i % 12) as u8,
                cpu_millis: 50 + (i as u32 % 61) * 20,
                memory_pages: 1_024 + (i % 97) * 2_048,
                disk_pages: None,
                diff_machine: i % 7 == 0,
                task_index: i as u32,
                job: crate::taskfeat::JobId(i % 5),
                user: crate::taskfeat::UserId(i % 3),
            };
            if i % 11 == 0 {
                t.cpu_millis = 7_900;
                t.memory_pages = 1_000_000;
            }
            let features = src.observe_submit(i * 60, &t);
            let mut c = cand(i + 1, 64 + (i % 7) * 32, (i % 12) as u8, i * 60, false);
            c.features = features;
            cands.push(c);
        }
        let ranked = with.rank(&cands);
        let plain = without.rank(&cands);
        let tiers_monotone = ranked.windows(2).all(|w| w[0].tier <= w[1].tier);
        let mut within_tier_deterministic = true;
        for tier in [Tier::EvictionLikely, Tier::Unknown, Tier::CompletionLikely] {
            let a: Vec<u64> = ranked
                .iter()
                .filter(|x| x.tier == tier)
                .map(|x| x.candidate.task.0)
                .collect();
            let b: Vec<u64> = plain
                .iter()
                .filter(|x| a.contains(&x.candidate.task.0))
                .map(|x| x.candidate.task.0)
                .collect();
            within_tier_deterministic &= a == b;
        }
        let l = with.ledger();
        check!(
            ranked.len() == 24
                && plain.len() == 24
                && tiers_monotone
                && within_tier_deterministic
                && l.advised + l.unadvised == 24
                && without.ledger().unadvised == 24
                && ranked.iter().all(|x| x.advice.is_some())
                && plain.iter().all(|x| x.advice.is_none()),
            "reclaim: the forest sets the tier and nothing else - within a tier the model-free order holds"
        );
    }

    // 8 — the seam is asked exactly once per chosen task, in rank order, with the task's own
    //     owner; the ledger sums to what the rounds did.
    {
        let mut r = Reclaimer::without_model();
        let cands = [
            cand(1, 700, 5, 0, false),
            cand(2, 700, 2, 0, false),
            cand(3, 50, 0, 0, false),
        ];
        let mut ops = MockOps::with(&cands);
        let out = r.reclaim(SUITE_PRESSURE, &cands, &mut ops).unwrap();
        let l = r.ledger();
        check!(
            out.evicted == [TaskId(2), TaskId(1), TaskId(3)]
                && ops.calls
                    == [
                        (TaskId(2), Owner::address_space(2).unwrap()),
                        (TaskId(1), Owner::address_space(1).unwrap()),
                        (TaskId(3), Owner::address_space(3).unwrap())
                    ]
                && out.frames_reclaimed == 1_450
                && out.shortfall == 50
                && l.rounds == 1
                && l.evictions == 3
                && l.frames_reclaimed == 1_450
                && l.shortfalls == 1,
            "reclaim: the seam is asked once per chosen task in rank order and the ledger sums exactly"
        );
    }

    // 9 — determinism: two reclaimers, same forest, same meter, same candidates - same outcome,
    //     same ledger, same seam calls.
    {
        let run = || {
            let mut r = Reclaimer::load(BUNDLED_RECLAIM_MODEL);
            let cands = [
                cand(1, 300, 5, 0, false),
                cand(2, 900, 7, 3, false),
                cand(3, 900, 1, 9, true),
                cand(4, 450, 0, 1, false),
            ];
            let mut ops = MockOps::with(&cands);
            let out = r.reclaim(SUITE_PRESSURE, &cands, &mut ops).unwrap();
            (out, r.ledger(), ops.calls)
        };
        let a = run();
        let b = run();
        check!(
            a == b && a.0.protected_skipped == 1 && a.0.frames_reclaimed >= 1_500,
            "reclaim: the same pressure over the same tasks evicts the same tasks in the same order"
        );
    }

    Ok(n)
}

/// What the live storm reports (the targets print it, the gates read it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StormReport {
    pub free_before: u64,
    pub total: u64,
    pub taken: u64,
    pub free_at_pressure: u64,
    pub frames_reclaimed: u64,
    pub free_after: u64,
}

impl StormReport {
    /// The storm's verdict: pressure was really entered, the reclaim returned exactly what the
    /// storm took, and the machine is back to the frame it started with.
    pub fn holds(&self) -> bool {
        self.taken > 0
            && MemoryMeter {
                total_pages: self.total,
                free_pages: self.free_at_pressure,
            }
            .under_pressure()
            && self.frames_reclaimed == self.taken
            && self.free_after == self.free_before
    }
}
