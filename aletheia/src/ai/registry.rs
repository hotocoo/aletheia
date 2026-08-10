//! The model registry — what models this machine actually HAS, and which one it is running
//! (ADR-052, REQ-AI-004).
//!
//! **The catalog is discovered, not declared.** Aletheia scans the local Hugging Face cache and
//! reports the models that are really there, with their real files and their real sizes. A
//! hardcoded list would be the same defect this subsystem was built to remove — the model as a
//! property of the source tree — moved up one level: a machine that had pulled a model Aletheia's
//! source had never heard of could not run it, and a machine that had NOT pulled a listed one would
//! offer it anyway. Neither is a registry; both are a guess about somebody else's disk.
//!
//! **Manifests characterize; they do not enumerate.** `models/*.toml` carries what cannot be learned
//! from a directory listing: the checksum a file is supposed to have, the sampling parameters that
//! were measured rather than assumed, whether the chat template forces a `<think>` phase, and which
//! structured-output strategy actually works for that model. When a manifest matches a discovered
//! model, its facts are overlaid. When it does not, the model is still listed — as `unpinned`, which
//! is said out loud rather than hidden, because an unpinned model is one whose parameters Aletheia
//! is guessing at.
//!
//! **A manifest with no matching model is still shown.** That is how `aletheia-lm` — this OS's own
//! model, still pretraining — is selectable before its weights exist: the operator can line the
//! switch up now, and selecting it reports `NOT YET TRAINED` instead of silently serving something
//! else.
//!
//! **Selection is persisted, not inferred.** `<data>/ai/selected-model` holds one id, written only
//! by `model use`, so a machine that has been switched keeps running that choice across reboots
//! without an environment variable anyone has to remember.
//!
//! **Parsing is a deliberate subset of TOML.** These manifests are `key = value` under section
//! headers, with strings, integers, floats and booleans — nothing else. A subset parser that refuses
//! what it does not understand is a smaller thing to trust than a general one.
use std::path::{Path, PathBuf};

/// One model this machine can run, or one Aletheia has characterized and is waiting for.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    /// Short id the operator types: `aletheiad model use <id>`. Derived from the repo name for a
    /// discovered model; a manifest may declare a shorter alias.
    pub id: String,
    /// Human name, as the model's own publisher writes it.
    pub name: String,
    /// Hugging Face repo id, or empty for a model produced locally.
    pub repo: String,
    /// The GGUF file — the one actually found on disk for a discovered model.
    pub file: String,
    pub quant: String,
    /// Pinned SHA-256, or empty when nothing has pinned this model. Empty means UNPINNED, which is
    /// reported, never silently treated as verified.
    pub sha256: String,
    /// Real size on disk for a discovered model; the manifest's figure otherwise.
    pub size_bytes: u64,
    /// Substring the serving backend's advertised model id must contain (see `super::bench`).
    pub serve_id: String,
    /// Preferred when nothing has been selected.
    pub default: bool,
    /// `ready` or `pretraining`.
    pub status: String,
    /// Environment variable naming locally produced weights, for models with no hub artifact.
    pub path_env: String,
    pub backend: String,
    pub endpoint: String,
    pub context: u32,
    /// True for a model whose chat template forces a `<think>` phase.
    pub thinking: bool,
    pub temperature: f32,
    pub top_p: f32,
    pub structured_output: String,
    /// Was this model found on this machine?
    pub present: bool,
    /// Where it was found. `None` for a characterized model whose weights are absent.
    pub path: Option<PathBuf>,
    /// Did a manifest characterize it, or are its parameters defaults?
    pub pinned: bool,
}

impl ModelEntry {
    /// Can its weights be fetched at all? A model with no repo is produced locally.
    pub fn is_provisionable(&self) -> bool {
        !self.repo.is_empty() && !self.file.is_empty()
    }
    /// Is the model declared finished?
    pub fn is_ready(&self) -> bool {
        self.status != "pretraining"
    }
    /// One short phrase for `model list`.
    pub fn tag(&self) -> &'static str {
        match (self.present, self.is_ready(), self.pinned) {
            (_, false, _) => "not yet trained",
            (true, _, true) => "present, pinned",
            (true, _, false) => "present, unpinned",
            (false, _, _) => "not on this machine",
        }
    }
}

