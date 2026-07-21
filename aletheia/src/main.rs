//! `aletheiad` — Aletheia Core Alpha daemon + hosted experience surface.
//!
//! Two modes, both exercising the SAME capability-gated service boundary (ADR-016, SAD §17):
//!   aletheiad serve [--socket PATH] [--data DIR]   long-running Core behind the Unix-socket IPC
//!                                                  boundary (clients connect and issue Requests)
//!   aletheiad [demo] [--data DIR]                  runs the UC-001..004 scenario AS A CLIENT over
//!                                                  the in-process boundary — the app never touches
//!                                                  Core internals, only Request/Response.
use aletheia::domain::EntityType;
use aletheia::experience;
use aletheia::intent_action::{Intent, Trace, Verb};
use aletheia::service::{serve_unix, CoreService, Request, Response};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn data_dir(args: &[String]) -> String {
    arg_value(args, "--data")
        .or_else(|| std::env::var("ALETHEIA_DATA").ok())
        .unwrap_or_else(|| std::env::temp_dir().join(format!("aletheia-{}", aletheia::domain::new_id())).to_string_lossy().into_owned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()).unwrap_or("demo") {
        "serve" => serve(&args),
        _ => demo(&args),
    }
}

fn serve(args: &[String]) {
    let dir = data_dir(args);
    let sock = arg_value(args, "--socket")
        .or_else(|| std::env::var("ALETHEIA_SOCK").ok())
        .unwrap_or_else(|| std::env::temp_dir().join("aletheia.sock").to_string_lossy().into_owned());
    let svc = CoreService::open(&dir).expect("open core");
    println!("Aletheia Core Alpha — serving on {sock}");
    println!("  data-dir = {dir}");
    println!("  clients connect via the capability-gated Unix-socket IPC boundary; Ctrl-C to stop.");
    serve_unix(svc, &sock).expect("serve");
}

fn demo(args: &[String]) {
    let dir = data_dir(args);
    println!("Aletheia Core Alpha (hosted) — demo CLIENT over the in-process service boundary");
    println!("data-dir = {dir}");
    println!("(the app below touches ONLY Request/Response — never Core internals)\n");

    let mut svc = CoreService::open(&dir).expect("open core");

    // Bootstrap the owner's root capability (hosted root of trust).
    let owner = svc
        .handle(Request::BootstrapOwner { subject: "human:owner".into() })
        .data["token"]
        .as_str()
        .expect("owner token")
        .to_string();

    // world: create a recording.
    let rec_id = svc
        .handle(Request::CreateEntity {
            caps: vec![owner.clone()],
            subject: "human:owner".into(),
            etype: EntityType::Output,
            content: "take-3.wav bytes".into(),
            metadata: serde_json::json!({ "name": "vocal take 3" }),
        })
        .data["id"]
        .as_str()
        .expect("entity id")
        .to_string();
    println!("created recording entity {rec_id}\n");

    // intents: derive a master, then traverse the world model.
    print_trace(&svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent { subject: "human:owner".into(), verb: Verb::Derive { source: rec_id.clone(), into_type: EntityType::Output, content: "master-v1.wav bytes".into() } },
        approve: false,
    }));
    print_trace(&svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent { subject: "human:owner".into(), verb: Verb::Traverse { from: rec_id.clone(), edge: "derived_from".into() } },
        approve: false,
    }));

    // policy: a destructive op stops for approval, then a human grants it via the policy surface.
    let del = svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent { subject: "human:owner".into(), verb: Verb::Delete { id: rec_id.clone() } },
        approve: false,
    });
    print_trace(&del);
    if let Some(approval_id) = del.data["approval_id"].as_str() {
        println!("-> pending approval [{approval_id}] on the policy surface; granting...");
        print_trace(&svc.handle(Request::ResolveApproval { caps: vec![owner.clone()], approval_id: approval_id.to_string(), granted: true }));
    }

    // capabilities: a read-only agent, scoped and revocable.
    let agent_cap = svc
        .handle(Request::Grant { caps: vec![owner.clone()], subject: "agent:reviewer".into(), action: "entity.read".into(), scope_entities: vec![rec_id.clone()], approval: false })
        .data["token"]
        .as_str()
        .map(|s| s.to_string());
    if let Some(cap) = agent_cap {
        let denied = svc.handle(Request::SubmitIntent {
            caps: vec![cap],
            intent: Intent { subject: "agent:reviewer".into(), verb: Verb::Delete { id: rec_id.clone() } },
            approve: true,
        });
        println!("read-only agent attempts destructive op -> ok={} ({})", denied.ok, denied.data["capability_decision"].as_str().unwrap_or(""));
    }

    // audit: the immutable event log.
    let audit = svc.handle(Request::QueryAudit { limit: 100 });
    let n = audit.data.as_array().map(|a| a.len()).unwrap_or(0);
    println!("\naudit surface: {n} immutable events recorded.");
    println!("run `aletheiad serve` for the long-running Core behind the socket boundary;");
    println!("set MODEL_ENDPOINT + start llama-server to route interpretation through the local model.");
}

fn print_trace(resp: &Response) {
    match serde_json::from_value::<Trace>(resp.data.clone()) {
        Ok(tr) => {
            print!("{}", experience::render_trace(&tr));
            println!();
        }
        Err(_) => {
            if let Some(e) = &resp.error {
                println!("error: {e}\n");
            }
        }
    }
}
