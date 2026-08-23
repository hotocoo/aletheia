//! P2 acceptance - component DEPENDENCY RESOLUTION as a security boundary
//! (ALET-P1-024, ADR-068).
//!
//! Multi-agent composition lets one component pull ANOTHER INSTALLED APPLICATION into execution.
//! Before this wave the edge was authorized by nothing but the request itself: naming an app id
//! queued it, whatever the caller held. Now resolving a dependency is itself a capability-checked
//! operation:
//!
//! * the parent must hold `component.spawn` covering THIS child - a grant that can be scoped to
//!   exactly the dependencies it is meant to use (`Scope::Entities`), so an operator can pin which
//!   apps a component may ever pull in;
//! * an approval-constrained spawn grant behaves like every other governed action: refused inline,
//!   human gate preserved;
//! * the guest-side check is audited per attempt, AND the System Core re-evaluates authority
//!   itself before fulfilling any queued edge - the queue is a request, never a verdict.
use aletheia::capabilities::{Constraints, Scope};
use aletheia::intelligence::DeterministicRuntime;
use aletheia::syscore::SysCore;

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-dep-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

fn open() -> (SysCore, String) {
    let mut core = SysCore::open(temp_dir(), Box::new(DeterministicRuntime)).unwrap();
    let owner = core.bootstrap_owner("human:owner").unwrap();
    (core, owner.token)
}

fn guest() -> Vec<u8> {
    let mut w = wat::parse_str(
        r#"(module
  (memory (export "memory") 1)
  (func (export "run") (result i32) (i32.const 0)))"#,
    )
    .expect("guest wat compiles");
    aletheia::component::stamp_abi_section(&mut w, aletheia::component::ABI_VERSION);
    w
}

/// A component that spawns `child_id`, requesting `action` authority for it.
fn spawner_for(child_id: &str, action: &str) -> Vec<u8> {
    let wat = format!(
        r#"(module
  (import "aletheia" "spawn" (func $spawn (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{child_id}")
  (data (i32.const 64) "{action}")
  (func (export "run") (result i32)
    (drop (call $spawn (i32.const 0) (i32.const {idlen}) (i32.const 64) (i32.const {actlen})))
    (i32.const 0)))"#,
        child_id = child_id,
        action = action,
        idlen = child_id.len(),
        actlen = action.len()
    );
    let mut w = wat::parse_str(&wat).expect("spawner wat compiles");
    aletheia::component::stamp_abi_section(&mut w, aletheia::component::ABI_VERSION);
    w
}

fn grant(
    core: &mut SysCore,
    owner: &str,
    subject: &str,
    action: &str,
    scope: Scope,
    cons: Constraints,
) -> String {
    core.grant_to(&[owner.to_string()], subject, action, scope, cons)
        .expect("grant")
        .token
}

/// Naming an installed application authorizes NOTHING: without `component.spawn` the spawn is
/// refused at the guest boundary, audited as DENY, and no child ever runs.
#[test]
fn spawning_without_spawn_authority_is_refused_and_audited() {
    let (mut core, owner) = open();
    let child = core
        .install_component(std::slice::from_ref(&owner), "human:owner", "dep", &guest())
        .unwrap();
    // The parent holds an unrelated capability - enough to DO things, not to RESOLVE dependencies.
    let write_cap = grant(
        &mut core,
        &owner,
        "component:parent",
        "entity.write",
        Scope::All,
        Constraints::none(),
    );
    let parent = spawner_for(&child.id, "entity.read");

    let outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[write_cap],
            "component:parent",
            &parent,
            1_000_000,
        )
        .unwrap();

    assert!(outcome.ok, "the parent itself ran");
    assert!(outcome.denied("spawn"), "the attempt was refused BY NAME");
    assert_eq!(outcome.spawns.len(), 0, "nothing was queued");
    assert_eq!(outcome.spawned.len(), 0, "no child ever ran");
    assert_eq!(count(core.store(), "ComponentSpawned"), 0);
}

/// Spawn authority is SCOPABLE: a grant over exactly ONE application resolves that dependency and
/// no other. This is the operator's tool for pinning which apps a component may ever pull in.
#[test]
fn scoped_spawn_authority_resolves_only_the_named_dependency() {
    let (mut core, owner) = open();
    let allowed = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "allowed-dep",
            &guest(),
        )
        .unwrap();
    let forbidden = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "forbidden-dep",
            &guest(),
        )
        .unwrap();
    let spawn_cap = grant(
        &mut core,
        &owner,
        "component:parent",
        "component.spawn",
        Scope::Entities(vec![allowed.id.clone()]),
        Constraints::none(),
    );

    let in_scope = core
        .run_component(
            std::slice::from_ref(&owner),
            std::slice::from_ref(&spawn_cap),
            "component:parent",
            &spawner_for(&allowed.id, "entity.read"),
            1_000_000,
        )
        .unwrap();
    assert_eq!(in_scope.spawned.len(), 1, "the named dependency resolved");
    assert!(in_scope.spawned[0].ok);

    let out_of_scope = core
        .run_component(
            std::slice::from_ref(&owner),
            &[spawn_cap],
            "component:parent",
            &spawner_for(&forbidden.id, "entity.read"),
            1_000_000,
        )
        .unwrap();
    assert_eq!(
        out_of_scope.spawned.len(),
        0,
        "an unscoped dependency NEVER resolved"
    );
    assert!(out_of_scope.denied("spawn"));
    // The guest-level refusal means nothing was ever queued, so the host's fulfilment-time
    // re-check (which guards OTHER callers of the resolution path) is not reached here.
    assert_eq!(count(core.store(), "ComponentSpawnDenied"), 0);
}

