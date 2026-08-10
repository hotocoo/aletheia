//! Model runtime: discovery + lifecycle (ADR-017).
//!
//! Aletheia OWNS the model's lifecycle even though inference currently runs as an external macOS
//! `llama-server` process. The model is referenced by a configurable Hugging Face repo id and
//! resolved to a local GGUF through the HF cache — never a hardcoded machine-specific path, and the
//! weights are never copied into the repo. When the native Aletheia OS exists, the same `AiConfig`
//! resolves to a native model service and this file is replaced without touching orchestration.
use super::config::AiConfig;
use super::llama::endpoint_host_port;
use std::path::{Path, PathBuf};

/// Default Hugging Face hub cache root. Delegates to the registry's resolver so the directory this
/// module reads and the directory the catalog scans cannot diverge — two answers to "where is the
/// cache" is how a model appears in `model list` and then fails to load.
pub fn default_hf_hub() -> PathBuf {
    super::registry::hf_hub_root()
}

/// HF cache directory name for a repo id: `org/name` → `models--org--name`.
pub fn ref_to_cache_dirname(model_ref: &str) -> String {
    format!("models--{}", model_ref.replace('/', "--"))
}

/// Find the GGUF for `model_ref` under `cache_root`, choosing the largest `.gguf` across snapshots
/// (the highest-fidelity available quant). Returns None if the model isn't cached.
pub fn resolve_in_cache(cache_root: &Path, model_ref: &str) -> Option<PathBuf> {
    resolve_in_cache_file(cache_root, model_ref, "")
}