/// Sensible parameters for a model nobody has characterized. Every field here is a GUESS, which is
/// why `pinned` is false and why `model list` says so.
fn defaults() -> ModelEntry {
    ModelEntry {
        id: String::new(),
        name: String::new(),
        repo: String::new(),
        file: String::new(),
        quant: String::new(),
        sha256: String::new(),
        size_bytes: 0,
        serve_id: String::new(),
        default: false,
        status: "ready".into(),
        path_env: String::new(),
        backend: "llama_cpp".into(),
        endpoint: super::config::DEFAULT_ENDPOINT.into(),
        context: super::config::DEFAULT_MODEL_CTX,
        thinking: false,
        temperature: 0.0,
        top_p: 1.0,
        // json-schema rather than the grammar, for an uncharacterized model. The grammar is the
        // stricter constraint and therefore the better choice WHEN it is known to work — but it
        // fails by producing empty output on a model whose chat template opens with a token it has
        // no rule for, which is indistinguishable from a model that cannot plan. The schema path
        // degrades more honestly, so it is what an unknown model gets until someone measures it.
        structured_output: "json-schema".into(),
        present: false,
        path: None,
        pinned: false,
    }
}

/// Default Hugging Face hub cache root (`~/.cache/huggingface/hub`), honoring `HF_HOME`/`HF_HUB_CACHE`
/// the way the Hugging Face tooling itself does — a machine that has moved its cache has moved it
/// for a reason, and a scanner that ignored that would report an empty catalog on a full disk.
pub fn hf_hub_root() -> PathBuf {
    if let Ok(p) = std::env::var("HF_HUB_CACHE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(p) = std::env::var("HF_HOME") {
        if !p.is_empty() {
            return Path::new(&p).join("hub");
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home)
        .join(".cache")
        .join("huggingface")
        .join("hub")
}

/// `models--org--name` → `org/name`. The inverse of the layout `runtime::ref_to_cache_dirname`
/// writes, kept next to nothing else so the two can be read together.
fn cache_dirname_to_ref(dirname: &str) -> Option<String> {
    let rest = dirname.strip_prefix("models--")?;
    // A repo id has exactly one `/`; HF encodes it as `--`. An org or model name may itself contain
    // a hyphen, so splitting on the FIRST `--` is the only correct reading.
    let (org, name) = rest.split_once("--")?;
    if org.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{org}/{name}"))
}

/// A short, typeable id from a repo id: the model name, lowercased, with the `-GGUF` marker dropped.
/// `LiquidAI/LFM2.5-2.6B-GGUF` → `lfm2.5-2.6b`.
fn id_from_ref(model_ref: &str) -> String {
    let name = model_ref.rsplit('/').next().unwrap_or(model_ref);
    let name = name
        .strip_suffix("-GGUF")
        .or_else(|| name.strip_suffix("-gguf"))
        .unwrap_or(name);
    name.to_ascii_lowercase()
}

/// The quant, read out of the file name (`…-Q4_K_M.gguf` → `Q4_K_M`). Empty when the name does not
/// carry one — reported as unknown rather than guessed.
fn quant_from_file(file: &str) -> String {
    let stem = file.strip_suffix(".gguf").unwrap_or(file);
    stem.rsplit('-')
        .next()
        .filter(|q| {
            q.starts_with('Q') || q.starts_with('q') || q.starts_with("BF") || q.starts_with('F')
        })
        .unwrap_or("")
        .to_string()
}

/// A serve-id substring that identifies this model in a backend's `/v1/models`: the longest leading
/// run of the model name that is stable across quants. In practice the family + version, e.g.
/// `LFM2.5-2.6B`. Conservative on purpose — this string gates whether a benchmark may record a
/// number, so it must not match a DIFFERENT model that happens to share a prefix word.
fn serve_id_from_ref(model_ref: &str) -> String {
    let name = model_ref.rsplit('/').next().unwrap_or(model_ref);
    name.strip_suffix("-GGUF")
        .or_else(|| name.strip_suffix("-gguf"))
        .unwrap_or(name)
        .to_string()
}

/// Every GGUF model in the cache at `root`, one entry per repo.
///
/// A repo may hold several quants; the LARGEST is chosen, because absent any pin that is the
/// highest-fidelity copy the operator chose to keep — and the choice is recorded in `file`, so what
/// is reported is what will actually be loaded rather than a guess that can differ.
pub fn discover(root: &Path) -> Vec<ModelEntry> {
    let mut found: Vec<ModelEntry> = Vec::new();
    let Ok(dir) = std::fs::read_dir(root) else {
        return found;
    };
    for repo_dir in dir.flatten() {
        let Some(dirname) = repo_dir.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let Some(model_ref) = cache_dirname_to_ref(&dirname) else {
            continue;
        };
        let snaps = repo_dir.path().join("snapshots");
        let Ok(snap_iter) = std::fs::read_dir(&snaps) else {
            continue;
        };
        let mut best: Option<(u64, PathBuf)> = None;
        for snap in snap_iter.flatten() {
            let Ok(files) = std::fs::read_dir(snap.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("gguf") {
                    continue;
                }
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map(|(b, _)| sz > *b).unwrap_or(true) {
                    best = Some((sz, p));
                }
            }
        }
        let Some((size_bytes, path)) = best else {
            continue; // a cached repo with no GGUF is not a model this backend can run
        };
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        found.push(ModelEntry {
            id: id_from_ref(&model_ref),
            name: model_ref
                .rsplit('/')
                .next()
                .unwrap_or(&model_ref)
                .to_string(),
            quant: quant_from_file(&file),
            serve_id: serve_id_from_ref(&model_ref),
            size_bytes,
            repo: model_ref,
            file,
            present: true,
            path: Some(path),
            ..defaults()
        });
    }
    // Deterministic order: two runs on one machine must list models the same way, and a directory
    // iteration order is neither stable nor the same across filesystems.
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// The manifests Aletheia ships. These CHARACTERIZE models — they do not decide which exist.
const MANIFESTS: &[&str] = &[
    include_str!("../../../models/lfm2.5.toml"),
    include_str!("../../../models/minicpm.toml"),
    include_str!("../../../models/aletheia-lm.toml"),
];

/// The parsed manifests.
pub fn manifests() -> Vec<ModelEntry> {
    MANIFESTS.iter().filter_map(|m| parse(m)).collect()
}

/// The catalog: what is on this machine, characterized by a manifest wherever one matches, plus any
/// characterized model that is not here yet (so it can still be selected).
pub fn catalog() -> Vec<ModelEntry> {
    catalog_in(&hf_hub_root())
}

/// The catalog against a specific cache root — the form the tests drive.
pub fn catalog_in(root: &Path) -> Vec<ModelEntry> {
    let mut discovered = discover(root);
    let mut extra: Vec<ModelEntry> = Vec::new();

    for m in manifests() {
        match discovered
            .iter_mut()
            .find(|d| d.repo == m.repo && !m.repo.is_empty())
        {
            Some(d) => {
                // Overlay only what a manifest KNOWS and a listing cannot: identity, the pin, and
                // the measured behavior. `file`, `size_bytes` and `path` stay as discovered, because
                // the truth about what is on the disk is the disk.
                d.id = m.id.clone();
                d.name = m.name.clone();
                d.sha256 = m.sha256.clone();
                d.serve_id = m.serve_id.clone();
                d.default = m.default;
                d.status = m.status.clone();
                d.path_env = m.path_env.clone();
                d.backend = m.backend.clone();
                d.endpoint = m.endpoint.clone();
                d.context = m.context;
                d.thinking = m.thinking;
                d.temperature = m.temperature;
                d.top_p = m.top_p;
                d.structured_output = m.structured_output.clone();
                d.pinned = true;
                // The manifest pinned a specific quant and the cache holds it: prefer that file over
                // the largest one, so the checksum being verified is the checksum that was pinned.
                if !m.file.is_empty() && d.file != m.file {
                    if let Some(p) = super::runtime::resolve_in_cache_file(root, &d.repo, &m.file) {
                        if p.file_name().and_then(|n| n.to_str()) == Some(m.file.as_str()) {
                            d.size_bytes = std::fs::metadata(&p).map(|x| x.len()).unwrap_or(0);
                            d.file = m.file.clone();
                            d.quant = m.quant.clone();
                            d.path = Some(p);
                        }
                    }
                }
            }
            None => {
                // Characterized but not here. A locally produced model may still be PRESENT via the
                // path its manifest names — that is how this OS's own model becomes runnable the
                // moment its weights land, with no edit to any source or manifest.
                let mut m = m;
                if !m.path_env.is_empty() {
                    if let Some(p) = std::env::var(&m.path_env).ok().filter(|v| !v.is_empty()) {
                        let p = PathBuf::from(p);
                        if p.exists() {
                            m.size_bytes = std::fs::metadata(&p).map(|x| x.len()).unwrap_or(0);
                            m.file = p
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or_default()
                                .to_string();
                            m.present = true;
                            m.path = Some(p);
                            // Weights that exist are weights that are trained. The manifest's
                            // `pretraining` is a statement about the world at authoring time; the
                            // file on the disk is a statement about the world now, and the newer
                            // fact wins.
                            m.status = "ready".into();
                        }
                    }
                }
                extra.push(m);
            }
        }
    }
    discovered.append(&mut extra);
    discovered
}

/// Look one up by id, then by unique prefix, then by repo id. A machine may hold a dozen models with
/// long names; requiring the whole name to switch would make the switch unusable, and accepting an
/// AMBIGUOUS prefix would switch to a model the operator did not name — so a prefix is honored only
/// when exactly one model matches it.
pub fn find(id: &str) -> Option<ModelEntry> {
    find_in(&catalog(), id)
}

/// The resolution itself, against a supplied catalog.
pub fn find_in(all: &[ModelEntry], id: &str) -> Option<ModelEntry> {
    if let Some(e) = all.iter().find(|e| e.id == id) {
        return Some(e.clone());
    }
    if let Some(e) = all.iter().find(|e| e.repo == id) {
        return Some(e.clone());
    }
    let mut matches = all.iter().filter(|e| e.id.starts_with(id));
    let first = matches.next()?;
    match matches.next() {
        None => Some(first.clone()),
        Some(_) => None, // ambiguous: refuse rather than pick
    }
}

/// What a machine runs when nobody has chosen.
///
/// Preference order: a PRESENT model whose manifest marks it default (what this OS was tuned for and
/// what the operator actually has), then any present model, then a characterized one. The last case
/// matters — a machine with no models at all still resolves to an entry, and the deterministic
/// interpreter carries the OS until weights arrive.
pub fn default_entry() -> Option<ModelEntry> {
    default_of(&catalog())
}

pub fn default_of(all: &[ModelEntry]) -> Option<ModelEntry> {
    all.iter()
        .find(|e| e.default && e.present)
        .or_else(|| all.iter().find(|e| e.present && e.is_ready()))
        .or_else(|| all.iter().find(|e| e.default))
        .or_else(|| all.first())
        .cloned()
}

/// Where a machine's selection lives, under the data directory the Core already owns.
pub fn selection_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ai").join("selected-model")
}

/// The persisted selection, if there is one AND it still names a model this machine knows about. An
/// id that no longer resolves — a model deleted from the cache — reads as "no selection" rather than
/// as an error the whole daemon dies of.
pub fn load_selection(data_dir: &Path) -> Option<ModelEntry> {
    let id = std::fs::read_to_string(selection_path(data_dir)).ok()?;
    find(id.trim())
}

/// Persist a selection. Fails loudly: an operator who ran `model use` and got no error is entitled
/// to assume the next boot runs what they chose.
pub fn save_selection(data_dir: &Path, id: &str) -> std::io::Result<()> {
    let p = selection_path(data_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, id)
}

// ---------------------------------------------------------------------------------------------
// The manifest parser: a subset of TOML, and no more.
// ---------------------------------------------------------------------------------------------

/// Strip a trailing `#` comment that is not inside a string, then trim.
fn strip_comment(v: &str) -> &str {
    let bytes = v.as_bytes();
    let mut in_str = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_str = !in_str,
            b'#' if !in_str => return v[..i].trim_end(),
            _ => {}
        }
    }
    v.trim_end()
}

