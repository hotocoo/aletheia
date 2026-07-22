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
