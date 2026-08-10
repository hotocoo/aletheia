//! `aletheiad` — Aletheia Core Alpha daemon + hosted experience surface.
//!
//! Two modes, both exercising the SAME capability-gated service boundary (ADR-016, SAD §17):
//!   aletheiad serve [--socket PATH] [--data DIR]   long-running Core behind the endpoint IPC
//!                                                  boundary (clients connect and issue Requests)
//!   aletheiad [demo] [--data DIR]                  runs the UC-001..004 scenario AS A CLIENT over
//!                                                  the in-process boundary — the app never touches
//!                                                  Core internals, only Request/Response.
use aletheia::ai::provider::ModelProvider as _;
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
        "console" => console_cmd(&args),
        _ => demo(&args),
    }
}

/// `aletheiad console <ops|plan|bench>` — the kernel console as a planning surface (ADR-053).
///
/// This command PLANS; it does not type. What it prints on stdout is console lines and nothing else,
/// one per line, so a driver can pipe them at a live machine's serial port; everything explanatory
/// goes to stderr. That split is the whole reason it is usable as a gate: a message that leaked into
/// stdout would be typed at the console as a command.
fn console_cmd(args: &[String]) {
    let dir = model_dir(args);
    let dirp = std::path::Path::new(&dir);
    match args.get(2).map(|s| s.as_str()) {
        Some("ops") => console_ops_list(),
        Some("cases") => console_cases(),
        Some("agent-cases") => console_agent_cases(),
        Some("bench") => console_bench(dirp),
        Some("plan") => {
            let request = console_request(args);
            if request.trim().is_empty() {
                eprintln!("usage: aletheiad console plan [--approve] [--interpreter model|deterministic] <request>");
                std::process::exit(2);
            }
            console_plan(dirp, &request, args);
        }
        Some("agent") => {
            let request = console_request(args);
            if request.trim().is_empty() {
                eprintln!(
                    "usage: aletheiad console agent --transcript FILE [--approve] [--budget N]\n\
                     \x20                          [--context-file BRIEF] [--observation-file CAPTURE]\n\
                     \x20                          [--interpreter model|deterministic] <request>"
                );
                std::process::exit(2);
            }
            console_agent(dirp, &request, args);
        }
        _ => {
            eprintln!("usage: aletheiad console <ops|cases|plan|agent|bench>");
            std::process::exit(2);
        }
    }
}

/// The agent cases, as tab-separated records, for the same reason `console cases` exists: the gate
/// drives the table the tests hold, rather than keeping its own copy of it in shell.
///
/// Fields: natural request, scripted request, the line that must be typed, what the live console
/// says when it runs, what the control arm's answer must contain, approved.
/// The case table, as tab-separated rows for the shell gate.
///
/// A case may be reachable by more than one command, so the `must_type` and `console_says` columns
/// are `|`-separated and INDEX-ALIGNED: the driver requires some i where alternative i was typed and
/// the console printed reply i. Aligned rather than two independent sets, because "some asserted
/// line ran and some asserted text appeared" is a much weaker claim than the one this gate makes,
/// and the difference between them is invisible in a passing log.
fn console_agent_cases() {
    for c in aletheia::ai::agent::AGENT_CASES {
        let lines: Vec<&str> = c.must_type.iter().map(|r| r.line).collect();
        let says: Vec<&str> = c.must_type.iter().map(|r| r.console_says).collect();
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            c.natural,
            c.scripted,
            lines.join("|"),
            says.join("|"),
            c.answer_contains,
            c.approved
        );
    }
}

/// The request is every word that is neither a flag nor a flag's VALUE. Filtering only on the
/// leading `--` silently swallowed `--context-file /tmp/ctx` into the request, and the model was
/// then asked to plan a sentence with a path in the middle of it.
///
/// The list of value-taking flags is shared by `plan` and `agent` for that same reason: two copies
/// of it would drift, and the drift presents as a path appearing in the middle of an operator's
/// sentence, which reads as the model being confused.
fn console_request(args: &[String]) -> String {
    const TAKES_VALUE: &[&str] = &[
        "--interpreter",
        "--context",
        "--context-file",
        "--data",
        "--transcript",
        "--observation-file",
        "--budget",
    ];
    let mut words: Vec<String> = Vec::new();
    let mut skip_next = false;
    for a in args.iter().skip(3) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with("--") {
            skip_next = TAKES_VALUE.contains(&a.as_str());
            continue;
        }
        words.push(a.clone());
    }
    words.join(" ")
}

