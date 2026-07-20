# Aletheia
## Product Requirements Document (PRD)

**Document ID:** ALETHEIA-PRD-002
**Version:** 2.0.0
**Status:** Product Definition / Engineering Source of Truth
**Supersedes:** ALETHEIA-PRD-001 (see `Aletheia_Product_Requirements_Document_v1_superseded.md`)
**Product:** Aletheia
**Product Category:** AI-native operating system (from first principles)
**Initial Target Users:** Creators, developers, knowledge workers, and the engineers building Aletheia itself
**Initial Resident Intelligence Model:** A small local model (initially MiniCPM-class, ~1B, quantized) — a *component*, never the authority
**External Development Agent:** Fable — external tooling, not part of Aletheia
**Initial Deployment Model:** Hosted reference implementation of the system core in userspace, with a defined evolution path to a native microkernel on bare metal

---

# Document Control

## Foundational Correction Notice

This document is a **ground-up rewrite**, not a revision. Version 1 (now `*_v1_superseded.md`) defined Aletheia as a local-first AI *application* running on top of an existing host operating system (Linux), inheriting the traditional model of `Process → Thread → File → Socket → Window → Application`.

That premise is rejected.

Aletheia is **an operating system designed from first principles** around intelligence, context, intent, memory, relationships, and capabilities as fundamental primitives. Linux is not the foundation. Linux exists, if at all, only as an optional sandboxed compatibility environment for legacy software.

Every architectural decision inherited from traditional operating systems has been re-evaluated. This is not a word-swap of "Linux" for "Aletheia"; the abstractions themselves are rebuilt.

## Document Purpose

This PRD establishes a detailed and testable definition of:

- what Aletheia is (an AI-native operating system)
- the primitives the system is organized around
- who Aletheia is for
- what problems it solves that legacy operating systems cannot
- how humans and intelligence collaborate through it
- what the resident intelligence is *allowed* to do, and what it is not
- what the deterministic system must guarantee
- what belongs in the first testable milestone
- what is intentionally deferred to later hardware-bound phases
- how success is measured
- what conditions must be satisfied before each milestone is accepted

This document is the primary product source of truth. Implementation MUST NOT begin from vision alone; it proceeds only after the downstream design documents (Software Architecture, System Design, Storage Design, Security Design, SDK/API contracts) are rewritten to agree with this PRD.

## What Carries Forward From v1

The correction changed the *substrate and abstractions*, not the *safety spine*. These v1 principles survive intact and are load-bearing here:

- **Intelligence is untrusted.** Model output is probabilistic input to be validated, never system truth.
- **Capability-gated execution.** Nothing acts on the system without an explicit, scoped, unforgeable capability.
- **Deterministic verification.** The system verifies the real effect of every action against reality.
- **Fail closed.** Uncertainty denies; it never widens authority.
- **Untrusted content is data, not instruction.** Prompt injection is structurally impossible to escalate.
- **Human remains in control.** Intelligence proposes; the human and the deterministic system dispose.

The rest of this document rebuilds *what the system is made of* on top of that spine.

---

# 1. Critical Terminology

## 1.1 Aletheia

Aletheia is an **AI-native operating system**. It is the complete stack from a minimal privileged kernel up through an intent- and context-driven experience layer. It is organized around semantic entities and capabilities rather than files and processes, and it treats intelligence as a native, capability-controlled system service rather than an application.

## 1.2 Fable

Fable is external development tooling used by the engineers who build Aletheia. Fable is not a runtime component, not a resident model, not an Aletheia service, and must never appear as a production dependency.

## 1.3 Resident Intelligence Model

The initial resident model is a small, local, quantized language model (MiniCPM-class ~1B to start). It is one interchangeable component of the **Agent Runtime**. It is an untrusted probabilistic inference source. It is not the operating system, not an authority, not a source of truth, and cannot directly execute anything.

## 1.4 Aletheia Microkernel

The minimal privileged core. It owns only what must be privileged: CPU scheduling primitives, memory management and address spaces, interrupts, hardware isolation, **capability enforcement**, secure IPC, device isolation, and secure-boot primitives. It contains no policy about files, applications, windows, or intelligence — those live above it.

## 1.5 Aletheia System Core

The unprivileged (or lower-privilege) layer directly above the microkernel that provides the *new* operating-system abstractions: the semantic object model, the capability system, the actor/task execution model, the context and memory systems, the relationship/world model, the agent runtime, and the application/capability model.

## 1.6 The Seven Primitives

Aletheia is organized around seven first-class primitives, in place of the legacy chain:

```text
Legacy OS:    Process → Thread → File → Socket → Window → Application
Aletheia:     Entity → Capability → Context → Intent → Action → Memory → Relationship
```

Each is defined in Section 6.

## 1.7 Entity

The universal unit of meaning. Documents, applications, people, projects, tasks, devices, events, agents, sessions, and capabilities are all entities: first-class objects with identity, type, content (or reference), relationships, provenance, permissions, versions, and context. **Files are not the primary abstraction** — a file, if one exists at all, is a compatibility projection of an entity.

## 1.8 Capability

An unforgeable, scoped, revocable token of authority to perform a class of action on a set of entities under constraints. Capabilities are the *only* way anything acts. There is no ambient authority: possessing a reference is not permission; holding a capability is.

## 1.9 Context

The system-maintained, provenance-tracked understanding of the current situation: active workspace, relevant entities, recent actions, applicable memory, present agents, and available capabilities. Context is a native system service, deliberately assembled and bounded, not a prompt string.

## 1.10 Intent

A structured expression of what a human (or authorized agent) wants to achieve, decoupled from how it is achieved. Intent is interpreted (possibly by the resident model), then compiled into a validated Action plan. Intent is never executed directly.

## 1.11 Action

A concrete, validated, capability-gated operation the deterministic system performs and then verifies. Actions are the only things that change system state, and every action is authorized, executed, verified, and recorded.

## 1.12 Memory

Persistent, classified, provenance-bearing knowledge held natively by the system: observed facts, human statements, decisions, derived relationships, and intelligence-produced summaries — each labeled by origin and confidence, and controllable by the human.

## 1.13 Relationship

A typed, directed, provenance-bearing edge between entities. The full set of relationships is the **world model**: the graph that lets the system understand how the user's world is connected, rather than forcing the human to reconstruct it.

## 1.14 Agent

A first-class, capability-controlled system actor with an explicit identity, permissions, memory, context, tools, goals, and relationships. Intelligence enters the system through agents, not through a chatbot bolted onto a desktop.

