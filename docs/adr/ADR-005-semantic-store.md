# ADR-005 — Semantic content-addressed, versioned, encrypted store as system of record

**Status:** Accepted
**Context:** The filesystem loses type, relationships, provenance, and versions.
**Decision:** The system of record is a semantic object store: entities + typed relationships with provenance; content is content-addressed and immutable; mutation creates linked versions; content and sensitive metadata are encrypted at rest; access is capability-gated (no global readable namespace). Filesystem semantics are an optional non-authoritative projection.
**Consequences:** Provenance and world-model queries are first-class. Engine sits behind a `Store` trait (SQLite/sled in M1). Enables INV-009 and PRD §14.
