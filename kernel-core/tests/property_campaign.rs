//! Larger and more diverse deterministic property campaigns (ALET-P2-010).
//!
//! The normal soak is deliberately one fixed workload. This suite complements it by generating
//! many distinct load shapes from a deterministic seed, including boundary-biased and asymmetric
//! cases. Each generated case is executed twice and its scale-free invariants are checked. The
//! generator is intentionally tiny and dependency-free so the campaign can run in the same hosted
//! environment as the kernel-core tests.

use kernel_core::soak::{campaign, soak_suite, SoakLoad, SoakReport};
use kernel_core::Hal;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;
use std::time::Instant;

struct HostHal;

impl Hal for HostHal {
    fn arch_name() -> &'static str {
        "host-property"
    }
    fn timer_ticks() -> u64 {
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

/// Small deterministic generator used instead of a third-party property-testing dependency.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next() as usize % (hi - lo + 1))
    }
}

fn assert_properties(r: &SoakReport, load: SoakLoad) {
    assert_eq!(r.journal.commit_errors, 0);
    assert_eq!(r.journal.mismatches, 0);
    assert_eq!(r.journal.recovers, 3);
    assert_eq!(r.journal.recovers_replayed, 3);
    assert_eq!(r.journal.post_recover_mismatches, 0);
    assert_eq!(r.fs.audits, r.fs.ops);
    assert_eq!(r.fs.tally_violations, 0);
    assert_eq!(r.fs.mismatches, 0);
    assert_eq!(r.fs.verifies, r.fs.ops);
    assert!(r.fs.final_ok);
    assert_eq!(r.grants.cycles, load.grant_cycles);
    assert_eq!(r.grants.zero_copy_mismatches, 0);
    assert_eq!(r.grants.refcount_violations, 0);
    assert_eq!(
        r.grants.unauthorized_refused,
        r.grants.unauthorized_attempted
    );
    assert_eq!(r.grants.amplify_refused, r.grants.amplify_attempted);
    assert_eq!(r.grants.revoked_refused, r.grants.revoked_attempted);
    assert_eq!(r.tasks.generations, load.task_generations);
    assert_eq!(r.tasks.finished_redispatches, 0);
    assert_eq!(r.tasks.blocked_redispatches, 0);
    assert_eq!(r.tasks.drains_not_empty, 0);
    assert_eq!(r.tasks.unknown_violations, 0);
    assert_eq!(r.tasks.priority_dispatched, load.task_generations * 16);
}

fn generated_cases(seed: u64, count: usize) -> Vec<SoakLoad> {
    let mut g = Gen(seed);
    let mut cases = Vec::with_capacity(count);
    if count == 0 {
        return cases;
    }
    cases.push(SoakLoad {
        journal_txs: 1,
        fs_cycles: 1,
        grant_cycles: 1,
        task_generations: 1,
    });
    if count == 1 {
        return cases;
    }
    cases.push(SoakLoad {
        journal_txs: 8_192,
        fs_cycles: 240,
        grant_cycles: 1_024,
        task_generations: 512,
    });
    while cases.len() < count {
        cases.push(SoakLoad {
            journal_txs: g.range(2, 4_096),
            fs_cycles: g.range(1, 160),
            grant_cycles: g.range(1, 768),
            task_generations: g.range(1, 384),
        });
    }
    cases
}

fn run_case(load: SoakLoad) {
    let (report, n) = soak_suite(
        load,
        |l| campaign::<HostHal>(l, None),
        |_, ok, name| assert!(ok, "{name}"),
    )
    .unwrap_or_else(|(n, name)| panic!("failed property {n}: {name}"));
    assert_eq!(n, 12);
    assert_properties(&report, load);
}

/// Shrink a failing generated shape toward the smallest counterexample by repeatedly halving each
/// dimension. This is deliberately independent of the generator: a future generator can change
/// shape without making the stored failure harder to reproduce.
fn minimize_failure(mut load: SoakLoad) -> SoakLoad {
    let mut changed = true;
    while changed {
        changed = false;
        for field in [0usize, 1, 2, 3] {
            loop {
                let mut candidate = load;
                let value = match field {
                    0 => load.journal_txs,
                    1 => load.fs_cycles,
                    2 => load.grant_cycles,
                    _ => load.task_generations,
                };
                if value <= 1 {
                    break;
                }
                let shrunk = value / 2;
                match field {
                    0 => candidate.journal_txs = shrunk,
                    1 => candidate.fs_cycles = shrunk,
                    2 => candidate.grant_cycles = shrunk,
                    _ => candidate.task_generations = shrunk,
                }
                let still_fails = catch_unwind(AssertUnwindSafe(|| run_case(candidate))).is_err();
                if still_fails {
                    load = candidate;
                    changed = true;
                } else {
                    break;
                }
            }
        }
    }
    load
}

#[test]
fn diverse_generated_loads_preserve_all_scale_free_properties() {
    let seed = std::env::var("ALETHEIA_PROPERTY_SEED")
        .ok()
        .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0xA1E7_0210_5EED);
    let count = std::env::var("ALETHEIA_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(32);
    assert!(
        count > 0,
        "property campaign must execute at least one generated case"
    );
    let cases = generated_cases(seed, count);

    let mut checks = 0usize;
    for (i, load) in cases.into_iter().enumerate() {
        let result = catch_unwind(AssertUnwindSafe(|| run_case(load)));
        if let Err(payload) = result {
            let minimized = minimize_failure(load);
            eprintln!(
                "PROPERTY FAILURE seed=0x{seed:016x} case={i} original={load:?} minimized={minimized:?}"
            );
            std::panic::resume_unwind(payload);
        }
        checks += 12;
    }
    assert_eq!(checks, count * 12);
}

#[test]
fn repeated_boundary_shapes_are_stable() {
    // Re-run deliberately awkward shapes. This catches state leakage between cases while keeping
    // the campaign small enough for debug CI.
    let shapes = [
        SoakLoad {
            journal_txs: 1,
            fs_cycles: 120,
            grant_cycles: 2,
            task_generations: 257,
        },
        SoakLoad {
            journal_txs: 4_001,
            fs_cycles: 1,
            grant_cycles: 257,
            task_generations: 2,
        },
        SoakLoad {
            journal_txs: 97,
            fs_cycles: 159,
            grant_cycles: 767,
            task_generations: 383,
        },
    ];
    for (round, load) in shapes.into_iter().enumerate() {
        let a = campaign::<HostHal>(load, None);
        let b = campaign::<HostHal>(load, None);
        assert_properties(&a, load);
        assert_properties(&b, load);
        assert_eq!(a.journal.checksum, b.journal.checksum, "round {round}");
        assert_eq!(a.fs.checksum, b.fs.checksum, "round {round}");
        assert_eq!(a.grants.checksum, b.grants.checksum, "round {round}");
    }
}
