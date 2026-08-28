//! Host-exhaustive proofs of the Lethe advisor (REQ-ML-006, ADR-077).
//!
//! The in-kernel `lethe_suite` proves the core promises at boot on every target on a fixed
//! platform; these tests are the EXHAUSTIVE sweeps the boot heap cannot afford (ADR-063):
//! every named load refusal by mutation, parity with the trainer over the whole committed
//! fixture, the baseline-equivalence and abstain-equivalence sweeps over randomized traces,
//! the never-past-nominal sweep with grants minted, park legality with exact idle accounting,
//! observer bounds, engine-level determinism, and ledger monotonicity under wraparound.
//! Timings are REPORTED, never gated (the mlrisk_stress posture).

use kernel_core::lethe::*;
use kernel_core::lethe_contract::{FEATURE_CONTRACT, FEATURE_DOMAIN, FEATURE_NAMES, N_FEATURES};
use kernel_core::pm::{IdleState, OperatingPoint, PmEngine};

// ---------------------------------------------------------------------------
// A deterministic xorshift so every sweep is reproducible (the repo has no external crates).
// ---------------------------------------------------------------------------
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn ladder(khz: &[u32]) -> Vec<OperatingPoint> {
    khz.iter()
        .enumerate()
        .map(|(i, k)| OperatingPoint {
            khz: *k,
            mv: 700 + 50 * i as u16,
        })
        .collect()
}

/// The standard platform: 3 governor rungs + a 2-rung OC band (top == envelope), trip 95 C.
fn platform(secret: u64) -> PmEngine {
    let mut pm = PmEngine::new(secret);
    pm.register_domain(
        1,
        &ladder(&[800_000, 1_200_000, 2_000_000, 2_400_000, 2_800_000]),
        2_000_000,
        2_800_000,
        95_000,
    )
    .unwrap();
    pm.register_domain(
        2,
        &ladder(&[500_000, 1_000_000]),
        1_000_000,
        1_000_000,
        95_000,
    )
    .unwrap();
    pm
}

/// A randomized multi-regime demand trace: idle, steady, bursty, staccato, sawtooth.
fn random_trace(rng: &mut Lcg, len: usize) -> Vec<u8> {
    let regime = rng.below(5);
    let mut out = Vec::with_capacity(len);
    match regime {
        0 => {
            for i in 0..len {
                out.push(if rng.below(16) == 0 {
                    rng.below(15) as u8
                } else {
                    0
                });
                let _ = i;
            }
        }
        1 => {
            let mut lvl = 30 + rng.below(60) as i32;
            for _ in 0..len {
                lvl = (lvl + rng.below(13) as i32 - 6).clamp(0, 100);
                out.push(lvl as u8);
            }
        }
        2 => {
            let mut i = 0;
            while i < len {
                let burst = 8 + rng.below(32) as usize;
                let mut lvl = 55 + rng.below(45) as i32;
                for _ in 0..burst.min(len - i) {
                    lvl = (lvl + rng.below(19) as i32 - 9).clamp(40, 100);
                    out.push(lvl as u8);
                    i += 1;
                }
                let dip = 2 + rng.below(10) as usize;
                for _ in 0..dip.min(len - i) {
                    out.push(rng.below(21) as u8);
                    i += 1;
                }
            }
        }
        3 => {
            let period = 4 + rng.below(4) as usize;
            let (hi, lo) = (85 + rng.below(15) as u8, 5 + rng.below(20) as u8);
            for i in 0..len {
                out.push(if (i / period).is_multiple_of(2) {
                    hi
                } else {
                    lo
                });
            }
        }
        _ => {
            let period = 20 + rng.below(40) as usize;
            for i in 0..len {
                out.push((100 * (i % period) / period) as u8);
            }
        }
    }
    out
}

