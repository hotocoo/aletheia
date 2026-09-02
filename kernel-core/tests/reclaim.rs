//! Host proofs for reclaim under pressure (REQ-ML-005 wired, ADR-082). The boot suite is
//! host-run first; then the parts a boot cannot sweep: the need arithmetic at every edge, the
//! seam contract under a lying ops, the ledger across many rounds, and the model-free /
//! abstaining-forest identity.

use kernel_core::frameown::Owner;
use kernel_core::mlrisk_contract::N_FEATURES;
use kernel_core::mlsched::{MemoryMeter, LOW_WATERMARK_PERMILLE};
use kernel_core::priosched::Priority;
use kernel_core::reclaim::{
    reclaim_suite, Candidate, MockOps, ReclaimOps, ReclaimRefusal, Reclaimer, StormReport, Tier,
    BUNDLED_RECLAIM_MODEL, HEADROOM_FACTOR, SUITE_PRESSURE,
};
use kernel_core::sched::TaskId;

fn cand(id: u64, footprint: u64, prio: u8, at: u64, protected: bool) -> Candidate {
    Candidate {
        task: TaskId(id),
        owner: Owner::address_space(id as u32).unwrap(),
        footprint_pages: footprint,
        priority: Priority(prio),
        submitted_secs: at,
        protected,
        features: [0; N_FEATURES],
    }
}

#[test]
fn the_boot_suite_passes_on_the_host() {
    let mut seen = Vec::new();
    let n = reclaim_suite(|i, ok, name| seen.push((i, ok, name))).expect("every invariant holds");
    assert_eq!(n, 9);
    assert_eq!(seen.len(), 9);
    assert!(seen.iter().all(|(_, ok, _)| *ok));
    let names: Vec<&str> = seen.iter().map(|(_, _, n)| *n).collect();
    let mut dedup = names.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 9, "every invariant has its own name");
}

#[test]
fn the_need_is_the_shortfall_to_twice_the_watermark_and_zero_above_it() {
    let total = 10_000u64;
    let watermark = total * LOW_WATERMARK_PERMILLE / 1000; // 1_000
    let target = watermark * HEADROOM_FACTOR; // 2_000
                                              // Not under pressure: at the watermark exactly, and anywhere above.
    for free in [watermark, watermark + 1, target, total] {
        assert_eq!(
            Reclaimer::need(MemoryMeter {
                total_pages: total,
                free_pages: free
            }),
            0
        );
    }
    // Under pressure: exactly what is missing to the headroom target.
    assert_eq!(
        Reclaimer::need(MemoryMeter {
            total_pages: total,
            free_pages: watermark - 1
        }),
        target - (watermark - 1)
    );
    assert_eq!(
        Reclaimer::need(MemoryMeter {
            total_pages: total,
            free_pages: 0
        }),
        target
    );
}

#[test]
fn a_reclaimer_without_a_forest_ranks_like_one_whose_forest_abstains() {
    // Zero feature vectors are degenerate input, which the forest abstains on (ADR-056): the
    // tier is Unknown for every candidate, exactly as with no model at all.
    let cands: Vec<Candidate> = (1..=12u64)
        .map(|i| cand(i, 10 * (i % 5) + 1, (i % 4) as u8, i, false))
        .collect();
    let mut with = Reclaimer::load(BUNDLED_RECLAIM_MODEL);
    let mut without = Reclaimer::without_model();
    let a: Vec<(u64, Tier)> = with
        .rank(&cands)
        .iter()
        .map(|r| (r.candidate.task.0, r.tier))
        .collect();
    let b: Vec<(u64, Tier)> = without
        .rank(&cands)
        .iter()
        .map(|r| (r.candidate.task.0, r.tier))
        .collect();
    assert_eq!(a, b);
    assert!(a.iter().all(|(_, t)| *t == Tier::Unknown));
    assert_eq!(with.ledger().unadvised, 12);
    assert_eq!(with.ledger().advised, 0);
}

#[test]
fn a_refused_blob_is_named_and_the_reclaimer_still_reclaims() {
    let mut r = Reclaimer::load(&[0u8; 8]);
    assert!(!r.active());
    assert!(r.model_error().is_some(), "the refusal is named");
    let cands = [cand(1, 2_000, 5, 0, false)];
    let mut ops = MockOps::with(&cands);
    let out = r.reclaim(SUITE_PRESSURE, &cands, &mut ops).unwrap();
    assert_eq!(out.evicted, vec![TaskId(1)]);
    assert_eq!(out.shortfall, 0);
}

/// An ops seam that returns fewer frames than the candidate claimed - the policy must count
/// what came back, not what was promised, and keep evicting to cover the need.
struct Stingy;
impl ReclaimOps for Stingy {
    fn evict(&mut self, _task: TaskId, _owner: Owner) -> u64 {
        100
    }
}

#[test]
fn the_policy_counts_what_the_seam_returns_not_what_the_candidate_claimed() {
    let mut r = Reclaimer::without_model();
    let cands: Vec<Candidate> = (1..=20u64).map(|i| cand(i, 5_000, 5, i, false)).collect();
    let out = r.reclaim(SUITE_PRESSURE, &cands, &mut Stingy).unwrap();
    // need 1_500, 100 frames per eviction -> 15 evictions, no shortfall.
    assert_eq!(out.evicted.len(), 15);
    assert_eq!(out.frames_reclaimed, 1_500);
    assert_eq!(out.shortfall, 0);
    assert_eq!(r.ledger().evictions, 15);
}

#[test]
fn the_ledger_sums_across_rounds_and_refusals() {
    let mut r = Reclaimer::without_model();
    let cands = [cand(1, 800, 5, 0, false), cand(2, 800, 5, 0, false)];
    let mut ops = MockOps::with(&cands);
    assert!(r.reclaim(SUITE_PRESSURE, &cands, &mut ops).is_ok());
    assert_eq!(
        r.reclaim(
            MemoryMeter {
                total_pages: 10_000,
                free_pages: 9_000
            },
            &cands,
            &mut ops
        ),
        Err(ReclaimRefusal::NotUnderPressure {
            free_pages: 9_000,
            total_pages: 10_000
        })
    );
    assert_eq!(
        r.reclaim(SUITE_PRESSURE, &[], &mut ops),
        Err(ReclaimRefusal::NothingEvictable {
            candidates: 0,
            protected: 0
        })
    );
    let l = r.ledger();
    assert_eq!(
        (l.rounds, l.refusals, l.evictions, l.frames_reclaimed),
        (1, 2, 2, 1_600)
    );
    assert_eq!(l.shortfalls, 0);
}

#[test]
fn a_storm_report_holds_only_when_pressure_was_entered_and_every_frame_came_back() {
    let good = StormReport {
        free_before: 28_000,
        total: 28_000,
        taken: 25_500,
        free_at_pressure: 2_500,
        frames_reclaimed: 25_500,
        free_after: 28_000,
    };
    assert!(good.holds());
    assert!(
        !StormReport {
            free_at_pressure: 3_000,
            ..good
        }
        .holds(),
        "never entered the band"
    );
    assert!(
        !StormReport {
            frames_reclaimed: 25_499,
            ..good
        }
        .holds(),
        "a frame went missing"
    );
    assert!(
        !StormReport {
            free_after: 27_999,
            ..good
        }
        .holds(),
        "the machine did not come back to where it started"
    );
    assert!(
        !StormReport { taken: 0, ..good }.holds(),
        "a storm that took nothing proves nothing"
    );
}