/// An approval-constrained spawn grant does not authorize composition inline: the human gate is
/// preserved at the component boundary, same as every other governed action.
#[test]
fn approval_constrained_spawn_is_refused_inline() {
    let (mut core, owner) = open();
    let child = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "gated-dep",
            &guest(),
        )
        .unwrap();
    let gated = grant(
        &mut core,
        &owner,
        "component:parent",
        "component.spawn",
        Scope::All,
        // Delegation may not LOOSEN constraints: the owner root is local-only, so an
        // approval-constrained child is local-only too (Constraints::approval() spells exactly this).
        Constraints {
            expires_at: None,
            max_count: None,
            approval_required: true,
            local_only: true,
        },
    );
    let parent = spawner_for(&child.id, "entity.read");

    let outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[gated],
            "component:parent",
            &parent,
            1_000_000,
        )
        .unwrap();

    assert!(outcome.ok);
    assert_eq!(outcome.spawns.len(), 0, "approval was not bypassed");
    assert!(
        outcome
            .calls
            .iter()
            .any(|c| c.func == "spawn" && c.decision == "REQUIRE_APPROVAL"),
        "the attempt is audited as needing approval"
    );
}

/// Authority is checked against the LIVE registry at every evaluation: revoking the spawn grant
/// before the parent runs means the dependency does not resolve — no stale verdict, no cached yes.
#[test]
fn a_revoked_spawn_grant_resolves_nothing() {
    let (mut core, owner) = open();
    let child = core
        .install_component(
            std::slice::from_ref(&owner),
            "human:owner",
            "revoked-dep",
            &guest(),
        )
        .unwrap();
    let spawn_cap = grant(
        &mut core,
        &owner,
        "component:parent",
        "component.spawn",
        Scope::All,
        Constraints::none(),
    );

    // Revoke BEFORE the parent ever runs.
    core.revoke_capability(std::slice::from_ref(&owner), &spawn_cap)
        .expect("the holder may revoke");
    let parent = spawner_for(&child.id, "entity.read");

    let outcome = core
        .run_component(
            std::slice::from_ref(&owner),
            &[spawn_cap],
            "component:parent",
            &parent,
            1_000_000,
        )
        .unwrap();

    assert_eq!(outcome.spawned.len(), 0, "a revoked grant resolves NOTHING");
    assert!(outcome.denied("spawn"));
}

fn count(store: &aletheia::storage::Store, etype: &str) -> usize {
    store.events().iter().filter(|e| e.etype == etype).count()
}
