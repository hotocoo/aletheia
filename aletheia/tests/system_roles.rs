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
