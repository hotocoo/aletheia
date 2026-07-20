# ADR-007 — Agents as first-class capability-controlled actors

**Status:** Accepted
**Context:** Intelligence must not be a chatbot bolted on; it must be a controllable system citizen.
**Decision:** Agents are entities with explicit identity, capability set, memory, context, tools, goals, and relationships. They act only through capabilities granted to their identity, only via the action pipeline, isolated, bounded (step/loop/resource), cancellable, and audited. Their grants are independently revocable.
**Consequences:** Multi-agent composition is mediated by capabilities, never shared authority. Safe autonomy by construction (AG-001…007).
