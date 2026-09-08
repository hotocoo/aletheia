use aletheia::domain::EntityType;
use aletheia::service::{CoreService, Request};

fn dir() -> String {
    std::env::temp_dir()
        .join(format!("aletheia-gui-test-{}", aletheia::domain::new_id()))
        .to_string_lossy()
        .into_owned()
}

#[test]
fn gui_queries_are_capability_gated_and_never_expose_tokens() {
    let mut svc = CoreService::open_deterministic(dir()).unwrap();
    let root = svc
        .handle(Request::BootstrapOwner {
            subject: "human:operator".into(),
        })
        .data["token"]
        .as_str()
        .unwrap()
        .to_string();

    let created = svc.handle(Request::CreateEntity {
        caps: vec![root.clone()],
        subject: "human:operator".into(),
        etype: EntityType::Document,
        content: "GUI-visible document".into(),
        metadata: serde_json::json!({"source":"experience-test"}),
    });
    assert!(created.ok);

    let world = svc.handle(Request::QueryWorld { caps: vec![root.clone()] });
    assert!(world.ok);
    assert_eq!(world.data["entities"].as_array().unwrap().len(), 1);

    let caps = svc.handle(Request::QueryCapabilities { caps: vec![root] });
    assert!(caps.ok);
    let rendered = caps.data.to_string();
    assert!(!rendered.contains("token"));
}

#[test]
fn gui_world_query_fails_closed_to_empty_without_read_authority() {
    let mut svc = CoreService::open_deterministic(dir()).unwrap();
    let world = svc.handle(Request::QueryWorld { caps: vec![] });
    assert!(world.ok);
    assert_eq!(world.data["entities"].as_array().unwrap().len(), 0);
    assert_eq!(world.data["relationships"].as_array().unwrap().len(), 0);
}
