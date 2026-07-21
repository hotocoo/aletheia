# ADR-017: AI as a first-class Aletheia subsystem behind a model-agnostic provider

**Status:** Accepted · **Date:** 2026-07-21

## Context

The model had been an unrelated external process. Aletheia must own the AI integration —
lifecycle, configuration, context construction, prompt/response protocol, and a model-provider
abstraction — while keeping the actual inference process (a macOS `llama-server` in the hosted
phase) an implementation detail. The Core must not depend on llama.cpp APIs or a hardcoded model.

## Decision

An `ai/` subsystem structured around a model-agnostic `ModelProvider` (the pipeline's interpreter
trait). Submodules: `config` (env: `AI_PROVIDER`/`MODEL_BACKEND`/`MODEL_ENDPOINT`/`MODEL_REF`,
`MODEL_PATH`), `provider`, `context`, `prompt` (protocol + GBNF plan grammar + `<think>`-stripping
extraction), `runtime` (HF-cache GGUF discovery + `llama-server` lifecycle), `llama`
(`LlamaCppProvider` — dependency-free localhost HTTP).

Hosted-dev primary model: `GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF` (Q8_0), resolved
from the local Hugging Face cache **by configurable reference** — never a hardcoded path, weights
never in git. The deterministic interpreter remains the fallback and test oracle; the OS is fully
functional with no resident model (INT-004).

**Structured-output strategy (live-validated):** MiniCPM is a *thinking* model; a strict JSON
grammar from token 0 collides with its forced `<think>` phase and yields empty output. The provider
runs it in **no-think mode + GBNF grammar** (`enable_thinking=false`, temp 0.7), producing clean,
correct plan JSON.

## Consequences

- llama.cpp is confined to one file; a future native Aletheia model service implements the same
  `ModelProvider` and drops in without touching orchestration, world model, capabilities, or
  execution.
- The model never executes: it proposes a `Plan` the Core independently validates, authorizes,
  policy-checks, executes, verifies, and records (INV-014).
- No heavy dependency added (no HTTP client crate, no async runtime).
