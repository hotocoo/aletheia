# ADR-018: Native Context Engine (Context Fabric), not RAG

**Status:** Accepted · **Date:** 2026-07-21

## Context

A small model (MiniCPM5-1B) makes context efficiency critical. The temptation is a heavyweight RAG
pipeline (always-on embedding server, vector DB, rerankers, OCR). That is the wrong shape for an OS
core: Aletheia already understands its own world — entities, relationships, provenance, permissions,
temporal state, ownership — and should retrieve the *minimum relevant, authorized* information per
task, not dump histories or documents into the model.

## Decision

A native, provider-independent **Context Engine** (`ai/context.rs`) built around the World Model.
Layered, structured-first retrieval: **direct** (subject, focus entity, held authority) → **structured
world** (entity queries) → **relationship traversal** (from the focus) → **memory** (recent actions),
with **semantic** (embeddings) and **knowledge** (documents) as OPTIONAL trait seams
(`SemanticRetriever`, `KnowledgeService`) — never core dependencies. No embedding server or vector
database is required for normal operation.

Retrieval is **capability-aware**: it runs after identity/capability is established and authorizes
every entity (`entity.read`) *before* it enters the model context — a subject with no capability
gets no world context. Output is a compact, typed `AiContext` with an explicit `ContextBudget`
(tight for the 1B model), rendered rather than dumped. Wired into the pipeline: `run_intent` assembles
context through the engine and records authorized provenance in the trace.

## Consequences

- "Compress the recording I edited yesterday" resolves via structured + relationship + time queries,
  not a vector search over every object.
- Semantic/vector and document knowledge can be added later behind the optional interfaces when a
  real use case requires them, without changing the core runtime.
- The engine is an Aletheia-owned abstraction (World Model + capabilities), not a Linux service or a
  document-chatbot stack; it re-implements natively on the future OS unchanged.
