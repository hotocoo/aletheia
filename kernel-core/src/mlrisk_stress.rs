//! What the risk advisor costs, and what it actually changes, measured under load on the real target.
//!
//! [`mlrisk`](crate::mlrisk) proves the forest is *correct*: it verifies, it matches the trainer, it
//! refuses malformed blobs by name, and it never outranks priority. None of that answers the two
//! questions an operator asks before letting a model near a scheduler:
//!
//! 1. **What does it cost?** A bound in compares (`worst_case_compares`) is arithmetic, not a
//!    measurement. This module times [`RiskAdvisor::advise`] on the machine that will run it, in
//!    that machine's own nanoseconds, over a load large enough that a per-call cost is meaningful.
//! 2. **What does it change?** The advice is a tiebreak hint among equal-priority tasks (ADR-056),
//!    so the honest measure is not "is the model accurate" but *how often it is consulted at all,
//!    and how often it moves a task*. [`schedule_ab`] runs the same admission stream twice — once
//!    model-free, once advised — and reports the difference by observing both drains.
//!
//! Both are **reported, never gated**. Nanoseconds under QEMU-TCG are an emulator's numbers; making
//! a boot succeed or fail on them would be a flake generator. What [`stress_suite`] *does* gate on
//! is the part that must hold on any machine at any scale: the same input gives the same verdict
//! however many times it is asked, the census adds up, and the advised schedule is a permutation of
//! the model-free one — the model may reorder equals, and may not invent, drop or starve a task.
//!
//! Row generation is deliberate. Vectors are drawn *inside* the blob's own feature-range table, so
//! the range guard does not fire and the population is decisive; a separate slice is pushed
//! deliberately outside it to exercise abstention. A stress test built from uniform random i32s
//! would be 100% out-of-range, measure the guard instead of the forest, and report a scheduler
//! difference of exactly zero while looking like it had proved something.

use alloc::vec::Vec;

use crate::mlrisk::{parity_inputs, Advice, RiskAdvisor, Verdict};
use crate::mlrisk_contract::N_FEATURES;
use crate::priosched::{Priority, PriorityScheduler};
use crate::sched::TaskId;
use crate::Hal;

/// Deterministic SplitMix64. A stress result that changed run to run would not be evidence of
/// anything, and no target has an entropy source this early in boot anyway.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`, saturating rather than panicking on an inverted or empty range.
    fn in_range(&mut self, lo: i32, hi: i32) -> i32 {
        if hi <= lo {
            return lo;
        }
        let span = (hi as i64 - lo as i64 + 1) as u64;
        (lo as i64 + (self.next() % span) as i64) as i32
    }
}

/// A feature vector drawn from inside the blob's training box — the population the forest was fitted
/// on, and therefore the only one on which it has an opinion.
fn in_box_row(model: &RiskAdvisor<'_>, rng: &mut Rng) -> [i32; N_FEATURES] {
    let mut x = [0i32; N_FEATURES];
    for (i, slot) in x.iter_mut().enumerate() {
        let (lo, hi) = model.feature_range(i);
        *slot = rng.in_range(lo, hi);
    }
    x
}

/// The cost and the verdict census of `advices` calls on the running machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct AdviceStress {
    pub advices: usize,
    pub low: usize,
    pub abstain: usize,
    pub elevated: usize,
    pub out_of_range: usize,
    /// Wall time for the whole run, in the target's own nanoseconds.
    pub ns_total: u64,
    /// Cost of one advice, in picoseconds, so a sub-nanosecond result is still a number.
    pub ps_per_advice: u64,
    /// Abstentions attributable to the conformal band rather than to the range guard. Zero for a
    /// blob whose band shipped empty — which is a fact about that blob, not a failure here.
    pub band_abstain: usize,
    /// Sum of every margin produced, wrapped. Two runs that disagree here did different work — this
    /// is what makes "deterministic under load" checkable without keeping every margin.
    pub checksum: i64,
}

