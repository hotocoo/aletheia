# Aletheia
## Software Architecture Document (SAD)

**Document ID:** ALETHEIA-SAD-002
**Version:** 2.0.0
**Status:** Architecture Definition / Engineering Source of Truth
**Supersedes:** ALETHEIA-SAD-001 (`Aletheia_Software_Architecture_Document_v1_superseded.md`)
**Related PRD:** ALETHEIA-PRD-002
**Product:** Aletheia — AI-native operating system
**Initial Deployment:** Hosted System-Core reference (userspace) with a defined path to a native microkernel

---

# Document Control

This SAD is a ground-up rewrite that agrees with PRD-002. It defines the *architecture* of Aletheia as an operating system whose primitives are Entity, Capability, Context, Intent, Action, Memory, and Relationship. It supersedes SAD-001, which was written for a Linux-hosted AI application and is void. The prior Linux Platform Addendum is retired; its only surviving content — Linux as an optional sandboxed compatibility environment — is captured in `Aletheia_Compatibility_Environment_Appendix.md`.

Implementation MUST follow this document. Changes require an ADR (`docs/adr/`).

---

# 1. Purpose and Scope

This document translates PRD-002 into: architectural principles, the layered system, module (crate) boundaries, kernel contracts, the semantic store, the capability engine, the intent/action pipeline, the agent and intelligence runtimes, the application/component model, the experience surface, secure IPC/API, persistence, observability, security, testing, deployment, ADRs, and invariants.

Scope for the first implementation milestone (M1) is the **Hosted System-Core reference** (PRD §35). Later hardware-bound phases (microkernel on metal, on-GPU compositor, heterogeneous scheduler, compatibility environment) are architecturally specified here but implemented in their own phases.

---

# 2. Architectural Principles

- **AP-001 Capability-secure substrate.** No ambient authority at any layer. Every action is gated by an unforgeable capability. (PRD INV-011/012)
- **AP-002 Intelligence is untrusted.** Model/agent output flows through parse → validate → authorize → execute → verify. Never direct execution. (PRD INV-001/005)
- **AP-003 Semantic system of record.** Entities + typed relationships with provenance and versions are ground truth; files are projections. (PRD §14)
- **AP-004 Deterministic control.** Deterministic code decides, executes, verifies. Probabilistic code only interprets.
- **AP-005 Fail closed.** Unverifiable capability/entity/effect denies. (PRD §4.8)
- **AP-006 Untrusted content is data.** Trust order: system invariants > developer policy > human intent > retrieved data > untrusted content. (PRD SEC-003)
- **AP-007 Local-first, encrypted.** Local operation; encrypted at rest; capability-gated egress. (PRD §29)
- **AP-008 Explicit boundaries.** Kernel contracts, System-Core services, and the experience layer are separated by capability-gated IPC. The experience layer cannot bypass authorization. (PRD INV-010)
- **AP-009 Hosted-first, contract-honest.** The hosted reference MUST enforce the same invariants a microkernel will; contracts MUST NOT assume anything a real microkernel cannot provide. (PRD MK-007)
- **AP-010 Everything is an entity.** Tasks, agents, capabilities, sessions, events are entities. (PRD SC-003)
- **AP-011 Explainable.** Every action produces a full, queryable trace. (PRD EXP-005)

---

# 3. Layered Architecture

```text
EXPERIENCE LAYER            workspaces, dynamic interfaces, intent surfaces, control surfaces
        │  (capability-gated API / IPC)
SYSTEM CORE                 semantic store · capability engine · context · memory · world model
                            intent/action pipeline · agent runtime · intelligence runtime
                            actor/task model · app/component model · scheduler(abstract) · HAL(abstract)
        │  (kernel contracts: capability enforcement, secure IPC, task isolation, memory)
MICROKERNEL                 (P4 on metal; hosted realization in M1)
        │
HARDWARE ABSTRACTION LAYER
        │
HARDWARE (CPU/GPU/NPU/devices)
```

