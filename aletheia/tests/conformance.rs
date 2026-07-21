//! Conformance tests — the M1 scenario driven THROUGH the service boundary (ADR-016, SAD §17).
//!
//! Where `acceptance.rs` calls the Core directly (proving the Core's invariants), this suite proves
//! the SERVICE BOUNDARY preserves them: apps/tests interact only via `Request`/`Response`, never by
//! calling Core internals. The UC-001..004 scenario and the load-bearing criteria (no ambient
//! authority, destructive→approval, capability scope) are reproduced over the API. A final test
//! proves the same contract over the Unix-socket transport.
use aletheia::domain::EntityType;
use aletheia::intent_action::{Intent, Verb};
use aletheia::service::{serve_unix, CoreService, Request, UnixClient};

fn dir() -> String {
    std::env::temp_dir().join(format!("aletheia-conf-{}", aletheia::domain::new_id())).to_string_lossy().into_owned()
}

fn owner_token(svc: &mut CoreService) -> String {
    let r = svc.handle(Request::BootstrapOwner { subject: "human:owner".into() });
    assert!(r.ok, "bootstrap owner via API");
    r.data["token"].as_str().unwrap().to_string()
}

#[test]
fn uc_create_derive_traverse_through_the_api() {
    let mut svc = CoreService::open(dir()).unwrap();
    let owner = owner_token(&mut svc);

    // UC: create a recording (world command).
    let created = svc.handle(Request::CreateEntity {
        caps: vec![owner.clone()],
        subject: "human:owner".into(),
        etype: EntityType::Output,
        content: "take-3.wav bytes".into(),
        metadata: serde_json::json!({ "name": "vocal take 3" }),
    });
    assert!(created.ok);
    let rec_id = created.data["id"].as_str().unwrap().to_string();

    // UC: derive a master (intent command through the full pipeline).
    let derived = svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent { subject: "human:owner".into(), verb: Verb::Derive { source: rec_id.clone(), into_type: EntityType::Output, content: "master-v1.wav".into() } },
        approve: false,
    });
    assert!(derived.ok, "derive succeeds through the boundary");
    let derived_id = derived.data["result"][0]["derived_id"].as_str().unwrap().to_string();

    // UC: traverse the world model back to the derived entity.
    let tr = svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent { subject: "human:owner".into(), verb: Verb::Traverse { from: rec_id.clone(), edge: "derived_from".into() } },
        approve: false,
    });
    assert!(tr.ok);
    let results = tr.data["result"][0]["results"].as_array().unwrap();
    assert!(results.iter().any(|v| v.as_str() == Some(derived_id.as_str())), "world model resolves the derived entity via the API");

    // audit query surface returns the immutable events.
    let audit = svc.handle(Request::QueryAudit { limit: 50 });
    assert!(audit.ok && audit.data.as_array().map(|a| !a.is_empty()).unwrap_or(false));
}

#[test]
fn no_ambient_authority_at_the_boundary() {
    let mut svc = CoreService::open(dir()).unwrap();
    let owner = owner_token(&mut svc);
    let created = svc.handle(Request::CreateEntity {
        caps: vec![owner],
        subject: "human:owner".into(),
        etype: EntityType::Document,
        content: "secret".into(),
        metadata: serde_json::json!({}),
    });
    let id = created.data["id"].as_str().unwrap().to_string();
    // A client offering NO capabilities can read nothing (fail-closed at the boundary).
    let denied = svc.handle(Request::SubmitIntent {
        caps: vec![],
        intent: Intent { subject: "task:x".into(), verb: Verb::Read { id } },
        approve: false,
    });
    assert!(!denied.ok, "no capability -> denied through the API");
    assert!(denied.data["capability_decision"].as_str().unwrap_or("").contains("DENY"));
}

