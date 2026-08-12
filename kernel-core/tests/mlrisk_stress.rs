//! The risk advisor under load, on the host: the same harness the three targets run at boot, taken
//! an order of magnitude further because a host has a real allocator, a real clock and no emulator.
//!
//! The VM gate proves the properties hold on the real CPU at a bounded load that a TCG-emulated
//! riscv64 can finish. This proves they still hold when the load is large enough that a per-call
//! cost is a measurement rather than a rounding error, and it prints the numbers — with `--nocapture`
//! — so "what does the model cost" has an answer taken from a machine rather than from a parameter.
//!
//! Nothing here asserts a timing. Timings are printed; the assertions are the scale-invariant
//! properties, exactly as in `mlrisk_stress::stress_suite`.

use std::time::Instant;

use kernel_core::mlrisk::{RiskAdvisor, Verdict, BUNDLED_MODEL};
use kernel_core::mlrisk_stress::{
    advice_stress, measure, schedule_ab, stress_suite, AdviceStress, StressReport, HOT_SEED,
};
use kernel_core::Hal;

/// A host `Hal` that only implements what the stress harness uses. Everything else is unreachable:
/// this is a measurement backend, not a machine.
struct HostHal;

impl Hal for HostHal {
    fn arch_name() -> &'static str {
        "host"
    }
    fn timer_ticks() -> u64 {
        // Nanoseconds since a fixed process-local origin, so `ticks_to_ns` is the identity and no
        // frequency calibration is involved in the number this test prints.
        use std::sync::OnceLock;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
    fn timer_freq_hz() -> u64 {
        1_000_000_000
    }
    fn ticks_to_ns(ticks: u64) -> u64 {
        ticks
    }
    fn current_privilege() -> u64 {
        0
    }
    fn exit(code: i32) -> ! {
        std::process::exit(code)
    }
}

fn advisor() -> RiskAdvisor<'static> {
    RiskAdvisor::load(BUNDLED_MODEL).expect("the bundled blob must verify")
}

fn run(advices: usize, tasks: usize) -> StressReport {
    measure::<HostHal>(&advisor(), advices, tasks)
}

/// Debug builds walk 171 trees per advice through an unoptimised loop; a million advices there is
/// minutes of nothing. The large load is the release number, and the debug run stays a correctness
/// run at a load that still exercises every path.
const ADVICES: usize = if cfg!(debug_assertions) {
    100_000
} else {
    2_000_000
};
const TASKS: usize = if cfg!(debug_assertions) { 1_000 } else { 8_000 };

#[test]
fn the_stress_suite_holds_on_the_host_at_scale() {
    let mut seen = 0u32;
    let rerun_hot =
        |a: usize| -> AdviceStress { advice_stress::<HostHal>(&advisor(), a, 0, HOT_SEED) };
    let (r, n) = stress_suite(ADVICES, TASKS, run, rerun_hot, |i, passed, name| {
        seen += 1;
        assert_eq!(seen, i, "checks must be reported once each, in order");
        println!(
            "  [{} {:>2}] {}",
            if passed { "pass" } else { "FAIL" },
            i,
            name
        );
    })
    .expect("every scale-invariant property must hold");
    assert_eq!(seen, n);

    println!(
        "[mlrisk-stress] {} advices in {} ns => {} ps/advice, {} advices/s",
        r.hot.advices,
        r.hot.ns_total,
        r.hot.ps_per_advice,
        r.hot.per_second()
    );
    println!(
        "[mlrisk-stress] in-box census: {} low / {} elevated / {} abstain (of which {} from the conformal band)",
        r.hot.low, r.hot.elevated, r.hot.abstain, r.hot.band_abstain
    );
    println!(
        "[mlrisk-stress] mixed census: {} out-of-box of {} => {} abstain",
        r.mixed.out_of_range, r.mixed.advices, r.mixed.abstain
    );
    println!(
        "[mlrisk-stress] schedule all-tied  : {} tasks, {} decisive, {} positions move, plain {} ns vs advised {} ns (+{}%)",
        r.tied.tasks,
        r.tied.decisive,
        r.tied.divergences,
        r.tied.plain_ns,
        r.tied.advised_ns,
        r.tied.overhead_pct()
    );
    println!(
        "[mlrisk-stress] schedule REAL rows : {} tasks, {} decisive, {} abstain, {} positions move, plain {} ns vs advised {} ns (+{}%)",
        r.real.tasks,
        r.real.decisive,
        r.real.abstained,
        r.real.divergences,
        r.real.plain_ns,
        r.real.advised_ns,
        r.real.overhead_pct()
    );
    println!(
        "[mlrisk-stress] schedule 8 bands   : {} tasks, {} decisive, {} positions move, plain {} ns vs advised {} ns (+{}%)",
        r.banded.tasks,
        r.banded.decisive,
        r.banded.divergences,
        r.banded.plain_ns,
        r.banded.advised_ns,
        r.banded.overhead_pct()
    );
}