impl AdviceStress {
    /// Advices per second, derived from the measured total. Zero when the clock did not move (a
    /// target whose timer is too coarse for the load) rather than a division by zero.
    pub fn per_second(&self) -> u64 {
        if self.ns_total == 0 {
            return 0;
        }
        (self.advices as u64).saturating_mul(1_000_000_000) / self.ns_total
    }
}

/// Time `advices` calls to [`RiskAdvisor::advise`] over in-box rows, and census the verdicts.
///
/// `ood_every` forces every Nth row outside the training box, so the abstain path is exercised under
/// the same load rather than being a special case tested once. Pass 0 for an all-in-box run.
pub fn advice_stress<H: Hal>(
    model: &RiskAdvisor<'_>,
    advices: usize,
    ood_every: usize,
    seed: u64,
) -> AdviceStress {
    // Rows are generated BEFORE the clock starts: the measurement is of `advise`, not of the RNG.
    // Generating a bounded window and cycling it keeps the memory cost flat as `advices` grows into
    // the millions, which is what makes this runnable on a target with a bump allocator.
    let window = if advices < 1024 { advices.max(1) } else { 1024 };
    let mut rng = Rng(seed);
    let mut rows: Vec<[i32; N_FEATURES]> = Vec::with_capacity(window);
    for i in 0..window {
        let mut x = in_box_row(model, &mut rng);
        if ood_every != 0 && i % ood_every == 0 {
            x[0] = i32::MAX; // outside every plausible range table: the guard must fire
        }
        rows.push(x);
    }

    let mut out = AdviceStress {
        advices,
        ..Default::default()
    };
    let t0 = H::timer_ticks();
    for i in 0..advices {
        let a: Advice = model.advise(&rows[i % window]);
        out.checksum = out.checksum.wrapping_add(a.margin);
        if a.out_of_range {
            out.out_of_range += 1;
        }
        match a.verdict {
            Verdict::Low => out.low += 1,
            Verdict::Abstain => {
                out.abstain += 1;
                if !a.out_of_range {
                    out.band_abstain += 1;
                }
            }
            Verdict::Elevated => out.elevated += 1,
        }
    }
    let ticks = H::timer_ticks().wrapping_sub(t0);
    out.ns_total = H::ticks_to_ns(ticks);
    out.ps_per_advice = if advices == 0 {
        0
    } else {
        out.ns_total.saturating_mul(1_000) / advices as u64
    };
    out
}

/// The same admission stream scheduled twice: model-free, and advised.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScheduleAb {
    pub tasks: usize,
    /// Tasks admitted with a decisive verdict — the only ones the tiebreak can ever see.
    pub decisive: usize,
    pub abstained: usize,
    /// Positions at which the advised drain order differs from the model-free one. This is the
    /// number the whole feature is worth: zero means the model changed nothing on this workload.
    pub divergences: usize,
    /// Whether the advised order is a permutation of the model-free one — no task invented, dropped
    /// or starved. This is an invariant, not a metric, and [`stress_suite`] gates on it.
    pub same_multiset: bool,
    pub plain_ns: u64,
    pub advised_ns: u64,
}

impl ScheduleAb {
    /// Scheduling overhead attributable to the advice, in percent of the model-free drain. Negative
    /// costs are reported as 0 rather than as a speedup that timing noise invented.
    pub fn overhead_pct(&self) -> u64 {
        if self.plain_ns == 0 || self.advised_ns <= self.plain_ns {
            return 0;
        }
        (self.advised_ns - self.plain_ns).saturating_mul(100) / self.plain_ns
    }
}

fn drain<H: Hal>(sched: &mut PriorityScheduler) -> (Vec<u64>, u64) {
    let mut order = Vec::new();
    let t0 = H::timer_ticks();
    while let Some(t) = sched.schedule_next() {
        order.push(t.0);
        sched.finish(t);
    }
    let ns = H::ticks_to_ns(H::timer_ticks().wrapping_sub(t0));
    (order, ns)
}