/// One closed-loop advised run; returns the per-step state snapshots.
fn run_advised(
    pm: &mut PmEngine,
    advisor: Option<&Advisor>,
    trace_a: &[u8],
    trace_b: &[u8],
    seed_secret: u64,
) -> (Vec<Vec<(u32, u32)>>, GovernReport) {
    let mut obs = PmObserver::new();
    let mut states = Vec::new();
    let mut total = GovernReport::default();
    for t in 0..trace_a.len() as u64 {
        pm.set_demand(1, trace_a[t as usize]).unwrap();
        pm.set_demand(2, trace_b[t as usize]).unwrap();
        let rep = govern_advised(pm, advisor, &mut obs, t, |dom| {
            if dom == 1 {
                45_000 + (t as i32 % 40) * 500
            } else {
                60_000
            }
        });
        total.steps += rep.steps;
        total.consultations += rep.consultations;
        total.decisive += rep.decisive;
        total.abstains += rep.abstains;
        total.out_of_range += rep.out_of_range;
        total.degenerate += rep.degenerate;
        total.moves += rep.moves;
        total.parks += rep.parks;
        total.wakes += rep.wakes;
        total.pm_refusals += rep.pm_refusals;
        states.push(pm.all_current_khz());
    }
    let _ = seed_secret;
    (states, total)
}

// ---------------------------------------------------------------------------
// 1 - every way a blob can be wrong is a named refusal (the full mutation table).
// ---------------------------------------------------------------------------
#[test]
fn load_refusal_table_is_exact() {
    let blob = BUNDLED_ADVISOR;
    let body = 56 + 8 * N_FEATURES;
    let mutate = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut v = Vec::from(blob);
        f(&mut v);
        Advisor::load(&v).err().unwrap()
    };
    assert_eq!(mutate(&|v| v.truncate(55)), LoadError::TooShort);
    assert_eq!(mutate(&|v| v[0] = b'X'), LoadError::BadMagic);
    assert_eq!(mutate(&|v| v[4] = 9), LoadError::UnsupportedVersion(9));
    assert_eq!(
        mutate(&|v| v[8] = 11),
        LoadError::FeatureCount {
            expected: N_FEATURES,
            found: 11
        }
    );
    assert_eq!(
        mutate(&|v| v[8] = 13),
        LoadError::FeatureCount {
            expected: N_FEATURES,
            found: 13
        }
    );
    for i in [12, 20, 31, 43] {
        assert_eq!(
            mutate(&|v| v[i] ^= 0xff),
            LoadError::ContractMismatch,
            "hash byte {i}"
        );
    }
    assert_eq!(
        mutate(&|v| {
            v.pop();
        }),
        LoadError::Truncated
    );
    // Empty forests.
    let empty = |nf: u32, ni: u32| -> LoadError {
        let mut v = Vec::from(blob);
        v[44..48].copy_from_slice(&nf.to_le_bytes());
        v[48..52].copy_from_slice(&ni.to_le_bytes());
        v.truncate(body + 16 * (nf as usize + ni as usize));
        Advisor::load(&v).err().unwrap()
    };
    assert_eq!(empty(0, 1), LoadError::EmptyForest);
    assert_eq!(empty(1, 0), LoadError::EmptyForest);
    // Size lies: declared nodes don't match the byte length.
    let lie = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut v = Vec::from(blob);
        f(&mut v);
        Advisor::load(&v).err().unwrap()
    };
    assert_eq!(
        lie(&|v| v[44..48].copy_from_slice(&99u32.to_le_bytes())),
        LoadError::Truncated
    );
    // Bad child index and a cycle both refuse as BadIndex.
    assert_eq!(
        lie(&|v| {
            let n = u32::from_le_bytes(v[44..48].try_into().unwrap());
            v[body + 8..body + 12].copy_from_slice(&(n + 9).to_le_bytes());
        }),
        LoadError::BadIndex
    );
    assert_eq!(
        lie(&|v| {
            v[body..body + 4].copy_from_slice(&0i32.to_le_bytes());
            v[body + 8..body + 12].copy_from_slice(&0i32.to_le_bytes());
            v[body + 12..body + 16].copy_from_slice(&0i32.to_le_bytes());
        }),
        LoadError::BadIndex
    );
    // An internal node naming a feature outside the contract.
    assert_eq!(
        lie(&|v| v[body..body + 4].copy_from_slice(&99i32.to_le_bytes())),
        LoadError::BadIndex
    );
    // Bad leaf class.
    assert_eq!(
        lie(&|v| {
            v[body..body + 4].copy_from_slice(&(-1i32).to_le_bytes());
            v[body + 4..body + 8].copy_from_slice(&7i32.to_le_bytes());
        }),
        LoadError::BadClass
    );
    // Inverted box row.
    assert_eq!(
        lie(&|v| {
            v[56..60].copy_from_slice(&2i32.to_le_bytes());
            v[60..64].copy_from_slice(&1i32.to_le_bytes());
        }),
        LoadError::InvertedRange
    );
    // Box row outside the contract's domain.
    assert_eq!(
        lie(&|v| v[60..64].copy_from_slice(&101i32.to_le_bytes())),
        LoadError::BoxOutsideDomain
    );
    assert_eq!(
        lie(&|v| v[56..60].copy_from_slice(&(-1i32).to_le_bytes())),
        LoadError::BoxOutsideDomain
    );
}

