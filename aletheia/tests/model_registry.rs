//! The model registry and the benchmark, exercised through the PUBLIC surface (REQ-AI-004,
//! REQ-AI-005, ADR-052).
//!
//! The unit tests inside `ai/registry.rs` and `ai/bench.rs` prove the pieces. These prove the thing
//! an operator actually does: switch the model, and have the switch mean something. They are an
//! integration test rather than more unit tests because the property under test spans three modules
//! — a selection written by one, resolved by another, and honored by a third — and a property that
//! spans modules is exactly the one that unit tests each pass while the system is broken.
use aletheia::ai::config::{AiConfig, DEFAULT_MODEL_FILE, DEFAULT_MODEL_REF, DEFAULT_MODEL_SHA256};
use aletheia::ai::{bench, prompt, registry};

/// A fresh, isolated data directory. Named per test so a failing test never leaves a selection that
/// changes what the NEXT test resolves — which would make the suite order-dependent in the one place
/// where order-dependence is invisible.
fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aletheia-reg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// A machine with no selection still resolves to SOMETHING — the catalog is discovered, so which
/// model that is depends on what this machine has pulled, and asserting `lfm2.5` here would be a
/// test that passes on the author's laptop and fails on a runner with a different cache. What must
/// hold everywhere is that resolution never comes back empty and never comes back incoherent.
#[test]
fn a_machine_with_no_selection_still_resolves_a_coherent_model() {
    let dir = scratch("default");
    let cfg = AiConfig::resolve(Some(&dir));
    let e = cfg
        .entry
        .as_ref()
        .expect("resolution always yields an entry");
    assert!(!e.id.is_empty());
    assert_eq!(
        cfg.model_ref, e.repo,
        "the configuration must describe the entry it came from"
    );
    if e.pinned && !e.sha256.is_empty() {
        assert_eq!(e.sha256.len(), 64, "a pin is a full sha256 or nothing");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The compiled-in constants describe the DEFAULT manifest, and a copy is a thing that drifts. This
/// is machine-independent because it reads the manifests, not the cache.
#[test]
fn the_constants_match_the_manifest_marked_default() {
    let d = registry::manifests()
        .into_iter()
        .find(|e| e.default)
        .expect("one manifest is marked default");
    assert_eq!(d.repo, DEFAULT_MODEL_REF);
    assert_eq!(d.file, DEFAULT_MODEL_FILE);
    assert_eq!(d.sha256, DEFAULT_MODEL_SHA256);
}

/// The catalog is DISCOVERED. A model this machine holds must appear even though no manifest and no
/// line of source has ever named it — that is the difference between a registry and a hardcoded list.
#[test]
fn the_catalog_reports_models_that_no_manifest_names() {
    let known: Vec<String> = registry::manifests().into_iter().map(|e| e.repo).collect();
    for e in registry::catalog() {
        if e.present && !known.contains(&e.repo) {
            assert!(!e.pinned, "an uncharacterized model must not claim a pin");
            assert!(
                e.sha256.is_empty(),
                "{} claims a checksum nobody pinned",
                e.id
            );
            assert!(
                e.path.is_some(),
                "a present model names the file it will load"
            );
            return;
        }
    }
    // A machine whose cache holds only the characterized models is a legitimate state; there is
    // simply nothing to assert here. Say so rather than passing silently on a vacuous run.
    eprintln!("note: this machine holds no uncharacterized GGUF model — discovery not exercised");
}

#[test]
fn switching_the_model_changes_what_the_next_resolution_returns() {
    let dir = scratch("switch");
    registry::save_selection(&dir, "minicpm").expect("persist");
    let cfg = AiConfig::resolve(Some(&dir));
    let e = cfg.entry.as_ref().expect("entry");
    assert_eq!(e.id, "minicpm");
    // The whole point: the SAMPLING and the strategy travel with the model, so switching does not
    // silently keep the previous model's parameters.
    assert!(e.thinking, "minicpm is a forced-thinking model");
    assert_eq!(e.structured_output, "gbnf-grammar");
    assert_eq!(cfg.model_ref, e.repo);

    registry::save_selection(&dir, "lfm2.5").expect("persist");
    let back = AiConfig::resolve(Some(&dir));
    let e = back.entry.as_ref().expect("entry");
    assert_eq!(e.id, "lfm2.5");
    assert!(!e.thinking);
    assert_eq!(e.structured_output, "json-schema");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The switch has to be usable BEFORE the weights exist, or it is not a switch anyone can prepare.
/// And selecting a model that does not exist yet must never resolve to a path — a fallback here is
/// how an operator ends up believing their own model is answering when it is not.
#[test]
fn the_first_party_model_can_be_selected_before_it_is_trained() {
    let dir = scratch("firstparty");
    registry::save_selection(&dir, "aletheia-lm").expect("persist");
    let cfg = AiConfig::resolve(Some(&dir));
    let e = cfg.entry.as_ref().expect("entry");
    assert_eq!(e.id, "aletheia-lm");
    assert!(!e.is_ready());
    assert!(!e.is_provisionable());
    assert!(
        aletheia::ai::runtime::resolve_model_path(&cfg).is_none(),
        "an untrained model must not resolve to any weights"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unknown_selection_falls_back_to_the_default_rather_than_failing() {
    let dir = scratch("unknown");
    registry::save_selection(&dir, "a-model-that-was-removed-from-this-machine").expect("persist");
    let cfg = AiConfig::resolve(Some(&dir));
    let picked = cfg
        .entry
        .as_ref()
        .expect("a stale selection must not break resolution");
    assert_ne!(picked.id, "a-model-that-was-removed-from-this-machine");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The benchmark must refuse rather than measure when it cannot establish what is answering. This
/// is the guard that stops another process holding the port from being reported as this model.
#[test]
fn the_benchmark_refuses_an_endpoint_it_cannot_identify() {
    let dir = scratch("identity");
    let cfg = AiConfig {
        endpoint: "http://127.0.0.1:59997".into(),
        ..AiConfig::resolve(Some(&dir))
    };
    let err = bench::run(&cfg).expect_err("nothing is listening, so nothing may be measured");
    assert!(err.contains("is not serving"), "unexpected refusal: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every operation the prompt offers a model is one the benchmark measures, and every one of those
/// operations has its ARGUMENTS named. If these lists diverge, the benchmark reports a score over a
/// surface the model was never properly asked about.
#[test]
fn the_prompt_names_every_operation_and_every_argument() {
    let sys = prompt::system_prompt();
    for op in prompt::OPERATIONS {
        assert!(sys.contains(op), "{op} is not in the system prompt");
        let meta = aletheia::tools::lookup(op).expect("registered operation");
        for arg in meta.args {
            assert!(
                sys.contains(arg),
                "the prompt does not name `{arg}`, an argument of {op}"
            );
        }
    }
    let schema = prompt::plan_json_schema();
    let listed = schema["properties"]["steps"]["items"]["properties"]["op"]["enum"]
        .as_array()
        .expect("the schema enumerates operations");
    assert_eq!(listed.len(), prompt::OPERATIONS.len());
}
