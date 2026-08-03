//! Capability concurrency semantics (GAPS2 gap #9 — "the capability model needs a formal
//! concurrency specification before SMP"). ADR-027.
//!
//! The single-core kernel is safe today only because Rust's borrow checker serializes access: a
//! `&self` `evaluate` and a `&mut self` `revoke` cannot overlap in one thread. SMP breaks that
//! assumption — two cores can hold the engine behind a lock and interleave. The classic bug:
//!
//! ```text
//!   CPU 0: evaluate(cap) -> Allow          (time-of-check)
//!   CPU 1: revoke(cap)                      (interleaves in the gap)
//!   CPU 0: execute()                        (time-of-use — acts on a now-dead capability)
//! ```
//!
//! ADR-027 specifies the guarantee (Option A: authorization and effect commit inside ONE critical
//! section) and `CapEngine::with_authorization` implements it. This suite is the executable proof:
//! it first shows the naive `check(); …; act();` pattern is stale by construction, then hammers the
//! disciplined primitive under real `std::thread` contention and asserts the effect never commits
//! under a revoked capability, and that revocation is permanent (no authority resurrection).
//!
//! Honesty (STATUS/TRACEABILITY): this proves the MECHANISM under host threads. It does NOT prove an
//! SMP-safe kernel — none exists yet. Wiring `with_authorization` into each target's real trap path
//! is the SMP integration deferred under gap #4 (REQ-SMP-001).

use kernel_core::spine::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, RwLock};
use std::thread;

fn engine_with_cap() -> (CapEngine, CapToken) {
    let mut e = CapEngine::new(0xBEEF_CAFE, 1_000);
    let cap = e.mint("svc", "entity.write", Scope::All, Constraints::none());
    (e, cap)
}

// ---------------------------------------------------------------------------
// The gap exists — naive check-then-act is stale by construction.
// ---------------------------------------------------------------------------

#[test]
fn naive_check_then_act_is_stale_by_construction() {
    // Deterministic model of the interleaving above, sequenced in one thread: authorize, then a
    // revoke lands in the gap, then the naive code would act on the earlier `Allow`.
    let (mut e, cap) = engine_with_cap();

    // time-of-check
    let decision = e.evaluate("entity.write", &Target::default(), &[cap]);
    assert_eq!(decision, Decision::Allow);

    // a concurrent revoke lands in the gap between check and use
    e.revoke(cap);

    // time-of-use: the capability is now dead, yet the decision the naive caller is holding still
    // says `Allow`. Acting on `decision` here is the stale-authorization bug gap #9 warns about —
    // the stored verdict cannot see the revoke that happened after it was computed.
    assert!(e.is_revoked(cap), "revoke landed in the check→use gap");
    assert_eq!(
        decision,
        Decision::Allow,
        "the stale decision is unaware of the revoke"
    );
}

// ---------------------------------------------------------------------------
// authorize() reports which token matched (evaluate discards this).
// ---------------------------------------------------------------------------

#[test]
fn authorize_reports_the_matching_token() {
    let (mut e, _broad) = engine_with_cap();
    // A second, narrower cap that is the one actually covering a Document write.
    let narrow = e.mint(
        "svc",
        "entity.write",
        Scope::Type(EntityType::Document),
        Constraints::none(),
    );
    let t = Target {
        id: None,
        etype: Some(EntityType::Document),
    };
    match e.authorize("entity.write", &t, &[narrow]) {
        AuthOutcome::Allow(a) => assert_eq!(a.capability(), narrow, "must name the matching token"),
        other => panic!("expected Allow, got {other:?}"),
    }

    // Fail-closed mirrors evaluate: no offered cap ⇒ Deny.
    assert!(matches!(
        e.authorize("entity.write", &t, &[]),
        AuthOutcome::Deny(_)
    ));
}

// ---------------------------------------------------------------------------
// with_authorization is fail-closed: the effect runs iff Allow.
// ---------------------------------------------------------------------------

#[test]
fn with_authorization_commits_only_when_authorized() {
    let (mut e, cap) = engine_with_cap();
    let ran = AtomicUsize::new(0);

    // Allow ⇒ effect runs exactly once, Ok returned.
    let out = e.with_authorization("entity.write", &Target::default(), &[cap], |_eng, _a| {
        ran.fetch_add(1, Ordering::SeqCst);
        42
    });
    assert_eq!(out, Ok(42));
    assert_eq!(ran.load(Ordering::SeqCst), 1);

    // After revoke ⇒ effect does NOT run, Err(Deny) returned (fail-closed).
    e.revoke(cap);
    let out = e.with_authorization("entity.write", &Target::default(), &[cap], |_eng, _a| {
        ran.fetch_add(1, Ordering::SeqCst);
        99
    });
    assert!(matches!(out, Err(Decision::Deny(_))));
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "revoked cap must not run the effect"
    );
}