/// Admit `tasks` tasks twice — once with [`PriorityScheduler::admit`], once with
/// [`PriorityScheduler::admit_with_advice`] — and compare the orders both drain in.
///
/// `priority_bands` controls how much of the workload is *eligible* for the tiebreak: with one band
/// every task ties with every other and the model has maximum room; with many bands, priority decides
/// most pairs and the model is consulted rarely. Both are realistic, and reporting one without the
/// other would be choosing the answer.
pub fn schedule_ab<H: Hal>(
    model: &RiskAdvisor<'_>,
    tasks: usize,
    priority_bands: u8,
    ood_every: usize,
    seed: u64,
) -> ScheduleAb {
    let mut rng = Rng(seed);
    let mut plain = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut advised = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut out = ScheduleAb {
        tasks,
        ..Default::default()
    };

    for i in 0..tasks {
        let id = TaskId(i as u64 + 1);
        let band: u8 = if priority_bands <= 1 {
            5
        } else {
            5 + (i % priority_bands as usize) as u8
        };
        let mut x = in_box_row(model, &mut rng);
        if ood_every != 0 && i % ood_every == 0 {
            x[0] = i32::MAX;
        }
        let a = model.advise(&x);
        if a.verdict.is_decisive() {
            out.decisive += 1;
        } else {
            out.abstained += 1;
        }
        // Identical admission order and identical priorities in both arms: the ONLY difference
        // between the two schedulers is whether the advice was passed in.
        plain.admit(id, Priority(band));
        advised.admit_with_advice(id, Priority(band), a);
    }

    let (plain_order, plain_ns) = drain::<H>(&mut plain);
    let (advised_order, advised_ns) = drain::<H>(&mut advised);
    out.plain_ns = plain_ns;
    out.advised_ns = advised_ns;
    out.divergences = plain_order
        .iter()
        .zip(advised_order.iter())
        .filter(|(a, b)| a != b)
        .count()
        + plain_order.len().abs_diff(advised_order.len());

    let mut a_sorted = plain_order.clone();
    let mut b_sorted = advised_order;
    a_sorted.sort_unstable();
    b_sorted.sort_unstable();
    out.same_multiset = a_sorted == b_sorted;
    out
}

/// The scheduler A/B over the trainer's OWN held-out rows, cycled to `tasks`.
///
/// This is the arm that answers the operator's question. The synthetic arms prove the *properties*
/// hold under load; only real rows carry the joint distribution between features that decides
/// whether a Low and an Elevated task ever tie in the first place. Where the synthetic arms report
/// zero movement, that is a fact about uniform sampling inside a 20-dimensional box; where this one
/// does, it is a fact about the model.
pub fn schedule_ab_real<H: Hal>(
    model: &RiskAdvisor<'_>,
    tasks: usize,
    priority_bands: u8,
) -> ScheduleAb {
    let rows = parity_inputs();
    let mut plain = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut advised = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut out = ScheduleAb {
        tasks,
        ..Default::default()
    };
    if rows.is_empty() {
        return out;
    }
    for i in 0..tasks {
        let id = TaskId(i as u64 + 1);
        let band: u8 = if priority_bands <= 1 {
            5
        } else {
            5 + (i % priority_bands as usize) as u8
        };
        let a = model.advise(&rows[i % rows.len()]);
        if a.verdict.is_decisive() {
            out.decisive += 1;
        } else {
            out.abstained += 1;
        }
        plain.admit(id, Priority(band));
        advised.admit_with_advice(id, Priority(band), a);
    }
    let (plain_order, plain_ns) = drain::<H>(&mut plain);
    let (advised_order, advised_ns) = drain::<H>(&mut advised);
    out.plain_ns = plain_ns;
    out.advised_ns = advised_ns;
    out.divergences = plain_order
        .iter()
        .zip(advised_order.iter())
        .filter(|(a, b)| a != b)
        .count()
        + plain_order.len().abs_diff(advised_order.len());
    let mut a_sorted = plain_order;
    let mut b_sorted = advised_order;
    a_sorted.sort_unstable();
    b_sorted.sort_unstable();
    out.same_multiset = a_sorted == b_sorted;
    out
}