/// Every command the model may propose, with its arguments and its risk — the same table the prompt
/// is generated from, so an operator can read exactly what the model was offered.
fn console_ops_list() {
    println!("{:<10} {:<22} {:<12} does", "command", "args", "risk");
    for op in aletheia::console_ops::all() {
        let args = if op.args.is_empty() {
            "-".to_string()
        } else {
            op.args.join(", ")
        };
        println!(
            "{:<10} {:<22} {:<12} {}",
            op.name,
            args,
            format!("{:?}", op.risk),
            op.doc
        );
    }
}

/// The benchmark's cases, as tab-separated records, so the E2E gate drives the SAME eight requests
/// the benchmark measures instead of keeping its own copy of them in shell. Two lists of cases would
/// drift, and the drift would show up as a gate and a benchmark disagreeing about a model that had
/// not changed.
///
/// Fields: natural request, literal request, expected line, approved, what the live console says.
fn console_cases() {
    for c in aletheia::ai::console::CASES {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            c.natural, c.literal, c.expect, c.approved, c.console_says
        );
    }
}

/// Which interpreter a plan came from, chosen the way the pipeline chooses: the model when one is
/// really there, the deterministic arm otherwise — and it is always SAID, because a silent fallback
/// is how a live model and no model at all came to look identical once already (ADR-052).
fn console_interpreter(
    dir: &std::path::Path,
    args: &[String],
) -> Box<dyn aletheia::ai::provider::ModelProvider> {
    let forced = arg_value(args, "--interpreter").unwrap_or_else(|| "auto".into());
    if forced == "deterministic" {
        eprintln!("interpreter: deterministic-console (forced)");
        return Box::new(aletheia::ai::console::DeterministicConsole);
    }
    let cfg = aletheia::ai::config::AiConfig::resolve(Some(dir));
    if cfg.wants_local_model() {
        let p = aletheia::ai::llama::LlamaCppProvider::from_config(&cfg).for_console();
        if p.healthy() {
            eprintln!("interpreter: {} at {}", p.name(), cfg.endpoint);
            return Box::new(p);
        }
        if forced == "model" {
            eprintln!(
                "interpreter: no model answers {} — refusing, because `--interpreter model` was asked for",
                cfg.endpoint
            );
            std::process::exit(3);
        }
        eprintln!(
            "interpreter: deterministic-console (nothing answers {})",
            cfg.endpoint
        );
    } else {
        eprintln!(
            "interpreter: deterministic-console (AI_PROVIDER={})",
            cfg.provider
        );
    }
    Box::new(aletheia::ai::console::DeterministicConsole)
}

/// The console's current state, handed to the interpreter as DATA (ADR-018 applied to this surface).
///
/// `--context-file` is how the E2E driver passes what the live guest just printed: it types `ls`,
/// captures the answer and hands it back, so the model plans against the machine that exists rather
/// than the one it imagines. Absent, the brief is empty and the model is planning blind — which is
/// legitimate for a one-off request and measurably worse, so it is not silent.
fn console_context(args: &[String]) -> String {
    match arg_value(args, "--context-file") {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("context: cannot read {p}: {e} — planning with no console state");
                String::new()
            }
        },
        None => arg_value(args, "--context").unwrap_or_default(),
    }
}

fn console_plan(dir: &std::path::Path, request: &str, args: &[String]) {
    let approved = args.iter().any(|a| a == "--approve");
    let provider = console_interpreter(dir, args);
    let context = console_context(args);
    match aletheia::ai::console::plan_lines(
        provider.as_ref(),
        "human:operator",
        request,
        &context,
        approved,
    ) {
        Ok((lines, _raw)) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(e) => {
            eprintln!("refused: {e}");
            std::process::exit(1);
        }
    }
}

