//! Security-behaviour regression suite (gap-register Issue 11: "expand testing from invariant
//! validation to system behaviour", security category).
//!
//! Where `invariants.rs` proves the M1 acceptance criteria hold, this suite attacks the capability
//! engine the way a real adversary would: confused-deputy, capability laundering, TOCTOU / stale
//! capability reuse, and cross-principal leakage. Each test is a permanent regression — if a future
//! refactor reintroduces ambient authority, an authorization cache, or an amplifying delegation path,
//! exactly one named test breaks and says why. All are hosted (no QEMU), so they run in the fast
//! `cargo test` gate and in CI.

use kernel_core::spine::*;

fn derive_plan(source: u64) -> Plan {
    Plan {
        steps: vec![Step {
            op: "derive_summary".into(),
            source,
            content: "tldr".into(),
        }],
    }
}

// ---------------------------------------------------------------------------
// Confused deputy — a privileged actor cannot have its authority borrowed.
// ---------------------------------------------------------------------------

#[test]
fn confused_deputy_ambient_authority_is_impossible() {
    // A privileged "service" holds a broad capability. A client offers only its OWN (narrow) authority.
    // Because `evaluate` consults ONLY the offered tokens — never any ambient/registered authority — the
    // service's broad cap can never be borrowed to satisfy a client request it did not authorize.
    let mut e = CapEngine::new(0xA5A5, 1000);
    let _service_broad = e.mint("service", "entity.*", Scope::All, Constraints::none());
    let client_narrow = e.mint(
        "client",
        "entity.derive",
        Scope::Type(EntityType::Document),
        Constraints::none(),
    );

    // The client asks for a destructive action it was never granted. The broad service cap EXISTS in
    // the engine, but is not offered — so the request is denied. No confused deputy.
    let deny = e.evaluate("entity.delete", &Target::default(), &[client_narrow]);
    assert!(matches!(deny, Decision::Deny(_)), "ambient service authority must not leak to a client");

    // And a request with NO offered caps is denied even though broad authority exists in the engine.
    let deny_empty = e.evaluate("entity.delete", &Target::default(), &[]);
    assert!(matches!(deny_empty, Decision::Deny(_)), "no ambient authority: empty offer => deny");
}

#[test]
fn deputy_must_forward_the_clients_capability_not_its_own() {
    // The pipeline authorizes against the caps OFFERED for THAT request. A deputy acting on behalf of a
    // client forwards the client's cap; only then does it succeed, and only within the client's scope.
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut s = Store::new();
    let doc = s.put(EntityType::Document, "hello", "client");
    let client_cap = e.mint(
        "client",
        "entity.derive",
        Scope::Type(EntityType::Document),
        Constraints::none(),
    );
    // Deputy forwards the client's capability -> authorized, exactly within the client's grant.
    let r = run_pipeline(&e, &mut s, "deputy-for-client", &derive_plan(doc), &[client_cap]);
    assert!(r.ok && r.verified, "forwarding the client's own capability authorizes the request");
}

// ---------------------------------------------------------------------------
// Capability laundering — you cannot mint fresh authority from stale/invalid authority.
// ---------------------------------------------------------------------------

#[test]
fn cannot_launder_authority_from_a_revoked_parent() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let root = e.mint("A", "entity.*", Scope::All, Constraints::none());
    e.revoke(root);
    // Delegating from a revoked capability must fail — no fresh, unrevoked child can be laundered out.
    let laundered = e.delegate(root, "B", "entity.derive", Scope::All, Constraints::none());
    assert!(laundered.is_err(), "a revoked parent cannot mint a valid child");
}

#[test]
fn cannot_launder_a_longer_lived_capability_from_an_expired_one() {
    // now (5000) is past the parent's expiry (1000): the parent is expired.
    let mut e = CapEngine::new(0xA5A5, 5000);
    let expired = e.mint(
        "A",
        "entity.derive",
        Scope::All,
        Constraints { expires_at: Some(1000), approval_required: false, local_only: true },
    );
    // Attempt to launder a fresh, longer-lived (or never-expiring) child out of the expired parent.
    let longer = e.delegate(
        expired,
        "B",
        "entity.derive",
        Scope::All,
        Constraints { expires_at: Some(9000), approval_required: false, local_only: true },
    );
    assert!(longer.is_err(), "a child cannot outlive its parent (constraints must not loosen)");
    let never = e.delegate(expired, "B", "entity.derive", Scope::All, Constraints::none());
    assert!(never.is_err(), "cannot delegate a never-expiring child from an expiring parent");

    // Even the ONE allowed child (expiry <= parent) is itself already expired => still denied at use.
    let same = e
        .delegate(
            expired,
            "B",
            "entity.derive",
            Scope::All,
            Constraints { expires_at: Some(1000), approval_required: false, local_only: true },
        )
        .expect("equal-expiry delegation is structurally allowed");
    assert!(
        matches!(e.evaluate("entity.derive", &Target::default(), &[same]), Decision::Deny(_)),
        "a child laundered at the parent's own (past) expiry is itself expired => no usable authority"
    );
}