## 1.15 Workspace

An intent- and context-scoped environment in the Experience Layer, composed dynamically around entities, relationships, tasks, and knowledge — not a static desktop of windows.

## 1.16 Compatibility Environment

An optional, sandboxed, capability-confined guest (e.g. a Linux/POSIX personality) that runs legacy software. It is never the foundation and never a source of ambient authority.

---

# 2. Executive Summary

Every mainstream operating system is organized around primitives invented for 1970s hardware and single-actor, non-intelligent computing:

```text
Application → File → Folder
Process → Thread → Socket → Window
```

Humans do not think this way, and intelligent systems cannot reason well over it. A person thinks in terms of *their project*, *the decision they made last week*, *the person who sent that*, *the thing derived from this other thing*. The relationships, provenance, and context that make work meaningful live only in the human's head — the OS discards them. Worse, the legacy security model grants **ambient authority**: any process the user runs inherits the user's power over everything, which is precisely why bolting an AI onto such a system is dangerous.

Aletheia rebuilds the operating system so that the things humans and intelligences actually reason about — entities, their relationships, context, intent, memory — are the native primitives, and so that **authority is explicit and capability-scoped everywhere**, which is exactly what makes native intelligence safe.

The central loop is:

```text
Human (or authorized Agent) expresses Intent
        ↓
System assembles bounded, provenance-tracked Context
        ↓
Intelligence interprets Intent into a proposed plan (untrusted)
        ↓
System compiles the plan into candidate Actions
        ↓
Capability System authorizes each Action (fail closed)
        ↓
Human approves where required
        ↓
Deterministic executors perform the Action
        ↓
System verifies the real effect against reality
        ↓
Entities, Relationships, and Memory are updated
        ↓
An immutable Event records what actually happened
```

The core product principle is unchanged from v1 and is now structural rather than aspirational:

> Intelligence may interpret and propose. The capability-secure deterministic system decides what is actually allowed to happen, does it, and verifies it.

---

# 3. Product Vision

## 3.1 Vision Statement

> Build an operating system whose fundamental primitives are entities, capabilities, context, intent, action, memory, and relationships — so that computing is organized around meaning and authority is explicit, and so that intelligence is a safe, native, capability-controlled collaborator rather than an unsafe add-on.

## 3.2 North Star

> The human expresses intent. The system understands the world. Intelligence proposes. Capabilities authorize. The system acts, verifies, and remembers. The human stays in control.

## 3.3 Long-Term Vision

A computing environment where the human never manually reconstructs context, where every object carries its relationships and provenance, where authority is always explicit and least-privilege, and where intelligent agents are trustworthy system citizens because the operating system — not the model — holds the power.

---

# 4. Product Philosophy

## 4.1 Aletheia Is an Operating System, Not an Assistant

The product is the OS and its primitives. Intelligence is one native capability among several. Aletheia must be coherent and useful as an operating system even with the resident model unloaded.

## 4.2 Intelligence Is Native but Untrusted

Intelligence is a first-class system service, delivered through capability-controlled agents. It is simultaneously *native* (deeply integrated, context-aware, always available as a service) and *untrusted* (its output is validated, capability-gated, verified, and never treated as truth or authority).

## 4.3 Capability-Secure by Construction

There is no ambient authority anywhere in the system. Every actor — human-driven, application, or agent — acts only through explicit, scoped, unforgeable, revocable capabilities. This is the property that makes native intelligence safe.

## 4.4 Semantic, Not File-Based

The system of record is a semantic object model: entities and typed relationships with provenance and versions. Filesystem semantics are an optional compatibility projection, never the ground truth.

## 4.5 Intent- and Context-Driven

Humans express intent; the system supplies context. Interfaces are composed dynamically around entities, relationships, tasks, and knowledge — not pre-built around windows and menus.

## 4.6 Deterministic Control Retained

Probabilistic components interpret; deterministic components decide, execute, and verify. System truth comes from the store, the capability system, verified action results, and device state — never from a model.

## 4.7 Local-First and Private by Default

The core experience runs locally. User data is encrypted at rest and never leaves the machine without an explicit, capability-controlled, auditable network action.

## 4.8 Fail Closed

If a capability, entity, relationship, or effect cannot be verified, the system refuses, explains, and asks or recovers. Uncertainty never widens authority.

## 4.9 Human-in-Control, Non-Intrusive

Intelligence and agents are explicitly invoked or explicitly authorized to act proactively. Proactive behavior is configurable, explainable, and dismissible.

---

# 5. Foundational Reframing

The correction is best understood as replacing the organizing abstractions. This table is normative: implementations MUST NOT reintroduce the left column as a primitive.

| Legacy primitive | Why it is wrong at the abstraction level | Aletheia primitive |
|---|---|---|
| File / folder | Discards type, relationships, provenance, versions; forces manual reconstruction | **Entity** in a semantic, content-addressed, versioned store |
| Process / thread | Ambient authority; identity tied to a user, not a role or grant | **Actor/Task** with explicit capabilities and identity |
| User-owns-everything permission model | Any code the user runs inherits full power; unsafe for AI | **Capability** — unforgeable, scoped, least-privilege, revocable |
| Socket / raw IPC | Unauthenticated, ambient, connection-oriented | **Secure IPC** gated by capabilities |
| Window / desktop | Static containers the human arranges manually | **Workspace / dynamic interface** composed around intent and entities |
| Application as data silo | Owns its data; opaque; non-composable | **Application as capability provider**; data lives as entities |
| AI as chatbot app | Bolted-on, no identity, no controlled authority | **Agent** — first-class, capability-controlled system actor |
| Implicit, lost context | Lives only in the human's head | **Context + Memory + Relationship** as native services |

The seven-primitive spine (Section 6) is derived from the right column.

---

# 6. The Seven Primitives

## 6.1 Entity

**Definition.** The universal unit of meaning and the atom of the system of record.

Every entity has:

```text
entity_id            stable, content-independent identity (ULID/UUID class)
type                 e.g. document, project, task, person, device, event, agent, capability, application, session, output
content_ref          content-addressed hash of immutable content (if any)
version              monotonic version within a version chain
relationships        typed, directed edges to other entities (see 6.7)
provenance           who/what created or derived it, when, from what
permissions          the capabilities that gate access and mutation
metadata             typed attributes
context_links        situations in which the entity has been relevant
created_at / updated_at
```