/// Which agent drove a session — chosen the same way, and said the same way, as the single-step
/// interpreter, because a silent fallback is how a live model and no model at all came to look
/// identical once already (ADR-052).
fn console_agent_interpreter(
    dir: &std::path::Path,
    args: &[String],
) -> Box<dyn aletheia::ai::agent::ConsoleAgent> {
    let forced = arg_value(args, "--interpreter").unwrap_or_else(|| "auto".into());
    if forced == "deterministic" {
        eprintln!("agent: deterministic-agent (forced)");
        return Box::new(aletheia::ai::agent::DeterministicAgent);
    }
    let cfg = aletheia::ai::config::AiConfig::resolve(Some(dir));
    if cfg.wants_local_model() {
        let p = aletheia::ai::llama::LlamaCppProvider::from_config(&cfg).for_console();
        if aletheia::ai::provider::ModelProvider::healthy(&p) {
            eprintln!(
                "agent: {} at {}",
                aletheia::ai::agent::ConsoleAgent::name(&p),
                cfg.endpoint
            );
            return Box::new(p);
        }
        if forced == "model" {
            eprintln!(
                "agent: no model answers {} — refusing, because `--interpreter model` was asked for",
                cfg.endpoint
            );
            std::process::exit(3);
        }
        eprintln!(
            "agent: deterministic-agent (nothing answers {})",
            cfg.endpoint
        );
    } else {
        eprintln!("agent: deterministic-agent (AI_PROVIDER={})", cfg.provider);
    }
    Box::new(aletheia::ai::agent::DeterministicAgent)
}

/// `aletheiad console agent` — one turn of the loop (ADR-054).
///
/// The command is deliberately ONE TURN rather than a loop of its own, because the thing that types
/// at the console is not this process. The driver — a gate script, or an operator — owns the serial
/// port; Aletheia owns the decision. So the state lives in a file between calls and the contract is
/// an exit code:
///
/// * **0** — stdout holds exactly one console line. Type it, capture the reply, call again with
///   `--observation-file`.
/// * **10** — the session answered. stdout is EMPTY; the answer is on stderr, because stdout on this
///   command means "type this" and an answer typed at a console is a syntax error at best.
/// * **1** — refused, terminally. The transcript on disk says why.
///
/// That stdout discipline is the same one `console plan` has, and it is what makes both usable from
/// a shell without a parser.
fn console_agent(dir: &std::path::Path, request: &str, args: &[String]) {
    use aletheia::ai::agent::{self, Advance, Session};

    let path = match arg_value(args, "--transcript") {
        Some(p) => p,
        None => {
            eprintln!("agent: --transcript FILE is required — the session's state lives there");
            std::process::exit(2);
        }
    };
    let budget: usize = arg_value(args, "--budget")
        .and_then(|b| b.parse().ok())
        .unwrap_or(agent::DEFAULT_BUDGET);
    let approved = args.iter().any(|a| a == "--approve");

    // Resume, or open. A transcript that exists belongs to a request; continuing it with a different
    // one would answer a question with another question's evidence.
    let mut session = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => match serde_json::from_str::<Session>(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("agent: {path} is not a session transcript: {e}");
                std::process::exit(2);
            }
        },
        _ => Session::new(request, &console_context(args), budget, approved),
    };
    if session.request != request {
        eprintln!("refused: {}", agent::AgentRefusal::RequestChanged);
        std::process::exit(1);
    }

    // The reply to the line typed last turn, if the driver brought one.
    if let Some(obs) = arg_value(args, "--observation-file") {
        match std::fs::read_to_string(&obs) {
            Ok(text) => {
                if let Err(r) = session.observe(&text) {
                    eprintln!("refused: {r}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("agent: cannot read {obs}: {e}");
                std::process::exit(2);
            }
        }
    }

    let driver = console_agent_interpreter(dir, args);
    let corrections_before = session.corrections.len();
    // Timed here rather than inside `advance`, because what an operator waits for is the whole turn
    // — every model call it took, including the ones that were corrected and never typed. A number
    // that counted only the successful call would report a turn as fast precisely when it was slow.
    let started = std::time::Instant::now();
    let outcome = agent::advance(&mut session, driver.as_ref());
    let elapsed_ms = started.elapsed().as_millis();
    // A correction is a model proposal that Aletheia refused and re-asked WITHOUT typing anything.
    // It is reported on stderr, never stdout, because stdout is the line the driver types — but it
    // is reported, because a turn that silently cost three model calls is a turn nobody can debug.
    for c in session.corrections.iter().skip(corrections_before) {
        eprintln!("corrected: `{}` — {}", c.proposed, c.refusal);
    }
    // One machine-readable line per turn, so the gate can report a median instead of a claim.
    eprintln!(
        "turn-ms: {elapsed_ms} calls: {}",
        1 + session.corrections.len() - corrections_before
    );
    // The transcript is written whatever happened — including on a refusal, which is the case where
    // somebody most wants to read it.
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&session).unwrap_or_default(),
    ) {
        eprintln!("agent: cannot write {path}: {e}");
        std::process::exit(2);
    }
    match outcome {
        Ok(Advance::Type(line)) => {
            eprintln!(
                "step {} of {}",
                session.turns.len(),
                session.turns.len() + session.budget
            );
            println!("{line}");
        }
        Ok(Advance::Done(answer)) => {
            eprintln!("answer: {answer}");
            std::process::exit(10);
        }
        Err(r) => {
            eprintln!("refused: {r}");
            std::process::exit(1);
        }
    }
}

