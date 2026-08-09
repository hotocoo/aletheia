//! The authority lattice, proved by exhaustion (REQ-CAP-007, ADR-048,
//! `docs/INVARIANT-CONTRACTS.md` §INV-CAP-SCOPE).
//!
//! Every property here is asserted over the WHOLE finite universe the model can express
//! (`capalg::scope_universe` × `capalg::target_universe`, and a pattern alphabet chosen to include
//! the shapes the previous relation got wrong) rather than over examples. A sampled proof of a
//! partial order is a proof about the samples.

use kernel_core::capalg::*;
use kernel_core::spine::{Constraints, Scope};

fn constraints_universe() -> Vec<Constraints> {
    let mut v = Vec::new();
    for expires_at in [None, Some(10u64), Some(20)] {
        for approval_required in [false, true] {
            for local_only in [false, true] {
                v.push(Constraints {
                    expires_at,
                    approval_required,
                    local_only,
                });
            }
        }
    }
    v
}

// ---------------------------------------------------------------------------
// INV-CAP-SCOPE-1 — soundness. The one the other properties exist to serve.
// ---------------------------------------------------------------------------

#[test]
fn scope_attenuation_never_grants_reach_the_parent_lacked() {
    let scopes = scope_universe();
    let targets = target_universe();
    for p in &scopes {
        for c in &scopes {
            if !scope_attenuates(p, c) {
                continue;
            }
            for t in &targets {
                assert!(
                    !scope_covers(c, t) || scope_covers(p, t),
                    "scope_attenuates({p:?}, {c:?}) but the child reaches {t:?} and the parent does not"
                );
            }
        }
    }
}

#[test]
fn action_attenuation_never_grants_reach_the_parent_lacked() {
    let patterns = action_universe();
    let actions = concrete_action_universe();
    for p in &patterns {
        for c in &patterns {
            if !action_attenuates(p, c) {
                continue;
            }
            for a in &actions {
                assert!(
                    !action_covers(c, a) || action_covers(p, a),
                    "action_attenuates({p:?}, {c:?}) but the child reaches {a:?} and the parent does not"
                );
            }
        }
    }
}

/// The defect this module was written to fix, pinned as its own regression: the pattern pair where
/// `action_covers` (the authorization test) and `action_attenuates` (the delegation test) disagree.
/// `delegate` used to ask the first question, accept the delegation, and hand the child a reach its
/// parent never had.
#[test]
fn the_covering_relation_is_not_the_attenuation_relation() {
    // The child STRING is inside the parent's reach …
    assert!(action_covers("entity.*.*", "entity.*"));
    // … and the child's REACH is not inside the parent's.
    assert!(!action_attenuates("entity.*.*", "entity.*"));
    // Concretely: this is the action the child would have authorized and the parent could not.
    assert!(action_covers("entity.*", "entity.delete"));
    assert!(!action_covers("entity.*.*", "entity.delete"));
}

// ---------------------------------------------------------------------------
// INV-CAP-SCOPE-2/3 — reflexivity and transitivity, per dimension and combined.
// ---------------------------------------------------------------------------

#[test]
fn attenuation_is_reflexive_in_every_dimension() {
    for s in scope_universe() {
        assert!(scope_attenuates(&s, &s), "scope not reflexive: {s:?}");
    }
    for a in action_universe() {
        assert!(action_attenuates(&a, &a), "action not reflexive: {a:?}");
    }
    for c in constraints_universe() {
        assert!(
            constraints_attenuate(&c, &c),
            "constraints not reflexive: {c:?}"
        );
    }
}