The seven primitives are realized in the System Core. The microkernel exists only to make capabilities unforgeable and to isolate tasks/devices/memory.

---

# 4. Module Boundaries (Rust Workspace)

Chosen stack: **Rust** for kernel and System Core (memory safety + systems performance; ADR-KERNEL-LANG). The M1 hosted reference is a cargo workspace of small crates with explicit dependency direction (domain depends on nothing; adapters depend inward).

```text
aletheia/
├── Cargo.toml                 workspace
├── crates/
│   ├── kernel-contracts/      capability-table, secure-IPC, task-isolation traits + hosted realization (AP-009)
│   ├── domain/                Entity, Version, Relationship, Provenance, Ids, Event, Errors (depends on nothing)
│   ├── storage/               semantic store: content-addressed, versioned, encrypted; engine behind a trait
│   ├── capabilities/          capability model, minting, delegation/attenuation, revocation, evaluation, audit
│   ├── worldmodel/            relationship graph + provenance traversal queries
│   ├── context/               bounded, provenance-tracked context assembly
│   ├── memory/                classified, provenance-bearing memory
│   ├── intelligence/          ModelRuntime trait; local-model adapter + deterministic interpreter adapter
│   ├── tools/                 operation registry (schema, required caps, risk, executor, verifier)
│   ├── intent-action/         intent types, parser, validator, planner, executor, verifier, trace
│   ├── agents/                agent identity, capability set, memory, isolation, lifecycle
│   ├── syscore/               composition root: wires the System Core; exposes command/query/event API
│   └── experience/            hosted experience surface (API server + minimal semantic UI)
├── crates/aletheiad/          binary: boots hosted System Core + experience surface
└── docs/
```

Dependency rule: `domain` is leaf; `storage`, `capabilities`, `worldmodel`, `context`, `memory`, `intelligence`, `tools` depend on `domain` (+ `kernel-contracts` where isolation is needed); `intent-action` and `agents` depend on those; `syscore` composes all; `experience`/`aletheiad` sit on top. No cycles; no crate reaches around the capability engine.

---

# 5. Microkernel Contracts

The microkernel (P4) provides privileged primitives; in M1 these are realized by `kernel-contracts` in userspace enforcing identical invariants.

- **KC-CAP:** A capability table mints typed, unforgeable handles and validates them. User code holds/delegates handles; it cannot fabricate one. In M1 this is an in-process authority holding a private table keyed by opaque tokens (no token is constructible from outside). On metal this becomes kernel-enforced capability slots.
- **KC-IPC:** Secure IPC requires a capability naming the endpoint; there is no global connectable namespace. In M1 this is typed in-process channels obtained only via a capability.
- **KC-TASK:** Task isolation — a task touches only entities/devices/endpoints for which it holds capabilities; a task crash cannot corrupt core state. In M1 this is enforced by construction (no task is handed a raw store handle; all access goes through capability-checked APIs).
- **KC-MEM:** Address-space/memory isolation (metal). In M1, Rust ownership + module boundaries stand in; the contract is "no shared mutable global authority."
- **KC-BOOT:** Secure-boot primitives (metal only).

Contract honesty (AP-009): the System Core is written against these traits, never against host-OS services, so P4 swaps the hosted realization for the real kernel without touching the layers above.

---

# 6. Semantic Store Architecture

The system of record (PRD §14). Realizes Entity, Version, Relationship, Provenance.

