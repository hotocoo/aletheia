//! `aletheiad` — Aletheia Core Alpha daemon + hosted experience surface.
//!
//! Two modes, both exercising the SAME capability-gated service boundary (ADR-016, SAD §17):
//!   aletheiad serve [--socket PATH] [--data DIR]   long-running Core behind the endpoint IPC
//!                                                  boundary (clients connect and issue Requests)
//!   aletheiad [demo] [--data DIR]                  runs the UC-001..004 scenario AS A CLIENT over
//!                                                  the in-process boundary — the app never touches
//!                                                  Core internals, only Request/Response.
use aletheia::domain::EntityType;
use aletheia::experience;
use aletheia::intent_action::{Intent, Trace, Verb};
use aletheia::service::{serve as serve_endpoint, CoreService, Request, Response};

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn data_dir(args: &[String]) -> String {
    arg_value(args, "--data")
        .or_else(|| std::env::var("ALETHEIA_DATA").ok())
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join(format!("aletheia-{}", aletheia::domain::new_id()))
                .to_string_lossy()
                .into_owned()
        })
}

/// Where the MODEL commands keep their state — and deliberately not `data_dir`.
///
/// `data_dir` invents a fresh temp directory when none is given, which is right for a demo that
/// should leave nothing behind and catastrophically wrong for `model use`: the selection would be
/// written into a directory that never gets read again, so the switch would report success and have
/// no effect. A machine-level choice needs a machine-level home, so this falls back to
/// `$HOME/.aletheia` and only then to a temp path (for a environment with no HOME at all).
fn model_dir(args: &[String]) -> String {
    if let Some(d) = arg_value(args, "--data").or_else(|| std::env::var("ALETHEIA_DATA").ok()) {
        return d;
    }
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => std::path::Path::new(&h)
            .join(".aletheia")
            .to_string_lossy()
            .into_owned(),
        _ => std::env::temp_dir()
            .join("aletheia")
            .to_string_lossy()
            .into_owned(),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()).unwrap_or("demo") {
        "serve" => serve(&args),
        "model" => model_cmd(&args),
        _ => demo(&args),
    }
}

