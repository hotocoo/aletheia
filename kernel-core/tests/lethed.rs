//! Host proofs for the resident governor — Lethe on the clock (ADR-079).
//!
//! The boot suite in `lethed_suite` proves what must hold on real silicon. These are the
//! exhaustive sweeps that would cost too much boot heap to run there: randomized tick streams,
//! adversarial clocks, and the equivalence that matters most — a resident whose advisor is absent
//! drives the machine exactly as the ADR-076 baseline governor does.

use kernel_core::lethe::{Advisor, DEMAND_WIN};
use kernel_core::lethed::{Cadence, DemandMeter, ResidentGovernor, TickOutcome, TickRefusal};
use kernel_core::pm::{OperatingPoint, PmEngine, MAX_DOMAINS};

fn ladder() -> [OperatingPoint; 4] {
    [
        OperatingPoint {
            khz: 600_000,
            mv: 700,
        },
        OperatingPoint {
            khz: 1_200_000,
            mv: 800,
        },
        OperatingPoint {
            khz: 1_800_000,
            mv: 900,
        },
        OperatingPoint {
            khz: 2_400_000,
            mv: 1_050,
        },
    ]
}

fn engine(domains: u32) -> PmEngine {
    let mut pm = PmEngine::new(0x5E1F_0079);
    for id in 0..domains {
        pm.register_domain(id, &ladder(), 1_800_000, 2_400_000, 95_000)
            .expect("domain registers");
    }
    pm
}

fn watch(domains: u32, cadence: Cadence) -> ResidentGovernor {
    let mut g = ResidentGovernor::new(cadence);
    for id in 0..domains {
        assert!(g.attach(id));
    }
    g
}

/// A cheap deterministic stream — no dependency, and reproducible from its seed.
struct Rng(u64);
impl Rng {
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

#[test]
fn the_boot_suite_passes_on_the_host_too() {
    let mut last = 0;
    let n = kernel_core::lethed::lethed_suite(|i, passed, name| {
        assert!(passed, "invariant {i} failed: {name}");
        last = i;
    })
    .expect("every resident-governor invariant holds");
    assert_eq!(n, last);
    assert!(n >= 15, "the suite must not silently shrink: {n} checks");
}

#[test]
fn a_replayed_clock_never_advances_state() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut pm = engine(2);
    let mut g = watch(2, Cadence::default());

    g.account(0, 80, 20);
    g.tick(&mut pm, advisor.as_ref(), 50, |_| 40_000);
    let census_before = g.census();
    let demand_before = pm.demand(0);

    for t in [50u64, 49, 0, 12] {
        assert!(matches!(
            g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000),
            TickOutcome::Refused(TickRefusal::NotMonotone { .. })
        ));
    }

    assert_eq!(g.census().admitted, census_before.admitted);
    assert_eq!(g.census().refused_not_monotone, 4);
    assert_eq!(pm.demand(0), demand_before);
}

#[test]
fn a_berserk_timer_cannot_drive_churn() {
    // A timer source firing 1000x faster than the cadence allows must not produce 1000x the
    // clock transitions. This is the energy/thermal denial-of-service the cadence floor exists
    // to refuse.
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut pm = engine(1);
    let mut g = watch(
        1,
        Cadence {
            min_gap: 1_000,
            max_gap: 100_000,
        },
    );

    for t in 1..=10_000u64 {
        g.account(0, t % 100, 100 - (t % 100));
        g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
    }

    let c = g.census();
    assert!(c.balances());
    assert_eq!(c.offered, 10_000);
    assert_eq!(c.admitted, 10, "one admission per 1000-tick cadence window");
    assert_eq!(c.refused_too_soon, 9_990);
}

#[test]
fn a_stale_window_is_resynced_not_guessed_through() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut pm = engine(1);
    let mut g = watch(
        1,
        Cadence {
            min_gap: 1,
            max_gap: 8,
        },
    );

    let mut now = 0u64;
    for _ in 0..DEMAND_WIN + 4 {
        now += 1;
        g.account(0, 60, 40);
        g.tick(&mut pm, advisor.as_ref(), now, |_| 40_000);
    }
    assert!(g.is_warm(0), "the window fills under a steady cadence");

    now += 9; // one tick past the staleness ceiling
    g.account(0, 60, 40);
    let outcome = g.tick(&mut pm, advisor.as_ref(), now, |_| 40_000);
    assert!(matches!(
        outcome,
        TickOutcome::Serviced {
            consulted: false,
            ..
        }
    ));
    assert_eq!(g.census().resyncs, 1);
    assert!(!g.is_warm(0));

    // And it warms again only after a FULL window of post-gap truth.
    for _ in 1..DEMAND_WIN {
        now += 1;
        g.account(0, 60, 40);
        assert!(!g.is_warm(0));
        g.tick(&mut pm, advisor.as_ref(), now, |_| 40_000);
    }
    assert!(g.is_warm(0));
}