- **Model.** `Entity{ id, type, content_ref, version, metadata, created_at, updated_at }`; content is immutable and content-addressed (`content_ref = hash(content)`); `Relationship{ id, type, from, to, provenance, created_at }`; `Provenance{ actor, action_id, source_entities, at }`.
- **Content addressing (ST-002).** Content blobs stored under their hash; identical content deduplicates; identity is name/location independent.
- **Versioning (ST-003).** Mutation writes a new `Entity` row sharing a `version_chain_id`, linked by a `version_of` relationship; prior versions recoverable; content of old versions retained.
- **Encryption at rest (ST-004).** Content blobs and metadata encrypted with a local key (authenticated encryption); key derived from a local secret; nothing plaintext by default. In M1 the store file/DB holds only ciphertext for content and sensitive metadata.
- **Engine.** Behind a `Store` trait. M1 uses an embedded engine (SQLite via `rusqlite`, or `sled`) for durability + atomic transactions; the trait keeps it swappable.
- **Permissioning (ST-006).** The store exposes no unauthenticated read/write; all access is via System-Core APIs that check capabilities first. There is no global readable namespace.
- **Atomicity (ST-008).** Entity/version/relationship writes and their event are one transaction.
- **Queries (ST-007).** Relationship traversal (by type, direction, time, provenance) is first-class — the world-model query surface (`worldmodel` crate).
- **Filesystem projection (ST-009).** Optional, later phase; a path→entity view for the compatibility environment, never authoritative, never bypassing capabilities.

---

# 7. Capability Engine Architecture

The only authority mechanism (PRD §15).

- **Representation.** `Capability{ id, subject, action, scope, constraints, provenance }`. `action` is a class (e.g. `entity.read`, `entity.write`, `entity.derive`, `relationship.add`, `capability.grant`, `agent.invoke`, `device.*`, `network.connect`). `scope` selects entities by type, relationship-defined set, explicit id set, device, or endpoint. `constraints` include expiry, max-count, approval-required, local-only.
- **Minting (CAP-002).** Capabilities are minted only by the engine (rooted in KC-CAP). A capability is an unforgeable handle; the struct alone is not authority — the engine validates the handle against its table.
- **Delegation with attenuation (CAP-005).** `delegate(parent, narrower)` succeeds only if the child's action/scope/constraints are ⊆ the parent's. Amplification is rejected. Delegation edges are recorded.
- **Revocation (CAP-006).** `revoke(cap)` removes the capability and all descendants transitively; immediate on next evaluation.
- **Evaluation (CAP-007).** `evaluate(subject, action, target, ctx) -> ALLOW | DENY | REQUIRE_APPROVAL`. No matching capability → DENY (fail closed). A matching capability with `approval-required` (or a destructive risk operation) → REQUIRE_APPROVAL.
- **Audit (CAP-008).** grant/delegate/evaluate/use/revoke each emit an immutable event.
- **Agents as subjects (CAP-009).** Agent identities hold capabilities distinct from the human's, independently revocable.

---

# 8. Actor / Task Runtime

`agents`/`syscore`. Tasks are capability-scoped actors (PRD §23). `Task{ id, subject, capabilities, goal, state, correlation_id }`; lifecycle `Created→Queued→Running→(Waiting|AwaitingApproval)→(Completed|Failed|Cancelled)`, persisted (survives restart, AT-003). Cancellation is cooperative and immediate at the next checkpoint; loop/step/no-progress detection bounds autonomous tasks. A task holds only capabilities explicitly granted to its subject; it obtains store/device access only through capability-checked System-Core APIs (KC-TASK).

---

# 9. Context Engine

`context` (PRD §16). `assemble(intent, subject, ctx_budget) -> Context` gathers ranked, provenance-tagged items (active workspace/entities, recent actions, applicable memory, present agents, held capabilities), bounded by budget. Each `ContextItem{ source_type, source_id, retrieved_at, relevance, confidence }`. Never dumps the store. Assembly is inspectable (included/excluded/why).

---

# 10. Intent & Action Pipeline

`intent-action` — the deterministic heart (PRD §17). Stages, each a pure, testable function:

```text
Intent(structured)
  → interpret()      via intelligence runtime (untrusted): produces a proposed Plan
  → parse()          Plan JSON → typed structure (rejects malformed)
  → validate()       schema · semantic · operation-exists · argument-types
  → plan()           compile to candidate Actions (operation + args + required capabilities)
  → authorize()      capability engine per action → ALLOW|DENY|REQUIRE_APPROVAL (fail closed)
  → approve()        if required, await human decision (bound to exact action/scope/expiry)
  → execute()        deterministic executor from the tool registry
  → verify()         re-read store/device state; confirm real effect
  → record()         immutable Event with the full trace
```

