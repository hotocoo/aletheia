# ADR-002 — Seven-primitive domain model

**Status:** Accepted
**Context:** Legacy primitives (file/process/window/socket) discard relationships, provenance, context, and authority — the very things humans and intelligences reason about.
**Decision:** Organize the OS around Entity → Capability → Context → Intent → Action → Memory → Relationship. Files/processes/windows may exist only as compatibility projections, never as primitives (INV-013).
**Consequences:** The domain crate models these seven; all higher layers derive from them. Uniform querying, provenance, and permissioning become possible. Forbids reintroducing legacy primitives as system concepts.
