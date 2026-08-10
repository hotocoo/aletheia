//! The model registry — which models this OS knows about, and which one it is running (ADR-052,
//! REQ-AI-004).
//!
//! Before this module the resident model was a pair of `const`s. That made the model a property of
//! the *source*, so changing it meant editing and rebuilding the OS, and there was no way for the
//! machine to answer "which model am I actually running?" other than by reciting a compile-time
//! string. The registry makes the model a property of the *system*: a set of pinned manifests, one
//! of them selected, the selection persisted next to the data the Core already owns.
//!
//! **The manifests are embedded, not read from disk.** `include_str!` puts them in the binary, so
//! `aletheiad` on a machine with no checkout still knows its own model set — and so a manifest can
//! never disagree with the binary that was built from it. The registry is a closed set on purpose:
//! an operator selects among models Aletheia has pinned (repo, file, quant, checksum), rather than
//! naming an arbitrary hub path the OS has made no claim about. `MODEL_REF`/`MODEL_PATH` remain the
//! documented escape hatch for anything outside the set, and they say plainly that they are one.
//!
//! **Selection is persisted, not inferred.** `<data>/ai/selected-model` holds one id. It is written
//! only by `model use`, so a machine that has never been switched runs the manifest marked
//! `default` — and a machine that HAS been switched keeps running that choice across reboots
//! without an environment variable anyone has to remember to set.
//!
//! **Parsing is a deliberate subset of TOML.** These manifests are `key = value` under section
//! headers, with strings, integers, floats and booleans — nothing else. A subset parser that
//! refuses what it does not understand is a smaller thing to trust than a general one, and this
//! crate's dependency budget (ADR-004) is spent where the risk is, not on reading eight files it
//! also authors.
use std::path::{Path, PathBuf};

/// One pinned model. Every field the OS needs to *find*, *serve*, *verify* and *report* a model —
/// so no caller has to reach past this struct into a manifest again.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    /// Short id the operator types: `aletheiad model use <id>`.
    pub id: String,
    /// Human name, as the model's own publisher writes it.
    pub name: String,
    /// Hugging Face repo id, or empty for a model produced locally (Aletheia-LM).
    pub repo: String,
    /// The exact GGUF within the repo. Honored before "largest file in the cache", so a repo that
    /// ships several quants resolves to the one this manifest pinned.
    pub file: String,
    pub quant: String,
    /// Measured SHA-256 of the artifact, or empty when there is no published artifact to pin.
    pub sha256: String,
    pub size_bytes: u64,
    /// Substring the serving backend's advertised model id must contain. The benchmark refuses to
    /// record a number until this matches — see `super::bench`.
    pub serve_id: String,
    /// The entry used when nothing has been selected. Exactly one manifest may set it.
    pub default: bool,
    /// `ready` (an artifact exists to fetch or a path is set) or `pretraining` (it does not yet).
    pub status: String,
    /// Environment variable naming locally produced weights, for entries with no hub artifact.
    pub path_env: String,
    pub backend: String,
    pub endpoint: String,
    pub context: u32,
    /// True for a model whose chat template forces a `<think>` phase; the request path then asks
    /// the backend to disable it, because a strict grammar and a forced think phase collide.
    pub thinking: bool,
    pub temperature: f32,
    pub top_p: f32,
    pub structured_output: String,
}

impl ModelEntry {
    /// Is this a model whose weights can be fetched at all? An entry with no repo is produced
    /// locally, and `model pull` must say so rather than build an impossible URL.
    pub fn is_provisionable(&self) -> bool {
        !self.repo.is_empty() && !self.file.is_empty()
    }

    /// Is the model declared finished? A `pretraining` entry is selectable — the operator may line
    /// the switch up before the weights land — but never reported as present.
    pub fn is_ready(&self) -> bool {
        self.status != "pretraining"
    }
}

/// The manifests, embedded at build time. Adding a model is adding a `.toml` and one line here.
const MANIFESTS: &[&str] = &[
    include_str!("../../../models/lfm2.5.toml"),
    include_str!("../../../models/minicpm.toml"),
    include_str!("../../../models/aletheia-lm.toml"),
];

/// Every model this OS knows about, in manifest order.
pub fn builtin() -> Vec<ModelEntry> {
    MANIFESTS.iter().filter_map(|m| parse(m)).collect()
}

/// Look one up by id.
pub fn find(id: &str) -> Option<ModelEntry> {
    builtin().into_iter().find(|e| e.id == id)
}

/// The entry a machine runs when nobody has chosen: the manifest marked `default`, and if none is
/// (which a unit test forbids), the first one — a registry that returned nothing would take the AI
/// subsystem down over a missing flag.
pub fn default_entry() -> Option<ModelEntry> {
    let all = builtin();
    all.iter()
        .find(|e| e.default)
        .cloned()
        .or_else(|| all.first().cloned())
}

/// Where a machine's selection lives, under the data directory the Core already owns.
pub fn selection_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ai").join("selected-model")
}

/// The persisted selection, if there is one AND it names a model still in the registry. An id that
/// no longer exists (a manifest removed between builds) reads as "no selection" rather than as an
/// error the whole daemon dies of.
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

/// Parse one manifest. Unknown keys are ignored (a manifest may carry documentation fields the code
/// has no use for); missing keys take the stated default below, so a manifest never has to restate
/// what every model shares.
fn parse(src: &str) -> Option<ModelEntry> {
    let mut e = ModelEntry {
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
        temperature: 0.3,
        top_p: 0.95,
        structured_output: "gbnf-grammar".into(),
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
            "temperature" => e.temperature = val.parse().unwrap_or(0.3),
            "top_p" => e.top_p = val.parse().unwrap_or(0.95),
            "structured_output" => e.structured_output = val.into(),
            _ => {}
        }
    }
    // An entry with no id cannot be selected and would sit in `model list` as a blank row. Refusing
    // it here is what keeps a malformed manifest from becoming an unreachable menu item.
    if e.id.is_empty() {
        return None;
    }
    Some(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_manifest_parses() {
        let all = builtin();
        assert_eq!(all.len(), MANIFESTS.len(), "a manifest failed to parse");
        for e in &all {
            assert!(!e.id.is_empty());
            assert!(!e.name.is_empty());
        }
    }

    #[test]
    fn exactly_one_manifest_is_the_default_and_it_is_lfm2_5() {
        let all = builtin();
        let defaults: Vec<&ModelEntry> = all.iter().filter(|e| e.default).collect();
        assert_eq!(defaults.len(), 1, "exactly one model may be the default");
        assert_eq!(defaults[0].id, "lfm2.5");
        assert_eq!(default_entry().unwrap().id, "lfm2.5");
    }

    #[test]
    fn ids_are_unique() {
        let all = builtin();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.id, b.id, "duplicate model id {}", a.id);
            }
        }
    }

    #[test]
    fn the_default_entry_pins_a_checksum_and_a_size() {
        let e = default_entry().unwrap();
        assert_eq!(e.sha256.len(), 64, "a pinned model needs a full sha256");
        assert!(e.size_bytes > 0);
        assert!(e.is_provisionable());
        assert!(e.is_ready());
    }

    #[test]
    fn the_first_party_model_is_selectable_but_not_ready() {
        let e = find("aletheia-lm").expect("aletheia-lm is registered before its weights exist");
        assert!(!e.is_ready(), "it is still pretraining");
        assert!(
            !e.is_provisionable(),
            "there is no hub artifact to pull for it"
        );
        assert_eq!(e.path_env, "ALETHEIA_LM_MODEL");
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
        save_selection(&dir, "no-such-model").unwrap();
        assert!(load_selection(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