// ---------------------------------------------------------------------------
// 2 - the compiled contract and the fixture agree with each other and with the blob.
// ---------------------------------------------------------------------------
#[test]
fn contract_and_blob_agree() {
    // The blob carries the contract hash and count the kernel was built against.
    assert_eq!(&BUNDLED_ADVISOR[0..4], b"ALTH");
    assert_eq!(
        u32::from_le_bytes(BUNDLED_ADVISOR[8..12].try_into().unwrap()) as usize,
        N_FEATURES
    );
    assert_eq!(&BUNDLED_ADVISOR[12..44], &FEATURE_CONTRACT);
    let a = Advisor::load(BUNDLED_ADVISOR).unwrap();
    let (fnodes, inodes, compares) = a.shape();
    assert!(fnodes >= 1 && inodes >= 1 && compares >= 1);
    // Feature names are distinct and domains are sane.
    for (i, name) in FEATURE_NAMES.iter().enumerate() {
        assert!(FEATURE_NAMES[i + 1..].iter().all(|other| other != name));
        assert!(FEATURE_DOMAIN[i].0 <= FEATURE_DOMAIN[i].1);
    }
}

// ---------------------------------------------------------------------------
// 3 - parity with the trainer over the whole committed fixture, and determinism.
// ---------------------------------------------------------------------------
#[test]
fn fixture_parity_is_exact() {
    let rows = parity_fixture();
    assert!(
        rows.len() >= 10,
        "the fixture must be large enough to be evidence"
    );
    let advisor = Advisor::load(BUNDLED_ADVISOR).unwrap();
    for row in &rows {
        let mut obs = PmObserver::new();
        for &(d, t, idx, tick) in &row.stream {
            obs.observe(1, d, t, idx, tick);
        }
        let last_idx = row.stream.last().map(|s| s.2).unwrap_or(0);
        let x = obs
            .features(1, last_idx, row.nominal_idx, row.trip_mc)
            .unwrap_or_else(|| panic!("row {} must derive features", row.name));
        assert_eq!(
            x, row.features,
            "row {}: feature derivation drifted",
            row.name
        );
        let a = advisor.advise(&x);
        assert_eq!(a.freq, row.freq, "row {}: freq class drifted", row.name);
        assert_eq!(a.idle, row.idle, "row {}: idle class drifted", row.name);
        assert_eq!(a.out_of_range, row.out_of_range, "row {}", row.name);
        assert_eq!(a.degenerate, row.degenerate, "row {}", row.name);
        // Determinism: the same question, the same answer, twice.
        assert_eq!(advisor.advise(&x), a, "row {}", row.name);
    }
}