An entity's content is immutable and content-addressed; mutation produces a new version linked to the prior one. There is no "file path" as identity. A path, filename, or byte stream is at most a *view* produced by the compatibility layer.

## 6.2 Capability

**Definition.** The sole unit of authority.

```text
subject              the actor granted the authority (human session, application, or agent identity)
action               a class of operation (e.g. entity.read, entity.write, agent.invoke, device.camera.use, network.connect)
scope                the set of entities/resources it applies to (by type, relationship, or explicit set)
constraints          time bounds, count bounds, approval requirements, local-only, etc.
provenance           who granted it, when, delegated from which capability
```

Properties: **unforgeable** (cannot be fabricated, only granted or delegated), **least-privilege** (default set is minimal), **scoped** (never system-wide by default), **delegable** (attenuated, never amplified), **revocable** (revocation is immediate and propagates), and **auditable** (every grant, delegation, use, and revocation is recorded).

Possessing a reference to an entity is *not* authority. Only a matching capability is.

## 6.3 Context

**Definition.** The native, bounded, provenance-tracked model of "what is going on right now."

Sources include the active workspace, the entities in focus, recent actions, applicable memory, present agents, and the capabilities currently held. Context is *assembled* deliberately and *budgeted*; the whole store is never dumped into a model. Every context item carries `source`, `retrieved_at`, `relevance`, and `confidence`, and is traceable.

## 6.4 Intent

**Definition.** A structured expression of a desired outcome, independent of mechanism.

Intent may originate from a human (via the experience layer, natural language, or direct manipulation) or from an authorized agent pursuing a goal. Intent is interpreted into a *proposed plan* (this is where the untrusted model participates), then compiled into candidate Actions. Intent is never executed as-is and never carries authority.

## 6.5 Action

**Definition.** The only thing that changes state.

An Action names a registered operation, its arguments, and the capabilities it requires. Its lifecycle is: proposed → validated (syntax, schema, semantics, tool existence) → authorized (capability evaluation, fail closed) → approved (if required) → executed (deterministic executor) → verified (real effect checked) → recorded (immutable event). Failure at any stage stops the Action without side effects on core state.

## 6.6 Memory

**Definition.** Native persistent knowledge with classification and provenance.

Classes: **observed fact**, **human statement**, **decision**, **system-derived relationship**, **agent/AI summary**. Each memory item carries `source`, `timestamp`, `confidence`, and links to the entities it concerns. Memory is inspectable, correctable, deletable, and disableable by the human.

## 6.7 Relationship

**Definition.** A typed, directed, provenance-bearing edge between two entities. The complete graph is the **world model**.

Examples: `derived_from`, `part_of`, `authored_by`, `used_by`, `version_of`, `references`, `owned_by`, `granted`, `occurred_in`. Relationships are first-class (queryable, versioned, provenance-tracked) — they are how the system knows, e.g., that an export was derived from a specific source through a specific action by a specific agent.

## 6.8 How the Primitives Compose

```text
Human/Agent forms INTENT over ENTITIES within a CONTEXT
        ↓  (intelligence interprets — untrusted)
System compiles ACTIONS, each requiring CAPABILITIES
        ↓  (capabilities authorize — fail closed)
Executors act; system verifies; ENTITIES and RELATIONSHIPS change
        ↓
MEMORY is updated with provenance; an EVENT (an ENTITY) records reality
```

Everything else in Aletheia — storage, agents, applications, the experience layer, the kernel primitives — is derived from serving these seven primitives safely.

---

# 7. Target Users

The v1 creator personas remain valid *users*, but they are now served by an OS that understands their world semantically rather than an app observing files. Two user classes:

## 7.1 Creators and Knowledge Workers

Gamers, music producers, video creators, 3D artists, game developers, streamers, writers, researchers. They think in projects, sessions, versions, decisions, and relationships. Aletheia represents those directly as entities and relationships, so questions like "the version before I changed the chorus" or "the source this was derived from" are first-class queries, not manual archaeology.

## 7.2 Aletheia Developers

Engineers building applications, agents, and system components. They need a capability-secure SDK, a WASM/WASI component model, a deterministic action/tool contract, and clear system-core APIs — so that new capabilities extend the OS without weakening its security model.

Representative intents (mechanism-independent):

```text
"Show everything derived from this recording."
"Who and what touched this project this week?"
"Give this agent read-only access to my music project for the next hour."
"Recover the state of this scene before the lighting change."
"Compose an interface for reviewing today's rendered outputs."
```

---

# 8. Problem Definition

## 8.1 The Abstraction Problem

Legacy operating systems expose files, processes, and windows. The relationships, provenance, versions, and context that make work meaningful are not represented; the human maintains them mentally and loses them constantly.

## 8.2 The Authority Problem

Legacy security is ambient: a process runs with the full authority of the user. Every application can read the user's whole home directory. This is unsafe in general and *catastrophic* once an intelligent, autonomous agent is involved — the model's mistakes or an injected instruction inherit total power.

## 8.3 The Intelligence-Integration Problem

Because legacy systems have no native context, capability, or memory model, AI is bolted on as an app with either no real power (a chatbot) or unsafe power (a shell-executing agent). Neither is acceptable. Intelligence needs a substrate where authority is explicit and context is native.

## 8.4 The Composition Problem

Applications are silos that own their data and expose nothing composable. The system cannot assemble capabilities and entities around what the human is trying to do.

## 8.5 The Provenance and Trust Problem

Users cannot answer "where did this come from, what produced it, and can I trust it?" because the OS never recorded the causal chain. Aletheia records reality as immutable events and typed relationships.

---

# 9. Product Goals

## PG-001 — Semantic System of Record
The OS represents work as entities and typed relationships with provenance and versions, natively.

## PG-002 — Capability-Secure Everywhere
No ambient authority anywhere. Every action is gated by an explicit, scoped, revocable capability.

## PG-003 — Native, Untrusted Intelligence
Intelligence is a first-class system capability delivered through capability-controlled agents, whose output is always validated and verified.

## PG-004 — Intent- and Context-Driven Interaction
Humans express intent; the system supplies bounded context and composes interfaces dynamically.

## PG-005 — Deterministic Control
All state changes pass through validation, capability authorization, deterministic execution, and verification.

## PG-006 — Persistent Context, Memory, and World Model
The system maintains context, classified memory, and a relationship graph across sessions.

## PG-007 — Local-First and Private
Local operation; encrypted-at-rest storage; no silent exfiltration.

## PG-008 — Reliability and Recovery
Durable state, atomic transitions, isolation of failures, and recovery/rollback.