/// What one target measured, so the caller prints it with its own console.
#[derive(Clone, Copy, Debug, Default)]
pub struct StressReport {
    pub hot: AdviceStress,
    pub mixed: AdviceStress,
    /// One priority band: every task ties, so the tiebreak has maximum room.
    pub tied: ScheduleAb,
    /// Eight priority bands: priority decides most pairs, as it does in a real workload.
    pub banded: ScheduleAb,
    /// Every task out of box, so every verdict abstains: the ADR-056 fallback, at scale.
    pub quiet: ScheduleAb,
    /// The trainer's own held-out rows, all tied: what the model does to a REAL workload.
    pub real: ScheduleAb,
}

/// Seed of the all-in-box advice arm. Exported because the determinism check re-runs exactly that
/// arm, and a re-run with a different seed would compare two different workloads.
pub const HOT_SEED: u64 = 0xA1E7_4E1A;

/// The default boot-time load. Deliberately bounded: `schedule_next` is a linear scan over the ready
/// queue, so draining N tasks is O(N²) and a "massive" task count would hang a TCG-emulated riscv64
/// boot rather than measure it. The *advice* path is O(1) per call and is where the large number
/// goes; the host test (`tests/mlrisk_stress.rs`) takes both an order of magnitude further on real
/// hardware with a real allocator.
/// Sized against the tightest constraint on any target, which is NOT time — it is memory. Every
/// kernel target allocates from a bump allocator that never frees (`kernel/src/heap.rs`), so the
/// `BTreeMap` churn of eight schedulers and the row windows are permanently retained for the rest
/// of the boot. A load that fits comfortably on the host will exhaust an 8 MiB kernel heap and
/// panic the machine — which is exactly what the first run of this gate did, and why these numbers
/// are small and the host test carries the scale instead.
///
/// Sized against the SLOWEST gate too: riscv64 boots under QEMU-TCG, where every
/// advice is interpreted, and the boot scripts carry a watchdog. `stress_suite` runs the whole
/// measurement twice (check 4 is "the same load twice"), and `measure` runs four scheduler arms, so
/// the real boot cost is roughly 2 x (advices + advices/4 + 8 drains). These numbers keep that
/// inside a few seconds of emulated time while still being four orders of magnitude more advice
/// calls than the invariant suite makes. The host test takes the same harness to millions.
pub const BOOT_ADVICES: usize = 4_000;
/// Tasks per scheduler arm at boot. Four arms, two schedulers each, and `schedule_next` is a linear
/// scan — so this is O(N²) eight times over and cannot be raised carelessly.
pub const BOOT_TASKS: usize = 128;