// ---------------------------------------------------------------------------
// 4 - with the advisor ABSENT the advised path is bit-identical to the baseline governor
// over randomized multi-regime traces; the census says abstain on every step.
// ---------------------------------------------------------------------------
#[test]
fn absent_advisor_matches_baseline_exactly() {
    let mut rng = Lcg::new(0x1E77_0001);
    for case in 0..40 {
        let ta = random_trace(&mut rng, 128);
        let tb = random_trace(&mut rng, 128);
        let mut base = platform(0xA11CE);
        let mut adv = platform(0xA11CE);
        let (states, rep) = run_advised(&mut adv, None, &ta, &tb, 0);
        let obs = PmObserver::new();
        let mut identical = true;
        for t in 0..ta.len() as u64 {
            base.set_demand(1, ta[t as usize]).unwrap();
            base.set_demand(2, tb[t as usize]).unwrap();
            base.govern(t);
            if base.all_current_khz() != states[t as usize] {
                identical = false;
                break;
            }
        }
        assert!(identical, "case {case}: advised(None) diverged from govern");
        // The control arm consults nothing: abstains on every domain-step, parks nothing.
        assert_eq!(rep.consultations, 0);
        assert_eq!(rep.abstains, rep.steps);
        assert_eq!(rep.parks, 0);
        assert_eq!(rep.pm_refusals, 0);
        let _ = obs;
    }
}

// ---------------------------------------------------------------------------
// 5 - with the advisor LOADED but abstaining on every step (a collapsed training box), the
// state sequence is still the baseline's exactly.
// ---------------------------------------------------------------------------
#[test]
fn abstaining_advisor_matches_baseline_exactly() {
    // Collapse the demand_now box to [50, 50]: every real history falls outside it.
    let mut degenerate = Vec::from(BUNDLED_ADVISOR);
    degenerate[56..60].copy_from_slice(&50i32.to_le_bytes());
    degenerate[60..64].copy_from_slice(&50i32.to_le_bytes());
    let abstainer = Advisor::load(&degenerate).unwrap();
    let mut rng = Lcg::new(0x1E77_0002);
    let mut ever_abstained = false;
    for case in 0..40 {
        let ta = random_trace(&mut rng, 128);
        let tb = random_trace(&mut rng, 128);
        let mut base = platform(0xA11CE);
        let mut adv = platform(0xA11CE);
        let (states, _) = run_advised(&mut adv, Some(&abstainer), &ta, &tb, 0);
        ever_abstained = ever_abstained || !states.is_empty();
        let mut identical = true;
        for t in 0..ta.len() as u64 {
            base.set_demand(1, ta[t as usize]).unwrap();
            base.set_demand(2, tb[t as usize]).unwrap();
            base.govern(t);
            if base.all_current_khz() != states[t as usize] {
                identical = false;
                break;
            }
        }
        assert!(
            identical,
            "case {case}: abstaining advisor diverged from govern"
        );
    }
    assert!(ever_abstained);
}

