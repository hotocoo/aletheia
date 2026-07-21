//! Arch-independent invariant suite, proved on the HOST.
//!
//! The capability-secure spine now lives in `kernel-core` (gap-register Issue 1), so its invariants
//! are arch-independent by construction — the exact same source compiles for aarch64, x86-64, and
//! RISC-V. This suite proves them in a fast hosted `cargo test` with no QEMU, complementing (not
//! replacing) the per-target QEMU VM gates that prove the SAME `selftest::run()` boots and holds on
//! real emulated CPUs. Issue 1 acceptance criterion: "architecture-independent invariants run in
//! hosted tests."
//!
//! Two layers:
//!   1. `whole_suite_*` — runs the SINGLE shared `selftest::run()` (the identical function the three
//!      kernels call at boot) and asserts all 11 invariants hold, capturing the per-check reports.
//!   2. `invariant_*` — named, granular host tests over the spine API, so a regression names the
//!      exact broken property instead of a bare index. These are the M1 acceptance criteria.

use kernel_core::selftest;
use kernel_core::spine::*;

// ---------------------------------------------------------------------------
// Layer 1 — the whole shared suite, host-run with a capturing logger.
// ---------------------------------------------------------------------------

#[test]
fn whole_suite_all_invariants_hold_on_host() {
    // Capture every (index, passed, name) the shared suite reports — the same closure shape each
    // kernel target passes, but here we record instead of printing.
    let mut reported: Vec<(u32, bool, &'static str)> = Vec::new();
    let result = selftest::run(|n, passed, name| reported.push((n, passed, name)));

    assert_eq!(result, Ok(11), "all 11 shared spine invariants must hold on the host");
    assert_eq!(reported.len(), 11, "every check must report exactly once");
    assert!(
        reported.iter().all(|(_, passed, _)| *passed),
        "no check may report a failure: {reported:?}"
    );
    // Indices are dense 1..=11 in order — the same numbering the VM gate maps to exit 10+idx.
    for (i, (n, _, _)) in reported.iter().enumerate() {
        assert_eq!(*n as usize, i + 1, "check indices must be dense and in order");
    }
}

#[test]
fn whole_suite_reports_before_returning() {
    // The logger must be invoked for a check BEFORE run() can return that check's verdict — the
    // property the VM gate relies on to attribute a failing exit code to a specific invariant.
    let mut count = 0u32;
    let _ = selftest::run(|_, _, _| count += 1);
    assert_eq!(count, 11);
}

// ---------------------------------------------------------------------------
// Layer 2 — granular, named invariants (M1 acceptance) over the spine API.
// ---------------------------------------------------------------------------

fn derive_plan(source: u64) -> Plan {
    Plan {
        steps: vec![Step {
            op: "derive_summary".into(),
            source,
            content: "tldr".into(),
        }],
    }
}

#[test]
fn invariant_fail_closed_no_capability_denies() {
    let e = CapEngine::new(0xA5A5, 1000);
    let d = e.evaluate("entity.derive", &Target::default(), &[]);
    assert!(matches!(d, Decision::Deny(_)));
}

#[test]
fn invariant_authorized_pipeline_verifies_and_records_event() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut s = Store::new();
    let doc = s.put(EntityType::Document, "hello world", "user");
    let cap = e.mint(
        "user",
        "entity.derive",
        Scope::Type(EntityType::Document),
        Constraints::none(),
    );
    let r = run_pipeline(&e, &mut s, "user", &derive_plan(doc), &[cap]);
    assert!(r.ok && r.executed && r.verified);
    assert_eq!(s.event_count(), 1, "an immutable event is recorded only after verified success");
}

#[test]
fn invariant_forged_token_is_not_authority() {
    let e = CapEngine::new(0xA5A5, 1000);
    let forged = CapToken::forge_for_test(0xDEAD_BEEF);
    let d = e.evaluate("entity.derive", &Target::default(), &[forged]);
    assert!(matches!(d, Decision::Deny(_)), "a fabricated handle absent from the registry is denied");
}