fn console_bench(dir: &std::path::Path) {
    let cfg = aletheia::ai::config::AiConfig::resolve(Some(dir));
    match aletheia::ai::console::bench(&cfg) {
        Ok(report) => {
            print!("{}", aletheia::ai::console::render(&report));
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

/// Every model this machine can run — DISCOVERED from the local model cache, not from a list
/// compiled into the binary — plus any model Aletheia has characterized that is not here yet. `*`
/// marks the running selection.
fn model_list(dir: &std::path::Path) {
    let selected = aletheia::ai::config::AiConfig::resolve(Some(dir))
        .entry
        .map(|e| e.id);
    let all = aletheia::ai::registry::catalog();
    if all.is_empty() {
        println!(
            "no models found in {}",
            aletheia::ai::registry::hf_hub_root().display()
        );
        println!(
            "pull one, or set MODEL_PATH — the OS runs on the deterministic interpreter meanwhile"
        );
        return;
    }
    println!("   {:<26} {:<10} {:>9}  state", "id", "quant", "size");
    for e in &all {
        let mark = if Some(&e.id) == selected.as_ref() {
            "*"
        } else {
            " "
        };
        let size = if e.size_bytes > 0 {
            format!("{} MiB", e.size_bytes / (1024 * 1024))
        } else {
            "-".to_string()
        };
        // A model name can be enormous (community GGUF repos routinely run past 60 characters), and
        // one long row must not push every other column out of alignment. The id is ELIDED in the
        // middle rather than truncated at the end: the tail is what distinguishes two quants of the
        // same family, so cutting it off would produce two rows that look identical. A unique
        // prefix still selects, so a shortened display never costs the operator the ability to type
        // it.
        let shown = if e.id.chars().count() > 26 {
            let head: String = e.id.chars().take(14).collect();
            let tail: String =
                e.id.chars()
                    .skip(e.id.chars().count().saturating_sub(9))
                    .collect();
            format!("{head}…{tail}")
        } else {
            e.id.clone()
        };
        println!(
            "{mark}  {:<26} {:<10} {:>9}  {}{}",
            shown,
            if e.quant.is_empty() { "-" } else { &e.quant },
            size,
            e.tag(),
            if e.default { ", default" } else { "" }
        );
    }
    println!(
        "\nscanned {}\nselect one with: aletheiad model use <id>   (a unique prefix is enough)",
        aletheia::ai::registry::hf_hub_root().display()
    );
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
        Some(p) if p.exists() => {
            println!("weights:   present — {}", p.display());
            // Hashing gigabytes takes seconds, so it happens HERE — where an operator asked a
            // question — and never in front of an interpretation. A check that made the OS slow
            // would be a check somebody turns off.
            println!(
                "integrity: {}",
                aletheia::ai::runtime::verify_integrity(&cfg).describe()
            );
        }
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