## PG-009 — Extensibility via Capabilities and Components
New applications and agents extend the OS through capabilities and a WASM/WASI component model without weakening security.

## PG-010 — Heterogeneous Compute Awareness
The system understands and schedules across CPU, GPU, and NPU/accelerators as first-class resources.

## PG-011 — Legacy Compatibility Without Foundation Dependence
Legacy software runs only inside optional, sandboxed, capability-confined compatibility environments.

---

# 10. Non-Goals

The initial product MUST NOT attempt to:

1. Build on, embed, or depend on the Linux kernel, systemd, X11/Wayland, or POSIX as its foundation.
2. Reintroduce files, processes, or windows as primitives (they may appear only as compatibility projections).
3. Grant any actor ambient authority.
4. Let the resident model directly execute operations or hold authority.
5. Ship a chatbot as the product.
6. Silently transmit user data off the machine.
7. Boot on arbitrary bare-metal hardware in the first milestone (deferred to a later, explicitly hardware-bound phase).
8. Implement a production-grade native compositor on real GPUs in the first milestone (architecture only; hosted surface first).
9. Rebuild every legacy application or force every application to adopt Aletheia-native APIs.
10. Support every device/driver immediately.
11. Achieve NPU-aware bare-metal scheduling in the first milestone (modeled and abstracted first).

The strategy is: **prove the premise-defining primitives in a hosted reference implementation first; grow downward toward the microkernel and hardware, and outward toward the compositor and compatibility, in explicitly scoped later phases.**

---

# 11. System Architecture Overview

Aletheia's layered stack (each layer defined in the sections that follow):

```text
┌─────────────────────────────────────────────────────────┐
│                 EXPERIENCE LAYER                        │
│  Workspaces │ Dynamic Interfaces │ Intent Surfaces      │
│  Entities · Relationships · Tasks · Knowledge           │
└───────────────────────────┬─────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────┐
│                 ALETHEIA SYSTEM CORE                    │
│                                                         │
│  Semantic Object Model & Storage (world model)          │
│  Capability System                                       │
│  Context System        Memory System                     │
│  Intent & Action Pipeline                                │
│  Agent Runtime (intelligence as a capability)            │
│  Actor / Task Execution Model                            │
│  Application / Capability Model (native + WASM/WASI)     │
│  Native Graphics & Compositor services                   │
│  Heterogeneous Scheduler (CPU/GPU/NPU)                   │
└───────────────────────────┬─────────────────────────────┘
                            │  secure IPC (capability-gated)
┌───────────────────────────▼─────────────────────────────┐
│                 ALETHEIA MICROKERNEL                    │
│  CPU scheduling · Memory & address spaces · Interrupts  │
│  Hardware isolation · CAPABILITY ENFORCEMENT            │
│  Secure IPC · Device isolation · Secure-boot primitives │
└───────────────────────────┬─────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────┐
│              HARDWARE ABSTRACTION LAYER                 │
└───────────────────────────┬─────────────────────────────┘
                            │
┌───────────────────────────▼─────────────────────────────┐
│                       HARDWARE                          │
│              CPU · GPU · NPU · Memory · Devices          │
└──────────────────────────────────────────────────────────┘

        (optional, sandboxed, capability-confined)
┌──────────────────────────────────────────────────────────┐
│  COMPATIBILITY ENVIRONMENT — legacy POSIX/Linux guest    │
└──────────────────────────────────────────────────────────┘
```

The primitive stack `Entity → Capability → Context → Intent → Action → Memory → Relationship` is realized primarily in the System Core; the microkernel exists to enforce isolation and capabilities beneath it.

---

# 12. Aletheia Microkernel

## MK-001 — Minimal Privileged Surface
The microkernel MUST contain only primitives that require privilege. All policy about entities, applications, intelligence, and interfaces lives above it.

## MK-002 — Kernel Responsibilities
The microkernel provides: CPU scheduling primitives; memory management, address spaces, and isolation; interrupt handling; hardware isolation boundaries; **capability enforcement** (the root of the capability system); secure, capability-gated IPC; device isolation; and secure-boot primitives.

## MK-003 — Capabilities Are Enforced in the Kernel
The unforgeability of capabilities MUST ultimately rest on kernel enforcement (e.g. capability tables / typed handles the kernel mints and validates). User-level code can hold and delegate capabilities but cannot fabricate them.

## MK-004 — No Ambient Authority
The kernel MUST NOT grant any task authority merely by virtue of running. All authority derives from held capabilities.

## MK-005 — IPC Is Capability-Gated
Inter-task communication MUST require a capability naming the endpoint. There is no global namespace of connectable endpoints.

## MK-006 — Language and Safety
The kernel and core system services SHOULD be implemented in a memory-safe systems language (Rust is the chosen candidate; see ADR-KERNEL-LANG). Unsafe code MUST be minimized and isolated.

## MK-007 — Hosted Simulation First
Until a hardware-bound phase, the microkernel's contracts (capability tables, secure IPC, task isolation) MAY be realized by a hosted reference implementation that enforces the *same* invariants in userspace, so the layers above can be built and tested. The contracts MUST NOT assume anything a real microkernel cannot provide.

---

# 13. Aletheia System Core

## SC-001 — Provides the New Abstractions
The System Core provides the semantic object model, capability system, context system, memory system, relationship/world model, intent/action pipeline, agent runtime, actor/task model, application/capability model, graphics/compositor services, and the heterogeneous scheduler.

## SC-002 — Depends Only on Kernel Contracts
The System Core MUST depend only on the microkernel's capability/IPC/isolation contracts, never on legacy OS services.

## SC-003 — Everything Is an Entity
All System Core objects — including tasks, agents, capabilities, sessions, and events — are representable as entities in the semantic model, enabling uniform querying, provenance, and permissions.

## SC-004 — Deterministic Authority
The System Core is the deterministic authority: it validates intents, authorizes actions, executes them, verifies effects, and records events. It never delegates authority to a model.

---

# 14. Semantic Object Model & Storage

The system of record. Replaces the filesystem as ground truth.

## ST-001 — Entities and Relationships Are Primary
Storage MUST persist entities (Section 6.1) and typed relationships (Section 6.7), not files. Filesystem semantics are a compatibility projection only (Section 32).

## ST-002 — Content-Addressed
Entity content MUST be content-addressed (hash-identified). Identical content deduplicates; content identity is independent of name or location.

