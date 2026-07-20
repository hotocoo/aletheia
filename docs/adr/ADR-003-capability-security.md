# ADR-003 — Capability-based security as the sole authority model

**Status:** Accepted
**Context:** Ambient authority (a process inherits the user's full power) is unsafe generally and catastrophic with autonomous intelligence.
**Decision:** Capabilities are the only authority: unforgeable, scoped, least-privilege, delegable-with-attenuation, revocable, audited. No ambient authority at any layer. Absence of a matching capability → DENY (fail closed). Unforgeability rooted in kernel enforcement.
**Consequences:** Every action passes capability evaluation. Agents/apps hold their own capabilities. This is what makes native intelligence safe (INV-011/012).
