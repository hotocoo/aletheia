//! Hosted proofs for the in-kernel risk advisor (ADR-056).
//!
//! Three things are asserted here, and the third is the one that protects the kernel:
//!
//! 1. **Refusals are named.** Every way a blob can be wrong — magic, version, feature count,
//!    contract hash, fixed-point scale, truncation, a child index that points backwards — produces a
//!    specific [`ModelError`], never a silent "advise nothing".
//! 2. **Parity with the trainer.** The committed fixture carries margins computed by the exporter's
//!    integer reference path in Python. This test recomputes them in Rust and requires **exact**
//!    equality, plus identical three-way verdicts. An exporter that drifts fails here instead of
//!    quietly changing a scheduler's mind in production.
//! 3. **The model cannot change a schedule it has no opinion about.** With no model loaded, and with
//!    an abstaining model, `PriorityScheduler` produces the identical order to the model-free
//!    kernel — asserted, not assumed (INV-014).
use kernel_core::mlrisk::{ModelError, RiskAdvisor, Verdict};
use kernel_core::mlrisk_contract::N_FEATURES;
use kernel_core::priosched::{Priority, PriorityScheduler};
use kernel_core::sched::TaskId;

/// The load result, reduced to its error for comparison: `RiskAdvisor` borrows bytes and has no
/// reason to implement `PartialEq`, but every refusal path must still be checkable by name.
fn err_of(r: Result<RiskAdvisor<'_>, ModelError>) -> Option<ModelError> {
    r.err()
}

const BLOB: &[u8] = include_bytes!("../models/aletheia_risk.altm");
const FIXTURE: &str = include_str!("../models/parity_fixture.tsv");

fn advisor() -> RiskAdvisor<'static> {
    RiskAdvisor::load(BLOB).expect("the committed blob must load")
}

struct Row {
    x: [i32; N_FEATURES],
    margin: i64,
    class: &'static str,
    out_of_range: bool,
}

fn fixture_rows() -> Vec<Row> {
    FIXTURE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|line| {
            let mut f = line.split_whitespace();
            let margin: i64 = f.next().unwrap().parse().unwrap();
            let class: &'static str = match f.next().unwrap() {
                "low" => "low",
                "abstain" => "abstain",
                "elevated" => "elevated",
                other => panic!("unknown class {other}"),
            };
            let out_of_range = f.next().unwrap() == "1";
            let mut x = [0i32; N_FEATURES];
            for slot in x.iter_mut() {
                *slot = f.next().expect("fixture row is short").parse().unwrap();
            }
            assert!(f.next().is_none(), "fixture row has extra columns");
            Row {
                x,
                margin,
                class,
                out_of_range,
            }
        })
        .collect()
}

#[test]
fn committed_blob_loads_and_is_bounded() {
    let m = advisor();
    assert!(m.trees() > 0 && m.nodes() > m.trees());
    // A scheduler may only call this on a hot path if the bound is known, so the bound is measured
    // from the table rather than taken on faith from a training parameter.
    let bound = m.worst_case_compares();
    assert!(bound > 0 && bound < 100_000, "unbounded forest: {bound}");
}

#[test]
fn margins_match_the_trainer_exactly() {
    let m = advisor();
    let rows = fixture_rows();
    assert!(rows.len() >= 64, "fixture too small to be evidence");
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(m.margin(&r.x), r.margin, "margin drift at fixture row {i}");
        let advice = m.advise(&r.x);
        assert_eq!(advice.margin, r.margin);
        assert_eq!(
            advice.out_of_range, r.out_of_range,
            "range guard differs at row {i}"
        );
        let got = match advice.verdict {
            Verdict::Low => "low",
            Verdict::Abstain => "abstain",
            Verdict::Elevated => "elevated",
        };
        assert_eq!(got, r.class, "verdict drift at fixture row {i}");
    }
}

#[test]
fn evaluation_is_deterministic() {
    let m = advisor();
    let rows = fixture_rows();
    for r in rows.iter().take(32) {
        let first = m.margin(&r.x);
        for _ in 0..8 {
            assert_eq!(m.margin(&r.x), first);
        }
    }
}

#[test]
fn every_malformed_blob_is_a_named_refusal() {
    assert_eq!(err_of(RiskAdvisor::load(&[])), Some(ModelError::TooShort));
    assert_eq!(
        err_of(RiskAdvisor::load(&[0u8; 87])),
        Some(ModelError::TooShort)
    );

    let mut bad = BLOB.to_vec();
    bad[0] = b'X';
    assert_eq!(err_of(RiskAdvisor::load(&bad)), Some(ModelError::BadMagic));

    let mut bad = BLOB.to_vec();
    bad[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert_eq!(
        err_of(RiskAdvisor::load(&bad)),
        Some(ModelError::UnsupportedVersion(99))
    );

    let mut bad = BLOB.to_vec();
    bad[8..12].copy_from_slice(&7u32.to_le_bytes());
    assert_eq!(
        err_of(RiskAdvisor::load(&bad)),
        Some(ModelError::FeatureCount {
            expected: N_FEATURES,
            found: 7
        })
    );

    let mut bad = BLOB.to_vec();
    bad[20..24].copy_from_slice(&11u32.to_le_bytes());
    assert!(matches!(
        err_of(RiskAdvisor::load(&bad)),
        Some(ModelError::LeafScale { .. })
    ));

    // Same shape, different feature MEANINGS: exactly the case a byte-length check cannot catch.
    let mut bad = BLOB.to_vec();
    bad[56] ^= 0xFF;
    assert_eq!(
        err_of(RiskAdvisor::load(&bad)),
        Some(ModelError::ContractMismatch)
    );

    let mut bad = BLOB.to_vec();
    bad.truncate(BLOB.len() - 4);
    assert_eq!(err_of(RiskAdvisor::load(&bad)), Some(ModelError::Truncated));

    let mut bad = BLOB.to_vec();
    bad.extend_from_slice(&[0, 0, 0, 0]);
    assert_eq!(err_of(RiskAdvisor::load(&bad)), Some(ModelError::Truncated));

    let mut bad = BLOB.to_vec();
    bad[12..16].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        err_of(RiskAdvisor::load(&bad)),
        Some(ModelError::EmptyForest)
    );

    // A backwards child edge would let evaluation loop forever; refuse the blob instead of hanging.
    let nodes_off = 88 + 8 * N_FEATURES + 4 * advisor().trees();
    let mut bad = BLOB.to_vec();
    let mut node = nodes_off;
    while i32::from_le_bytes([bad[node], bad[node + 1], bad[node + 2], bad[node + 3]]) == -1 {
        node += 16;
    }
    bad[node + 8..node + 12].copy_from_slice(&0i32.to_le_bytes());
    assert_eq!(err_of(RiskAdvisor::load(&bad)), Some(ModelError::BadIndex));
}

