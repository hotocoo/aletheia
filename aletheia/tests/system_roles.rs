//! System 1 and System 2 as registry ROLES (ADR-186), through the public surface.
//!
//! Nothing here names a model: the tests read which System-1 entries the manifests declare and
//! prove the properties every occupant must have, so replacing the occupant cannot break them.
use aletheia::ai::registry::{self, Role};

fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aletheia-roles-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

fn system1_manifests() -> Vec<registry::ModelEntry> {
    registry::manifests()
        .into_iter()
        .filter(|e| e.role == Role::System1)
        .collect()
}

#[test]
fn every_manifest_declares_a_role_and_system1_has_a_threshold() {
    let all = registry::manifests();
    assert!(all.iter().any(|e| e.role == Role::System2));
    for e in system1_manifests() {
        assert!(
            e.confidence > 0.0 && e.confidence <= 1.0,
            "{}: threshold {}",
            e.id,
            e.confidence
        );
        assert!(!e.serve_id.is_empty(), "{}: a port is not a model", e.id);
    }
}

#[test]
fn a_role_never_defaults_to_the_other_roles_model() {
    let empty = scratch("empty-cache");
    let all = registry::catalog_in(&empty);
    if let Some(d) = registry::default_of(&all) {
        assert_eq!(d.role, Role::System2);
    }
    if let Some(d) = registry::default_of_role(&all, Role::System1) {
        assert_eq!(d.role, Role::System1);
    }
}

#[test]
fn a_selection_of_the_wrong_role_reads_as_no_selection() {
    let Some(s1) = system1_manifests().into_iter().next() else {
        return; // no System-1 occupant characterized: nothing to cross
    };
    let dir = scratch("cross");
    // Hand-write the System-2 selection file with a System-1 id: it must not be honoured.
    registry::save_selection_for(&dir, Role::System2, &s1.id).unwrap();
    assert!(registry::load_selection_for(&dir, Role::System2).is_none());
    // Written where it belongs, it is.
    registry::save_selection_for(&dir, Role::System1, &s1.id).unwrap();
    assert_eq!(
        registry::load_selection_for(&dir, Role::System1).map(|e| e.id),
        Some(s1.id.clone())
    );
    assert_ne!(
        registry::selection_path_for(&dir, Role::System1),
        registry::selection_path_for(&dir, Role::System2)
    );
}

#[test]
fn a_manifest_file_of_any_format_is_found_in_the_cache() {
    let Some(s1) = system1_manifests().into_iter().next() else {
        return;
    };
    assert!(!s1.repo.is_empty() && !s1.file.is_empty());
    let root = scratch("cache");
    let snap = root
        .join(format!("models--{}", s1.repo.replace('/', "--")))
        .join("snapshots")
        .join("0000");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(snap.join(&s1.file), b"weights").unwrap();
    let found = registry::catalog_in(&root)
        .into_iter()
        .find(|e| e.id == s1.id)
        .expect("the manifest entry is listed");
    assert!(found.present, "the named file is in the cache");
    assert_eq!(found.path, Some(snap.join(&s1.file)));
    assert_eq!(found.size_bytes, 7);

    let bare = scratch("bare");
    let absent = registry::catalog_in(&bare)
        .into_iter()
        .find(|e| e.id == s1.id)
        .unwrap();
    assert!(!absent.present && absent.path.is_none());
}

#[test]
fn an_unfit_model_is_listed_but_never_fit() {
    for e in registry::manifests() {
        if e.status == "unfit" {
            assert!(!e.is_fit());
            assert!(e.tag().contains("unfit"));
        }
    }
}