// ---------------------------------------------------------------------------
// 6 - THE safety sweep: with a full-ceiling grant MINTED and the advisor decisive, every
// applied point stays in the governor range, demanded silicon is never parked, parks happen
// only at zero demand, idle residency never rewinds, and the pm contract never refused the
// advised path (its targets are legal by construction).
// ---------------------------------------------------------------------------
#[test]
fn advised_path_stays_in_the_governor_range_and_parks_legally() {
    for seed in [0x5EED_0001u64, 0x5EED_0002, 0x5EED_0003, 0x5EED_0004] {
        let mut rng = Lcg::new(seed);
        let ta = random_trace(&mut rng, 192);
        let tb = random_trace(&mut rng, 192);
        let mut pm = platform(seed);
        let _root = pm.mint_grant(1, 2_800_000, "platform-owner").unwrap();
        let advisor = Advisor::load(BUNDLED_ADVISOR).unwrap();
        let mut obs = PmObserver::new();
        let mut total = GovernReport::default();
        let mut residency = [0u64; 3];
        for t in 0..ta.len() as u64 {
            let (da, db) = (ta[t as usize], tb[t as usize]);
            pm.set_demand(1, da).unwrap();
            pm.set_demand(2, db).unwrap();
            let rep = govern_advised(&mut pm, Some(&advisor), &mut obs, t, |dom| {
                if dom == 1 {
                    45_000 + (t as i32 % 60) * 800
                } else {
                    60_000
                }
            });
            total.steps += rep.steps;
            total.consultations += rep.consultations;
            total.decisive += rep.decisive;
            total.abstains += rep.abstains;
            total.out_of_range += rep.out_of_range;
            total.degenerate += rep.degenerate;
            total.moves += rep.moves;
            total.parks += rep.parks;
            total.wakes += rep.wakes;
            total.pm_refusals += rep.pm_refusals;
            // THE invariant: the overclock band stays authority-only with Lethe present.
            for (dom, idx) in [
                (1u32, pm.point_index(1).unwrap()),
                (2, pm.point_index(2).unwrap()),
            ] {
                let nominal = pm.governor_shape(dom).unwrap().0;
                assert!(
                    idx <= nominal,
                    "seed {seed:#x} tick {t}: domain {dom} at idx {idx} > nominal {nominal}"
                );
                assert!(
                    pm.current_khz(dom).unwrap() <= pm.nominal_khz(dom).unwrap(),
                    "seed {seed:#x} tick {t}: clock above nominal"
                );
            }
            // Demanded silicon is never parked after the step; a park happened at zero demand.
            for (dom, d) in [(1u32, da), (2, db)] {
                if d > 0 {
                    assert!(
                        pm.idle_state(dom).is_none(),
                        "seed {seed:#x} tick {t}: domain {dom} parked with demand {d}"
                    );
                }
            }
            // Residency never rewinds.
            let r = pm.idle_residency(1).unwrap();
            assert!(r[0] >= residency[0] && r[1] >= residency[1] && r[2] >= residency[2]);
            residency = r;
        }
        // The census accounts for every consultation; the contract never refused the path.
        assert_eq!(total.consultations, total.decisive + total.abstains);
        assert!(total.out_of_range <= total.abstains && total.degenerate <= total.abstains);
        assert_eq!(total.pm_refusals, 0, "seed {seed:#x}");
        assert_eq!(total.consultations, 2 * ta.len() as u32);
        assert!(pm.wake_latency_ns(1).unwrap_or(0).is_multiple_of(1_000));
    }
}

// ---------------------------------------------------------------------------
// 7 - engine-level determinism: two engines, two observers, the same ops - identical
// states, identical reports, identical ledgers.
// ---------------------------------------------------------------------------
#[test]
fn engine_level_determinism() {
    let mut rng = Lcg::new(0x1E77_0003);
    let ta = random_trace(&mut rng, 96);
    let tb = random_trace(&mut rng, 96);
    let advisor = Advisor::load(BUNDLED_ADVISOR).unwrap();

    let mut pm1 = platform(0xCAFE);
    let _r1 = pm1.mint_grant(1, 2_400_000, "platform-owner").unwrap();
    let mut pm2 = platform(0xCAFE);
    let _r2 = pm2.mint_grant(1, 2_400_000, "platform-owner").unwrap();
    let (mut o1, mut o2) = (PmObserver::new(), PmObserver::new());

    for t in 0..ta.len() as u64 {
        for pm in [&mut pm1, &mut pm2] {
            pm.set_demand(1, ta[t as usize]).unwrap();
            pm.set_demand(2, tb[t as usize]).unwrap();
        }
        let rep1 = govern_advised(&mut pm1, Some(&advisor), &mut o1, t, |d| {
            if d == 1 {
                50_000 + (t as i32 * 300) % 20_000
            } else {
                55_000
            }
        });
        let rep2 = govern_advised(&mut pm2, Some(&advisor), &mut o2, t, |d| {
            if d == 1 {
                50_000 + (t as i32 * 300) % 20_000
            } else {
                55_000
            }
        });
        assert_eq!(rep1, rep2);
        assert_eq!(pm1.all_current_khz(), pm2.all_current_khz());
    }
    assert_eq!(pm1.audit(), pm2.audit());
    assert_eq!(pm1.audit_sequence(), pm2.audit_sequence());
}

