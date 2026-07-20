//! M1 vertical spine (PRD-002 §36, criteria 1,3,5,10,13,14): create entity -> store
//! (content-addressed, encrypted, persisted) -> intent -> deterministic interpret -> validate ->
//! capability-authorize -> execute -> verify against store -> record event/trace -> restart & decrypt.
use aletheia::domain::EntityType;
use aletheia::intelligence::DeterministicRuntime;
use aletheia::intent_action::{Intent, Verb};
use aletheia::syscore::SysCore;

fn temp_dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-test-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn spine_end_to_end() {
    let dir = temp_dir();
    let mut core = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();

    // Criterion 5 depends on this being the ONLY authority: the human's root capability.
    let owner = core.bootstrap_owner("human:owner").unwrap();
    let toks = vec![owner.token.clone()];

    // Criterion 1: create an entity; it is content-addressed.
    let e = core
        .create_entity(&toks, "human:owner", EntityType::Document, b"hello world secret", serde_json::json!({"name":"note"}))
        .unwrap();
    assert!(e.content_ref.is_some(), "entity must be content-addressed");

    // Criteria 10,13,14 + 5: full pipeline for a read intent.
    let trace = core.handle_intent(
        &toks,
        Intent { subject: "human:owner".into(), verb: Verb::Read { id: e.id.clone() } },
        false,
    );
    assert!(trace.ok, "pipeline should succeed: {:?}", trace);
    assert_eq!(trace.validation, "ok");                          // 10: validated before execution
    assert!(trace.capability_decision.contains("ALLOW"));        // 5: capability authorized
    assert!(!trace.verification.is_empty());                     // 13: verified against store
    assert_eq!(trace.result[0]["content"], "hello world secret");
    assert!(                                                     // 14: immutable event recorded
        core.store().events().iter().any(|ev| ev.etype == "AIActionExecuted"),
        "an AIActionExecuted event must be recorded"
    );

    // Criterion 3 (part a): encryption at rest — plaintext must be absent from raw store bytes.
    let raw = std::fs::read(core.store().log_path()).unwrap();
    assert!(!contains(&raw, b"hello world secret"), "plaintext leaked into store on disk");

    // Criterion 3 (part b) + 13: restart -> decrypt -> entity + content recover.
    drop(core);
    let core2 = SysCore::open(&dir, Box::new(DeterministicRuntime)).unwrap();
    let e2 = core2.store().get_entity(&e.id).expect("entity survives restart");
    let blob = core2.store().get_blob(e2.content_ref.as_ref().unwrap()).expect("content decrypts");
    assert_eq!(blob, b"hello world secret");
}