/// Unquote a string value; a bare value is returned as-is (numbers and booleans arrive here too).
fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
}

/// Parse one manifest. Unknown keys are ignored; missing keys take `defaults()`.
fn parse(src: &str) -> Option<ModelEntry> {
    let mut e = ModelEntry {
        pinned: true,
        ..defaults()
    };
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = unquote(strip_comment(v));
        match key {
            "id" => e.id = val.into(),
            "name" => e.name = val.into(),
            "repo" => e.repo = val.into(),
            "file" => e.file = val.into(),
            "quant" => e.quant = val.into(),
            "sha256" => e.sha256 = val.into(),
            "size_bytes" => e.size_bytes = val.parse().unwrap_or(0),
            "serve_id" => e.serve_id = val.into(),
            "default" => e.default = val == "true",
            "status" => e.status = val.into(),
            "path_env" => e.path_env = val.into(),
            "backend" => e.backend = val.into(),
            "endpoint" => e.endpoint = val.into(),
            "context" => e.context = val.parse().unwrap_or(super::config::DEFAULT_MODEL_CTX),
            "thinking" => e.thinking = val == "true",
            "temperature" => e.temperature = val.parse().unwrap_or(0.0),
            "top_p" => e.top_p = val.parse().unwrap_or(1.0),
            "structured_output" => e.structured_output = val.into(),
            _ => {}
        }
    }
    // An entry with no id cannot be selected and would sit in `model list` as a blank row.
    if e.id.is_empty() {
        return None;
    }
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic HF cache so discovery is tested against a layout we control rather than
    /// against whatever this developer happens to have pulled.
    fn fake_cache(tag: &str, repos: &[(&str, &[(&str, usize)])]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("hf-disc-{tag}-{}", crate::domain::new_id()));
        for (repo, files) in repos {
            let dirname = format!("models--{}", repo.replace('/', "--"));
            let snap = root.join(dirname).join("snapshots").join("abc123");
            std::fs::create_dir_all(&snap).unwrap();
            for (f, sz) in *files {
                std::fs::write(snap.join(f), vec![0u8; *sz]).unwrap();
            }
        }
        root
    }

    #[test]
    fn discovery_finds_models_the_source_has_never_heard_of() {
        let root = fake_cache(
            "unknown",
            &[(
                "SomeOrg/Totally-New-Model-GGUF",
                &[("Totally-New-Model-Q5_K_M.gguf", 64)],
            )],
        );
        let found = discover(&root);
        assert_eq!(found.len(), 1, "an uncatalogued model must still be found");
        let e = &found[0];
        assert_eq!(e.repo, "SomeOrg/Totally-New-Model-GGUF");
        assert_eq!(e.id, "totally-new-model");
        assert_eq!(e.quant, "Q5_K_M");
        assert_eq!(e.size_bytes, 64);
        assert!(e.present && !e.pinned, "found, but nobody characterized it");
        assert!(e.sha256.is_empty(), "an unpinned model claims no checksum");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_repo_with_several_quants_reports_the_file_it_would_load() {
        let root = fake_cache(
            "quants",
            &[("Org/M-GGUF", &[("M-Q4_K_M.gguf", 10), ("M-Q8_0.gguf", 100)])],
        );
        let e = discover(&root).remove(0);
        assert_eq!(e.file, "M-Q8_0.gguf");
        assert_eq!(e.quant, "Q8_0");
        assert_eq!(e.size_bytes, 100);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cached_repo_with_no_gguf_is_not_a_model() {
        let root = fake_cache("nogguf", &[("Org/Docs", &[("README.md", 5)])]);
        assert!(discover(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_is_deterministic() {
        let root = fake_cache(
            "order",
            &[
                ("Org/Zeta-GGUF", &[("Zeta-Q4_K_M.gguf", 8)]),
                ("Org/Alpha-GGUF", &[("Alpha-Q4_K_M.gguf", 8)]),
            ],
        );
        let ids: Vec<String> = discover(&root).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_org_or_model_name_containing_a_hyphen_round_trips() {
        assert_eq!(
            cache_dirname_to_ref("models--Liquid-AI--LFM2.5-2.6B-GGUF").as_deref(),
            Some("Liquid-AI/LFM2.5-2.6B-GGUF")
        );
        assert_eq!(cache_dirname_to_ref("datasets--org--x"), None);
        assert_eq!(cache_dirname_to_ref("models--onlyorg"), None);
    }

    #[test]
    fn a_manifest_characterizes_a_discovered_model_without_replacing_what_is_on_disk() {
        let root = fake_cache(
            "overlay",
            &[(
                "LiquidAI/LFM2.5-2.6B-GGUF",
                &[("LFM2.5-2.6B-Q4_K_M.gguf", 1234)],
            )],
        );
        let all = catalog_in(&root);
        let e = find_in(&all, "lfm2.5").expect("the manifest's short id resolves");
        assert!(e.pinned && e.present);
        assert_eq!(e.sha256.len(), 64, "the pin came from the manifest");
        assert_eq!(e.structured_output, "json-schema");
        assert_eq!(
            e.size_bytes, 1234,
            "the size is the file's, not the manifest's claim"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_characterized_model_that_is_absent_is_still_listed_and_selectable() {
        let root = fake_cache("absent", &[]);
        let all = catalog_in(&root);
        let e = find_in(&all, "aletheia-lm").expect("selectable before its weights exist");
        assert!(!e.present);
        assert!(!e.is_ready());
        assert_eq!(e.tag(), "not yet trained");
        // And the models Aletheia characterized but this machine has not pulled are visible too,
        // rather than vanishing from the menu.
        assert!(find_in(&all, "minicpm").is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_default_prefers_a_model_that_is_actually_present() {
        let root = fake_cache(
            "default",
            &[("Org/Other-GGUF", &[("Other-Q4_K_M.gguf", 9)])],
        );
        let all = catalog_in(&root);
        // lfm2.5 is marked default but is NOT on this machine, so the present model wins: a default
        // that resolves to absent weights is a default that does nothing.
        let d = default_of(&all).expect("something is always resolvable");
        assert_eq!(d.id, "other");
        assert!(d.present);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_prefix_selects_only_when_it_is_unambiguous() {
        let root = fake_cache(
            "prefix",
            &[
                ("Org/Qwen3-8B-GGUF", &[("Qwen3-8B-Q4_K_M.gguf", 8)]),
                ("Org/Qwen3-VL-GGUF", &[("Qwen3-VL-Q4_K_M.gguf", 8)]),
            ],
        );
        let all = catalog_in(&root);
        assert!(
            find_in(&all, "qwen3").is_none(),
            "an ambiguous prefix must refuse, not pick"
        );
        assert_eq!(
            find_in(&all, "qwen3-8b").map(|e| e.id),
            Some("qwen3-8b".into())
        );
        // A full repo id works too, for scripts that have one and no short id.
        assert_eq!(
            find_in(&all, "Org/Qwen3-VL-GGUF").map(|e| e.id),
            Some("qwen3-vl".into())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_embedded_manifest_parses_and_ids_are_unique() {
        let all = manifests();
        assert_eq!(all.len(), MANIFESTS.len(), "a manifest failed to parse");
        for (i, a) in all.iter().enumerate() {
            assert!(!a.id.is_empty() && !a.name.is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate model id {}", a.id);
            }
        }
    }

    #[test]
    fn exactly_one_manifest_is_marked_default_and_it_is_lfm2_5() {
        let defaults: Vec<ModelEntry> = manifests().into_iter().filter(|e| e.default).collect();
        assert_eq!(defaults.len(), 1, "exactly one model may be the default");
        assert_eq!(defaults[0].id, "lfm2.5");
        assert_eq!(defaults[0].sha256.len(), 64);
    }

    #[test]
    fn a_comment_after_a_value_is_not_part_of_the_value() {
        let e = parse("[model]\nid = \"x\"\nquant = \"Q8_0\"  # the highest we ship\n").unwrap();
        assert_eq!(e.quant, "Q8_0");
    }

    #[test]
    fn a_hash_inside_a_string_survives() {
        let e = parse("[model]\nid = \"x\"\nname = \"a # b\"\n").unwrap();
        assert_eq!(e.name, "a # b");
    }

    #[test]
    fn a_manifest_without_an_id_is_refused_rather_than_listed_blank() {
        assert!(parse("[model]\nname = \"nameless\"\n").is_none());
    }

    #[test]
    fn selection_round_trips_and_an_unknown_id_reads_as_no_selection() {
        let dir = std::env::temp_dir().join(format!("aletheia-sel-{}", crate::domain::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_selection(&dir).is_none());
        save_selection(&dir, "minicpm").unwrap();
        assert_eq!(load_selection(&dir).unwrap().id, "minicpm");
        save_selection(&dir, "no-such-model-anywhere").unwrap();
        assert!(load_selection(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