#[test]
fn destructive_requires_approval_lifecycle_over_the_api() {
    let mut svc = CoreService::open(dir()).unwrap();
    let owner = owner_token(&mut svc);
    let created = svc.handle(Request::CreateEntity {
        caps: vec![owner.clone()],
        subject: "human:owner".into(),
        etype: EntityType::Document,
        content: "doc".into(),
        metadata: serde_json::json!({}),
    });
    let id = created.data["id"].as_str().unwrap().to_string();

    // Delete without approval → stops, records a pending approval (policy axis).
    let attempt = svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent { subject: "human:owner".into(), verb: Verb::Delete { id: id.clone() } },
        approve: false,
    });
    assert!(!attempt.ok, "destructive op stops pending approval");
    let approval_id = attempt.data["approval_id"].as_str().expect("pending approval id returned").to_string();

    // policy query surface lists it.
    let pending = svc.handle(Request::ListApprovals);
    assert!(pending.data.as_array().unwrap().iter().any(|a| a["id"] == approval_id));

    // Human grants it → the bound intent executes (approval confers no authority; caps re-checked).
    let resolved = svc.handle(Request::ResolveApproval { caps: vec![owner], approval_id, granted: true });
    assert!(resolved.ok, "granted approval executes the bound intent");
}

#[test]
fn capability_scope_confined_over_the_api() {
    let mut svc = CoreService::open(dir()).unwrap();
    let owner = owner_token(&mut svc);
    let e1 = svc.handle(Request::CreateEntity { caps: vec![owner.clone()], subject: "human:owner".into(), etype: EntityType::Document, content: "one".into(), metadata: serde_json::json!({}) });
    let e2 = svc.handle(Request::CreateEntity { caps: vec![owner.clone()], subject: "human:owner".into(), etype: EntityType::Document, content: "two".into(), metadata: serde_json::json!({}) });
    let id1 = e1.data["id"].as_str().unwrap().to_string();
    let id2 = e2.data["id"].as_str().unwrap().to_string();

    // Grant an agent read scoped to e1 only.
    let grant = svc.handle(Request::Grant { caps: vec![owner], subject: "agent:a".into(), action: "entity.read".into(), scope_entities: vec![id1.clone()], approval: false });
    let agent_cap = grant.data["token"].as_str().unwrap().to_string();

    assert!(svc.handle(Request::SubmitIntent { caps: vec![agent_cap.clone()], intent: Intent { subject: "agent:a".into(), verb: Verb::Read { id: id1 } }, approve: false }).ok);
    assert!(!svc.handle(Request::SubmitIntent { caps: vec![agent_cap], intent: Intent { subject: "agent:a".into(), verb: Verb::Read { id: id2 } }, approve: false }).ok, "must not read outside granted scope");
}

#[test]
fn same_contract_holds_over_the_unix_socket_transport() {
    let sock = std::env::temp_dir().join(format!("aletheia-{}.sock", aletheia::domain::new_id())).to_string_lossy().into_owned();
    let data = dir();
    let sock_srv = sock.clone();
    // Server constructs its own CoreService inside the thread (nothing non-Send crosses the boundary).
    let server = std::thread::spawn(move || {
        let svc = CoreService::open(data).unwrap();
        let _ = serve_unix(svc, &sock_srv);
    });

    // Connect (retry briefly while the listener binds).
    let mut client = None;
    for _ in 0..100 {
        if let Ok(c) = UnixClient::connect(&sock) {
            client = Some(c);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut client = client.expect("connect to socket");

    let boot = client.call(&Request::BootstrapOwner { subject: "human:owner".into() }).unwrap();
    let owner = boot.data["token"].as_str().unwrap().to_string();
    let created = client
        .call(&Request::CreateEntity { caps: vec![owner.clone()], subject: "human:owner".into(), etype: EntityType::Document, content: "over the wire".into(), metadata: serde_json::json!({}) })
        .unwrap();
    assert!(created.ok, "create over socket");
    let id = created.data["id"].as_str().unwrap().to_string();

    let read = client.call(&Request::SubmitIntent { caps: vec![owner], intent: Intent { subject: "human:owner".into(), verb: Verb::Read { id } }, approve: false }).unwrap();
    assert!(read.ok, "read over socket");
    assert_eq!(read.data["result"][0]["content"], "over the wire");

    // No capability → denied over the wire too.
    let denied = client.call(&Request::SubmitIntent { caps: vec![], intent: Intent { subject: "x".into(), verb: Verb::Read { id: "nope".into() } }, approve: false }).unwrap();
    assert!(!denied.ok);

    drop(client); // closes the connection; the detached server thread ends at process exit.
    let _ = server;
}