// ---------------------------------------------------------------------------
// The disciplined primitive holds under real thread contention.
// ---------------------------------------------------------------------------

#[test]
fn with_authorization_never_acts_on_a_revoked_capability_under_contention() {
    const COMMITTERS: usize = 4;
    const ATTEMPTS: usize = 4_000;

    let (e0, cap) = engine_with_cap();
    let eng = Arc::new(RwLock::new(e0));
    let violations = Arc::new(AtomicUsize::new(0));
    let commits = Arc::new(AtomicUsize::new(0));
    // COMMITTERS committer threads + 1 revoker all released together.
    let go = Arc::new(Barrier::new(COMMITTERS + 1));

    let mut handles = Vec::new();
    for _ in 0..COMMITTERS {
        let (e, v, c, b) = (eng.clone(), violations.clone(), commits.clone(), go.clone());
        handles.push(thread::spawn(move || {
            b.wait();
            for _ in 0..ATTEMPTS {
                // Hold the READ lock across the whole authorize+effect (this is the discipline).
                let guard = e.read().unwrap();
                let _ = guard.with_authorization(
                    "entity.write",
                    &Target::default(),
                    &[cap],
                    |inner, _auth| {
                        // Inside the atomic section under the read lock, a concurrent revoke (which
                        // needs the WRITE lock) cannot be in progress — so the cap MUST still be
                        // live here. Observing it revoked would mean the atomicity guarantee broke.
                        if inner.is_revoked(cap) {
                            v.fetch_add(1, Ordering::SeqCst);
                        }
                        c.fetch_add(1, Ordering::SeqCst);
                    },
                );
                // guard drops here — the revoker's window is strictly BETWEEN attempts, never
                // inside one; that is exactly what makes each authorize→commit atomic.
            }
        }));
    }

    // Revoker: wait until committers have DEMONSTRABLY taken the Allow path (progress observed on
    // the shared counter, not a fixed spin that races thread wakeup and can let revoke win before
    // any committer starts), THEN revoke — concurrently with committers still looping.
    let (e, c, b) = (eng.clone(), commits.clone(), go.clone());
    let revoker = thread::spawn(move || {
        b.wait();
        while c.load(Ordering::SeqCst) < 100 {
            std::hint::spin_loop();
        }
        e.write().unwrap().revoke(cap);
    });

    for h in handles {
        h.join().unwrap();
    }
    revoker.join().unwrap();

    assert_eq!(
        violations.load(Ordering::SeqCst),
        0,
        "with_authorization committed under a revoked capability — atomicity broken"
    );
    assert!(
        commits.load(Ordering::SeqCst) > 0,
        "harness never exercised the Allow path (no head start?)"
    );
    // The cap is revoked now; a fresh disciplined attempt is fail-closed.
    assert!(matches!(
        eng.read().unwrap().with_authorization(
            "entity.write",
            &Target::default(),
            &[cap],
            |_e, _a| ()
        ),
        Err(Decision::Deny(_))
    ));
}

// ---------------------------------------------------------------------------
// Revocation is permanent under concurrency — authority is never resurrected.
// ---------------------------------------------------------------------------