`Trace{ intent, context_provenance, proposed_plan, validation, capability_decision, approval, execution, verification, result }`. Any failure short-circuits with no core-state mutation (IA-007). Interpretation is the *only* probabilistic stage; everything after is deterministic. The deterministic-interpreter fallback (INT-004) plugs in at `interpret()` and uses the identical downstream stages — never a bypass (INV-014).

---

# 11. Agent Runtime

`agents` (PRD §20). `Agent{ id(entity), identity, capabilities, memory_ref, context, tools, goals, relationships, state }`. Invocation: `invoke(agent, goal)` creates a capability-scoped Task whose actions all traverse the pipeline under the agent's identity. Isolation: agents get no raw handles; only capability-checked APIs; a crash/hang is contained (AG-003) and cancellable (AG-006). Bounds: explicit step/loop/resource limits (AG-004). Multi-agent work is mediated by capabilities and the pipeline, never shared authority (AG-005). Concurrency on the same entities is mediated by optimistic versioning + approval (AG-007).

---

# 12. Intelligence Runtime

`intelligence` (PRD §21). Port:

```rust
trait ModelRuntime {
  fn health(&self) -> Health;
  fn generate(&self, req: GenReq) -> Result<GenResult, ModelError>;
  fn stream(&self, req: GenReq) -> impl Stream<Item = GenEvent>;
  fn cancel(&self, id: RequestId);
}
```

Adapters: (a) **local model** adapter speaking to a local inference server (OpenAI-compatible / llama.cpp-class) when present; (b) **deterministic interpreter** adapter that maps a bounded set of structured intents to plans with no model. Selection is by availability/health; both emit the same `Plan` shape and route through §10. `ModelError` ∈ {NotLoaded, Crashed, Timeout, Cancelled, OutOfMemory, InvalidOutput, Runtime}; all handled explicitly (INT-004: OS fully functional with model absent).

---

# 13. Memory Subsystem

`memory` (PRD §18). `MemoryItem{ id(entity), class ∈ {ObservedFact,HumanStatement,Decision,DerivedRelationship,AISummary}, source, timestamp, confidence, entity_links }`. Distilled from events via an explicit extraction step; distinct from the event log; inspect/correct/delete/disable by the human.

---

# 14. World Model

`worldmodel` (PRD §19). Typed directed relationships form a queryable graph. Query API: traverse (from entity, edge types, direction, depth), filter by provenance/time, distinguish observed vs derived (with confidence). Answers provenance questions ("what produced this, from what") by walking `derived_from`/action-linked edges.

---

# 15. Application & Component Model

PRD §22. Applications are capability providers. Portable components target **WASM/WASI** with no ambient authority — they receive only imported host capabilities (mapped to Aletheia capabilities). Native components use System-Core APIs. Application data lives as entities. Each operation an app exposes is a registered tool `{schema, required_capabilities, risk, executor, verifier}` (SDK-003). M1 ships the native tool registry and the contract; the WASM/WASI runtime is P2 (architecture defined here: a wasm engine — e.g. wasmtime — with a capability-import ABI, sandboxed, least-privilege).

---

# 16. Experience Surface & Compositor

PRD §26–27. The native on-GPU compositor is P5. M1 provides a **hosted experience surface**: a capability-gated local API (command/query/event/stream) plus a minimal semantic UI that exercises intent → action, entity/relationship browsing, capability grant/revoke, memory, agent activity, approvals, and action-trace explainability (EXP-004/005). The UI composes around entities/intent (not windows); it cannot bypass authorization (AP-008) — every call carries a session capability and is evaluated by the engine.

---

# 17. Secure IPC & API

PRD §17/§28. The System Core exposes Commands (state-changing), Queries (read), Events (facts), Streams (ongoing). Every request carries a subject + capability and is authorized before effect. IPC is capability-gated (KC-IPC): no endpoint is reachable without a naming capability. The experience layer and apps are clients; they hold session/component capabilities and cannot escalate.