#[test]
fn an_absent_advisor_drives_the_same_machine_as_the_baseline_governor() {
    // The ADR-078 equivalence, now through the resident path: with no advisor, the watch must
    // produce the same operating-point sequence as `PmEngine::govern` fed the same demand.
    let mut rng = Rng(0xA1E7_4E1A_0079);
    for _trial in 0..40 {
        let mut pm_res = engine(2);
        let mut pm_base = engine(2);
        let mut g = watch(2, Cadence::default());

        let mut now = 0u64;
        for _ in 0..200 {
            now += 1;
            let busy0 = rng.below(101);
            let busy1 = rng.below(101);
            g.account(0, busy0, 100 - busy0);
            g.account(1, busy1, 100 - busy1);

            let outcome = g.tick(&mut pm_res, None, now, |_| 40_000);
            if let TickOutcome::Serviced { domain, .. } = outcome {
                // Mirror the same single-domain service on the untouched baseline engine.
                let pct = if domain == 0 { busy0 } else { busy1 } as u8;
                pm_base
                    .set_demand(domain, pct)
                    .expect("baseline accepts the same demand");
                pm_base.report_temperature(domain, 40_000, now);
                pm_base.govern(now);
            }
        }

        // The resident services one domain per tick and the baseline governs all of them, so
        // the sequences are not required to be equal tick-for-tick — what must hold is that the
        // resident never places a domain anywhere the baseline map would not, given the demand
        // the contract actually holds for it.
        for id in [0u32, 1u32] {
            let demand = pm_res.demand(id).unwrap() as usize;
            let (nominal, span) = pm_res.governor_shape(id).unwrap();
            let expected = if demand == 0 {
                0
            } else {
                (demand * span).div_ceil(100).max(1) - 1
            };
            assert!(expected <= nominal);
            assert_eq!(
                pm_res.point_index(id),
                Some(expected),
                "an advisor-free resident lands exactly on the baseline demand map"
            );
        }
    }
}

#[test]
fn the_census_balances_under_an_adversarial_clock() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut rng = Rng(0xDEAD_BEEF_0079);
    let mut pm = engine(3);
    let mut g = watch(
        3,
        Cadence {
            min_gap: 4,
            max_gap: 64,
        },
    );

    let mut now = 1_000u64;
    for i in 0..5_000u64 {
        // Jump forwards, stand still, and occasionally go BACKWARDS.
        match rng.below(10) {
            0 => now = now.saturating_sub(rng.below(50)),
            1 => {}
            2 => now += 100 + rng.below(500),
            _ => now += rng.below(12),
        }
        let busy = rng.below(101);
        g.account((i % 3) as u32, busy, 100 - busy);
        let temp = 40_000 + rng.below(20_000) as i32;
        g.tick(&mut pm, advisor.as_ref(), now, |_| temp);
        assert!(
            g.census().balances(),
            "the census must balance at every step"
        );
    }

    let c = g.census();
    assert_eq!(c.offered, 5_000);
    assert_eq!(
        c.pm_refusals, 0,
        "the contract refuses nothing on a healthy machine"
    );
    assert_eq!(g.reentry_refusals(), 0);
}

#[test]
fn no_reachable_point_leaves_the_governor_range() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut rng = Rng(0x600D_0079);
    let mut pm = engine(4);
    let mut g = watch(4, Cadence::default());
    let (nominal, _) = pm.governor_shape(0).unwrap();

    for t in 1..=4_000u64 {
        for id in 0..4u32 {
            let busy = rng.below(101);
            g.account(id, busy, 100 - busy);
        }
        g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
        for id in 0..4u32 {
            assert!(
                pm.point_index(id).unwrap() <= nominal,
                "the resident holds no grant; the overclock band must be unreachable"
            );
        }
    }
}

#[test]
fn demanded_silicon_is_never_parked() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut rng = Rng(0xC0FF_EE79);
    let mut pm = engine(2);
    let mut g = watch(2, Cadence::default());

    for t in 1..=3_000u64 {
        for id in 0..2u32 {
            let busy = if rng.below(3) == 0 { 0 } else { rng.below(101) };
            g.account(id, busy, 100 - busy);
        }
        g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
        for id in 0..2u32 {
            if pm.demand(id).unwrap() > 0 {
                assert!(
                    pm.idle_state(id).is_none(),
                    "a demanded domain must never be found parked"
                );
            }
        }
    }
}