/// Find the GGUF for `model_ref`, preferring the EXACT `file` the manifest pinned and falling back
/// to the largest `.gguf` when no name was given (or the named one is not cached).
///
/// The exact-name pass is not a nicety. A repo that ships `Q4_K_M` and `Q8_0` resolves by size to
/// whichever is bigger, which is a different set of weights from the one whose checksum, context and
/// sampling parameters this OS pinned — so `model use` would report one model and the provider would
/// load another, with nothing anywhere saying they differed.
pub fn resolve_in_cache_file(cache_root: &Path, model_ref: &str, file: &str) -> Option<PathBuf> {
    let snaps = cache_root
        .join(ref_to_cache_dirname(model_ref))
        .join("snapshots");
    let mut best: Option<(u64, PathBuf)> = None;
    for snap in std::fs::read_dir(&snaps).ok()?.flatten() {
        let entries = match std::fs::read_dir(snap.path()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for f in entries.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("gguf") {
                continue;
            }
            if !file.is_empty() && p.file_name().and_then(|n| n.to_str()) == Some(file) {
                return Some(p);
            }
            // Follow symlink (HF stores blobs behind snapshot symlinks) for the real size.
            let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if best.as_ref().map(|(b, _)| sz > *b).unwrap_or(true) {
                best = Some((sz, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Resolve the model to a concrete GGUF path: explicit `MODEL_PATH` wins, else HF-cache discovery
/// for the manifest's pinned file.
pub fn resolve_model_path(cfg: &AiConfig) -> Option<PathBuf> {
    if let Some(p) = &cfg.model_path {
        return Some(PathBuf::from(p));
    }
    // The catalog already FOUND this model on this machine — that path is the truth about what will
    // be loaded, and re-deriving it from the repo id could disagree with what `model list` showed.
    if let Some(p) = cfg.entry.as_ref().and_then(|e| e.path.clone()) {
        return Some(p);
    }
    // A model with no repo id (one produced locally, ADR-052) is not in any cache: it is present
    // only when its path was named. Returning None here is what makes `model status` say
    // "not present" instead of resolving some other repo's weights by accident.
    if cfg.model_ref.is_empty() {
        return None;
    }
    let file = cfg.entry.as_ref().map(|e| e.file.as_str()).unwrap_or("");
    resolve_in_cache_file(&default_hf_hub(), &cfg.model_ref, file)
}

/// What checking a model's pinned checksum concluded (ADR-052 follow-on).
///
/// Three outcomes rather than a bool, because "I did not check" and "it does not match" are
/// different facts and collapsing them is how an unverified model comes to be reported as a verified
/// one. `Unpinned` is a legitimate state — a model produced locally has no published artifact to pin
/// — and it is said out loud rather than counted as a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    /// The file's SHA-256 equals the manifest's.
    Verified,
    /// The file's SHA-256 differs. Carries what was found, so the operator can tell a corrupted
    /// download from a different quant sitting under the expected name.
    Mismatch { expected: String, found: String },
    /// The manifest pins no checksum (a locally produced model), so there is nothing to check.
    Unpinned,
    /// The file could not be read.
    Unreadable(String),
}

impl Integrity {
    /// True ONLY for a checked, matching file. `Unpinned` is deliberately not `ok`: a caller that
    /// wants to admit unpinned models must say so, rather than getting it by default.
    pub fn is_verified(&self) -> bool {
        matches!(self, Integrity::Verified)
    }
    /// One line for `model status`.
    pub fn describe(&self) -> String {
        match self {
            Integrity::Verified => "verified (sha256 matches the pinned manifest)".into(),
            Integrity::Mismatch { expected, found } => format!(
                "MISMATCH — the manifest pins {}…, this file hashes to {}…",
                &expected[..expected.len().min(16)],
                &found[..found.len().min(16)]
            ),
            Integrity::Unpinned => "not pinned (no published artifact to check against)".into(),
            Integrity::Unreadable(why) => format!("could not be read: {why}"),
        }
    }
}

/// Hash the resolved weights and compare against the manifest's pinned SHA-256.
///
/// ADR-052 shipped manifests that RECORD a checksum without anything verifying it, and named that
/// as its own open question. This closes it. The file is streamed rather than read whole: these are
/// gigabyte-scale artifacts, and a verification step that needs 1.6 GB of resident memory to run is
/// one that gets skipped on exactly the machines that most need it.
///
/// This is deliberately NOT called on the hot path. Hashing a multi-gigabyte file costs seconds, so
/// it belongs where an operator asks a question (`model status`, `model pull`) rather than in front
/// of every interpretation — a check that made the OS slow would be a check someone turns off.
pub fn verify_integrity(cfg: &AiConfig) -> Integrity {
    let Some(expected) = cfg.entry.as_ref().map(|e| e.sha256.clone()) else {
        return Integrity::Unpinned;
    };
    if expected.is_empty() {
        return Integrity::Unpinned;
    }
    let Some(path) = resolve_model_path(cfg) else {
        return Integrity::Unreadable("no weights are present".into());
    };
    match sha256_file(&path) {
        Err(e) => Integrity::Unreadable(e),
        Ok(found) if found.eq_ignore_ascii_case(&expected) => Integrity::Verified,
        Ok(found) => Integrity::Mismatch { expected, found },
    }
}

/// SHA-256 of a file, streamed in fixed-size chunks.
fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(crate::crypto::hex(&hasher.finalize()))
}

/// Best-effort: launch a hosted `llama-server` for the configured model. Hosted-dev convenience
/// only — the Core never requires it: an externally managed server or the deterministic fallback
/// both work. The caller owns the returned child process. `ctx` is the context window (`-c`).
///
/// Matches the model card's recommended invocation (chat template is embedded in the GGUF, so no
/// `--chat-template` is needed): `llama-server -m <gguf> -c <ctx> --port <port> --jinja`.
pub fn spawn_llama_server(cfg: &AiConfig, ctx: u32) -> std::io::Result<std::process::Child> {
    let path = resolve_model_path(cfg).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "model GGUF not found in cache",
        )
    })?;
    let (_, port) = endpoint_host_port(&cfg.endpoint);
    std::process::Command::new("llama-server")
        .arg("-m")
        .arg(&path)
        .arg("-c")
        .arg(ctx.to_string())
        .arg("--port")
        .arg(port.to_string())
        // `--jinja` renders the model's own chat template, which is what makes llama-server PARSE
        // tool calls into `message.tool_calls` instead of leaving them as prose in `content`. The
        // console planner (ADR-053) speaks that channel, and without this flag it sees a response
        // with no tool call — a model that answered correctly, reported as a model that said
        // nothing. The Core's JSON path is unaffected, so this is safe for every caller.
        .arg("--jinja")
        .spawn()
}

/// Provision the configured model into the local cache if missing, returning its path (ADR-017).
/// Aletheia-OWNED lifecycle: this is how the model "comes with" Aletheia without a 1.1 GB blob in
/// git (see `models/minicpm.toml`). Prefers `huggingface-cli`/`hf` (which verify LFS integrity),
/// falling back to `curl` into a cache snapshot the resolver will find. Best-effort — the OS never
/// requires it (deterministic fallback). NOT called by tests or the default demo; provisioning is
/// explicit via `aletheiad model pull`.
pub fn ensure_model(cfg: &AiConfig) -> Result<PathBuf, String> {
    if let Some(p) = resolve_model_path(cfg) {
        if p.exists() {
            return Ok(p);
        }
    }
    // The file to fetch is the SELECTED model's, not the compiled-in default's: pulling after
    // `model use minicpm` must fetch MiniCPM's GGUF, and a model with nothing to fetch must say so
    // rather than construct a hub URL out of two empty strings.
    let entry = cfg.entry.as_ref();
    if let Some(e) = entry {
        if !e.is_provisionable() {
            return Err(format!(
                "{} has no published artifact to pull — it is produced locally; set {} to its weights",
                e.id,
                if e.path_env.is_empty() { "MODEL_PATH" } else { &e.path_env }
            ));
        }
    }
    let file = entry
        .map(|e| e.file.as_str())
        .filter(|f| !f.is_empty())
        .unwrap_or(super::config::DEFAULT_MODEL_FILE);
    // Preferred: the HF CLI places the file in the standard cache and verifies its checksum.
    for tool in ["huggingface-cli", "hf"] {
        match std::process::Command::new(tool)
            .arg("download")
            .arg(&cfg.model_ref)
            .arg(file)
            .output()
        {
            Ok(o) if o.status.success() => {
                if let Some(p) = resolve_model_path(cfg) {
                    return Ok(p);
                }
                let printed = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !printed.is_empty() && Path::new(&printed).exists() {
                    return Ok(PathBuf::from(printed));
                }
            }
            _ => {} // tool absent or failed → try the next option
        }
    }
    // Fallback: curl the resolve URL into a manual snapshot dir the cache resolver will find.
    let dest_dir = default_hf_hub()
        .join(ref_to_cache_dirname(&cfg.model_ref))
        .join("snapshots")
        .join("manual");
    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    let dest = dest_dir.join(file);
    let url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        cfg.model_ref, file
    );
    let status = std::process::Command::new("curl")
        .args(["-fL", "--retry", "3", "-o"])
        .arg(&dest)
        .arg(&url)
        .status()
        .map_err(|e| format!("curl unavailable: {e}"))?;
    if status.success() && dest.exists() {
        return Ok(dest);
    }
    Err(format!(
        "could not provision model {}; run: huggingface-cli download {} {}",
        cfg.model_ref, cfg.model_ref, file
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dirname_matches_hf_layout() {
        assert_eq!(
            ref_to_cache_dirname("LiquidAI/LFM2.5-2.6B-GGUF"),
            "models--LiquidAI--LFM2.5-2.6B-GGUF"
        );
        assert_eq!(
            ref_to_cache_dirname("GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF"),
            "models--GnLOLot--MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF"
        );
    }

    /// The bug the exact-name pass exists to prevent: two quants in one repo, and the pinned one is
    /// the SMALLER. Size-based discovery alone would hand the provider the other set of weights.
    #[test]
    fn the_pinned_filename_wins_over_the_bigger_file() {
        let root = std::env::temp_dir().join(format!("hf-pin-{}", crate::domain::new_id()));
        let snap = root.join("models--org--m").join("snapshots").join("abc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("m-Q4_K_M.gguf"), vec![0u8; 10]).unwrap();
        std::fs::write(snap.join("m-Q8_0.gguf"), vec![0u8; 100]).unwrap();
        let found = resolve_in_cache_file(&root, "org/m", "m-Q4_K_M.gguf").unwrap();
        assert_eq!(found.file_name().unwrap(), "m-Q4_K_M.gguf");
        // With no name pinned, the old behavior stands: the largest available quant.
        let found = resolve_in_cache_file(&root, "org/m", "").unwrap();
        assert_eq!(found.file_name().unwrap(), "m-Q8_0.gguf");
        // A pinned name that is not cached falls back rather than failing: the operator asked for a
        // model, and the cache has a copy of it, just not that quant.
        let found = resolve_in_cache_file(&root, "org/m", "m-Q2_K.gguf").unwrap();
        assert_eq!(found.file_name().unwrap(), "m-Q8_0.gguf");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A locally produced model has no repo id, so there is no cache directory to search. It must
    /// resolve to nothing rather than to whatever the empty-string repo happens to hash to.
    #[test]
    fn a_model_with_no_repo_never_resolves_from_the_cache() {
        let cfg = AiConfig {
            model_ref: String::new(),
            model_path: None,
            entry: super::super::registry::find("aletheia-lm"),
            ..AiConfig::default()
        };
        assert!(resolve_model_path(&cfg).is_none());
    }

    #[test]
    fn resolves_largest_gguf_from_a_synthetic_cache() {
        let root = std::env::temp_dir().join(format!("hf-{}", crate::domain::new_id()));
        let snap = root.join("models--org--m").join("snapshots").join("abc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("small-Q4.gguf"), vec![0u8; 10]).unwrap();
        std::fs::write(snap.join("big-Q8.gguf"), vec![0u8; 100]).unwrap();
        std::fs::write(snap.join("README.md"), b"not a model").unwrap();
        let found = resolve_in_cache(&root, "org/m").unwrap();
        assert_eq!(found.file_name().unwrap(), "big-Q8.gguf");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_model_resolves_to_none() {
        let root = std::env::temp_dir().join(format!("hf-empty-{}", crate::domain::new_id()));
        assert!(resolve_in_cache(&root, "no/such").is_none());
    }
}
