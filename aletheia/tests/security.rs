//! Adversarial security tests beyond the 20 acceptance criteria (PRD-002 §38, SAD §19).
use aletheia::capabilities::{Constraints, Scope};
use aletheia::domain::EntityType;
use aletheia::intelligence::{DeterministicRuntime, ModelRuntime};
use aletheia::intent_action::{Intent, Verb};
use aletheia::syscore::SysCore;

fn dir() -> String {
    std::env::temp_dir().join(format!("aletheia-sec-{}", aletheia::domain::new_id())).to_string_lossy().into_owned()
}
fn det() -> Box<dyn ModelRuntime> { Box::new(DeterministicRuntime) }
fn owner(core: &mut SysCore) -> Vec<String> { vec![core.bootstrap_owner("human:owner").unwrap().token] }

#[test]
fn expired_capability_denied() {
    let mut core = SysCore::open(dir(), det()).unwrap();
    let t = owner(&mut core);
    let e = core.create_entity(&t, "human:owner", EntityType::Document, b"x", serde_json::json!({})).unwrap();
    let past = Constraints { expires_at: Some(1), max_count: None, approval_required: false, local_only: true };
    let cap = core.grant_to(&t, "agent:a", "entity.read", Scope::Entities(vec![e.id.clone()]), past).unwrap();
    let tr = core.handle_intent(&[cap.token], Intent { subject: "agent:a".into(), verb: Verb::Read { id: e.id } }, false);
    assert!(!tr.ok && tr.capability_decision.contains("DENY"), "expired capability must not authorize");
}

#[test]
fn scope_confinement() {
    let mut core = SysCore::open(dir(), det()).unwrap();
    let t = owner(&mut core);
    let e1 = core.create_entity(&t, "human:owner", EntityType::Document, b"one", serde_json::json!({})).unwrap();
    let e2 = core.create_entity(&t, "human:owner", EntityType::Document, b"two", serde_json::json!({})).unwrap();
    let cap = core.grant_to(&t, "agent:a", "entity.read", Scope::Entities(vec![e1.id.clone()]), Constraints::none()).unwrap();
    let toks = vec![cap.token];
    assert!(core.handle_intent(&toks, Intent { subject: "agent:a".into(), verb: Verb::Read { id: e1.id } }, false).ok);
    assert!(!core.handle_intent(&toks, Intent { subject: "agent:a".into(), verb: Verb::Read { id: e2.id } }, false).ok, "must not read outside scope");
}

#[test]
fn agent_cannot_self_escalate() {
    let mut core = SysCore::open(dir(), det()).unwrap();
    let t = owner(&mut core);
    let e = core.create_entity(&t, "human:owner", EntityType::Document, b"x", serde_json::json!({})).unwrap();
    let cap = core.grant_to(&t, "agent:a", "entity.read", Scope::Entities(vec![e.id.clone()]), Constraints::none()).unwrap();
    // Agent (read-only) tries to grant itself broad authority: needs capability.grant -> denied.
    let tr = core.handle_intent(
        &[cap.token],
        Intent { subject: "agent:a".into(), verb: Verb::Grant { subject: "agent:a".into(), action: "*".into(), scope_entities: vec![], approval: false } },
        true,
    );
    assert!(!tr.ok && tr.capability_decision.contains("DENY"), "agent cannot grant itself authority");
}