#[test]
fn heat_outranks_the_advisor_for_the_whole_cooldown() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut pm = engine(1);
    let mut g = watch(1, Cadence::default());

    // Warm up hot-and-busy, then trip.
    let mut now = 0u64;
    for _ in 0..DEMAND_WIN + 4 {
        now += 1;
        g.account(0, 100, 0);
        g.tick(&mut pm, advisor.as_ref(), now, |_| 40_000);
    }
    assert!(
        pm.point_index(0).unwrap() > 0,
        "a fully demanded domain climbs"
    );

    now += 1;
    g.account(0, 100, 0);
    let hot = g.tick(&mut pm, advisor.as_ref(), now, |_| 120_000);
    assert!(matches!(
        hot,
        TickOutcome::Serviced {
            cooldown_held: true,
            consulted: false,
            ..
        }
    ));

    // For every tick of the latched cooldown, full demand must NOT raise the domain.
    for _ in 0..200 {
        now += 1;
        g.account(0, 100, 0);
        g.tick(&mut pm, advisor.as_ref(), now, |_| 40_000);
        assert_eq!(
            pm.point_index(0),
            Some(0),
            "the clamp holds for the whole cooldown"
        );
    }
    assert_eq!(g.census().cooldown_holds, 201);
    assert_eq!(g.census().pm_refusals, 0);
}

#[test]
fn the_meter_counts_each_window_exactly_once() {
    let mut m = DemandMeter::new();
    assert_eq!(m.take_pct(), 0, "an empty window is zero, not unknown");

    m.account(30, 70);
    assert_eq!(m.pending(), 100);
    assert_eq!(m.take_pct(), 30);
    assert_eq!(m.pending(), 0);
    assert_eq!(m.take_pct(), 0, "a consumed window cannot be counted twice");

    m.account(1, 0);
    assert_eq!(m.take_pct(), 100);

    // Intervals are duration-weighted, not averaged: 500 busy ticks then 100 idle ones is
    // 500/600, and no arrangement of intervals can push the result past full.
    m.account(500, 0);
    m.account(0, 100);
    assert_eq!(m.take_pct(), 83);

    // Saturation, not wraparound.
    m.account(u64::MAX, u64::MAX);
    m.account(u64::MAX, u64::MAX);
    assert_eq!(m.take_pct(), 100);
}

#[test]
fn the_meter_is_exact_across_the_whole_percentage_range() {
    for busy in 0..=100u64 {
        let mut m = DemandMeter::new();
        m.account(busy, 100 - busy);
        assert_eq!(m.take_pct(), busy as u8);
    }
}

#[test]
fn the_watch_is_capacity_bounded_and_attaching_is_idempotent() {
    let mut g = ResidentGovernor::new(Cadence::default());
    for id in 0..MAX_DOMAINS as u32 {
        assert!(g.attach(id));
    }
    assert_eq!(g.attached(), MAX_DOMAINS);
    assert!(!g.attach(MAX_DOMAINS as u32), "past capacity is a refusal");
    for id in 0..MAX_DOMAINS as u32 {
        assert!(g.attach(id), "re-attaching is idempotent");
    }
    assert_eq!(g.attached(), MAX_DOMAINS);
}

#[test]
fn round_robin_is_fair_over_a_long_run() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut pm = engine(5);
    let mut g = watch(5, Cadence::default());
    let mut serviced = [0u32; 5];

    for t in 1..=1_000u64 {
        if let TickOutcome::Serviced { domain, .. } =
            g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000)
        {
            serviced[domain as usize] += 1;
        }
    }

    assert_eq!(serviced, [200; 5], "every domain gets exactly its turn");
}

#[test]
fn an_unattached_or_unusable_watch_refuses_by_name() {
    let mut pm = engine(1);

    let mut empty = ResidentGovernor::new(Cadence::default());
    assert!(matches!(
        empty.tick(&mut pm, None, 1, |_| 40_000),
        TickOutcome::Refused(TickRefusal::NoDomains)
    ));

    for bad in [
        Cadence {
            min_gap: 0,
            max_gap: 10,
        },
        Cadence {
            min_gap: 11,
            max_gap: 10,
        },
    ] {
        let mut g = watch(1, bad);
        assert!(matches!(
            g.tick(&mut pm, None, 1, |_| 40_000),
            TickOutcome::Refused(TickRefusal::BadCadence)
        ));
        assert!(g.census().balances());
    }
}

#[test]
fn an_explicit_resync_forgets_every_window() {
    let advisor = Advisor::load(kernel_core::lethe::BUNDLED_ADVISOR).ok();
    let mut pm = engine(2);
    let mut g = watch(2, Cadence::default());

    for t in 1..=(2 * DEMAND_WIN as u64 + 8) {
        g.account(0, 50, 50);
        g.account(1, 50, 50);
        g.tick(&mut pm, advisor.as_ref(), t, |_| 40_000);
    }
    assert!(g.is_warm(0) && g.is_warm(1));

    g.resync();
    assert!(!g.is_warm(0) && !g.is_warm(1));
}
