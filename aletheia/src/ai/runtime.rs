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

/// Default Hugging Face hub cache root (`~/.cache/huggingface/hub`).
pub fn default_hf_hub() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join(".cache").join("huggingface").join("hub")
}

/// HF cache directory name for a repo id: `org/name` → `models--org--name`.
pub fn ref_to_cache_dirname(model_ref: &str) -> String {
    format!("models--{}", model_ref.replace('/', "--"))
}

/// Find the GGUF for `model_ref` under `cache_root`, choosing the largest `.gguf` across snapshots
/// (the highest-fidelity available quant). Returns None if the model isn't cached.
pub fn resolve_in_cache(cache_root: &Path, model_ref: &str) -> Option<PathBuf> {
    let snaps = cache_root.join(ref_to_cache_dirname(model_ref)).join("snapshots");
    let mut best: Option<(u64, PathBuf)> = None;
    for snap in std::fs::read_dir(&snaps).ok()?.flatten() {
        let entries = match std::fs::read_dir(snap.path()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for f in entries.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) == Some("gguf") {
                // Follow symlink (HF stores blobs behind snapshot symlinks) for the real size.
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map(|(b, _)| sz > *b).unwrap_or(true) {
                    best = Some((sz, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Resolve the model to a concrete GGUF path: explicit `MODEL_PATH` wins, else HF-cache discovery.
pub fn resolve_model_path(cfg: &AiConfig) -> Option<PathBuf> {
    if let Some(p) = &cfg.model_path {
        return Some(PathBuf::from(p));
    }
    resolve_in_cache(&default_hf_hub(), &cfg.model_ref)
}

/// Best-effort: launch a hosted `llama-server` for the configured model. Hosted-dev convenience
/// only — the Core never requires it: an externally managed server or the deterministic fallback
/// both work. The caller owns the returned child process. `ctx` is the context window (`-c`).
///
/// Matches the model card's recommended invocation (chat template is embedded in the GGUF, so no
/// `--jinja`/`--chat-template` is needed): `llama-server -m <gguf> -c <ctx> --port <port>`.
pub fn spawn_llama_server(cfg: &AiConfig, ctx: u32) -> std::io::Result<std::process::Child> {
    let path = resolve_model_path(cfg)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "model GGUF not found in cache"))?;
    let (_, port) = endpoint_host_port(&cfg.endpoint);
    std::process::Command::new("llama-server")
        .arg("-m")
        .arg(&path)
        .arg("-c")
        .arg(ctx.to_string())
        .arg("--port")
        .arg(port.to_string())
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dirname_matches_hf_layout() {
        assert_eq!(
            ref_to_cache_dirname("GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF"),
            "models--GnLOLot--MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF"
        );
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