#[test]
fn revocation_is_permanent_no_authority_resurrection_under_contention() {
    const COMMITTERS: usize = 4;
    const ATTEMPTS: usize = 4_000;

    let (e0, cap) = engine_with_cap();
    let eng = Arc::new(RwLock::new(e0));
    // Set by the revoker AFTER revoke() returns (its write lock is released). A committer that
    // observes this flag knows the revoke has completed (release/acquire happens-before), so any
    // subsequent authorize MUST deny — otherwise authority was resurrected.
    let revoke_done = Arc::new(AtomicBool::new(false));
    let allow_after_revoke = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(Barrier::new(COMMITTERS + 1));

    let mut handles = Vec::new();
    for _ in 0..COMMITTERS {
        let (e, done, bad, att, b) = (
            eng.clone(),
            revoke_done.clone(),
            allow_after_revoke.clone(),
            attempts.clone(),
            go.clone(),
        );
        handles.push(thread::spawn(move || {
            b.wait();
            for _ in 0..ATTEMPTS {
                att.fetch_add(1, Ordering::SeqCst);
                let seen_done = done.load(Ordering::Acquire);
                let outcome = {
                    let guard = e.read().unwrap();
                    guard.authorize("entity.write", &Target::default(), &[cap])
                };
                if seen_done && matches!(outcome, AuthOutcome::Allow(_)) {
                    bad.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    // Revoke mid-run — once committers are ~halfway — so a substantial number of authorizes happen
    // AFTER the revoke completes (deterministic overlap, not timing-dependent), making the
    // no-resurrection assertion meaningful rather than vacuous.
    let (e, done, att, b) = (
        eng.clone(),
        revoke_done.clone(),
        attempts.clone(),
        go.clone(),
    );
    let revoker = thread::spawn(move || {
        b.wait();
        while att.load(Ordering::SeqCst) < COMMITTERS * ATTEMPTS / 2 {
            std::hint::spin_loop();
        }
        e.write().unwrap().revoke(cap);
        done.store(true, Ordering::Release);
    });

    for h in handles {
        h.join().unwrap();
    }
    revoker.join().unwrap();

    assert_eq!(
        allow_after_revoke.load(Ordering::SeqCst),
        0,
        "authorize returned Allow after a completed revoke — authority resurrected"
    );
}

// ---------------------------------------------------------------------------
// INV-CAP-REVOKE contract (docs/INVARIANT-CONTRACTS.md) — adversarial cases, ALET-P1-025.
//
// The tests above prove authorize+execute is atomic against a revoke. These attack the REVOCATION
// side: what a revoker is entitled to conclude once `revoke` has returned.
// ---------------------------------------------------------------------------

/// The action `engine_with_cap` mints authority for.
const ACTION: &str = "entity.write";

/// INV-CAP-REVOKE-1 + INV-CAP-REVOKE-3: once `revoke` returns, no later attempt can act — and
/// revoking again, or revoking a forged handle, is a no-op that grants nothing.
#[test]
fn after_revoke_returns_no_later_attempt_can_ever_act() {
    let (mut engine, cap) = engine_with_cap();
    let mut effects = 0usize;
    // Before: the capability works.
    engine
        .with_authorization(ACTION, &Target::default(), &[cap], |_, _| effects += 1)
        .expect("authorized before revoke");
    assert_eq!(effects, 1);

    engine.revoke(cap);

    // After: every attempt, however many times, is denied and performs NO effect.
    for _ in 0..50 {
        let outcome = engine.with_authorization(ACTION, &Target::default(), &[cap], |_, _| {
            effects += 1;
        });
        assert!(
            outcome.is_err(),
            "INV-CAP-REVOKE-1: a revoked capability authorized an effect"
        );
    }
    assert_eq!(
        effects, 1,
        "INV-CAP-REVOKE-1: {} effects ran after revoke returned",
        effects - 1
    );
    // Idempotent, and a forged token is not a channel.
    engine.revoke(cap);
    engine.revoke(CapToken::forge_for_test(0xDEAD_BEEF));
    assert!(engine.is_revoked(cap));
    let outcome = engine.with_authorization(
        ACTION,
        &Target::default(),
        &[CapToken::forge_for_test(0xDEAD_BEEF)],
        |_, _| effects += 1,
    );
    assert!(outcome.is_err(), "a forged token must never authorize");
    assert_eq!(effects, 1, "INV-CAP-REVOKE-3: a no-op revoke had an effect");
}

/// INV-CAP-REVOKE-2: revocation is permanent. Re-presenting the token, delegating from it, or minting
/// a fresh capability afterwards must never make the REVOKED token authoritative again.
#[test]
fn a_revoked_token_is_never_authoritative_again_however_it_is_presented() {
    let (mut engine, cap) = engine_with_cap();
    engine.revoke(cap);

    // Delegating from a revoked parent must fail — otherwise revocation is bypassable by one hop.
    let child = engine.delegate(cap, "child", ACTION, Scope::All, Constraints::none());
    assert!(
        child.is_err(),
        "INV-CAP-REVOKE-2: a revoked capability could still be delegated"
    );

    // A fresh mint is a DIFFERENT capability: it works, and it does not revive the old handle.
    let fresh = engine.mint("subject", ACTION, Scope::All, Constraints::none());
    assert_ne!(fresh, cap, "a fresh mint must not reuse a revoked id");
    let mut effects = 0usize;
    engine
        .with_authorization(ACTION, &Target::default(), &[fresh], |_, _| effects += 1)
        .expect("the fresh capability authorizes");
    assert_eq!(effects, 1);
    assert!(
        engine
            .with_authorization(ACTION, &Target::default(), &[cap], |_, _| effects += 1)
            .is_err(),
        "INV-CAP-REVOKE-2: the revoked token became authoritative again"
    );
    // Offering the revoked token ALONGSIDE a good one must not launder it: the effect runs, but the
    // authorization must be attributed to the live capability.
    let auth = engine
        .with_authorization(ACTION, &Target::default(), &[cap, fresh], |_, a| a.capability())
        .expect("a live capability in the set still authorizes");
    assert_eq!(
        auth, fresh,
        "INV-CAP-REVOKE-2: a revoked token was reported as the authorizing capability"
    );
}

/// INV-CAP-REVOKE-4: revoking a parent kills every descendant, transitively — a grandchild is not a
/// loophole.
#[test]
fn revoking_a_parent_kills_every_descendant_transitively() {
    let (mut engine, root) = engine_with_cap();
    let child = engine
        .delegate(root, "child", ACTION, Scope::All, Constraints::none())
        .expect("delegate child");
    let grandchild = engine
        .delegate(child, "grandchild", ACTION, Scope::All, Constraints::none())
        .expect("delegate grandchild");
    // All three work first, so the test cannot pass by them never having worked.
    for tok in [root, child, grandchild] {
        engine
            .with_authorization(ACTION, &Target::default(), &[tok], |_, _| ())
            .expect("authorized before revoke");
    }

    engine.revoke(root);

    for (name, tok) in [("root", root), ("child", child), ("grandchild", grandchild)] {
        assert!(
            engine.is_revoked(tok),
            "INV-CAP-REVOKE-4: {name} survived its ancestor's revocation"
        );
        let mut effects = 0usize;
        assert!(
            engine
                .with_authorization(ACTION, &Target::default(), &[tok], |_, _| effects += 1)
                .is_err(),
            "INV-CAP-REVOKE-4: {name} still authorizes after the root was revoked"
        );
        assert_eq!(effects, 0);
    }
}

/// INV-CAP-REVOKE-5: a revoke interleaved with an in-flight authorize+execute yields a CLEAN before or
/// after — the effect either completed under a live capability or never ran. Swept over every
/// interleaving position: the revoke happens at step k of an n-step commit body, for every k.
#[test]
fn an_interleaved_revoke_yields_a_clean_before_or_after_never_a_partial() {
    const STEPS: usize = 6;
    // Every position INSIDE the body — `revoke_at == STEPS` would mean "no revoke at all", which is
    // the plain authorized case the tests above already cover.
    for revoke_at in 0..STEPS {
        let (mut engine, cap) = engine_with_cap();
        // The commit body writes STEPS journal entries; a "partial" would be 1..STEPS-1 of them.
        let mut journal: Vec<usize> = Vec::new();
        // The revoke is applied by a hook the body calls at step `revoke_at`, mid-effect. Because
        // `with_authorization` borrows the engine immutably, the revoke is staged and applied after —
        // which is exactly the linearization claim: the effect is ordered BEFORE the revoke.
        let mut revoke_requested = false;
        let outcome = engine.with_authorization(ACTION, &Target::default(), &[cap], |_, _| {
            for step in 0..STEPS {
                if step == revoke_at {
                    revoke_requested = true;
                }
                journal.push(step);
            }
            journal.len()
        });
        if revoke_requested {
            engine.revoke(cap);
        }
        match outcome {
            Ok(n) => assert_eq!(
                n, STEPS,
                "INV-CAP-REVOKE-5: revoke_at={revoke_at} produced a PARTIAL effect ({n} of {STEPS})"
            ),
            Err(_) => assert!(
                journal.is_empty(),
                "INV-CAP-REVOKE-5: a denied authorization still wrote {} entries",
                journal.len()
            ),
        }
        // And after the revoke landed, nothing more can act.
        let mut after = 0usize;
        assert!(
            engine
                .with_authorization(ACTION, &Target::default(), &[cap], |_, _| after += 1)
                .is_err(),
            "INV-CAP-REVOKE-1: acted after revoke (revoke_at={revoke_at})"
        );
        assert_eq!(after, 0);
    }
}

/// INV-CAP-REVOKE-6: revoking one capability never disturbs its siblings. Over-broad revocation is an
/// availability bug that pushes callers toward asking for broader capabilities.
#[test]
fn revoking_one_capability_never_disturbs_its_siblings() {
    let (mut engine, root) = engine_with_cap();
    let siblings: Vec<CapToken> = (0..5)
        .map(|i| {
            engine
                .delegate(
                    root,
                    &format!("sib{i}"),
                    ACTION,
                    Scope::All,
                    Constraints::none(),
                )
                .expect("delegate sibling")
        })
        .collect();

    engine.revoke(siblings[2]);

    for (i, tok) in siblings.iter().enumerate() {
        let mut effects = 0usize;
        let outcome =
            engine.with_authorization(ACTION, &Target::default(), &[*tok], |_, _| effects += 1);
        if i == 2 {
            assert!(outcome.is_err(), "the revoked sibling must be denied");
            assert_eq!(effects, 0);
        } else {
            assert!(
                outcome.is_ok(),
                "INV-CAP-REVOKE-6: sibling {i} lost authority when sibling 2 was revoked"
            );
            assert_eq!(effects, 1);
        }
    }
    // The parent is untouched too — revocation runs downward, not upward.
    assert!(!engine.is_revoked(root), "INV-CAP-REVOKE-6: revoking a child revoked its parent");
    engine
        .with_authorization(ACTION, &Target::default(), &[root], |_, _| ())
        .expect("the parent still authorizes");
}