/// Measure the advisor under load on the running machine, and gate on what must hold at any scale.
///
/// `Ok((report, n))` = all `n` checks passed, with the numbers to print. `Err((idx, name))` = a
/// scale-invariant property broke. Timing is in the report and is never a pass/fail condition.
/// `run` is the target's own `|advices, tasks| measure::<ItsHal>(&model, advices, tasks)`: the core
/// owns the checks and their names, each target owns its clock. It is called more than once, on
/// purpose — checks 4 and 7 are about repeating the same load.
pub fn stress_suite(
    advices: usize,
    tasks: usize,
    run: impl Fn(usize, usize) -> StressReport,
    rerun_hot: impl Fn(usize) -> AdviceStress,
    mut report: impl FnMut(u32, bool, &'static str),
) -> Result<(StressReport, u32), (u32, &'static str)> {
    let r = run(advices, tasks);
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

    // 1 — the load actually ran. A census that does not add up means rows were skipped somewhere.
    check!(
        r.hot.advices == advices && r.hot.low + r.hot.abstain + r.hot.elevated == advices,
        "mlrisk-stress: every advice under load is accounted for in the verdict census"
    );

    // 2 — the range guard does not fire on rows drawn from inside the range table. If it did, the
    // benchmark would be measuring the guard rather than the forest, and the scheduler comparison
    // below would be vacuous. Note what this does NOT assert: in-box rows may still abstain via the
    // conformal band, and whether they do is a property of the installed blob, not of the kernel.
    check!(
        r.hot.out_of_range == 0,
        "mlrisk-stress: the range guard does not fire on rows drawn from inside the range table"
    );

    // 3 — and rows pushed outside it abstain, at the rate they were injected: the guard is load-
    // bearing under load, not only in the single-row check the invariant suite makes.
    check!(
        r.mixed.out_of_range > 0 && r.mixed.abstain >= r.mixed.out_of_range,
        "mlrisk-stress: out-of-box rows still abstain when they arrive at rate"
    );

    // 4 — determinism AT SCALE. Same seed, same window, same sum of margins. A tiebreak that drifted
    // between two runs of the same workload would make the machine irreproducible.
    //
    // Only the advice arm is repeated, deliberately: a kernel target allocates from a bump allocator
    // that never frees, so re-running the four scheduler arms would retain a second copy of their
    // `BTreeMap` churn and exhaust the heap. It did exactly that on the first aarch64 boot of this
    // gate. The scheduler arms are covered by checks 5-8 on the run that already happened.
    let again = rerun_hot(advices);
    check!(
        again.checksum == r.hot.checksum
            && again.low == r.hot.low
            && again.elevated == r.hot.elevated,
        "mlrisk-stress: the same load produces the same margins and the same census, every time"
    );

    // 5/6 — the advised schedule is a PERMUTATION of the model-free one, in both the all-ties and
    // the banded workload. The model may reorder equals; it may not invent, drop or starve a task.
    check!(
        r.tied.same_multiset && r.tied.tasks == tasks,
        "mlrisk-stress: with every task tied, the advised schedule runs exactly the same tasks"
    );
    check!(
        r.banded.same_multiset,
        "mlrisk-stress: with priority bands, the advised schedule runs exactly the same tasks"
    );

    // 7 — the fallback, at scale: a workload the model abstains on everywhere drains in EXACTLY the
    // model-free order. Not "similar" — identical, position by position. This is the property that
    // makes the model safe to ship at all, and it is asserted on thousands of tasks rather than on
    // the eight rows the invariant suite can afford.
    //
    // Note what is deliberately NOT asserted: a bound on `divergences` for the decisive arms. One
    // swap moves two positions and can cascade, so any such bound would be a guess dressed as an
    // invariant. What bounds the damage is check 5/6 (same multiset) plus priority dominance, which
    // `tests/mlrisk_stress.rs` asserts directly.
    check!(
        r.quiet.decisive == 0 && r.quiet.divergences == 0 && r.quiet.same_multiset,
        "mlrisk-stress: a workload the model abstains on drains in exactly the model-free order"
    );

    // 8 — the real-row arm ran on real rows and left the task set intact. Its `divergences` is a
    // MEASUREMENT, not a threshold: a model that happens to agree with FIFO on this workload is not
    // broken, and gating on "the model must change something" would be gating on it being wrong
    // whenever it is right.
    check!(
        r.real.tasks == tasks && r.real.same_multiset,
        "mlrisk-stress: on the trainer's own held-out rows, the advised schedule runs the same tasks"
    );

    Ok((r, n))
}

/// The default measurement: two advice loads (all-in-box, and one row in eight out of box) and two
/// scheduler A/B runs (all tied, and eight priority bands). Generic over `Hal` so each target times
/// it with its own clock.
pub fn measure<H: Hal>(model: &RiskAdvisor<'_>, advices: usize, tasks: usize) -> StressReport {
    StressReport {
        hot: advice_stress::<H>(model, advices, 0, HOT_SEED),
        mixed: advice_stress::<H>(model, advices / 4, 8, 0xA1E7_4E1B),
        tied: schedule_ab::<H>(model, tasks, 1, 0, 0xA1E7_4E1C),
        banded: schedule_ab::<H>(model, tasks, 8, 0, 0xA1E7_4E1D),
        quiet: schedule_ab::<H>(model, tasks, 1, 1, 0xA1E7_4E1E),
        real: schedule_ab_real::<H>(model, tasks, 1),
    }
}