#[test]
fn invariant_delegation_attenuates_but_never_amplifies() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
    // equal-or-narrower is allowed
    assert!(e
        .delegate(root, "agent", "entity.derive", Scope::Type(EntityType::Document), Constraints::none())
        .is_ok());
    // a narrow cap cannot be delegated into a broader action/scope
    let narrow = e.mint(
        "user",
        "entity.derive",
        Scope::Type(EntityType::Document),
        Constraints::none(),
    );
    assert!(e
        .delegate(narrow, "agent", "entity.delete", Scope::All, Constraints::none())
        .is_err());
}

#[test]
fn invariant_revocation_cascades_to_descendants() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let root = e.mint("user", "entity.*", Scope::All, Constraints::none());
    let child = e
        .delegate(root, "agent", "entity.derive", Scope::All, Constraints::none())
        .unwrap();
    e.revoke(root);
    assert!(e.is_revoked(child), "revoking a parent revokes its children transitively");
    let d = e.evaluate("entity.derive", &Target::default(), &[child]);
    assert!(matches!(d, Decision::Deny(_)));
}

#[test]
fn invariant_malformed_plan_cannot_execute() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut s = Store::new();
    let doc = s.put(EntityType::Document, "x", "u");
    let cap = e.mint("u", "entity.derive", Scope::All, Constraints::none());
    let bad = Plan {
        steps: vec![Step { op: "rm -rf /".into(), source: doc, content: "".into() }],
    };
    let r = run_pipeline(&e, &mut s, "u", &bad, &[cap]);
    assert!(!r.ok && !r.executed && r.validation == "rejected");
    assert_eq!(s.event_count(), 0, "a rejected plan records no event");
}

#[test]
fn invariant_expired_capability_denied() {
    let mut e = CapEngine::new(0xA5A5, 5000);
    let cap = e.mint(
        "u",
        "entity.derive",
        Scope::All,
        Constraints { expires_at: Some(1000), approval_required: false, local_only: true },
    );
    assert!(matches!(e.evaluate("entity.derive", &Target::default(), &[cap]), Decision::Deny(_)));
}

#[test]
fn invariant_scope_confinement() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("u", "entity.derive", Scope::Entities(vec![0x1000]), Constraints::none());
    let d = e.evaluate(
        "entity.derive",
        &Target { id: Some(0x2000), etype: Some(EntityType::Document) },
        &[cap],
    );
    assert!(matches!(d, Decision::Deny(_)), "a cap scoped to entity A does not authorize entity B");
}

#[test]
fn invariant_secure_ipc_is_capability_gated() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut ch = Channel::new("ipc.send");
    // unauthorized send is dropped fail-closed; receiver never observes it
    let d0 = ch.send(&e, Message { from: "A".into(), to: "B".into(), body: 1 }, &[]);
    assert!(matches!(d0, Decision::Deny(_)) && ch.recv().is_none());
    // authorized send is delivered
    let cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let d1 = ch.send(&e, Message { from: "A".into(), to: "B".into(), body: 2 }, &[cap]);
    assert_eq!(d1, Decision::Allow);
    assert!(ch.recv().is_some());
}

#[test]
fn invariant_destructive_action_requires_approval() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("u", "entity.delete", Scope::All, Constraints::approval());
    assert_eq!(e.evaluate("entity.delete", &Target::default(), &[cap]), Decision::RequireApproval);
}

// ---------------------------------------------------------------------------
// Content addressing — arch-independent hashing the store relies on.
// ---------------------------------------------------------------------------

#[test]
fn invariant_content_hash_is_deterministic_and_distinguishing() {
    assert_eq!(content_hash(b"alpha"), content_hash(b"alpha"), "same bytes -> same hash");
    assert_ne!(content_hash(b"alpha"), content_hash(b"beta"), "different bytes -> different hash");
}