## ST-003 — Versioned
Mutation MUST create a new immutable version linked to its predecessor (`version_of`). History is queryable; prior versions are recoverable.

## ST-004 — Encrypted at Rest
Stored content and metadata MUST be encrypted at rest with local keys. Nothing is stored in plaintext by default.

## ST-005 — Provenance-Bearing
Every entity and relationship MUST record provenance: the actor/agent, the action, the source entities, and the time.

## ST-006 — Permissioned
Access to and mutation of an entity MUST be gated by capabilities. There is no readable-by-default global namespace.

## ST-007 — Queryable World Model
The store MUST support relationship queries (traversal, filtering by type/time/provenance) as a first-class operation — this is how "everything derived from X" is answered.

## ST-008 — Durable and Atomic
Entity/version/relationship writes and their corresponding events MUST be committed atomically and survive restart.

## ST-009 — Optional Filesystem Compatibility View
The store MAY expose a filesystem-shaped read/write view for the compatibility environment, mapping paths to entity projections. This view is never authoritative and never bypasses capabilities.

---

# 15. Capability System

The native security model. There is no other authority mechanism.

## CAP-001 — Capabilities Are the Only Authority
Every action on any entity, device, agent, or endpoint MUST require a matching capability. No ambient authority exists.

## CAP-002 — Unforgeable
Capabilities MUST be unforgeable references rooted in kernel enforcement. They can be granted or delegated, never fabricated.

## CAP-003 — Least Privilege by Default
The default capability set for any new actor MUST be minimal. Broad scopes are explicit, rare, and auditable.

## CAP-004 — Scoped
Capabilities MUST be scopeable to entity types, relationship-defined sets, explicit entity sets, devices, and endpoints — and to constraints (time, count, local-only, approval-required).

## CAP-005 — Delegable with Attenuation
An actor MAY delegate a capability it holds, only with equal or narrower scope/constraints. Delegation never amplifies authority.

## CAP-006 — Revocable
Revocation MUST be immediate and MUST propagate to delegated descendants.

## CAP-007 — Decision Outcomes
Evaluation yields exactly one of: `ALLOW`, `DENY`, `REQUIRE_APPROVAL`. Absence of a matching capability yields `DENY` (fail closed).

## CAP-008 — Auditable
Every grant, delegation, evaluation, use, and revocation MUST be recorded as an event.

## CAP-009 — Agents Are Subjects
Agents hold capabilities under their own identity, distinct from the human's. An agent can never exceed the capabilities explicitly granted to it, and its grants are independently revocable.

---

# 16. Context System

## CTX-001 — Native Context Service
Context (Section 6.3) MUST be a native system service, not a per-application concern.

## CTX-002 — Deliberate Assembly and Budget
Context MUST be assembled deliberately from ranked sources and bounded by a budget. The whole store MUST NEVER be dumped into a model.

## CTX-003 — Provenance for Every Item
Every context item MUST carry `source_type`, `source_id`, `retrieved_at`, `relevance`, and `confidence`.

## CTX-004 — Priority Order
Recommended ranking: current intent → active workspace/entities → active task → directly relevant system facts → relevant recent actions/events → relevant entities → relevant memory → general context.

## CTX-005 — Inspectable
Developers and users MUST be able to inspect what context was included, what was excluded, and why.

---

# 17. Intent & Action Pipeline

## IA-001 — Intent Is Structured and Authority-Free
Intent (Section 6.4) MUST be represented as structured data and MUST NOT carry authority or execute directly.

## IA-002 — Interpretation Is Untrusted
Interpreting intent into a proposed plan MAY use the resident model. The output is untrusted and MUST pass full validation.

## IA-003 — Action Compilation
Proposed plans MUST be compiled into candidate Actions naming registered operations, typed arguments, and required capabilities.

## IA-004 — Validation Stages
Every Action MUST pass, in order: syntax → schema → semantic → operation existence → capability evaluation → policy → approval (if required) → execution → verification.

## IA-005 — Verification Is Mandatory
After execution, the system MUST verify the real effect (entity/relationship/device state) before reporting success. Unverifiable success is a failure.

## IA-006 — Recording
Each Action MUST record an immutable event capturing intent, context provenance, proposed plan, validation, capability decision, approval, execution, and verification (the full trace).

## IA-007 — Malformed or Failed Interpretation Cannot Escalate
Invalid JSON, missing fields, unknown operations, hallucinated entities, contradictory plans, and mid-flight interpretation failures MUST be handled safely and MUST NOT execute anything or corrupt state.

---

# 18. Memory System

## MEM-001 — Native Classified Memory
Memory (Section 6.6) MUST be native, classified (observed fact / human statement / decision / derived relationship / AI summary), and provenance-bearing.

## MEM-002 — Provenance and Confidence
Each memory item MUST record `source`, `timestamp`, `confidence`, and links to concerned entities.

## MEM-003 — Human Control
The human MUST be able to inspect, correct, delete, and disable memory.

## MEM-004 — Memory Is Not Raw Events
Memory is distilled from events and interactions through an explicit extraction/classification step; it is distinct from the immutable event log.

---

# 19. Relationship & World Model

## WM-001 — Typed Directed Relationships
Relationships (Section 6.7) MUST be typed, directed, and provenance-bearing first-class objects.

## WM-002 — The World Model Is Queryable
The union of relationships forms a queryable world model supporting traversal and provenance-aware filtering.

## WM-003 — Relationships Are Derived Safely
System-derived relationships MUST be labeled as derived (with confidence) and MUST be distinguishable from observed or human-asserted relationships.

## WM-004 — Provenance Chains
The system MUST be able to answer "what produced this and from what" by traversing `derived_from`/`authored_by`/action-linked edges.

---

# 20. Agent Runtime

Intelligence enters the system here — as capability-controlled actors, never as a chatbot.

## AG-001 — Agents Are First-Class Actors
An agent MUST have an explicit identity, a capability set, its own memory and context, tools, goals, and relationships. It is an entity like any other.

## AG-002 — Capability-Controlled
An agent MUST act only through capabilities granted to its identity. It can never exceed them; its grants are independently revocable and auditable.

## AG-003 — Isolated
Agents MUST be isolated from one another and from the System Core such that a misbehaving or crashing agent cannot corrupt core state or seize authority.

## AG-004 — Explicit Goals and Bounds
Agent goals, step limits, loop detection, and resource bounds MUST be explicit. Autonomous behavior is bounded and interruptible.