#[test]
fn out_of_range_inputs_abstain_instead_of_extrapolating() {
    let m = advisor();
    let mut x = fixture_rows()[0].x;
    x[0] = i32::MAX; // far outside any training range
    let advice = m.advise(&x);
    assert!(advice.out_of_range);
    assert_eq!(advice.verdict, Verdict::Abstain);
}

// --------------------------------------------------------------------------
// The invariant that matters: advice may reorder equals, and nothing else.
// --------------------------------------------------------------------------
fn drain(sched: &mut PriorityScheduler) -> Vec<u64> {
    let mut order = Vec::new();
    while let Some(t) = sched.schedule_next() {
        order.push(t.0);
        sched.finish(t);
    }
    order
}

#[test]
fn advice_absent_matches_model_free_order() {
    let mut plain = PriorityScheduler::new("kernel.endpoint.acquire");
    let mut advised = PriorityScheduler::new("kernel.endpoint.acquire");
    let m = advisor();
    let rows = fixture_rows();
    for (i, r) in rows.iter().take(8).enumerate() {
        let id = TaskId(i as u64 + 1);
        plain.admit(id, Priority(5));
        // Same priorities, but every task carries an ABSTAIN verdict: no opinion must mean no change.
        let mut x = r.x;
        x[0] = i32::MAX; // force the range guard, hence Abstain
        advised.admit_with_advice(id, Priority(5), m.advise(&x));
    }
    assert_eq!(drain(&mut plain), drain(&mut advised));
}

#[test]
fn decisive_advice_reorders_only_within_equal_priority() {
    let m = advisor();
    let rows = fixture_rows();
    let low = rows
        .iter()
        .find(|r| r.class == "low")
        .expect("fixture must contain a low-risk row");
    let elevated = rows
        .iter()
        .find(|r| r.class == "elevated")
        .expect("fixture must contain an elevated-risk row");

    // Equal priority, elevated admitted FIRST: the advice moves the low-risk task ahead of it.
    let mut s = PriorityScheduler::new("kernel.endpoint.acquire");
    s.admit_with_advice(TaskId(1), Priority(5), m.advise(&elevated.x));
    s.admit_with_advice(TaskId(2), Priority(5), m.advise(&low.x));
    assert_eq!(s.risk_of(TaskId(1)), Some(Verdict::Elevated));
    assert_eq!(s.risk_of(TaskId(2)), Some(Verdict::Low));
    assert_eq!(drain(&mut s), vec![2, 1]);

    // Higher priority + elevated risk still beats lower priority + low risk: priority is never
    // traded away for risk.
    let mut s = PriorityScheduler::new("kernel.endpoint.acquire");
    s.admit_with_advice(TaskId(1), Priority(9), m.advise(&elevated.x));
    s.admit_with_advice(TaskId(2), Priority(5), m.advise(&low.x));
    assert_eq!(drain(&mut s), vec![1, 2]);
}

// ---------------------------------------------------------------------------
// The in-kernel suite, run on the host — same doctrine as `tests/invariants.rs`.
// ---------------------------------------------------------------------------

#[test]
fn the_in_kernel_suite_holds_on_the_host_and_reports_every_check_once() {
    let mut reported: Vec<(u32, bool, &'static str)> = Vec::new();
    let outcome =
        kernel_core::mlrisk::mlrisk_suite(|n, passed, name| reported.push((n, passed, name)));
    let count = match outcome {
        Ok(n) => n,
        Err((idx, name)) => panic!("in-kernel risk-advisor invariant {idx} failed: {name}"),
    };
    assert_eq!(reported.len() as u32, count);
    for (i, (n, passed, _)) in reported.iter().enumerate() {
        assert_eq!(*n, i as u32 + 1);
        assert!(passed);
    }
    // Pinned: the boot gates grep for this number.
    assert_eq!(count, 20);
}

#[test]
fn the_bundled_model_is_the_one_the_hosted_tests_verify() {
    // The kernel image embeds `BUNDLED_MODEL`; these tests read the file. If those two ever diverge
    // the VM gate would be proving something about bytes no test here has seen.
    assert_eq!(kernel_core::mlrisk::BUNDLED_MODEL, BLOB);
}
