//! Adversarial security tests beyond the 20 acceptance criteria (PRD-002 §38, SAD §19).
use aletheia::capabilities::{Constraints, Scope};
use aletheia::domain::EntityType;
use aletheia::intelligence::{DeterministicRuntime, ModelRuntime};
use aletheia::intent_action::{Intent, Verb};
use aletheia::syscore::SysCore;

fn dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-sec-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}
fn det() -> Box<dyn ModelRuntime> {
    Box::new(DeterministicRuntime)
}
fn owner(core: &mut SysCore) -> Vec<String> {
    vec![core.bootstrap_owner("human:owner").unwrap().token]
}

#[test]
fn expired_capability_denied() {
    let mut core = SysCore::open(dir(), det()).unwrap();
    let t = owner(&mut core);
    let e = core
        .create_entity(
            &t,
            "human:owner",
            EntityType::Document,
            b"x",
            serde_json::json!({}),
        )
        .unwrap();
    let past = Constraints {
        expires_at: Some(1),
        max_count: None,
        approval_required: false,
        local_only: true,
    };
    let cap = core
        .grant_to(
            &t,
            "agent:a",
            "entity.read",
            Scope::Entities(vec![e.id.clone()]),
            past,
        )
        .unwrap();
    let tr = core.handle_intent(
        &[cap.token],
        Intent {
            subject: "agent:a".into(),
            verb: Verb::Read { id: e.id },
        },
        false,
    );
    assert!(
        !tr.ok && tr.capability_decision.contains("DENY"),
        "expired capability must not authorize"
    );
}

#[test]
fn scope_confinement() {
    let mut core = SysCore::open(dir(), det()).unwrap();
    let t = owner(&mut core);
    let e1 = core
        .create_entity(
            &t,
            "human:owner",
            EntityType::Document,
            b"one",
            serde_json::json!({}),
        )
        .unwrap();
    let e2 = core
        .create_entity(
            &t,
            "human:owner",
            EntityType::Document,
            b"two",
            serde_json::json!({}),
        )
        .unwrap();
    let cap = core
        .grant_to(
            &t,
            "agent:a",
            "entity.read",
            Scope::Entities(vec![e1.id.clone()]),
            Constraints::none(),
        )
        .unwrap();
    let toks = vec![cap.token];
    assert!(
        core.handle_intent(
            &toks,
            Intent {
                subject: "agent:a".into(),
                verb: Verb::Read { id: e1.id }
            },
            false
        )
        .ok
    );
    assert!(
        !core
            .handle_intent(
                &toks,
                Intent {
                    subject: "agent:a".into(),
                    verb: Verb::Read { id: e2.id }
                },
                false
            )
            .ok,
        "must not read outside scope"
    );
}

#[test]
fn agent_cannot_self_escalate() {
    let mut core = SysCore::open(dir(), det()).unwrap();
    let t = owner(&mut core);
    let e = core
        .create_entity(
            &t,
            "human:owner",
            EntityType::Document,
            b"x",
            serde_json::json!({}),
        )
        .unwrap();
    let cap = core
        .grant_to(
            &t,
            "agent:a",
            "entity.read",
            Scope::Entities(vec![e.id.clone()]),
            Constraints::none(),
        )
        .unwrap();
    // Agent (read-only) tries to grant itself broad authority: needs capability.grant -> denied.
    let tr = core.handle_intent(
        &[cap.token],
        Intent {
            subject: "agent:a".into(),
            verb: Verb::Grant {
                subject: "agent:a".into(),
                action: "*".into(),
                scope_entities: vec![],
                approval: false,
            },
        },
        true,
    );
    assert!(
        !tr.ok && tr.capability_decision.contains("DENY"),
        "agent cannot grant itself authority"
    );
}

/// The delegation test is not the authorization test (REQ-CAP-007, ADR-048). Asking
/// `action_covers(parent, child_pattern)` — "is the child's STRING inside the parent's reach" —
/// instead of `action_attenuates` — "is the child's REACH inside the parent's" — accepts
/// `entity.*.*` → `entity.*`, and the child then authorizes `entity.delete`, which its parent
/// never could. The hosted Core and the kernel spine must agree about this or a component proved
/// safe on one is not proved safe on the other.
#[test]
fn a_child_pattern_reaching_past_its_parent_is_denied() {
    use aletheia::capabilities::{action_attenuates, action_covers, CapEngine};

    // The disagreement itself.
    assert!(action_covers("entity.*.*", "entity.*"));
    assert!(!action_attenuates("entity.*.*", "entity.*"));
    // …and what it would have cost: the action the child reaches and the parent does not.
    assert!(action_covers("entity.*", "entity.delete"));
    assert!(!action_covers("entity.*.*", "entity.delete"));

    let mut e = CapEngine::new();
    let odd = e.mint(
        "human:owner",
        "entity.*.*",
        Scope::All,
        Constraints::none(),
        "human:owner",
    );
    assert!(
        e.delegate(
            &odd.token,
            "agent:worker",
            "entity.*",
            Scope::All,
            Constraints::none(),
            "human:owner",
        )
        .is_err(),
        "a child whose reach exceeds its parent's must be refused"
    );

    // The legitimate narrowing the same rule must still allow, so the refusal above is not a
    // relation that simply denies everything.
    assert!(e
        .delegate(
            &odd.token,
            "agent:worker",
            "entity.*.x",
            Scope::All,
            Constraints::none(),
            "human:owner",
        )
        .is_ok());
}

/// An entity set with no members reaches nothing, so it is the narrowest scope there is and every
/// scope must be able to delegate to it. Mirrors `kernel_core::capalg::scope_attenuates`.
#[test]
fn an_empty_entity_scope_is_a_legal_narrowing_of_anything() {
    use aletheia::capabilities::CapEngine;

    let mut e = CapEngine::new();
    let typed = e.mint(
        "human:owner",
        "entity.derive",
        Scope::Type(EntityType::Document),
        Constraints::none(),
        "human:owner",
    );
    assert!(e
        .delegate(
            &typed.token,
            "agent:worker",
            "entity.derive",
            Scope::Entities(vec![]),
            Constraints::none(),
            "human:owner",
        )
        .is_ok());
    // …and nothing can be widened back out of it.
    let empty = e.mint(
        "human:owner",
        "entity.derive",
        Scope::Entities(vec![]),
        Constraints::none(),
        "human:owner",
    );
    assert!(e
        .delegate(
            &empty.token,
            "agent:worker",
            "entity.derive",
            Scope::Type(EntityType::Document),
            Constraints::none(),
            "human:owner",
        )
        .is_err());
}
