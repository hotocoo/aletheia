//! Host-exhaustive proofs of the power/performance contract (ALET-P2-022, ADR-076).
//!
//! The in-kernel `pm_suite` proves the core promises at boot on every target on a fixed
//! two-domain platform; these tests are the EXHAUSTIVE sweeps the boot heap cannot afford
//! (ADR-063): the full decision table over the overclock band, attenuation monotonicity by
//! sweep, revocation clamps with cascades, envelope absoluteness over every reachable state,
//! cooldown tick exactness, idle accounting exactness under transition interference, the
//! device-arc table, ledger completeness under wraparound, and a determinism check — two
//! engines fed the same op sequence must land in identical states with identical ledgers.

use kernel_core::pm::*;

fn ladder(khz: &[u32]) -> Vec<OperatingPoint> {
    // 700 mV at the bottom, +50 mV per rung, +200 mV in the OC band — voltage tracks clock.
    khz.iter()
        .enumerate()
        .map(|(i, k)| OperatingPoint {
            khz: *k,
            mv: (700 + 50 * i as u16),
        })
        .collect()
}

/// A standard single-domain platform: 3 governor rungs (0.8/1.2/2.0 GHz nominal) + a 2-rung
/// OC band (2.4/2.8 GHz, 2.8 == envelope), trip at 95 C.
fn platform() -> PmEngine {
    let mut pm = PmEngine::new(0x5EED_0001);
    let l = ladder(&[800_000, 1_200_000, 2_000_000, 2_400_000, 2_800_000]);
    pm.register_domain(1, &l, 2_000_000, 2_800_000, 95_000)
        .unwrap();
    pm
}

const A: u32 = 1;

// ---------------------------------------------------------------------------
// 1 - the OC band decision table: for EVERY point of the ladder, with and
// without grants, at every grant ceiling — the answer is exact.
// ---------------------------------------------------------------------------
#[test]
fn oc_band_decision_table_is_exact() {
    let points = [800_000, 1_200_000, 2_000_000, 2_400_000, 2_800_000];
    for ceiling in points {
        let mut pm = platform();
        let root = pm.mint_grant(A, ceiling, "owner").unwrap();
        for &p in &points {
            // With the grant: allowed iff p is in the governor range (free to anyone) or
            // p is at/below the grant ceiling. Authority gates the OC band only — it can
            // never NARROW the governor range.
            let with = pm.request_point(A, p, &[root], 0);
            assert_eq!(
                with.is_ok(),
                p <= ceiling || p <= 2_000_000,
                "with ceiling {ceiling}, point {p} must be {}",
                if p <= ceiling || p <= 2_000_000 {
                    "allowed"
                } else {
                    "refused"
                }
            );
            // Without the grant: allowed iff p <= nominal.
            pm.request_point(A, 800_000, &[], 0).unwrap();
            let without = pm.request_point(A, p, &[], 0);
            assert_eq!(
                without.is_ok(),
                p <= 2_000_000,
                "without a grant, point {p} must be {}",
                if p <= 2_000_000 { "allowed" } else { "refused" }
            );
        }
    }
}

#[test]
fn nothing_offered_is_no_authority_and_a_short_grant_is_not_granted() {
    let mut pm = platform();
    // Nothing offered -> NoAuthority.
    assert!(matches!(
        pm.request_point(A, 2_400_000, &[], 0),
        Err(PmFault::NoAuthority { domain: A })
    ));
    // A live grant for this domain that doesn't reach -> NotGranted naming both sides.
    let short = pm.mint_grant(A, 2_400_000, "owner").unwrap();
    assert!(matches!(
        pm.request_point(A, 2_800_000, &[short], 0),
        Err(PmFault::NotGranted {
            domain: A,
            requested_khz: 2_800_000,
            granted_khz: 2_400_000
        })
    ));
    // A live grant for ANOTHER domain is not authority here.
    let l = ladder(&[500_000, 1_000_000, 1_500_000]);
    pm.register_domain(2, &l, 1_500_000, 1_500_000, 90_000)
        .unwrap();
    let other = pm.mint_grant(2, 1_500_000, "owner").unwrap();
    assert!(matches!(
        pm.request_point(A, 2_400_000, &[other], 0),
        Err(PmFault::NoAuthority { domain: A })
    ));
    // A forged token is not authority, and doesn't change the state.
    assert!(matches!(
        pm.request_point(A, 2_400_000, &[0xDEAD_BEEF, 42], 0),
        Err(PmFault::NoAuthority { domain: A })
    ));
    assert_eq!(pm.current_khz(A), Some(800_000));
}