// ---------------------------------------------------------------------------
// 8 - the audit ledger under the advised path: ordered, complete, and monotonic across
// wraparound (more records than AUDIT_CAP).
// ---------------------------------------------------------------------------
#[test]
fn ledger_survives_wraparound_under_the_advised_path() {
    let mut pm = PmEngine::new(0x1E77_0004);
    pm.register_domain(
        1,
        &ladder(&[800_000, 2_000_000]),
        2_000_000,
        2_000_000,
        95_000,
    )
    .unwrap();
    pm.register_domain(
        2,
        &ladder(&[500_000, 1_000_000]),
        1_000_000,
        1_000_000,
        95_000,
    )
    .unwrap();
    let advisor = Advisor::load(BUNDLED_ADVISOR).unwrap();
    let mut obs = PmObserver::new();
    let mut rng = Lcg::new(0x1E77_0005);
    for t in 0..800u64 {
        pm.set_demand(1, rng.below(101) as u8).unwrap();
        pm.set_demand(2, rng.below(101) as u8).unwrap();
        let _ = govern_advised(&mut pm, Some(&advisor), &mut obs, t, |_| 45_000);
        // Every scan: the window is ordered, its newest record is the sequence head, and the
        // sequence count covers every accepted act and refusal.
        let ledger = pm.audit();
        assert!(ledger.windows(2).all(|w| w[0].seq < w[1].seq), "tick {t}");
        assert!(
            ledger.last().map(|r| r.seq) == Some(pm.audit_sequence()),
            "tick {t}"
        );
        assert!(ledger.iter().all(|r| !r.kind.is_empty()), "tick {t}");
    }
    // The ledger capped, the sequence did not: more records happened than fit.
    assert!(pm.audit_sequence() as usize > kernel_core::pm::AUDIT_CAP);
    assert_eq!(pm.audit().len(), kernel_core::pm::AUDIT_CAP);
    assert!(pm.audit_sequence() as usize >= pm.transitions() + pm.refusals());
}

