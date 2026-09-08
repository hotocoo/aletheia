# ADR-087: Context Is a Bounded, Owned, Expiring Resource

**Status:** Accepted · **Date:** 2026-09-08

## Context

ADR-018 defines capability-aware, structured-first context retrieval, but retrieval alone does not
define the lifetime of the resulting context. An AI session must not acquire an implicit, unbounded
history containing private world state.

## Decision

`aletheia/src/ai/context.rs` owns the lifecycle contract through `ContextLifecyclePolicy` and
`ContextLifecycle`:

- **size:** `ContextBudget` bounds entities, relationships, memory and rendered characters;
- **compression:** deterministic deduplication and priority-ordered eviction reduce a retained
  snapshot to the policy budget before retention;
- **summarization:** each retained snapshot receives a deterministic bounded summary of its owner,
  focus and retained source counts;
- **retention:** only a fixed number of snapshots may exist (`max_retained`, default 8), with oldest
  eviction when capacity is reached;
- **expiration:** every resource has an explicit TTL (default 300 time units) and expired payloads
  are cleared before removal;
- **privacy:** context is memory-resident only and is never persisted through `Store`; release and
  expiration clear the owned context payload;
- **ownership/access control:** retrieval and release require the exact owner subject; an owner cannot
  inspect another subject's retained resource even when its numeric id is known.

`SysCore::run_intent` retains the freshly authorized context through this lifecycle before rendering
it to the model. Thus the normal AI path has an explicit resource boundary instead of an implicit,
ever-growing transcript.

## Consequences

Context remains small enough for the hosted small model, private context cannot become durable merely
because an AI request ran, and repeated requests cannot grow memory without bound. The lifecycle is
deterministic and provider-independent. Semantic retrieval and model-generated natural-language
summaries remain optional extension points; the core contract does not require a resident model.