/// `aletheiad model <list|use ID|status|pull|bench>` — the model surface (ADR-017, ADR-052).
///
/// This is where an operator changes which intelligence the OS runs, and where they find out what it
/// is currently running. Both halves matter: a switch whose effect cannot be confirmed is a switch
/// nobody can trust, so `use` and `status` are the same command's two faces.
fn model_cmd(args: &[String]) {
    let dir = model_dir(args);
    let dirp = std::path::Path::new(&dir);
    match args.get(2).map(|s| s.as_str()) {
        Some("list") => model_list(dirp),
        Some("use") => match args.get(3) {
            Some(id) => model_use(dirp, id),
            None => {
                eprintln!("usage: aletheiad model use <id>   (see `aletheiad model list`)");
                std::process::exit(2);
            }
        },
        Some("bench") => model_bench(dirp),
        Some("pull") => {
            let cfg = aletheia::ai::config::AiConfig::resolve(Some(dirp));
            println!("provisioning {}...", cfg.label());
            match aletheia::ai::runtime::ensure_model(&cfg) {
                Ok(p) => println!("model ready: {}", p.display()),
                Err(e) => {
                    eprintln!("provisioning failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => model_status(dirp),
    }
}

/// Every model this OS knows about, with the selected one marked. `*` is the running selection;
/// `(default)` is what an unswitched machine would run.
fn model_list(dir: &std::path::Path) {
    let selected = aletheia::ai::config::AiConfig::resolve(Some(dir))
        .entry
        .map(|e| e.id);
    for e in aletheia::ai::registry::builtin() {
        let mark = if Some(&e.id) == selected.as_ref() {
            "*"
        } else {
            " "
        };
        let tags = match (e.default, e.is_ready()) {
            (true, true) => " (default)",
            (true, false) => " (default, not yet trained)",
            (false, true) => "",
            (false, false) => " (not yet trained)",
        };
        println!("{mark} {:<14} {}{}", e.id, e.name, tags);
    }
    println!("\nselect one with: aletheiad model use <id>");
}

/// Switch the machine's model. Persists the choice, then IMMEDIATELY reports whether the chosen
/// model is actually present — because the failure this guards against is a switch that appears to
/// work while the OS quietly keeps answering from the deterministic interpreter.
fn model_use(dir: &std::path::Path, id: &str) {
    let Some(entry) = aletheia::ai::registry::find(id) else {
        eprintln!("no model `{id}` is registered — try `aletheiad model list`");
        std::process::exit(1);
    };
    if let Err(e) = aletheia::ai::registry::save_selection(dir, &entry.id) {
        eprintln!(
            "could not persist the selection under {}: {e}",
            dir.display()
        );
        std::process::exit(1);
    }
    println!("selected {} ({})", entry.id, entry.name);
    model_status(dir);
}

/// What this machine is running, and whether it can actually run it.
fn model_status(dir: &std::path::Path) {
    let cfg = aletheia::ai::config::AiConfig::resolve(Some(dir));
    println!("model:     {}", cfg.label());
    println!("backend:   {} at {}", cfg.backend, cfg.endpoint);
    println!(
        "selection: {}",
        aletheia::ai::registry::selection_path(dir).display()
    );
    match aletheia::ai::runtime::resolve_model_path(&cfg) {
        Some(p) if p.exists() => println!("weights:   present — {}", p.display()),
        Some(p) => println!("weights:   MISSING — expected at {}", p.display()),
        // A model still in training is not "missing weights someone forgot to fetch": it is a model
        // that does not exist yet, and saying so names the actual next step instead of sending the
        // operator to `model pull` for an artifact no hub has.
        None => match cfg.entry.as_ref() {
            Some(e) if !e.is_ready() => println!(
                "weights:   NOT YET TRAINED — set {} to the finished weights once pretraining ends",
                if e.path_env.is_empty() {
                    "MODEL_PATH"
                } else {
                    &e.path_env
                }
            ),
            _ => println!("weights:   not present — run: aletheiad model pull"),
        },
    }
    // Said last because it is the only line that depends on something outside this machine's own
    // filesystem, and because "the OS still works without it" is the point being made.
    let serving = aletheia::ai::llama::served_models(&cfg.endpoint);
    match (serving, cfg.entry.as_ref()) {
        (Some(ids), Some(e))
            if ids.iter().any(|i| {
                i.to_ascii_lowercase()
                    .contains(&e.serve_id.to_ascii_lowercase())
            }) =>
        {
            println!("serving:   yes — {}", ids.join(", "))
        }
        (Some(ids), _) => println!(
            "serving:   a DIFFERENT model is on {} — {}",
            cfg.endpoint,
            ids.join(", ")
        ),
        (None, _) => println!(
            "serving:   nothing answers {} — the Core falls back to the deterministic interpreter",
            cfg.endpoint
        ),
    }
}

/// Run the operation-surface benchmark and print the table (ADR-052, REQ-AI-005).
fn model_bench(dir: &std::path::Path) {
    let cfg = aletheia::ai::config::AiConfig::resolve(Some(dir));
    match aletheia::ai::bench::run(&cfg) {
        Ok(report) => {
            print!("{}", aletheia::ai::bench::render(&report));
            // A benchmark that exits 0 whatever it measured cannot be a gate. Any operation the
            // model could not plan is a non-zero exit, so a script can depend on the verdict.
            if report.passed() != report.total() {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("benchmark refused: {e}");
            std::process::exit(2);
        }
    }
}

fn serve(args: &[String]) {
    let dir = data_dir(args);
    let sock = arg_value(args, "--socket")
        .or_else(|| std::env::var("ALETHEIA_SOCK").ok())
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("aletheia.sock")
                .to_string_lossy()
                .into_owned()
        });
    let svc = CoreService::open(&dir).expect("open core");
    println!(
        "Aletheia Core Alpha — serving on {sock} (transport: {})",
        aletheia::transport::backend_name()
    );
    println!("  data-dir = {dir}");
    println!("  clients connect via the capability-gated endpoint IPC boundary; Ctrl-C to stop.");
    serve_endpoint(svc, &sock).expect("serve");
}

fn demo(args: &[String]) {
    let dir = data_dir(args);
    println!("Aletheia Core Alpha (hosted) — demo CLIENT over the in-process service boundary");
    println!("data-dir = {dir}");
    println!("(the app below touches ONLY Request/Response — never Core internals)\n");

    let mut svc = CoreService::open(&dir).expect("open core");

    // Bootstrap the owner's root capability (hosted root of trust).
    let owner = svc
        .handle(Request::BootstrapOwner {
            subject: "human:owner".into(),
        })
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
        intent: Intent {
            subject: "human:owner".into(),
            verb: Verb::Derive {
                source: rec_id.clone(),
                into_type: EntityType::Output,
                content: "master-v1.wav bytes".into(),
            },
        },
        approve: false,
    }));
    print_trace(&svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent {
            subject: "human:owner".into(),
            verb: Verb::Traverse {
                from: rec_id.clone(),
                edge: "derived_from".into(),
            },
        },
        approve: false,
    }));

    // policy: a destructive op stops for approval, then a human grants it via the policy surface.
    let del = svc.handle(Request::SubmitIntent {
        caps: vec![owner.clone()],
        intent: Intent {
            subject: "human:owner".into(),
            verb: Verb::Delete { id: rec_id.clone() },
        },
        approve: false,
    });
    print_trace(&del);
    if let Some(approval_id) = del.data["approval_id"].as_str() {
        println!("-> pending approval [{approval_id}] on the policy surface; granting...");
        print_trace(&svc.handle(Request::ResolveApproval {
            caps: vec![owner.clone()],
            approval_id: approval_id.to_string(),
            granted: true,
        }));
    }

    // capabilities: a read-only agent, scoped and revocable.
    let agent_cap = svc
        .handle(Request::Grant {
            caps: vec![owner.clone()],
            subject: "agent:reviewer".into(),
            action: "entity.read".into(),
            scope_entities: vec![rec_id.clone()],
            approval: false,
        })
        .data["token"]
        .as_str()
        .map(|s| s.to_string());
    if let Some(cap) = agent_cap {
        let denied = svc.handle(Request::SubmitIntent {
            caps: vec![cap],
            intent: Intent {
                subject: "agent:reviewer".into(),
                verb: Verb::Delete { id: rec_id.clone() },
            },
            approve: true,
        });
        println!(
            "read-only agent attempts destructive op -> ok={} ({})",
            denied.ok,
            denied.data["capability_decision"].as_str().unwrap_or("")
        );
    }

    // audit: the immutable event log.
    let audit = svc.handle(Request::QueryAudit {
        caps: vec![owner.clone()],
        limit: 100,
    });
    let n = audit.data.as_array().map(|a| a.len()).unwrap_or(0);
    println!("\naudit surface: {n} immutable events recorded.");
    println!("run `aletheiad serve` for the long-running Core behind the endpoint boundary;");
    println!(
        "set MODEL_ENDPOINT + start llama-server to route interpretation through the local model."
    );
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