#[test]
fn a_workload_the_model_abstains_on_schedules_identically_to_the_model_free_kernel() {
    // `ood_every = 1` puts every task outside the training box, so every verdict is Abstain. This is
    // the ADR-056 fallback asserted at scale rather than on the eight rows the invariant suite uses:
    // a model with nothing to say must cost the schedule nothing at all.
    let m = advisor();
    let r = schedule_ab::<HostHal>(&m, TASKS, 1, 1, 0xDEAD_BEEF);
    assert_eq!(r.decisive, 0, "every out-of-box task must abstain");
    assert_eq!(
        r.divergences, 0,
        "an abstaining model must not move a single position"
    );
    assert!(r.same_multiset);
}

#[test]
fn advice_never_reorders_across_priority_bands_however_large_the_load() {
    // The property INV-014 rests on, at a load where a broken comparator would have thousands of
    // chances to show itself: within the advised drain, priority must be non-increasing.
    use kernel_core::priosched::{Priority, PriorityScheduler};
    use kernel_core::sched::TaskId;

    let m = advisor();
    let mut sched = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut want: Vec<(u64, u8)> = Vec::new();
    let mut x = [0i32; kernel_core::mlrisk_contract::N_FEATURES];
    for i in 0..TASKS {
        for (f, slot) in x.iter_mut().enumerate() {
            let (lo, hi) = m.feature_range(f);
            *slot =
                lo.saturating_add(((i as i64 * 2654435761) % (hi as i64 - lo as i64 + 1)) as i32);
        }
        let band = 1 + (i % 9) as u8;
        sched.admit_with_advice(TaskId(i as u64 + 1), Priority(band), m.advise(&x));
        want.push((i as u64 + 1, band));
    }

    let mut last = u8::MAX;
    let mut drained = 0usize;
    while let Some(t) = sched.schedule_next() {
        let band = want
            .iter()
            .find(|(id, _)| *id == t.0)
            .expect("known task")
            .1;
        assert!(
            band <= last,
            "a lower-priority task ran before a higher-priority one: risk outranked priority"
        );
        last = band;
        sched.finish(t);
        drained += 1;
    }
    assert_eq!(drained, TASKS, "every admitted task must eventually run");
}

#[test]
fn the_verdict_census_matches_what_the_advisor_says_row_by_row() {
    // The stress counters are a summary; a summary that drifted from the thing it summarises would
    // make every number above meaningless. Recount a sample the slow, obvious way.
    let m = advisor();
    let mut low = 0usize;
    let mut elevated = 0usize;
    let mut abstain = 0usize;
    let mut x = [0i32; kernel_core::mlrisk_contract::N_FEATURES];
    for i in 0..2_000i64 {
        for (f, slot) in x.iter_mut().enumerate() {
            let (lo, hi) = m.feature_range(f);
            *slot = lo.saturating_add(
                ((i * (f as i64 + 7) * 2654435761) % (hi as i64 - lo as i64 + 1)) as i32,
            );
        }
        match m.advise(&x).verdict {
            Verdict::Low => low += 1,
            Verdict::Elevated => elevated += 1,
            Verdict::Abstain => abstain += 1,
        }
    }
    assert_eq!(low + elevated + abstain, 2_000);
    // No claim that in-box rows are decisive: an abstention here is the conformal band, which is a
    // property of the installed blob, not of the kernel. The claim is that the range guard stayed
    // silent on rows that were built inside the range table.
    assert!(
        !m.advise(&x).out_of_range,
        "a row built inside the range table must not trip the range guard"
    );
}