---

# 18. Persistence, Concurrency, Observability, Errors

- **Persistence/transactions:** entity/version/relationship + event commit atomically; durable across restart (PRD REL-001/002).
- **Concurrency:** optimistic versioning (version checks) over locks; idempotent retryable actions; destructive actions never blindly retried; no two uncontrolled autonomous drivers (AG-007).
- **Observability:** structured logs (`correlation_id`, `subject`, `action`, `error_code`); per-request traces (§10); metrics (action latency, verification latency, capability denials, inference latency, unauthorized-action-prevented=must be 100%).
- **Errors:** `{code, message, category ∈ (Validation,Authorization,NotFound,Conflict,Timeout,Resource,Model,Persistence,Internal), retryable, human_action, correlation_id}`.

---

# 19. Security Architecture

Boundaries: untrusted content/model output → validation boundary → capability boundary → deterministic executor → (kernel enforcement) → hardware. Trust order per AP-006. Sensitive devices/network are capability + approval + audit (PRD SEC-007). Compatibility guests are confined (PRD §32). The absence of ambient authority is the central property and is tested directly.

---

# 20. Testing Architecture

Per PRD §39. Crate-level unit tests (domain, capabilities, context, intent-action, storage); integration (store durability/atomicity across restart, pipeline end-to-end); contract (ModelRuntime, tool, capability, IPC); security (ambient-authority absence, unforgeability, attenuation, fail-closed, injection-as-data, malformed output, sensitive-device gating); failure (interpretation failure mid-flight, agent crash isolation, cancellation). The 20 M1 acceptance criteria (PRD §42) are encoded as automated tests and are the acceptance bar.

---

# 21. Deployment

M1: `aletheiad` binary runs the hosted System Core + experience surface locally; data in a local encrypted store under a data directory. Later: P4 rehosts on the microkernel (VM-tested), P5 adds HAL/compositor/scheduler on hardware + secure boot + rollback, P6 adds the compatibility environment.

---

# 22. Architecture Decision Records

Required ADRs (in `docs/adr/`):

- ADR-001 Aletheia is a from-scratch OS, not a host-OS application (supersession).
- ADR-002 Seven-primitive domain model (Entity/Capability/Context/Intent/Action/Memory/Relationship).
- ADR-003 Capability-based security as the sole authority model; no ambient authority.
- ADR-004 Rust for kernel and System Core.
- ADR-005 Semantic content-addressed, versioned, encrypted store as system of record; files as projection.
- ADR-006 Intelligence is native but untrusted; deterministic pipeline authority.
- ADR-007 Agents as first-class capability-controlled actors.
- ADR-008 WASM/WASI capability-secure component model for portable apps.
- ADR-009 Native compositor & experience layer; hosted surface first.
- ADR-010 Hosted System-Core reference before microkernel-on-metal (contract-honest).
- ADR-011 Linux/POSIX only as an optional sandboxed compatibility environment.

---

# 23. Architecture Invariants

Mirror PRD §34 (INV-001…INV-014). Additionally: **SAD-INV-A** System Core depends only on kernel contracts, never host-OS services; **SAD-INV-B** no crate reaches around the capability engine; **SAD-INV-C** the hosted realization enforces the same invariants the microkernel will.

---

# 24. Acceptance Criteria Mapping

The 20 M1 criteria (PRD §42) map to crates: 1–4 → `domain`+`storage`+`worldmodel`; 5–9,15 → `capabilities`+`agents`; 10–14 → `intent-action`+`intelligence`+`tools`; 16 → `agents`/task runtime; 17 → `intelligence` (deterministic adapter); 18–19 → `capabilities`+security tests; 20 → `experience`. Each criterion is one or more automated tests; M1 is accepted when all pass.

---

# 25. North Star

> **Aletheia's architecture makes authority explicit and intelligence untrusted by construction: the semantic store holds truth, the capability engine holds authority, deterministic pipelines execute and verify, and every action is explainable. The microkernel exists only to make that unforgeable on real hardware.**