// ---------------------------------------------------------------------------
// 9 - observer bounds: slot overflow is refused, rings saturate, features stay in the
// contract's domain for ANY randomized stream, and no history means no guess.
// ---------------------------------------------------------------------------
#[test]
fn observer_is_bounded_and_in_domain() {
    let mut obs = PmObserver::new();
    for i in 0..kernel_core::pm::MAX_DOMAINS {
        assert!(obs.ensure(i as u32 + 1).is_some());
    }
    assert!(obs
        .ensure(kernel_core::pm::MAX_DOMAINS as u32 + 1)
        .is_none());
    assert!(PmObserver::new().features(3, 0, 4, 95_000).is_none());

    let mut rng = Lcg::new(0x1E77_0006);
    for case in 0..50 {
        let mut o = PmObserver::new();
        let len = 1 + rng.below(300) as usize;
        for i in 0..len {
            o.observe(
                1,
                rng.below(101) as u8,
                40_000 + rng.below(60_000) as i32,
                rng.below(5) as usize,
                i as u64,
            );
        }
        let x = o.features(1, rng.below(3) as usize, 4, 95_000).unwrap();
        for i in 0..N_FEATURES {
            let (lo, hi) = FEATURE_DOMAIN[i];
            assert!(
                x[i] >= lo && x[i] <= hi,
                "case {case}: feature {} = {} outside [{lo}, {hi}]",
                FEATURE_NAMES[i],
                x[i]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 10 - the advice census separates abstention causes, and a degenerate input (every feature
// equal) is withheld even though the trees would have spoken.
// ---------------------------------------------------------------------------
#[test]
fn degenerate_inputs_are_withheld() {
    let advisor = Advisor::load(BUNDLED_ADVISOR).unwrap();
    let mut degenerate_found = 0;
    // All-zero and all-saturation vectors are degenerate by construction.
    for v in [0i32, 100, 1_000] {
        let x = [v; N_FEATURES];
        let a = advisor.advise(&x);
        assert!(
            a.degenerate && !a.is_decisive(),
            "vector {v} must be withheld"
        );
        degenerate_found += 1;
    }
    assert_eq!(degenerate_found, 3);
    // A vector that varies is not degenerate (the flag is about information, not values).
    let mut x = [50i32; N_FEATURES];
    x[0] = 51;
    assert!(!advisor.advise(&x).degenerate);
}

// ---------------------------------------------------------------------------
// 11 - REPORTED, never gated: the cost of one advice on the host, in the mlrisk_stress
// posture. No assertion on the number - only that the advisor is still answering.
// ---------------------------------------------------------------------------
#[test]
fn advice_cost_reported() {
    let advisor = Advisor::load(BUNDLED_ADVISOR).unwrap();
    let mut x = [0i32; N_FEATURES];
    for (i, v) in x.iter_mut().enumerate() {
        *v = (i as i32 * 7 + 3) % 101;
    }
    let n = 200_000u32;
    let start = std::time::Instant::now();
    let mut sink = 0u64;
    for _ in 0..n {
        let a = advisor.advise(&x);
        sink += a.freq as u64 + a.idle as u64;
    }
    let elapsed = start.elapsed();
    println!(
        "lethe: {} advises in {:?} ({:.1} ns/advice) [sink={sink}]",
        n,
        elapsed,
        elapsed.as_nanos() as f64 / n as f64
    );
    assert!(sink > 0);
}

// ---------------------------------------------------------------------------
// 12 - the in-kernel suite itself, run on the host against the same bytes.
// ---------------------------------------------------------------------------
#[test]
fn boot_suite_holds_on_the_host() {
    let n = lethe_suite(|_, _, _| {}).expect("lethe suite must hold");
    assert_eq!(
        n, 12,
        "the marker map pins lethe=12 - the suite must prove exactly 12"
    );
}

// ---------------------------------------------------------------------------
// 13 - pm accessors added for the advised path are read-only views: they observe without
// moving anything.
// ---------------------------------------------------------------------------
#[test]
fn pm_observation_accessors_are_read_only() {
    let mut pm = platform(0xBEEF);
    pm.set_demand(1, 42).unwrap();
    assert_eq!(pm.demand(1), Some(42));
    assert_eq!(pm.demand(3), None);
    assert_eq!(pm.point_index(1), Some(0));
    assert_eq!(pm.governor_shape(1), Some((2, 3)));
    assert_eq!(pm.governor_shape(2), Some((1, 2)));
    assert_eq!(pm.trip_temp_mc(1), Some(95_000));
    assert_eq!(pm.idle_state(1), None);
    assert_eq!(pm.domain_ids(), vec![1, 2]);
    let before = pm.all_current_khz();
    let _ = pm.demand(1);
    let _ = pm.governor_shape(1);
    assert_eq!(pm.all_current_khz(), before);
    // Idle state reports parked states as they are.
    pm.set_demand(1, 0).unwrap();
    pm.enter_idle(1, IdleState::C2, 0).unwrap();
    assert_eq!(pm.idle_state(1), Some(IdleState::C2));
}