fn archive_entry(tag: &str) -> (registry::ModelEntry, std::path::PathBuf) {
    let src = scratch(&format!("{tag}-src"));
    std::fs::write(src.join("model.safetensors"), b"weights").unwrap();
    std::fs::write(src.join("rl_agent_config.json"), b"{}").unwrap();
    let tar = src.join("ckpt.tar");
    assert!(std::process::Command::new("tar")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(&src)
        .args(["model.safetensors", "rl_agent_config.json"])
        .status()
        .unwrap()
        .success());
    let digest = {
        use sha2::{Digest, Sha256};
        aletheia::crypto::hex(&Sha256::digest(std::fs::read(&tar).unwrap()))
    };
    let mut e = registry::manifests()
        .into_iter()
        .next()
        .expect("a manifest");
    e.id = format!("archive-{tag}");
    e.repo = "aletheia/test-archive".into();
    e.file = "model.safetensors".into();
    e.url = format!("file://{}", tar.display());
    e.archive_sha256 = digest;
    (e, scratch(&format!("{tag}-cache")))
}

#[test]
fn a_published_archive_is_verified_then_unpacked_where_discovery_looks() {
    let (e, cache) = archive_entry("ok");
    let p = aletheia::ai::runtime::pull_archive(&e, &cache).expect("pull");
    assert_eq!(std::fs::read(&p).unwrap(), b"weights");
    assert_eq!(
        aletheia::ai::runtime::cached_file(&cache, &e.repo, &e.file),
        Some(p.clone())
    );
    assert!(p.with_file_name("rl_agent_config.json").exists());
    // A second pull is a no-op that names the same file.
    assert_eq!(aletheia::ai::runtime::pull_archive(&e, &cache).unwrap(), p);
}

#[test]
fn an_archive_that_does_not_match_its_pin_unpacks_nothing() {
    let (mut e, cache) = archive_entry("bad");
    e.archive_sha256 = "0".repeat(64);
    let err = aletheia::ai::runtime::pull_archive(&e, &cache).unwrap_err();
    assert!(err.contains("MISMATCH"), "{err}");
    assert!(aletheia::ai::runtime::cached_file(&cache, &e.repo, &e.file).is_none());
    e.archive_sha256.clear();
    let err = aletheia::ai::runtime::pull_archive(&e, &cache).unwrap_err();
    assert!(err.contains("unverified"), "{err}");
}

#[test]
fn serving_is_built_from_the_manifest_and_refused_by_name() {
    use aletheia::ai::runtime::serve_command;
    let root = scratch("sidecars");
    std::fs::write(root.join("fake_server.py"), b"").unwrap();
    let weights = scratch("serve-weights").join("model.safetensors");
    std::fs::write(&weights, b"w").unwrap();
    let mut e = system1_manifests()
        .into_iter()
        .next()
        .unwrap_or_else(|| registry::manifests().into_iter().next().unwrap());
    e.role = Role::System1;
    e.backend = "fake".into();
    e.serve_id = "s1-under-test".into();

    e.path = None;
    let err = serve_command(&e, "http://127.0.0.1:8091", &root).unwrap_err();
    assert!(err.contains("model pull"), "{err}");

    e.path = Some(weights.clone());
    let c = serve_command(&e, "http://127.0.0.1:8123", &root).unwrap();
    let args: Vec<String> = c
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(c.get_program(), "python3");
    assert!(args[0].ends_with("fake_server.py"));
    assert_eq!(args[1], weights.parent().unwrap().to_string_lossy());
    assert_eq!(
        &args[2..],
        ["--serve-id", "s1-under-test", "--port", "8123"]
    );

    e.backend = "nobody".into();
    let err = serve_command(&e, "http://127.0.0.1:8123", &root).unwrap_err();
    assert!(err.contains("no System-1 server"), "{err}");

    e.role = Role::System2;
    e.backend = "llama_cpp".into();
    let c = serve_command(&e, "http://127.0.0.1:8099", &root).unwrap();
    let args: Vec<String> = c
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(c.get_program(), "llama-server");
    assert!(args.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
    assert!(args.windows(2).any(|w| w == ["--port", "8099"]));
    assert!(args.iter().any(|a| a == "--jinja"));
}
