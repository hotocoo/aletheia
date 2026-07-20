//! `aletheiad` - the hosted experience surface (M1). Boots the System Core and runs a scenario that
//! exercises the intent->action pipeline end to end, rendering explainable traces (PRD UC-001..004).
use aletheia::capabilities::{Constraints, Scope};
use aletheia::domain::EntityType;
use aletheia::experience;
use aletheia::intent_action::{Intent, Verb};
use aletheia::syscore::SysCore;

fn main() {
    let dir = std::env::var("ALETHEIA_DATA").unwrap_or_else(|_| {
        std::env::temp_dir().join(format!("aletheia-demo-{}", aletheia::domain::new_id())).to_string_lossy().into_owned()
    });
    println!("Aletheia System Core (hosted M1)  data-dir={dir}");

    let mut core = SysCore::open_default(&dir).expect("open store");
    println!("interpreter: {} (local model absent -> deterministic fallback, INT-004)\n", core.interpreter_name());

    let owner = core.bootstrap_owner("human:owner").expect("bootstrap");
    let owner_tokens = vec![owner.token.clone()];

    let recording = core
        .create_entity(&owner_tokens, "human:owner", EntityType::Output, b"take-3.wav bytes", serde_json::json!({"name": "vocal take 3"}))
        .expect("create recording");
    println!("created recording entity {}\n", recording.id);

    let t = core.handle_intent(
        &owner_tokens,
        Intent { subject: "human:owner".into(), verb: Verb::Derive { source: recording.id.clone(), into_type: EntityType::Output, content: "master-v1.wav bytes".into() } },
        false,
    );
    print!("{}", experience::render_trace(&t));
    let derived = t.result.get(0).and_then(|v| v["derived_id"].as_str()).unwrap_or("").to_string();
    println!();

    let t = core.handle_intent(
        &owner_tokens,
        Intent { subject: "human:owner".into(), verb: Verb::Traverse { from: recording.id.clone(), edge: "derived_from".into() } },
        false,
    );
    print!("{}", experience::render_trace(&t));
    println!();

    let mut agent = core.create_agent("agent:reviewer");
    let expires = aletheia::domain::now() + 3_600_000;
    let cons = Constraints { expires_at: Some(expires), max_count: None, approval_required: false, local_only: true };
    let acap = core
        .grant_to(&owner_tokens, "agent:reviewer", "entity.read", Scope::Entities(vec![recording.id.clone()]), cons)
        .expect("grant to agent");
    agent.caps.push(acap.token.clone());
    println!("granted review agent read-only, 1h, scoped to the recording ({} caps)\n", agent.caps.len());

    let t = core.handle_intent(&agent.caps, Intent { subject: "agent:reviewer".into(), verb: Verb::Read { id: recording.id.clone() } }, false);
    println!("agent read -> ok={}", t.ok);
    let t = core.handle_intent(&agent.caps, Intent { subject: "agent:reviewer".into(), verb: Verb::Delete { id: recording.id.clone() } }, true);
    println!("agent destructive op (even with approve) -> ok={} decision=[{}]\n", t.ok, t.capability_decision);

    if !derived.is_empty() {
        let t = core.handle_intent(&owner_tokens, Intent { subject: "human:owner".into(), verb: Verb::Delete { id: derived.clone() } }, false);
        println!("owner destructive op without approval -> ok={}", t.ok);
        print!("{}", experience::render_trace(&t));
    }

    println!("\n{}", experience::render_world(core.store()));
    println!("{}", experience::render_audit(core.store()));
}