## AG-005 — Multi-Agent Composition
Multiple agents MAY collaborate, but collaboration is mediated by capabilities and the action pipeline — never by shared ambient authority.

## AG-006 — Interruptible and Observable
Every agent task MUST be cancellable, and its actions MUST be observable through the same trace/event mechanism as human-initiated actions.

## AG-007 — Never Two Uncontrolled Autonomous Drivers
The system MUST NOT allow uncontrolled concurrent autonomous mutation of the same entities; concurrency is mediated by capabilities, optimistic versioning, and approval.

---

# 21. Intelligence as a System Capability

## INT-001 — Resident Model as a Component
The resident model is an interchangeable component behind a stable runtime interface (load/unload/health/generate/stream/cancel). The core MUST NOT couple to a specific model.

## INT-002 — Untrusted Output
All model output MUST be treated as untrusted input to the action pipeline.

## INT-003 — Structured Output
For executable behavior, the model MUST produce structured output (intent/plan) that is parsed and validated; free text alone never acts.

## INT-004 — Availability Independence
If the model is unavailable, the OS MUST continue to provide all deterministic functionality; a deterministic interpreter MAY handle a subset of intents, routed through the identical validation/capability/verification pipeline (never a bypass).

## INT-005 — Resource Awareness
The runtime MUST respect memory/compute pressure and support lazy load, unload, throttle, pause, and resume, coordinated with the heterogeneous scheduler.

---

# 22. Application & Capability Model

## APP-001 — Applications Provide Capabilities
Applications are primarily **capability providers**, not data silos. They expose typed operations (tools) and entity types to the system, which composes them around intent.

## APP-002 — Portable Components via WASM/WASI
Portable applications and components SHOULD target a capability-secure WebAssembly/WASI model: no ambient authority, only imported capabilities. Native Aletheia components MAY exist for system-level functionality.

## APP-003 — Data Lives as Entities
Application data MUST live in the semantic store as entities with relationships and provenance, not in private opaque silos, so the system can reason across applications.

## APP-004 — Composition Around Intent
The system MUST be able to compose capabilities, entities, and interfaces from multiple applications around a single user intent.

## APP-005 — Isolation and Least Privilege
Every application/component runs sandboxed with only its granted capabilities; failure is isolated.

---

# 23. Actor / Task Execution Model

## AT-001 — Actors, Not Processes
The unit of execution is an actor/task with an explicit identity and capability set — not a process inheriting user authority.

## AT-002 — Capability-Scoped Execution
A task can only touch entities/devices/endpoints for which it holds capabilities.

## AT-003 — Persistent Task State
Task state (including lifecycle: created → queued → running → waiting/awaiting-approval → completed/failed/cancelled) MUST be persisted and survive restart.

## AT-004 — Cancellation and Loop Detection
Tasks MUST be cancellable, and the system SHOULD detect repeated identical actions, repeated failures, and no-progress loops.

---

# 24. Heterogeneous Scheduler

## SCH-001 — CPU/GPU/NPU Awareness
The scheduler MUST model CPU, GPU, and NPU/accelerator resources as first-class and schedule workloads (interactive, intelligence/inference, background, maintenance) across them by policy.

## SCH-002 — Priority and Preemption
Interactive and human-facing work MUST take priority over background intelligence and maintenance. Policy MUST be configurable.

## SCH-003 — Pressure Response
Under memory/thermal/power pressure, the scheduler MUST shed optional work (background inference, indexing) before compromising interactivity or stability.

## SCH-004 — Hosted Modeling First
Until a hardware-bound phase, scheduling is modeled/abstracted; the contracts MUST NOT assume capabilities a real scheduler cannot provide.

---

# 25. Hardware Abstraction Layer

## HAL-001 — Uniform Device Model
Hardware MUST be exposed through a uniform device abstraction (id, category, vendor, model, capabilities, state, performance), with devices as entities.

## HAL-002 — Capability-Gated Device Access
All device access (including sensitive devices — camera, microphone) MUST require explicit capabilities and, for sensitive devices, explicit human approval and audit.

## HAL-003 — Isolation
Device access MUST be isolated so a device or driver failure degrades gracefully without crashing the core.

---

# 26. Native Graphics & Compositor

## GFX-001 — Native Modern Architecture
Graphics MUST be a native, modern compositor architecture — not X11/Wayland and not a conventional Linux desktop compositor as foundation.

## GFX-002 — Entity/Intent-Driven Surfaces
Surfaces are composed around entities, tasks, and intent (dynamic interfaces), not around application-owned windows.

## GFX-003 — GPU-Accelerated, Capability-Gated
Rendering uses GPU acceleration via the HAL; surface access and capture are capability-gated.

## GFX-004 — Hosted Surface First
The first milestone provides a hosted experience surface (rendered via a host window/web surface) that exercises the entity/intent composition model; the native on-GPU compositor is a later hardware-bound phase.

---

# 27. Experience Layer

## EXP-001 — Intent- and Context-Driven
The experience layer is organized around workspaces, entities, relationships, tasks, and knowledge — not a static desktop of windows and menus.

## EXP-002 — Dynamic Interfaces
Interfaces are composed dynamically from capabilities and entities around the current intent and context.

## EXP-003 — Semantic Navigation and Search
Users navigate by entity, relationship, provenance, time, and meaning — global semantic search over the world model.

## EXP-004 — Human Control Surfaces
The experience layer MUST expose: current context; held capabilities (grant/revoke); memory (inspect/correct/delete); agents (identity, permissions, activity); action approvals; and system/resource status.

## EXP-005 — Explainability
For any action the system took, the experience layer MUST be able to show the full trace (intent → context → interpretation → validation → capability decision → execution → verification → event).

---

# 28. Security Model

Aletheia is capability-secure end-to-end. Security is not a layer; it is the substrate.

## SEC-001 — Capability-Secure Foundation
Every action is authorized by an unforgeable capability. No ambient authority anywhere (kernel through experience layer).

## SEC-002 — Intelligence Is Untrusted
Model and agent output is validated, capability-gated, and verified. Intelligence never holds authority directly.

## SEC-003 — Untrusted Content Is Data
Content of entities (documents, messages, app data, model output) is untrusted and MUST NOT become instruction authority. The trust order is: system invariants > developer policy > human intent > retrieved data > untrusted content. Retrieved/embedded instructions can never escalate.

## SEC-004 — Fail Closed
Any unresolved capability, entity, relationship, or effect denies the action.

## SEC-005 — Least Privilege and Revocation
Default-minimal capabilities; immediate, propagating revocation.