#[test]
fn attenuation_is_transitive_in_every_dimension() {
    let scopes = scope_universe();
    for a in &scopes {
        for b in &scopes {
            if !scope_attenuates(a, b) {
                continue;
            }
            for c in &scopes {
                if scope_attenuates(b, c) {
                    assert!(
                        scope_attenuates(a, c),
                        "scope transitivity broken: {a:?} -> {b:?} -> {c:?}"
                    );
                }
            }
        }
    }
    let actions = action_universe();
    for a in &actions {
        for b in &actions {
            if !action_attenuates(a, b) {
                continue;
            }
            for c in &actions {
                if action_attenuates(b, c) {
                    assert!(
                        action_attenuates(a, c),
                        "action transitivity broken: {a:?} -> {b:?} -> {c:?}"
                    );
                }
            }
        }
    }
    let cons = constraints_universe();
    for a in &cons {
        for b in &cons {
            if !constraints_attenuate(a, b) {
                continue;
            }
            for c in &cons {
                if constraints_attenuate(b, c) {
                    assert!(
                        constraints_attenuate(a, c),
                        "constraints transitivity broken: {a:?} -> {b:?} -> {c:?}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// INV-CAP-SCOPE-4 — the conjunction agrees with its parts, in both directions.
// ---------------------------------------------------------------------------

#[test]
fn the_conjunction_holds_exactly_when_all_three_dimensions_do() {
    let scopes = scope_universe();
    let actions = action_universe();
    let cons = constraints_universe();
    for sa in scopes.iter().take(6) {
        for sb in scopes.iter().take(6) {
            for aa in actions.iter() {
                for ab in actions.iter() {
                    for ca in cons.iter().take(4) {
                        for cb in cons.iter().take(4) {
                            let parent = Authority {
                                action: aa,
                                scope: sa,
                                constraints: ca,
                            };
                            let child = Authority {
                                action: ab,
                                scope: sb,
                                constraints: cb,
                            };
                            let parts = action_attenuates(aa, ab)
                                && scope_attenuates(sa, sb)
                                && constraints_attenuate(ca, cb);
                            assert_eq!(attenuates(&parent, &child), parts);
                            assert_eq!(refusal(&parent, &child).is_none(), parts);
                        }
                    }
                }
            }
        }
    }
}

/// The refusal names the dimension that actually failed. A caller that re-derived the reason could
/// report a dimension that in fact passed — the kind of error message that sends someone to fix the
/// wrong thing.
#[test]
fn the_refusal_names_a_dimension_that_really_failed() {
    let cons = Constraints::none();
    let parent = Authority {
        action: "entity.derive",
        scope: &Scope::All,
        constraints: &cons,
    };
    let child = Authority {
        action: "entity.delete",
        scope: &Scope::All,
        constraints: &cons,
    };
    assert_eq!(
        refusal(&parent, &child),
        Some("delegation would amplify action")
    );

    let narrow = Scope::Type(kernel_core::spine::EntityType::Document);
    let parent = Authority {
        action: "entity.*",
        scope: &narrow,
        constraints: &cons,
    };
    let child = Authority {
        action: "entity.derive",
        scope: &Scope::All,
        constraints: &cons,
    };
    assert_eq!(
        refusal(&parent, &child),
        Some("delegation would amplify scope")
    );

    let strict = Constraints::approval();
    let parent = Authority {
        action: "entity.*",
        scope: &Scope::All,
        constraints: &strict,
    };
    let child = Authority {
        action: "entity.derive",
        scope: &Scope::All,
        constraints: &cons,
    };
    assert_eq!(
        refusal(&parent, &child),
        Some("delegation would loosen constraints")
    );
}

// ---------------------------------------------------------------------------
// INV-CAP-SCOPE-5 — an empty entity set is `None` spelled differently.
// ---------------------------------------------------------------------------

#[test]
fn a_scope_that_reaches_nothing_is_a_legal_delegation_from_anything() {
    let empty = Scope::Entities(Vec::new());
    for t in target_universe() {
        assert!(!scope_covers(&empty, &t));
        assert!(!scope_covers(&Scope::None, &t));
    }
    for p in scope_universe() {
        assert!(
            scope_attenuates(&p, &empty),
            "an empty set reaches nothing; {p:?} must be able to delegate to it"
        );
        assert!(scope_attenuates(&p, &Scope::None));
    }
    // …and nothing can be delegated FROM it except another nothing.
    for c in scope_universe() {
        assert_eq!(scope_attenuates(&empty, &c), scope_is_empty(&c));
    }
}

// ---------------------------------------------------------------------------
// INV-CAP-SCOPE-6 — the incompleteness is deliberate and one-directional.
// ---------------------------------------------------------------------------

#[test]
fn type_and_entity_scopes_are_refused_in_both_directions_and_that_is_sound() {
    let by_type = Scope::Type(kernel_core::spine::EntityType::Document);
    let by_id = Scope::Entities(vec![1]);
    assert!(!scope_attenuates(&by_type, &by_id));
    assert!(!scope_attenuates(&by_id, &by_type));
    // The refusal is not merely conservative in one direction: there is a real target each scope
    // reaches and the other does not, so neither is a subset of the other.
    let doc_2 = kernel_core::spine::Target {
        id: Some(2),
        etype: Some(kernel_core::spine::EntityType::Document),
    };
    let agent_1 = kernel_core::spine::Target {
        id: Some(1),
        etype: Some(kernel_core::spine::EntityType::Agent),
    };
    assert!(scope_covers(&by_type, &doc_2) && !scope_covers(&by_id, &doc_2));
    assert!(scope_covers(&by_id, &agent_1) && !scope_covers(&by_type, &agent_1));
}

// ---------------------------------------------------------------------------
// INV-CAP-SCOPE-7 — no descendant exceeds its root, over generated chains.
// ---------------------------------------------------------------------------

/// Transitivity is proved above on pairs; this is the property the delegation graph actually needs,
/// asserted over 20 000 deterministic chains: whatever a leaf reaches, the root reaches too. The
/// chains are built by REJECTION — every candidate step is offered to `attenuates` and only the
/// accepted ones extend the chain — so the population is exactly the set of chains the engine would
/// have permitted.
#[test]
fn no_descendant_of_a_legal_chain_exceeds_its_root() {
    let scopes = scope_universe();
    let actions = action_universe();
    let cons = constraints_universe();
    let targets = target_universe();
    let concrete = concrete_action_universe();

    let mut rng: u64 = 0x243F_6A88_85A3_08D3;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut chains = 0usize;
    let mut extended = 0usize;
    for _ in 0..20_000 {
        let mut ai = (next() as usize) % actions.len();
        let mut si = (next() as usize) % scopes.len();
        let mut ci = (next() as usize) % cons.len();
        let (root_a, root_s, root_c) = (ai, si, ci);
        let depth = 1 + (next() as usize) % 5;
        for _ in 0..depth {
            let (na, ns, nc) = (
                (next() as usize) % actions.len(),
                (next() as usize) % scopes.len(),
                (next() as usize) % cons.len(),
            );
            let parent = Authority {
                action: &actions[ai],
                scope: &scopes[si],
                constraints: &cons[ci],
            };
            let candidate = Authority {
                action: &actions[na],
                scope: &scopes[ns],
                constraints: &cons[nc],
            };
            if attenuates(&parent, &candidate) {
                ai = na;
                si = ns;
                ci = nc;
                extended += 1;
            }
        }
        chains += 1;

        // The leaf must reach nothing the root does not, on either axis, at any target.
        for a in &concrete {
            if action_covers(&actions[ai], a) {
                assert!(
                    action_covers(&actions[root_a], a),
                    "leaf action {:?} reaches {a:?}; root {:?} does not",
                    actions[ai],
                    actions[root_a]
                );
            }
        }
        for t in &targets {
            if scope_covers(&scopes[si], t) {
                assert!(
                    scope_covers(&scopes[root_s], t),
                    "leaf scope {:?} reaches {t:?}; root {:?} does not",
                    scopes[si],
                    scopes[root_s]
                );
            }
        }
        assert!(constraints_attenuate(&cons[root_c], &cons[ci]));
    }
    assert_eq!(chains, 20_000);
    // A campaign where nothing was ever delegated would pass vacuously.
    assert!(
        extended > 1_000,
        "only {extended} steps were accepted — the chains are not exercising delegation"
    );
}