#[test]
fn cannot_launder_a_broader_scope_through_a_transfer() {
    // The IPC transfer path is the other delegation surface; it must be laundering-proof too.
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut ch = Channel::new("ipc.send");
    let narrow = e.mint("A", "entity.derive", Scope::Type(EntityType::Document), Constraints::none());
    let send_cap = e.mint("A", "ipc.send", Scope::All, Constraints::none());
    let grant = CapGrant { action: "entity.delete".into(), scope: Scope::All, constraints: Constraints::none() };
    let result = ch.send_transfer(&mut e, Message::new("A", "B", 1), narrow, grant, &[send_cap]);
    assert!(matches!(result, Err(Decision::Deny(_))), "a transfer cannot launder broader authority");
    assert!(ch.recv().is_none(), "fail-closed: nothing enqueued, no token minted");
}

// ---------------------------------------------------------------------------
// TOCTOU / stale capability — no cached authority survives a revocation.
// ---------------------------------------------------------------------------

#[test]
fn revocation_is_immediate_no_stale_authorization_window() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    let mut s = Store::new();
    let doc = s.put(EntityType::Document, "data", "A");
    let cap = e.mint("A", "entity.derive", Scope::All, Constraints::none());

    // First use is authorized and verified.
    let ok = run_pipeline(&e, &mut s, "A", &derive_plan(doc), &[cap]);
    assert!(ok.ok && ok.verified);

    // Revoke, then re-attempt with the SAME token handle. There is no cached "already authorized"
    // state — authority is re-evaluated against live engine state every time, so it is now denied.
    e.revoke(cap);
    let after = run_pipeline(&e, &mut s, "A", &derive_plan(doc), &[cap]);
    assert!(!after.ok && !after.executed, "a revoked token authorizes nothing on the next request");
    assert!(matches!(after.authorization, Decision::Deny(_)));
    assert_eq!(s.event_count(), 1, "the denied re-attempt records no new event");
}

#[test]
fn targeted_revocation_does_not_disturb_siblings() {
    // Revoking one delegate must not revoke an unrelated sibling — and re-delegating from the still-live
    // parent yields FRESH legitimate authority (not a resurrection of the revoked child).
    let mut e = CapEngine::new(0xA5A5, 1000);
    let root = e.mint("A", "entity.*", Scope::All, Constraints::none());
    let child1 = e.delegate(root, "B", "entity.derive", Scope::All, Constraints::none()).unwrap();
    let child2 = e.delegate(root, "C", "entity.derive", Scope::All, Constraints::none()).unwrap();
    e.revoke(child1);
    assert!(e.is_revoked(child1));
    assert!(!e.is_revoked(child2), "revoking one delegate leaves its sibling intact");
    assert_eq!(e.evaluate("entity.derive", &Target::default(), &[child2]), Decision::Allow);
    // A brand-new delegate from the live root is valid authority — legitimate, not laundering.
    let child3 = e.delegate(root, "D", "entity.derive", Scope::All, Constraints::none()).unwrap();
    assert!(!e.is_revoked(child3));
}

// ---------------------------------------------------------------------------
// Cross-principal leakage — a capability confines to exactly its grant.
// ---------------------------------------------------------------------------

#[test]
fn capability_does_not_leak_across_entities_or_types() {
    let mut e = CapEngine::new(0xA5A5, 1000);
    // A cap scoped to entity 0x1000 must not authorize a different entity...
    let ent = e.mint("A", "entity.derive", Scope::Entities(vec![0x1000]), Constraints::none());
    assert!(matches!(
        e.evaluate("entity.derive", &Target { id: Some(0x2000), etype: Some(EntityType::Document) }, &[ent]),
        Decision::Deny(_)
    ));
    // ...and a cap scoped to Documents must not authorize a Summary target.
    let typ = e.mint("A", "entity.derive", Scope::Type(EntityType::Document), Constraints::none());
    assert!(matches!(
        e.evaluate("entity.derive", &Target { id: None, etype: Some(EntityType::Summary) }, &[typ]),
        Decision::Deny(_)
    ));
}

#[test]
fn action_wildcard_does_not_over_match_a_neighbouring_namespace() {
    // `entity.*` must authorize entity.* actions but NOT a same-prefix-substring neighbour like
    // `entityx.delete` — a laundering vector if prefix matching were sloppy.
    let mut e = CapEngine::new(0xA5A5, 1000);
    let cap = e.mint("A", "entity.*", Scope::All, Constraints::none());
    assert_eq!(e.evaluate("entity.delete", &Target::default(), &[cap]), Decision::Allow);
    assert!(matches!(e.evaluate("entityx.delete", &Target::default(), &[cap]), Decision::Deny(_)));
    assert!(matches!(e.evaluate("other.delete", &Target::default(), &[cap]), Decision::Deny(_)));
}