## SEC-006 — Secure Boot and Isolation
The kernel provides secure-boot primitives, address-space isolation, device isolation, and capability-gated IPC.

## SEC-007 — Sensitive Devices
Camera, microphone, screen capture, and network require explicit capability plus human approval, and are audited. Intelligence can never silently activate them.

## SEC-008 — Auditability
All security-sensitive operations (capability grants/uses/revocations, device access, destructive actions, network) MUST be recorded as immutable events.

## SEC-009 — Compatibility Confinement
Legacy compatibility environments run confined with only explicitly granted capabilities; they cannot reach the semantic store or devices except through capability-gated projections.

---

# 29. Privacy

Aletheia MUST run the resident intelligence locally; encrypt user data at rest; disclose and gate all network access; and never silently transmit entities, content, memory, or the world model off the machine. Network egress is an explicit, capability-controlled, auditable action with policies: disabled / local-only / specific-endpoints / unrestricted.

---

# 30. Reliability

## REL-001 — Durable State
Entities, versions, relationships, capabilities, tasks, memory, and events MUST survive restart.

## REL-002 — Atomic Transitions
State change + its event MUST commit atomically.

## REL-003 — Idempotency
Retryable actions MUST be idempotent; destructive actions MUST NOT be blindly retried.

## REL-004 — Failure Isolation
A failed interpretation, agent, application, or device MUST NOT corrupt core state or crash the core.

## REL-005 — Recovery and Rollback
The system MUST support recovery of prior entity versions and (in later OS phases) system rollback.

---

# 31. Developer SDK & Application Runtime

## SDK-001 — Capability-Secure SDK
The SDK MUST let developers build applications and agents that declare required capabilities, expose typed operations (tools) and entity types, and run sandboxed.

## SDK-002 — Component Model
Portable components target WASM/WASI with imported capabilities; native components use the System Core APIs.

## SDK-003 — Deterministic Tool Contract
Every operation an application/agent exposes MUST have a schema, declared required capabilities, a risk level, an executor, and a verifier — mirroring the action pipeline.

## SDK-004 — No Privileged Escape Hatches
The SDK MUST NOT provide a way to obtain ambient authority or to bypass the capability system.

---

# 32. Compatibility Environment

## COMPAT-001 — Optional, Sandboxed, Confined
A Linux/POSIX personality MAY be provided as an optional guest to run legacy software. It is sandboxed and holds only explicitly granted capabilities.

## COMPAT-002 — Filesystem as Projection
Legacy filesystem access is served by a projection over the semantic store (Section 14.9); it is never authoritative and never bypasses capabilities.

## COMPAT-003 — Never the Foundation
Aletheia MUST remain fully functional as an OS with no compatibility environment present. Compatibility is additive, never foundational.

---

# 33. Interaction Model

- **Explicit:** the human states an intent directly.
- **Contextual:** the human acts within a workspace/entity; that becomes primary context.
- **Agentic:** an authorized agent pursues a goal within its capabilities.
- **Proactive (optional):** configurable, non-intrusive, explainable, dismissible.

All modes flow through the same intent → action → capability → verify → record pipeline.

---

# 34. Deterministic Control Invariants

## INV-001 — Intelligence cannot directly execute operations.
## INV-002 — Every executable action passes through a registered operation and the action pipeline.
## INV-003 — Every action passes capability evaluation (fail closed).
## INV-004 — Destructive actions require appropriate authorization/approval.
## INV-005 — System facts are never derived solely from model output.
## INV-006 — A failed interpretation cannot corrupt core state.
## INV-007 — A failed application/agent/device cannot crash the core.
## INV-008 — Important operations are auditable as immutable events.
## INV-009 — User data is local and encrypted by default.
## INV-010 — The experience layer and applications cannot bypass capability authorization.
## INV-011 — No actor has ambient authority.
## INV-012 — Capabilities are unforgeable and rooted in kernel enforcement.
## INV-013 — Files/processes/windows never appear as primitives, only as compatibility projections.
## INV-014 — The deterministic no-model path uses the identical validation/capability/verification pipeline.

---

# 35. MVP Definition (Milestone M1 — Hosted System-Core Reference)

Because bare-metal boot, on-GPU compositing, and NPU scheduling are not testable without hardware, the first milestone is a **hosted reference implementation of the System Core** that proves the premise-defining behaviors in userspace, enforcing the *same* invariants a microkernel will later enforce.

M1 MUST provide a complete vertical slice:

- **Semantic object model & storage** — content-addressed, versioned, encrypted-at-rest entity store with typed relationships and provenance (the world model).
- **Capability system** — unforgeable, scoped, least-privilege, delegable, revocable capabilities; ALLOW/DENY/REQUIRE_APPROVAL; fail closed; audited.
- **Actor/task model** — capability-scoped tasks with persisted lifecycle and cancellation.
- **Context system** — bounded, provenance-tracked context assembly.
- **Intent & action pipeline** — intent → interpretation (model or deterministic) → validation → capability authorization → approval → deterministic execution → verification → immutable event/trace.
- **Agent runtime** — at least one capability-controlled agent identity that can be invoked, bounded, and revoked, acting only through the pipeline.
- **Intelligence as capability** — a model-runtime port with a real adapter (local model when available) and a deterministic interpreter fallback, both routed through the identical pipeline.
- **Experience surface** — a hosted surface exposing context, entities/relationships, capabilities (grant/revoke), memory, agents, approvals, and action traces.

M1 explicitly does **not** require: bare-metal boot, native on-GPU compositor, NPU scheduling, or a compatibility environment. Those are later phases (Section 41) with their own acceptance criteria.

---

# 36. MVP User Journey (M1)

```text
1. The human (or an authorized agent) expresses an intent over entities in a workspace.
2. The system assembles bounded, provenance-tracked context.
3. Intelligence (model or deterministic fallback) proposes a plan — untrusted.
4. The system compiles candidate actions and validates them.
5. The capability system authorizes each action (fail closed).
6. If required, the human approves.
7. Deterministic executors perform the action.
8. The system verifies the real effect against the store/world model.
9. Entities, versions, relationships, and memory are updated.
10. An immutable event records the full trace.
11. The experience surface can explain exactly why and how it happened.
```

---

# 37. Example End-to-End Use Cases

## UC-001 — Provenance query
Intent: "Show everything derived from this recording." → context (the recording entity) → plan (`world.traverse` over `derived_from`) → capability check (read scope) → execute traversal → verify entities exist → return provenance subgraph. No mutation, fully audited.