// ---------------------------------------------------------------------------
// 2 - attenuation monotonicity by sweep: no chain of delegations ever
// produces a grant that reaches above its root's ceiling.
// ---------------------------------------------------------------------------
#[test]
fn delegation_chain_never_amplifies() {
    let ceilings = [800_000, 1_200_000, 2_000_000, 2_400_000, 2_800_000];
    for &root_max in &ceilings {
        for &child_max in &ceilings {
            for &grand_max in &ceilings {
                let mut pm = platform();
                let root = pm.mint_grant(A, root_max, "owner").unwrap();
                let child = pm.delegate(root, A, child_max, "agent");
                if child_max > root_max {
                    assert!(matches!(child, Err(PmFault::Amplification { .. })));
                    continue;
                }
                let child = child.unwrap();
                let grand = pm.delegate(child, A, grand_max, "tool");
                if grand_max > child_max {
                    assert!(matches!(grand, Err(PmFault::Amplification { .. })));
                    continue;
                }
                let grand = grand.unwrap();
                // The grandchild can lift the domain to at most min(root, child, grand).
                let reach = grand_max.min(child_max).min(root_max);
                assert!(
                    pm.request_point(A, reach, &[grand], 0).is_ok(),
                    "grant chain {root_max}>{child_max}>{grand_max} must reach its own ceiling"
                );
                // A chain cannot reach past its ceiling — but only the OC BAND is
                // authority-gated: governor-range points stay free to every caller, so the
                // probe point must sit above BOTH the reach and nominal.
                if reach < 2_800_000 {
                    let floor = reach.max(2_000_000);
                    if let Some(&above) = ceilings.iter().find(|p| **p > floor) {
                        assert!(
                            pm.request_point(A, above, &[grand], 0).is_err(),
                            "chain {root_max}>{child_max}>{grand_max} must not reach {above}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn cross_domain_delegation_is_refused() {
    let mut pm = platform();
    let l = ladder(&[500_000, 1_000_000, 1_500_000]);
    pm.register_domain(2, &l, 1_500_000, 1_500_000, 90_000)
        .unwrap();
    let root = pm.mint_grant(A, 2_800_000, "owner").unwrap();
    assert!(matches!(
        pm.delegate(root, 2, 1_500_000, "agent"),
        Err(PmFault::CrossDomain {
            grant_domain: A,
            target_domain: 2
        })
    ));
}

// ---------------------------------------------------------------------------
// 3 - revocation: immediate clamp to nominal, cascade over the subtree,
// idempotence, and no resurrection by re-offering.
// ---------------------------------------------------------------------------
#[test]
fn revocation_clamps_immediately_and_cascades() {
    let mut pm = platform();
    let root = pm.mint_grant(A, 2_800_000, "owner").unwrap();
    let c1 = pm.delegate(root, A, 2_400_000, "agent").unwrap();
    let c2 = pm.delegate(c1, A, 2_400_000, "tool").unwrap();
    // Lift into the OC band under the grandchild's authority, then kill the root.
    pm.request_point(A, 2_400_000, &[c2], 0).unwrap();
    assert_eq!(pm.current_khz(A), Some(2_400_000));
    pm.revoke(root, 100);
    assert_eq!(
        pm.current_khz(A),
        Some(2_000_000),
        "a domain lifted under a dead grant is clamped to nominal before revoke returns"
    );
    for tok in [root, c1, c2] {
        assert!(matches!(
            pm.request_point(A, 2_400_000, &[tok], 0),
            Err(PmFault::NoAuthority { domain: A })
        ));
    }
    // Idempotent: revoking again is a silent no-op.
    pm.revoke(root, 100);
    pm.revoke(c1, 100);
    assert_eq!(pm.current_khz(A), Some(2_000_000));
}

#[test]
fn revocation_of_a_governor_range_grant_clamps_nothing() {
    // A grant that never reaches past nominal has no OC effects; killing it must not
    // disturb a domain sitting anywhere in the governor range.
    let mut pm = platform();
    let root = pm.mint_grant(A, 1_200_000, "owner").unwrap();
    pm.request_point(A, 1_200_000, &[root], 0).unwrap();
    pm.revoke(root, 0);
    assert_eq!(pm.current_khz(A), Some(1_200_000));
}

// ---------------------------------------------------------------------------
// 4 - the envelope is absolute: registration refuses dishonest ladders, mint
// refuses past-envelope grants, so NO reachable state exceeds the envelope.
// ---------------------------------------------------------------------------
#[test]
fn envelope_is_structural() {
    let mut pm = PmEngine::new(1);
    // A ladder point above the envelope is refused at registration.
    let l = ladder(&[800_000, 3_000_000]);
    assert!(matches!(
        pm.register_domain(A, &l, 800_000, 2_800_000, 95_000),
        Err(PmFault::MalformedLadder(A))
    ));
    // Nominal not on the ladder is refused.
    let l = ladder(&[800_000, 1_200_000]);
    assert!(matches!(
        pm.register_domain(A, &l, 2_000_000, 2_800_000, 95_000),
        Err(PmFault::MalformedLadder(A))
    ));
    // Non-ascending ladders are refused.
    let l = ladder(&[800_000, 800_000, 1_200_000]);
    assert!(matches!(
        pm.register_domain(A, &l, 1_200_000, 2_800_000, 95_000),
        Err(PmFault::MalformedLadder(A))
    ));
    // Empty ladders are refused.
    let l: Vec<OperatingPoint> = Vec::new();
    assert!(matches!(
        pm.register_domain(A, &l, 800_000, 2_800_000, 95_000),
        Err(PmFault::MalformedLadder(A))
    ));
    // Now an honest platform: mint refuses past the envelope, names it.
    let mut pm = platform();
    assert!(matches!(
        pm.mint_grant(A, 3_200_000, "owner"),
        Err(PmFault::AboveEnvelope {
            domain: A,
            requested_khz: 3_200_000,
            envelope_khz: 2_800_000
        })
    ));
    // And a delegation cannot sneak past either.
    let root = pm.mint_grant(A, 2_800_000, "owner").unwrap();
    assert!(matches!(
        pm.delegate(root, A, 3_200_000, "agent"),
        Err(PmFault::Amplification { .. })
    ));
}

// ---------------------------------------------------------------------------
// 5 - thermal trips: every domain clamps, every domain cools, the remaining
// ticks are named exactly, the governor range keeps serving, elevation
// returns exactly at expiry, and a sub-trip report does nothing.
// ---------------------------------------------------------------------------
#[test]
fn thermal_trip_clamps_all_and_cooldown_is_tick_exact() {
    let mut pm = platform();
    let root = pm.mint_grant(A, 2_800_000, "owner").unwrap();
    pm.request_point(A, 2_800_000, &[root], 0).unwrap();
    pm.report_temperature(A, 94_999, 1_000); // one milli-degree below the trip: nothing
    assert_eq!(pm.current_khz(A), Some(2_800_000));
    pm.report_temperature(A, 95_000, 2_000); // exactly the trip: clamp
    assert_eq!(pm.current_khz(A), Some(800_000));
    // Every tick inside the window is refused with the exact remaining count named.
    for t in 2_001..=2_998 {
        let r = pm.request_point(A, 2_400_000, &[root], t);
        match r {
            Err(PmFault::Cooldown {
                domain,
                remaining_ticks,
            }) => {
                assert_eq!(domain, A);
                assert_eq!(remaining_ticks, 3_000 - t, "tick {t}");
            }
            other => panic!("tick {t}: expected Cooldown, got {other:?}"),
        }
    }
    // The governor range keeps serving during cooldown.
    assert!(pm.request_point(A, 2_000_000, &[], 3_000).is_ok());
    // Expiry is exact: refused the tick before, allowed at the boundary.
    assert!(matches!(
        pm.request_point(A, 2_400_000, &[root], 2_999),
        Err(PmFault::Cooldown { .. })
    ));
    assert!(pm.request_point(A, 2_400_000, &[root], 3_000).is_ok());
    assert_eq!(pm.current_khz(A), Some(2_400_000));
}

#[test]
fn a_trip_on_one_domain_clamps_every_domain() {
    let mut pm = platform();
    let l = ladder(&[500_000, 1_000_000, 1_500_000]);
    pm.register_domain(2, &l, 1_500_000, 1_500_000, 90_000)
        .unwrap();
    pm.request_point(A, 2_000_000, &[], 0).unwrap();
    pm.request_point(2, 1_500_000, &[], 0).unwrap();
    pm.report_temperature(2, 99_000, 10);
    assert_eq!(pm.current_khz(A), Some(800_000), "domain A did not trip");
    assert_eq!(pm.current_khz(2), Some(500_000), "domain 2 did not trip");
    // Temperature reports on unknown domains are refused (recorded), not silent.
    pm.report_temperature(77, 99_000, 10);
    assert!(pm.audit().iter().any(|r| !r.accepted && r.domain == 77));
}

// ---------------------------------------------------------------------------
// 6 - the governor: demand mapping is deterministic over the governor range,
// never enters the OC band, parks zero-demand domains, ignores cooldown for
// governor-range moves.
// ---------------------------------------------------------------------------
#[test]
fn governor_maps_demand_deterministically_and_never_overclocks() {
    for demand in 0u8..=100 {
        let mut pm = platform();
        pm.set_demand(A, demand).unwrap();
        pm.govern(0);
        let cur = pm.current_khz(A).unwrap();
        assert!(
            cur <= 2_000_000,
            "demand {demand} lifted the governor into the OC band"
        );
        if demand == 0 {
            assert_eq!(cur, 800_000);
        } else {
            // t = ceil(demand * span / 100), span = 3 rungs.
            let span = 3usize;
            let t = ((demand as usize) * span).div_ceil(100);
            let expect = [800_000, 1_200_000, 2_000_000][(t.max(1) - 1).min(2)];
            assert_eq!(cur, expect, "demand {demand}");
        }
    }
}

#[test]
fn demand_must_be_a_percentage() {
    let mut pm = platform();
    assert!(matches!(
        pm.set_demand(A, 101),
        Err(PmFault::BadDemand {
            domain: A,
            pct: 101
        })
    ));
    assert!(pm.set_demand(A, 100).is_ok());
    assert!(matches!(
        pm.set_demand(77, 10),
        Err(PmFault::UnknownDomain(77))
    ));
}

// ---------------------------------------------------------------------------
// 7 - idle accounting: exact under wake, under transition interference, and
// under the busy/parked refusals. Residency is real time and never lost.
// ---------------------------------------------------------------------------
#[test]
fn idle_accounting_is_exact_under_interference() {
    let mut pm = platform();
    // Park C1 at t=100, a governor-range request at t=600 BREAKS the span (books 500),
    // park C2 at t=1000, wake at t=3000 (books 2000 + 10us latency).
    pm.enter_idle(A, IdleState::C1, 100).unwrap();
    pm.request_point(A, 1_200_000, &[], 600).unwrap();
    assert_eq!(pm.idle_residency(A), Some([0, 500, 0]));
    pm.enter_idle(A, IdleState::C2, 1_000).unwrap();
    pm.wake(A, 3_000).unwrap();
    assert_eq!(pm.idle_residency(A), Some([0, 500, 2_000]));
    assert_eq!(pm.wake_latency_ns(A), Some(10_000));
    // Waking a running domain is refused by name.
    assert!(matches!(pm.wake(A, 3_100), Err(PmFault::NotIdle(A))));
    // A trip while parked books the span, then clamps.
    pm.enter_idle(A, IdleState::C2, 4_000).unwrap();
    pm.report_temperature(A, 100_000, 4_250);
    assert_eq!(pm.idle_residency(A), Some([0, 500, 2_250]));
    assert_eq!(pm.current_khz(A), Some(800_000));
    // Double park is refused; C0 is not a parking state.
    pm.enter_idle(A, IdleState::C1, 5_000).unwrap();
    assert!(matches!(
        pm.enter_idle(A, IdleState::C2, 5_100),
        Err(PmFault::AlreadyIdle(A))
    ));
    pm.wake(A, 5_200).unwrap();
    assert!(matches!(
        pm.enter_idle(A, IdleState::C0, 5_300),
        Err(PmFault::NotAnIdleState(A))
    ));
}

#[test]
fn demanded_silicon_is_never_parked() {
    for demand in 1u8..=100 {
        let mut pm = platform();
        pm.set_demand(A, demand).unwrap();
        assert!(matches!(
            pm.enter_idle(A, IdleState::C2, 0),
            Err(PmFault::DomainBusy { domain: A, pct }) if pct == demand
        ));
    }
}

// ---------------------------------------------------------------------------
// 8 - device power arcs: the full transition table.
// ---------------------------------------------------------------------------
#[test]
fn device_power_arc_table_is_exact() {
    let mut pm = platform();
    pm.register_device(9).unwrap();
    assert_eq!(pm.device_state(9), Some(DState::D0));
    let arcs = [
        (DState::D0, DState::D1, true),
        (DState::D1, DState::D0, true),
        (DState::D0, DState::D3, true),
        (DState::D1, DState::D3, true),
        (DState::D3, DState::D0, true),
        (DState::D3, DState::D1, false), // wake through D0 or not at all
        (DState::D0, DState::D0, false),
        (DState::D1, DState::D1, false),
        (DState::D3, DState::D3, false),
    ];
    for (from, to, legal) in arcs {
        // Drive the device to `from` through legal arcs (a self-transition is a refusal,
        // so only reset when the device is not already at D0).
        if pm.device_state(9) != Some(DState::D0) {
            pm.set_device_power(9, DState::D0).unwrap();
        }
        match from {
            DState::D0 => {}
            DState::D1 => pm.set_device_power(9, DState::D1).unwrap(),
            DState::D3 => pm.set_device_power(9, DState::D3).unwrap(),
        }
        assert_eq!(pm.device_state(9), Some(from), "setup for {from:?}");
        let r = pm.set_device_power(9, to);
        assert_eq!(r.is_ok(), legal, "{from:?} -> {to:?}");
        if !legal {
            assert!(matches!(r, Err(PmFault::IllegalDState { device: 9, .. })));
            assert_eq!(
                pm.device_state(9),
                Some(from),
                "a refused arc changed nothing"
            );
        }
    }
    assert!(matches!(
        pm.set_device_power(77, DState::D0),
        Err(PmFault::UnknownDevice(77))
    ));
}

// ---------------------------------------------------------------------------
// 9 - the ledger: every act recorded, sequence monotonic, wraparound keeps
// the count, records classified.
// ---------------------------------------------------------------------------
#[test]
fn audit_ledger_is_complete_and_wraps_with_monotonic_sequence() {
    let mut pm = platform();
    // > AUDIT_CAP acts of every shape: accepted elevations, refused elevations, and
    // governor-range moves — every state change lands in the ledger (a request to the
    // point already held is a silent no-op, so the sweep is sized past the record count).
    let root = pm.mint_grant(A, 2_400_000, "owner").unwrap();
    for i in 0..300u64 {
        let khz = if i % 2 == 0 { 2_400_000 } else { 1_200_000 };
        let offered: Vec<u64> = if i % 3 == 0 { vec![root] } else { vec![] };
        let _ = pm.request_point(A, khz, &offered, i);
    }
    assert!(
        pm.audit_sequence() > AUDIT_CAP as u64,
        "the sweep must exceed the ledger capacity to prove wraparound"
    );
    assert_eq!(pm.audit().len(), AUDIT_CAP);
    let ledger = pm.audit();
    assert!(ledger.windows(2).all(|w| w[0].seq < w[1].seq));
    assert_eq!(
        ledger.last().unwrap().seq,
        pm.audit_sequence(),
        "the newest record carries the newest sequence number"
    );
    assert!(ledger.iter().all(|r| !r.kind.is_empty()));
    // The counters agree with the ledger's completeness witness.
    assert!(pm.transitions() + pm.refusals() <= pm.audit_sequence() as usize);
}

// ---------------------------------------------------------------------------
// 10 - registration refusals and capacity bounds.
// ---------------------------------------------------------------------------
#[test]
fn registration_is_refused_for_doubles_and_overflows() {
    let mut pm = platform();
    let l = ladder(&[800_000, 1_200_000]);
    assert!(matches!(
        pm.register_domain(A, &l, 1_200_000, 2_800_000, 95_000),
        Err(PmFault::AlreadyRegistered(A))
    ));
    // Fill the domain table to capacity.
    for id in 2..MAX_DOMAINS as u32 + 2 {
        let r = pm.register_domain(id, &l, 1_200_000, 2_800_000, 95_000);
        if id <= MAX_DOMAINS as u32 {
            assert!(r.is_ok(), "id {id} should fit");
        } else {
            assert!(
                matches!(r, Err(PmFault::NoSpace)),
                "id {id} should overflow"
            );
        }
    }
    // Device table capacity.
    for id in 0..MAX_DEVICES as u32 + 2 {
        let r = pm.register_device(id);
        if id < MAX_DEVICES as u32 {
            assert!(r.is_ok(), "device {id} should fit");
        } else {
            assert!(
                matches!(r, Err(PmFault::NoSpace)),
                "device {id} should overflow"
            );
        }
    }
}

#[test]
fn grants_are_bounded() {
    let mut pm = platform();
    let l = ladder(&[800_000, 1_200_000]);
    pm.register_domain(2, &l, 1_200_000, 2_800_000, 95_000)
        .unwrap();
    let root = pm.mint_grant(A, 2_800_000, "owner").unwrap();
    // The table counts the root: MAX_GRANTS - 1 delegations fit beside it.
    for _ in 0..MAX_GRANTS - 1 {
        pm.delegate(root, A, 800_000, "agent").unwrap();
    }
    assert!(matches!(
        pm.delegate(root, A, 800_000, "agent"),
        Err(PmFault::NoSpace)
    ));
}

// ---------------------------------------------------------------------------
// 11 - determinism: two engines, same ops, same states, same ledgers.
// ---------------------------------------------------------------------------
#[test]
fn identical_op_sequences_are_bit_identical() {
    let run = || {
        let mut pm = platform();
        let root = pm.mint_grant(A, 2_800_000, "owner").unwrap();
        let child = pm.delegate(root, A, 2_400_000, "agent").unwrap();
        let _ = pm.request_point(A, 2_400_000, &[child], 10);
        let _ = pm.request_point(A, 2_000_000, &[], 20);
        pm.set_demand(A, 50).unwrap();
        pm.govern(30);
        pm.set_demand(A, 0).unwrap();
        pm.govern(40);
        pm.enter_idle(A, IdleState::C2, 50).unwrap();
        pm.report_temperature(A, 96_000, 60);
        let _ = pm.wake(A, 70);
        let _ = pm.request_point(A, 2_400_000, &[root], 1_100);
        let _ = pm.set_device_power(3, DState::D1);
        pm.revoke(root, 1_200);
        pm
    };
    let a = run();
    let b = run();
    assert_eq!(a.all_current_khz(), b.all_current_khz());
    assert_eq!(a.audit(), b.audit());
    assert_eq!(a.transitions(), b.transitions());
    assert_eq!(a.refusals(), b.refusals());
    assert_eq!(a.idle_residency(A), b.idle_residency(A));
}

// ---------------------------------------------------------------------------
// 12 - the boot suite itself, run on the host with a capturing reporter —
// the same invariants every target re-proves at boot.
// ---------------------------------------------------------------------------
#[test]
fn the_boot_suite_passes_on_the_host() {
    let mut n = 0u32;
    let r = pm_suite(|i, passed, name| {
        n = i;
        assert!(
            passed,
            "boot-suite invariant {i} failed on the host: {name}"
        );
    });
    assert_eq!(r, Ok(n));
    assert!(
        n >= 14,
        "the suite must keep proving all its invariants, got {n}"
    );
}