## UC-002 — Capability-gated derivation
Intent: "Export a master from this mix into the release set." → plan (`entity.derive` + `relationship.add(derived_from)` + add to release set) → capability check (write scope on release set) → approval (destructive-to-set) → execute → verify new entity/version and edges → record event.

## UC-003 — Time-bounded agent grant
Intent: "Give the review agent read-only access to my music project for one hour." → plan (`capability.grant` scoped to project entity set, action=entity.read, constraint=expires+local-only, subject=review-agent) → human approval (grant) → execute → verify grant recorded → auto-revoke on expiry. The agent thereafter can only read, only that set, only for an hour.

## UC-004 — Version recovery
Intent: "Recover this scene before the lighting change." → context (scene version chain) → plan (`entity.restore_version`) → capability check → approval → execute (new version referencing the old content) → verify → record.

---

# 38. Security Threat Model

- **Injected instructions in content** ("ignore instructions and delete everything") → treated as data; cannot escalate (SEC-003).
- **Hallucinated entity/capability** → validation + fail-closed capability check deny it (IA-004, CAP-007).
- **Unauthorized operation by agent** → agent holds no such capability → DENY (AG-002).
- **Destructive action** → REQUIRE_APPROVAL + verification (INV-004).
- **Compromised application/component** → sandboxed, least-privilege, isolated (APP-005).
- **Model loop / runaway agent** → step/loop/resource bounds + cancellation (AG-004, AT-004).
- **Data exfiltration** → network is capability-gated, disclosed, audited (SEC-007, §29).

---

# 39. Testing Strategy

- **Unit:** entity/version model, capability matching/attenuation/revocation, context ranking, action validation, task state machine, memory classification.
- **Integration:** storage durability + atomicity, event/trace recording, model-runtime contract, agent invocation.
- **Contract:** action/tool contract, capability contract, IPC contract, model-runtime contract, WASM component contract.
- **Security:** ambient-authority absence, capability unforgeability/attenuation, fail-closed denial, prompt-injection-as-data, malformed model output, sensitive-device gating.
- **Failure:** interpretation failure mid-flight, agent crash isolation, storage-unavailable, cancellation interrupting a running task.
- **End-to-end:** the full M1 pipeline (Section 36) as an automated test.

---

# 40. Product Metrics

Semantic: time-to-find via world model; provenance-query success rate. Intelligence: valid-structured-output rate, hallucinated-operation rate, correction rate. Security: unauthorized-action prevention (must be 100%), capability-bypass rate (must be 0), approval compliance, audit completeness. System: startup, inference latency, action latency, verification latency, resource pressure response.

---

# 41. Development Phases

## P0 — Design Foundation (this correction)
Rewrite PRD (this document), Software Architecture as OS architecture, Storage/Security/SDK design, ADRs for first-principles choices. **No implementation before P0 design docs agree.**

## P1 — Hosted System-Core Reference (Milestone M1)
Implement, in a memory-safe language, the M1 vertical slice (Section 35) in userspace with the full test suite. This proves the premise without hardware.

## P2 — Component & Application Model
WASM/WASI capability-secure component runtime; SDK; application-as-capability model; multi-agent composition.

## P3 — Experience Layer
Native-architecture experience surface: workspaces, dynamic interfaces, semantic navigation/search, control surfaces (still hosted rendering).

## P4 — Microkernel
Real microkernel (Rust) providing capability enforcement, secure IPC, task isolation, memory/address spaces, interrupts; System Core rehosted on it. Hardware-bound; VM-tested.

## P5 — Hardware, Graphics, Scheduler
HAL on real devices; native on-GPU compositor; heterogeneous CPU/GPU/NPU scheduler; secure boot; recovery/rollback.

## P6 — Compatibility Environment
Optional sandboxed Linux/POSIX personality; filesystem-projection over the semantic store.

Each phase has its own acceptance criteria; later phases MUST preserve all invariants of Section 34.

---

# 42. MVP Acceptance Criteria (M1)

M1 is accepted only when, as automated tests:

1. An entity can be created, is content-addressed, and is retrievable by identity.
2. Mutating an entity creates a new linked version; the prior version is recoverable.
3. Entities and relationships are persisted encrypted and survive a real process restart.
4. A typed relationship can be created and the world model traversed (e.g. "derived_from").
5. Every action requires a capability; absence of one yields DENY (fail closed).
6. Capabilities are unforgeable (cannot be constructed, only granted/delegated) — verified by test.
7. A capability can be delegated only with equal-or-narrower scope; amplification is rejected.
8. Revocation is immediate and propagates to delegated descendants.
9. A destructive action triggers REQUIRE_APPROVAL and does not execute without approval.
10. Intent is interpreted (model or deterministic) into a plan that is validated before execution.
11. Malformed/garbage interpretation output cannot execute anything and cannot corrupt state.
12. A failed interpretation mid-flight leaves core state intact.
13. An action is verified against real store state before success is reported.
14. Every action records an immutable event with the full trace (intent→…→verification).
15. An agent identity holds its own capabilities and cannot exceed them; its grants are revocable.
16. A running task can be cancelled and stops without corrupting state.
17. The system operates fully with the resident model unavailable (deterministic path, same pipeline).
18. No ambient authority exists: a task with no capabilities can perform no actions.
19. Untrusted content containing instructions cannot cause an unauthorized action.
20. The experience surface can render the full trace explaining any action.

---

# 43. Future Direction

Bare-metal microkernel boot; native on-GPU compositor; NPU-aware heterogeneous scheduling; secure-boot + rollback; distributed Aletheia across trusted devices; richer multi-agent societies; semantic-native application ecosystem. All future work MUST preserve capability security, untrusted intelligence, deterministic control, local-first privacy, and human control.

---

# 44. Source-of-Truth Priority

1. This Product Requirements Document (ALETHEIA-PRD-002)
2. Software Architecture Document (OS architecture, to be rewritten to agree)
3. System / Storage / Security / SDK Design
4. API & Component Contracts
5. Implementation Plan
6. Source Code

Any lower-level conflict MUST be surfaced and resolved explicitly. Engineering MUST NOT silently redefine the product or reintroduce legacy primitives.

---

# 45. North Star

> **Aletheia is not an AI assistant running on an operating system. Aletheia is an operating system whose primitives are entities, capabilities, context, intent, action, memory, and relationships — where intelligence is a native but untrusted collaborator, authority is always explicit, and the human remains in control.**
