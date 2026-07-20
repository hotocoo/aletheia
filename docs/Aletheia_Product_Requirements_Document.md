# Aletheia
## Product Requirements Document (PRD)

**Document ID:** ALETHEIA-PRD-003
**Version:** 0.3.0
**Status:** Product Definition / Engineering Source of Truth
**Supersedes:** ALETHEIA-PRD-002
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

## Revision Notice (V&V-first)

PRD-002 defined the primitives, the security spine, and the M1 milestone. It treated testing as a single closing section (PRD-002 §39, "Testing Strategy"; now §43, an index into §38) — adequate for a hosted userspace reference, insufficient for an operating system that will run on real hardware, own real address spaces, and host an untrusted resident model.

This revision, PRD-003, makes one structural correction: **verification, validation, observability, fuzzing, and hardware qualification are elevated to first-class architecture**, integrated into every subsystem section rather than deferred to a closing QA pass. The operating principle is stated once and applies everywhere in this document:

> Tests provide evidence that known scenarios work. They do not prove the system is correct.

No amount of passing unit and integration tests establishes that a capability cannot be forged, that an untrusted model output cannot escalate, or that the scheduler holds its latency bounds under adversarial load. Each of those requires a different discipline — property-based testing, fuzzing, chaos engineering, formal verification of the narrow critical core, and eventually real hardware. Section 38 (Verification & Validation Architecture) defines the full eleven-layer validation pyramid this document now requires; Sections 12 through 30 each carry a subsystem-specific `V&V` block naming the invariants, targets, and gates that apply to that subsystem; Sections 39-41 add the supporting Observability, CI/CD, and Hardware Qualification architecture.

This revision also records the first delivered evidence at the VM-testing layer of the pyramid: a bootable `no_std` aarch64 microkernel (`kernel/`) that re-proves the M1 System-Core invariants in kernel space, on QEMU `virt`, gated by an automated CI script (`scripts/vm-e2e.sh`). It is cited, with its measurement caveats intact, in Sections 38.10, 45 (P4), and wherever the VM-testing layer is discussed. No new product scope changes; the primitives, goals, non-goals, and M1 acceptance bar of PRD-002 are unchanged and carried forward in full.

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

#### V&V — Microkernel

- **Functional requirements:** CPU scheduling primitives, address-space isolation, interrupt dispatch, capability enforcement (root of trust), capability-gated secure IPC, device isolation, secure-boot primitives (MK-001..006).
- **Security invariants:** capability unforgeability is rooted in kernel enforcement, not convention (MK-003); no task acquires authority merely by running (MK-004); IPC requires a capability naming the endpoint, with no global connectable namespace (MK-005).
- **Safety invariants:** address-space isolation prevents one task's fault from corrupting another's memory; interrupt handling never leaves capability state partially updated (evaluation and mutation are atomic with respect to preemption).
- **Performance targets:** the authority check on the IPC path adds less than one bare privilege-boundary (`svc`) crossing of overhead on the same emulated CPU (substrate-fair ratio; see §38.10 — this bounds the *added check* cost, not IPC transport, which must still cross the privilege/address-space boundary like any microkernel IPC or Linux pipe); a real cross-address-space IPC round-trip and syscall/trap + context-switch latency are tracked as regression-gated benchmarks once real address spaces and scheduling exist.
- **Failure / recovery behavior:** a faulting task is terminated and its capabilities revoked without taking down the kernel; kernel panics halt to a diagnosable state (never silent corruption) and are surfaced as a hard exit code in the VM/hardware test harness.
- **Test strategy:** unit tests on capability-table logic in isolation; property-based tests for unforgeability/attenuation/revocation (§38.3); fuzzing of the syscall/IPC surface (§38.4); VM-testing as the CI gate (§38.10, delivered); real-hardware testing once P4/P5 land (§38.11); formal verification is the long-term target for capability enforcement and IPC security (§38.7).
- **Fuzzing strategy:** structured fuzzing of syscall argument encoding, IPC message framing, and capability-table entries, oracled against "no fabricated capability is ever accepted" and "no malformed input reaches privileged execution."
- **Observability requirements:** every capability-table mutation, IPC delivery/drop, and exception/trap MUST be traceable as a kernel-level diagnostic event (see §39).
- **Hardware validation requirements:** N/A until P4 (hosted-reference contracts today; see ADR-010). The first executed instance is the `kernel/` VM-testing spine described in §38.10 — real EL1 boot, PL011 UART, in-kernel selftests — which is the bridge artifact toward P4/P5 bare-metal and hardware-lab qualification (§41).
- **Release gates:** the hosted-kernel contracts MUST pass their unit/property/fuzz suite before System Core work depends on them; the VM-testing gate (`scripts/vm-e2e.sh`) MUST be green before any P4 claim of "microkernel-backed" is made.

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

#### V&V — System Core

- **Functional requirements:** compose the semantic object model, capability system, context/memory systems, relationship/world model, intent/action pipeline, agent runtime, actor/task model, application/capability model, and scheduler into one coherent deterministic authority (SC-001..004).
- **Security invariants:** the System Core depends only on kernel capability/IPC/isolation contracts, never on legacy OS services (SC-002); it never treats model output as authority (SC-004).
- **Safety invariants:** every System Core object is representable as an entity with provenance and permissions (SC-003), so no subsystem can hold state that is invisible to audit or capability control.
- **Performance targets:** action-pipeline latency (validate→authorize→execute→verify→record) MUST stay within the interactive budget defined by the Experience Layer's responsiveness targets (§27); the deterministic authority path adds no more than one verification pass of overhead per action.
- **Failure / recovery behavior:** a failure in any one subsystem (agent runtime, scheduler, storage) MUST NOT corrupt the deterministic authority's own state; System Core restart MUST replay durable state without re-executing already-recorded actions (idempotent recovery).
- **Test strategy:** integration testing is the primary layer here — this is precisely the "microkernel↔system-core↔capability↔actor-runtime↔storage↔world-model↔AI-runtime↔experience" interaction surface (§38.2); chaos testing exercises cross-subsystem failure isolation (§38.6).
- **Fuzzing strategy:** fuzz the inter-subsystem call contracts (capability evaluation calls from actor-runtime, storage calls from the action pipeline) with malformed/adversarial arguments, oracled against "no subsystem failure escalates into ambient authority."
- **Observability requirements:** the System Core MUST expose which subsystems are loaded/healthy, and MUST be able to render the full cross-subsystem trace for any action (ties to IA-006, EXP-005; see §39).
- **Hardware validation requirements:** N/A until P4 rehosts the System Core on the real microkernel; hosted-reference integration tests are the current bar.
- **Release gates:** full M1 vertical-slice integration suite green (§46 acceptance criteria) before any subsystem is considered independently shippable.

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

#### V&V — Semantic Object Model & Storage

- **Functional requirements:** content-addressed, versioned, provenance-bearing entity/relationship storage with world-model traversal (ST-001..007).
- **Security invariants:** encrypted at rest with local keys, nothing plaintext by default (ST-004); access and mutation gated by capabilities, no readable-by-default namespace (ST-006).
- **Safety invariants:** crash consistency — entity/version/relationship writes and their events commit atomically and survive restart (ST-008); a torn write MUST NEVER leave an entity in a state where its content hash does not match its stored bytes.
- **Performance targets:** entity read latency and relationship-traversal latency tracked as benchmarks (target: bounded by working-set size, not full-store scans); write-path throughput sized to sustain the M1 vertical slice's action rate without backpressure on interactive actions.
- **Failure / recovery behavior:** a mid-write crash MUST leave the store in its last-committed state on restart, never a partially-written entity; storage unavailability MUST fail actions closed (deny/defer), never fall back to an unverified in-memory state.
- **Test strategy:** unit tests on the entity/version model (§38.1); property-based tests for "storage recovery preserves consistency after crashes" (§38.3); fuzzing of serialization/deserialization and on-disk formats (§38.4); stress testing with huge semantic graphs and storage exhaustion (§38.5); chaos testing that kills the process mid-write (§38.6); differential testing against a reference model of the store's invariants (§38.9).
- **Fuzzing strategy:** fuzz the on-disk entity/relationship encoding and the content-addressing/hash-verification path; oracle is "corrupted or truncated input is detected and rejected, never silently accepted as valid content."
- **Observability requirements:** expose store size, write/read latency, encryption status, and the provenance chain for any entity on demand; every entity/relationship mutation MUST be traceable to the action and event that produced it (ties to SEC-008).
- **Hardware validation requirements:** N/A until P5 (real storage devices, wear/latency characteristics); hosted-reference persistence (local disk under the hosted OS) is today's validation surface.
- **Release gates:** crash-consistency test suite green (including simulated kill-mid-write) and encrypted-at-rest verified by test (no plaintext on disk) before storage is considered milestone-complete.

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

#### V&V — Capability System

- **Functional requirements:** mint, delegate, evaluate, and revoke capabilities with scope/constraint matching, yielding exactly one of ALLOW/DENY/REQUIRE_APPROVAL (CAP-001..009).
- **Security invariants:** unforgeability (CAP-002) — a capability cannot be fabricated, only granted or delegated; attenuation-only delegation (CAP-005) — an actor can never delegate a broader scope than it holds; cascading revocation (CAP-006) — revoking a capability immediately revokes every capability delegated from it; fail-closed evaluation (CAP-007) — absence of a matching capability is DENY, never ALLOW-by-default.
- **Safety invariants:** evaluation is a pure function of (subject, action, scope, constraints, current-revocation-state) — it MUST NOT have side effects that could be exploited to force a different outcome on re-evaluation (no TOCTOU window between check and use).
- **Performance targets:** capability evaluation latency MUST stay off the critical path for interactive actions (target: sub-millisecond per evaluation on reference hardware, single-digit-microsecond in the kernel-space measurement of §38.10).
- **Failure / recovery behavior:** an engine restart MUST NOT resurrect a revoked capability; ambiguous or corrupted capability state MUST deny rather than guess.
- **Test strategy:** unit tests on scope/constraint matching (§38.1); property-based testing is the primary discipline here — "an actor never accesses a resource without the required capability," "revocation prevents all future access," "delegation never amplifies" are exactly the checkable properties named in §38.3; fuzzing of capability serialization and evaluation inputs (§38.4); formal verification is prioritized for this subsystem above all others (§38.7) because capability enforcement is the single invariant the rest of the OS's safety claims rest on.
- **Fuzzing strategy:** fuzz capability-table entries, delegation chains, and scope/constraint encodings with adversarial and malformed inputs; oracle is "no fuzzed input ever produces ALLOW without an unbroken, un-revoked delegation chain rooted in a legitimate grant."
- **Observability requirements:** every grant, delegation, evaluation, use, and revocation MUST be an immutable, queryable event (CAP-008); the experience layer MUST be able to show a capability's full delegation lineage on demand.
- **Hardware validation requirements:** N/A until P4 roots enforcement in real kernel-space capability tables; today's unforgeability rests on Rust-type-system construction (module-private token fields) proved by the in-kernel selftests of §38.10.
- **Release gates:** the unforgeability, attenuation, and cascading-revocation property tests (§38.3) MUST be green, and the fail-closed default MUST hold under fuzzing, before any milestone that grants agents new capability classes.

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

#### V&V — Context System

- **Functional requirements:** deliberate, ranked, budgeted context assembly from workspace/entities/actions/memory/agents/capabilities, with per-item provenance (CTX-001..004).
- **Security invariants:** context assembly MUST NOT bypass capability checks to read entities it surfaces — inclusion in context is never itself a grant of access.
- **Safety invariants:** the context budget MUST be enforced (CTX-002) — the whole store is never dumped into a model; a budget overrun MUST truncate by the documented priority order (CTX-004), never silently include unranked material.
- **Performance targets:** context assembly latency MUST stay within the interactive budget (target: low tens of milliseconds for a typical workspace on reference hardware); ranking MUST be deterministic for identical inputs (reproducible for debugging and testing).
- **Failure / recovery behavior:** a source that fails to respond (memory store unavailable, world-model query timeout) MUST degrade the context (mark the gap, proceed with what is available) rather than block or silently fabricate.
- **Test strategy:** unit tests on ranking/budget logic (§38.1); integration tests against real memory/world-model sources (§38.2); property-based tests that budget is never exceeded and every included item carries provenance (§38.3).
- **Fuzzing strategy:** fuzz the ranking inputs (malformed relevance/confidence scores, adversarially large candidate sets) oracled against "budget is never exceeded and ranking never crashes the assembler."
- **Observability requirements:** what was included, what was excluded, and why (CTX-005) MUST be inspectable per assembly — this is a direct observability requirement, not just a functional one; expose it through the same trace mechanism as actions (§39).
- **Hardware validation requirements:** N/A — context assembly is a System Core concern independent of the hardware phase.
- **Release gates:** budget-enforcement and inspectability tests green before context is wired into any new agent or application surface.

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

#### V&V — Intent & Action Pipeline

- **Functional requirements:** structured intent → untrusted interpretation → action compilation → the ordered validation stages → deterministic execution → verification → immutable recording (IA-001..006).
- **Security invariants:** intent never carries authority (IA-001); malformed or hallucinated interpretation output cannot execute anything or escalate (IA-007); the ordered validation stages (syntax→schema→semantic→operation-existence→capability→policy→approval→execution→verification) MUST all run, in order, with no stage skippable (IA-004).
- **Safety invariants:** verification is mandatory — "unverifiable success is a failure" (IA-005); a failure at any stage stops the action with no side effects on core state (Section 6.5).
- **Performance targets:** end-to-end pipeline latency (validate→verify) tracked per action class; interactive-class actions target sub-100ms pipeline overhead excluding model inference time.
- **Failure / recovery behavior:** mid-flight interpretation failure leaves core state intact (IA-007, INV-006); a crash between execution and verification MUST be recoverable by re-verifying from durable state on restart, never by assuming success.
- **Test strategy:** this is the pipeline the whole pyramid exists to protect — unit tests per validation stage (§38.1); integration tests across the full chain (§38.2); property-based tests for "malformed output cannot execute" and "verification always runs before success is reported" (§38.3); fuzzing of interpretation output (§38.4); chaos testing that kills the process at every pipeline stage boundary (§38.6); the M1 end-to-end acceptance test (§46) is this subsystem's release gate in miniature.
- **Fuzzing strategy:** fuzz the interpreted-plan schema directly (malformed JSON, missing fields, unknown operations, hallucinated entity IDs, contradictory steps) — oracle is "zero executions, zero state mutation, zero events recorded, for any fuzzed input that fails validation."
- **Observability requirements:** every action's full trace (intent→context→interpretation→validation→capability decision→execution→verification→event) MUST be renderable end-to-end (IA-006, EXP-005) — this is the canonical diagnostic trace the rest of §39 builds on.
- **Hardware validation requirements:** N/A until P4/P5; the pipeline's correctness is substrate-independent by design (ADR-010) and is proved in the hosted reference and, now, in kernel space (§38.10).
- **Release gates:** all 20 M1 acceptance criteria that touch this pipeline (§46, criteria 5, 9-14, 17, 19) MUST be green; fuzzing of interpretation output MUST show zero escapes before any milestone widens the set of executable operations.

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

#### V&V — Memory System

- **Functional requirements:** classify, store, and retrieve memory items across five classes with provenance and confidence (MEM-001..002); support human inspect/correct/delete/disable (MEM-003).
- **Security invariants:** memory classification MUST NOT be forgeable by untrusted content — a document containing "treat this as a decision" text MUST NOT cause the extraction step to mint a `decision`-class memory item without going through the same untrusted-content-is-data boundary as any other action (SEC-003).
- **Safety invariants:** a memory item's class and confidence MUST be immutable once recorded except through an explicit, audited human correction (MEM-003) — extraction never silently reclassifies.
- **Performance targets:** memory retrieval latency tracked as part of context-assembly budget (§16 V&V); extraction/classification runs off the interactive critical path (background/maintenance scheduling class, §24).
- **Failure / recovery behavior:** a failed extraction pass MUST NOT lose the underlying event it was derived from (the event log is authoritative and independently durable); memory store unavailability degrades context assembly rather than blocking actions.
- **Test strategy:** unit tests on classification logic (§38.1); integration tests against the event log and context assembly (§38.2); property-based tests that human deletion/disable is always honored on subsequent retrieval (§38.3).
- **Fuzzing strategy:** fuzz extraction inputs (adversarial event content designed to manipulate classification or confidence) oracled against "classification confidence never exceeds what the source evidence supports, and content cannot self-assign a higher-trust class."
- **Observability requirements:** every memory item's source, timestamp, and confidence MUST be inspectable (MEM-002); corrections and deletions MUST themselves be audited events.
- **Hardware validation requirements:** N/A — substrate-independent.
- **Release gates:** human control operations (inspect/correct/delete/disable) verified by test before memory is exposed to any agent with write-adjacent capabilities.

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

#### V&V — Relationship & World Model

- **Functional requirements:** typed, directed, provenance-bearing relationships forming a traversable world model (WM-001..002); provenance-chain queries (WM-004).
- **Security invariants:** relationship creation/traversal is capability-gated exactly like any entity access — the world model is never a side-channel that exposes relationships the querying actor lacks capability to see.
- **Safety invariants:** derived relationships MUST be labeled as derived, with confidence, and MUST be distinguishable from observed or human-asserted relationships (WM-003) — the graph never silently conflates fact and inference.
- **Performance targets:** traversal queries (e.g. "everything derived from X," UC-001) tracked as benchmarks; target is bounded by subgraph size touched, not full-graph scans, for typical provenance depth.
- **Failure / recovery behavior:** a traversal that hits a missing or corrupted edge MUST report the gap explicitly rather than silently truncating the result set.
- **Test strategy:** unit tests on relationship typing/direction (§38.1); integration tests on traversal against the real store (§38.2); property-based tests that derived-vs-observed labeling is preserved under every mutation path (§38.3); stress testing with huge semantic graphs (§38.5).
- **Fuzzing strategy:** fuzz traversal queries (deep chains, cycles, huge fan-out) oracled against "traversal always terminates and never returns an edge the querying actor lacks capability to see."
- **Observability requirements:** provenance-chain queries (UC-001) MUST be directly renderable in the experience layer; traversal cost/depth should be exposed for performance diagnosis.
- **Hardware validation requirements:** N/A — substrate-independent.
- **Release gates:** derived-vs-observed labeling and provenance-chain query correctness verified by test before the world model is exposed to a new query surface.

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

#### V&V — Agent Runtime

- **Functional requirements:** first-class agent identity with capability set, memory, context, tools, and goals (AG-001); bounded, interruptible autonomy (AG-004, AG-006).
- **Security invariants:** an agent can never exceed its granted capabilities (AG-002); agents are isolated from one another and from the System Core so a misbehaving agent cannot corrupt core state or seize authority (AG-003).
- **Safety invariants:** explicit step limits, loop detection, and resource bounds on every autonomous goal (AG-004); no uncontrolled concurrent autonomous mutation of the same entities — concurrency is mediated by capability, optimistic versioning, and approval (AG-007, and see the project's own hard-won operating rule: never run two uncontrolled autonomous drivers against shared state).
- **Performance targets:** agent invocation latency, step latency, and cancellation latency (time from cancel request to observable stop) tracked as benchmarks; cancellation target: sub-second observable stop for any single step.
- **Failure / recovery behavior:** a crashing agent MUST be isolated (AG-003) and its task marked failed without corrupting core state; a cancelled agent MUST stop without partial, unrecorded side effects (AT-004, INV-007).
- **Test strategy:** unit tests on goal/bound/loop-detection logic (§38.1); integration tests on multi-agent composition via capabilities (§38.2); property-based tests for isolation ("an isolated actor cannot corrupt another's memory," §38.3); chaos testing that kills the agent runtime outright — the OS and other agents MUST survive it (§38.6); this is also where the "AI is never a single point of failure" requirement is enforced end to end.
- **Fuzzing strategy:** fuzz agent goal specifications and tool-call sequences (malformed goals, adversarial loops, resource-exhaustion patterns) oracled against "step/loop/resource bounds always trigger before unbounded resource consumption."
- **Observability requirements:** every agent's identity, capability set, activity, and cancellation state MUST be inspectable in the experience layer (EXP-004); every agent action is observable through the same trace/event mechanism as human-initiated actions (AG-006).
- **Hardware validation requirements:** N/A until P5 (per-agent resource isolation on real accelerators); today's isolation is process/module-level in the hosted reference.
- **Release gates:** loop-detection and isolation tests green, and a chaos test proving "kill the agent runtime, the rest of the OS survives" passes, before any milestone grants agents write-capable default tool sets.

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

#### V&V — Intelligence as a System Capability

- **Functional requirements:** a stable model-runtime interface (load/unload/health/generate/stream/cancel) behind which any model is interchangeable (INT-001); structured-output production for anything executable (INT-003); resource-aware lifecycle management (INT-005).
- **Security invariants:** all model output is untrusted input to the action pipeline, with no exception (INT-002); free text alone never acts (INT-003) — this is the containment boundary for untrusted model output, and it is enforced by the pipeline (§17 V&V), not by this subsystem trusting the model.
- **Safety invariants:** **the AI subsystem MUST NEVER be a single point of failure for the OS** — if the model is unavailable, crashed, or hung, the OS MUST continue to provide all deterministic functionality via the identical validation/capability/verification pipeline through the deterministic interpreter fallback (INT-004, INV-014); this is both a functional requirement and the chaos-testing invariant of §38.6.
- **Performance targets:** model load/unload latency, generation latency (time-to-first-token and tokens/sec), and cancellation latency (time from cancel to observable stop) tracked as benchmarks; the deterministic fallback path's latency is tracked separately and MUST NOT regress when the model is present.
- **Failure / recovery behavior:** a hung or crashed model process MUST be detected (health check) and the runtime MUST fail over to the deterministic interpreter without operator intervention; the runtime MUST support pause/resume/throttle under memory/thermal pressure coordinated with the scheduler (§24) without losing in-flight request state.
- **Test strategy:** unit tests on the runtime port/adapter contract (§38.1); integration tests against a real local-model adapter and the deterministic fallback (§38.2); chaos testing that kills the model process mid-generation — the pipeline MUST recover to the fallback (§38.6, this is the direct test of "never a single point of failure"); stress testing under concurrent AI workloads and GPU/NPU saturation (§38.5).
- **Fuzzing strategy:** fuzz raw model output (malformed structured output, adversarial free text, truncated streams) — the oracle is IA-007's guarantee: no fuzzed model output ever executes an action or corrupts state, regardless of how well-formed it appears.
- **Observability requirements:** current model identity/version, health, load state, and resource pressure MUST be exposed; every generation MUST be traceable to the action(s) it fed, with the untrusted-output boundary visible in the trace.
- **Hardware validation requirements:** N/A until P5 (NPU-aware bare-metal scheduling); today's validation is hosted-reference with a real local-model adapter and CPU/GPU inference.
- **Release gates:** the chaos test "kill the model mid-generation, OS continues via deterministic fallback" MUST pass before any milestone increases agent autonomy that depends on model availability.

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

#### V&V — Application & Capability Model

- **Functional requirements:** applications as capability providers exposing typed operations and entity types (APP-001); portable components via capability-secure WASM/WASI (APP-002); composition of capabilities/entities/interfaces around a single intent (APP-004).
- **Security invariants:** no ambient authority for any application/component — only imported capabilities (APP-002); application data lives as entities with relationships and provenance, never in a private opaque silo the system cannot reason across (APP-003).
- **Safety invariants:** every application/component runs sandboxed with only its granted capabilities; failure is isolated (APP-005) — a crashing application cannot take down another application or the System Core.
- **Performance targets:** WASM/WASI component instantiation and call-boundary overhead tracked as a benchmark; target is low-single-digit-percent overhead versus a native call for typical tool invocations.
- **Failure / recovery behavior:** a crashing or hung application/component is terminated and its task marked failed without corrupting core state or other applications' state (isolation, INV-007).
- **Test strategy:** unit tests on the tool/operation contract (SDK-003) in isolation (§38.1); integration tests on multi-application composition around one intent (§38.2); contract testing of the WASM/WASI component boundary (a form of differential testing, §38.9); chaos testing that crashes an application mid-operation (§38.6).
- **Fuzzing strategy:** fuzz the WASM/WASI import surface and the tool-call schema with malformed component binaries and adversarial arguments; oracle is "no imported capability call ever grants authority the component was not explicitly given, and no malformed component crashes the host."
- **Observability requirements:** which capabilities each installed application/component holds MUST be inspectable and revocable from the experience layer (EXP-004); a failed application's crash MUST be attributable in the trace.
- **Hardware validation requirements:** N/A until P2 delivers the WASM/WASI runtime; today this is architecture only.
- **Release gates:** isolation-under-crash test green for at least one reference application before the component model is considered load-bearing for third-party extension.

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

#### V&V — Actor / Task Execution Model

- **Functional requirements:** capability-scoped task execution with a persisted lifecycle (created→queued→running→waiting/awaiting-approval→completed/failed/cancelled) that survives restart (AT-001..003); cancellation and loop detection (AT-004).
- **Security invariants:** a task can only touch entities/devices/endpoints for which it holds capabilities (AT-002) — no process-style ambient authority ever leaks into a task.
- **Safety invariants:** task lifecycle transitions MUST be monotonic and durable — a restart MUST recover the exact last-committed lifecycle state, never re-run a completed task or lose a running one silently (AT-003).
- **Performance targets:** task-state persistence write latency and restart-recovery time tracked as benchmarks; cancellation-to-stop latency target: sub-second for any single in-flight step.
- **Failure / recovery behavior:** a crash mid-task MUST leave the task in a recoverable, well-defined state on restart (waiting or failed, never corrupted); loop detection MUST flag repeated-identical-action and no-progress patterns before they exhaust resources.
- **Test strategy:** unit tests on the lifecycle state machine (§38.1); property-based tests that every reachable state transition preserves durability and capability-scoping (§38.3); chaos testing that restarts the process mid-task (§38.6); stress testing with many concurrent actors (§38.5).
- **Fuzzing strategy:** fuzz the task lifecycle event stream (out-of-order events, duplicate cancellations, concurrent state transitions) oracled against "the state machine never reaches an undefined or capability-inconsistent state."
- **Observability requirements:** task lifecycle, current step, and cancellation state MUST be inspectable; loop-detection triggers MUST be surfaced as diagnosable events, not silent throttling.
- **Hardware validation requirements:** N/A until P4/P5 (real preemptive scheduling); today's task execution is cooperative in the hosted reference.
- **Release gates:** restart-recovery test (crash mid-task, verify correct recovered state) green before task execution is considered durable for production-shaped workloads.

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

#### V&V — Heterogeneous Scheduler

- **Functional requirements:** model CPU/GPU/NPU as first-class resources and schedule interactive/inference/background/maintenance workloads across them by configurable policy (SCH-001..002).
- **Security invariants:** scheduling decisions MUST NOT be influenced by capabilities the requesting actor does not hold — a low-privilege background task cannot starve an interactive task by manipulating scheduler hints it has no capability to set.
- **Safety invariants:** interactive/human-facing work MUST take priority over background intelligence and maintenance (SCH-002) — this is a hard priority-inversion bound, not a soft preference; under memory/thermal/power pressure, optional work is shed before interactivity or stability is compromised (SCH-003).
- **Performance targets:** scheduling latency and context-switch cost tracked as core benchmarks (§38.13); interactive-task dispatch latency target: bounded and regression-gated once real preemptive scheduling exists (P4/P5); priority-inversion duration for any interactive task MUST have a documented upper bound.
- **Failure / recovery behavior:** a runaway background/inference workload MUST be throttleable or preemptible without operator intervention; scheduler policy failures MUST fail toward protecting interactivity, never toward starving it.
- **Test strategy:** unit tests on scheduling-policy logic in isolation (§38.1); property-based tests for priority-inversion and deadline bounds (§38.3, "the scheduler's safety invariants are deadline/priority-inversion bounds"); stress testing under CPU/GPU/NPU saturation (§38.5); performance benchmarking of scheduling/context-switch latency (§38.13).
- **Fuzzing strategy:** fuzz scheduler policy inputs and workload-mix combinations (adversarial priority requests, pathological workload interleavings) oracled against "interactive work never starves regardless of background workload shape."
- **Observability requirements:** current scheduling policy, per-workload-class resource share, and pressure-response actions (what was shed and why) MUST be inspectable.
- **Hardware validation requirements:** N/A until P5 — real CPU/GPU/NPU scheduling is explicitly hardware-bound (Non-Goal 11); today's scheduler is modeled/abstracted (SCH-004) and its contracts are written so they do not assume anything a real scheduler cannot provide.
- **Release gates:** priority-inversion bound tests and pressure-response tests green before the scheduler's modeled contracts are considered stable enough for P4/P5 rehosting.

---

# 25. Hardware Abstraction Layer

## HAL-001 — Uniform Device Model
Hardware MUST be exposed through a uniform device abstraction (id, category, vendor, model, capabilities, state, performance), with devices as entities.

## HAL-002 — Capability-Gated Device Access
All device access (including sensitive devices — camera, microphone) MUST require explicit capabilities and, for sensitive devices, explicit human approval and audit.

## HAL-003 — Isolation
Device access MUST be isolated so a device or driver failure degrades gracefully without crashing the core.

#### V&V — Hardware Abstraction Layer

- **Functional requirements:** uniform device model (id, category, vendor, model, capabilities, state, performance) with devices as entities (HAL-001).
- **Security invariants:** all device access, and especially sensitive devices (camera, microphone), requires an explicit capability and, for sensitive devices, explicit human approval plus audit (HAL-002, SEC-007) — intelligence can never silently activate a sensitive device.
- **Safety invariants:** device or driver failure degrades gracefully and is isolated — it MUST NOT crash the core (HAL-003).
- **Performance targets:** device-capability-check latency tracked as part of the action pipeline's overhead budget; device enumeration/discovery latency at boot tracked once real hardware enumeration exists (P5).
- **Failure / recovery behavior:** a failed or disconnected device MUST surface as a degraded-capability state, not a crash; reconnection MUST be detected and re-authorized through the same capability path, not auto-trusted.
- **Test strategy:** unit tests on the device-entity model (§38.1); integration tests on capability-gated device access (§38.2); chaos testing that simulates device failure/disconnection mid-operation (§38.6); this subsystem carries the heaviest real-hardware-testing dependency of the whole PRD (§38.11).
- **Fuzzing strategy:** fuzz device-descriptor and driver-response parsing (malformed device metadata, adversarial driver responses) oracled against "a malformed device response never grants capability-bypassing access or crashes the core."
- **Observability requirements:** device inventory, capability grants per device, and sensitive-device activation events MUST be inspectable and auditable (HAL-002, SEC-007, SEC-008).
- **Hardware validation requirements:** N/A until P5, at which point this is the primary subsystem the automated hardware lab (§41) exists to validate — per-device-class flash/boot/exercise/reset cycles on real hardware are mandatory before any device driver ships.
- **Release gates:** capability-gated access and approval-for-sensitive-devices tests green in the hosted model before P5 promotes any device class to real-hardware support.

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

#### V&V — Native Graphics & Compositor

- **Functional requirements:** entity/intent-driven dynamic surfaces (GFX-002); GPU-accelerated rendering via the HAL (GFX-003); hosted surface first, native on-GPU compositor deferred (GFX-004).
- **Security invariants:** surface access and capture are capability-gated (GFX-003) — no application or agent can capture another surface's content, or the screen, without an explicit capability and (for screen capture) the sensitive-device approval path (SEC-007).
- **Safety invariants:** a compositor/rendering fault MUST be isolated per surface — one misbehaving surface's rendering failure MUST NOT crash the whole compositor or take down unrelated surfaces.
- **Performance targets:** frame latency and dropped-frame rate tracked as benchmarks once the native compositor exists (P5); the hosted surface targets responsiveness consistent with the Experience Layer's interactive budget (§27 V&V).
- **Failure / recovery behavior:** a rendering crash MUST recover to a known-good surface state (or an explicit error surface) rather than a frozen or corrupted frame.
- **Test strategy:** unit tests on surface-composition logic (§38.1); integration tests on entity/intent-driven surface assembly (§38.2); visual-regression testing of the hosted surface at CI time; the native on-GPU compositor's stress/load and real-hardware testing (§38.5, §38.11) are explicitly deferred to P5.
- **Fuzzing strategy:** fuzz the surface-composition input (malformed entity/intent descriptors driving surface layout) oracled against "no malformed composition input crashes the compositor or bypasses capture capability checks."
- **Observability requirements:** which surfaces exist, what entities/intents compose them, and what capture capabilities are active MUST be inspectable.
- **Hardware validation requirements:** N/A until P5 — a production-grade native compositor on real GPUs is explicitly out of scope for M1 (Non-Goal 8); GPU driver stability and frame-timing consistency are hardware-lab qualification targets when P5 begins.
- **Release gates:** capability-gated capture verified by test in the hosted surface before any milestone exposes a screen/window-capture capability to agents.

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

#### V&V — Experience Layer

- **Functional requirements:** dynamic, intent/context-driven interfaces (EXP-001..002); semantic navigation/search (EXP-003); human control surfaces for context, capabilities, memory, agents, approvals, and system status (EXP-004); full-trace explainability for any action (EXP-005).
- **Security invariants:** the experience layer and applications cannot bypass capability authorization (INV-010) — no UI shortcut, batch action, or "quick approve" flow may skip the pipeline's authorization stage.
- **Safety invariants:** an approval surface MUST accurately represent what it is asking the human to approve — the rendered action description MUST be derived from the same validated plan the pipeline will execute, never a separately-generated (and potentially divergent) summary.
- **Performance targets:** perceived interaction latency (input to visible response) target: low tens of milliseconds; full context/trace render target: sub-second. Stated as targets, not yet measured — no native surface exists before P3.
- **Failure / recovery behavior:** a failure to render a trace or control surface MUST fail visibly (explicit error state) rather than silently omitting information that would change a human's approval decision.
- **Test strategy:** unit tests on trace-rendering logic (§38.1); integration tests against the real action pipeline (§38.2); accessibility and visual-regression testing once a native surface exists (P3); the explainability requirement (EXP-005) is directly verified by the M1 acceptance criterion that the experience surface can render the full trace (§46, criterion 20).
- **Fuzzing strategy:** fuzz trace/context payloads feeding the render layer (deeply nested, malformed, or adversarially large trace structures) oracled against "the renderer never crashes and never silently drops a field that would change an approval decision."
- **Observability requirements:** this subsystem *is* the primary observability surface for the rest of the OS (§39) — every requirement in EXP-004/EXP-005 is itself an observability requirement.
- **Hardware validation requirements:** N/A until P5 (native on-GPU rendering); the hosted surface is the current validation target.
- **Release gates:** the full-trace-explainability acceptance criterion (§46, criterion 20) MUST be green before any milestone that introduces a new class of destructive or high-risk action.

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

#### V&V — Security Model

- **Functional requirements:** capability-secure foundation end to end, kernel through experience layer (SEC-001); untrusted intelligence (SEC-002); untrusted content (SEC-003); fail closed (SEC-004); least privilege and revocation (SEC-005); secure boot and isolation (SEC-006); sensitive-device gating (SEC-007); auditability (SEC-008); compatibility confinement (SEC-009).
- **Security invariants:** this section *is* the invariant set — every SEC-00x requirement is itself a security invariant, and every INV-0xx in §34 restates one of them as a testable claim. There is no separate "safety" layer beneath this one for this subsystem; safety here means "the security model itself does not have a gap that degrades into an availability or corruption failure."
- **Safety invariants:** fail-closed behavior (SEC-004) MUST hold even under internal error — an exception, panic, or unexpected code path inside the capability evaluator MUST resolve to DENY, never to an ambiguous or default-allow state.
- **Performance targets:** see the per-subsystem targets in §15 (Capability System) and §12 (Microkernel) — this section defines the invariants those targets are measured against, not new numbers of its own.
- **Failure / recovery behavior:** any unresolved capability, entity, relationship, or effect denies the action (SEC-004) — "unresolved" includes internal errors in the resolution machinery itself.
- **Test strategy:** this section is why the pyramid exists at all. Independent security auditing and adversarial/penetration testing (§38.12) target this section directly; formal verification's priority list (§38.7) is drawn entirely from SEC-00x; the security threat model (§42) is the scenario catalogue this section must survive.
- **Fuzzing strategy:** continuous fuzzing of every capability-evaluation, IPC, and untrusted-content-boundary code path (§38.4) — this section names the oracle for all of them: no fuzzed input ever produces ambient authority, an escalated capability, or an executed instruction from untrusted content.
- **Observability requirements:** every security-sensitive operation MUST be an immutable event (SEC-008); the trust order (system invariants > developer policy > human intent > retrieved data > untrusted content, SEC-003) MUST be visible in any trace that shows why content was denied instruction authority.
- **Hardware validation requirements:** N/A until P4/P5 for secure boot and hardware isolation (SEC-006); today's isolation is process/module-level in the hosted reference and kernel-space in the §38.10 VM evidence.
- **Release gates:** independent security review (§38.12) plus a clean fuzzing pass on the capability/IPC/untrusted-content surfaces MUST precede any milestone that widens agent autonomy or exposes a new sensitive-device class.

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

#### V&V — Reliability

- **Functional requirements:** durable state across restart for entities, versions, relationships, capabilities, tasks, memory, and events (REL-001); atomic state+event transitions (REL-002); idempotent retries (REL-003); failure isolation (REL-004); version recovery and, later, system rollback (REL-005).
- **Security invariants:** recovery paths MUST re-derive authority from currently-valid capabilities, never resurrect a revoked capability or bypass re-authorization because "it was already approved once."
- **Safety invariants:** destructive actions MUST NOT be blindly retried (REL-003) — idempotency is required for retryable actions specifically so retries are safe, not as a blanket license to retry anything.
- **Performance targets:** restart-to-ready time and recovery-replay time tracked as benchmarks; target is bounded by durable-log size, not full-history replay, for typical restart scenarios.
- **Failure / recovery behavior:** this section defines the behavior the rest of the document points to — a failed interpretation, agent, application, or device must not corrupt core state or crash the core (REL-004); recovery of prior entity versions must always be available (REL-005).
- **Test strategy:** this is where chaos and fault-injection testing (§38.6) earns its place in the pyramid — durability and atomicity are unverifiable by unit tests alone; they require actually crashing the process and inspecting recovered state; property-based testing for "storage recovery preserves consistency after crashes" (§38.3) is the formal statement of REL-001/002.
- **Fuzzing strategy:** fuzz the recovery/replay path itself (truncated logs, out-of-order durable writes, partial commits) oracled against "recovery never reports success on a state it cannot verify."
- **Observability requirements:** recovery events (what was replayed, what was rolled back, what could not be recovered) MUST be recorded as immutable events, not silent internal bookkeeping.
- **Hardware validation requirements:** N/A until P5 for system rollback across real hardware state (secure boot, firmware); today's recovery is process-restart-level in the hosted reference and VM-level in the §38.10 evidence.
- **Release gates:** the crash-mid-write and crash-mid-task recovery tests (also gating §14 and §23) MUST be green before any milestone claims durability as a shipped property rather than an architectural intent.

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

M1 explicitly does **not** require: bare-metal boot, native on-GPU compositor, NPU scheduling, or a compatibility environment. Those are later phases (Section 45) with their own acceptance criteria.

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

# 38. Verification & Validation Architecture

Testing, verification, validation, observability, fuzzing, and hardware qualification are **architecture**, not a final QA phase. This section defines the discipline that Sections 12-28 and 30 each apply in their own `V&V` block. The core principle, stated once here because it governs everything below it:

> Tests provide evidence that known scenarios work. They do not prove the system is correct.

A green test suite demonstrates that the scenarios someone thought to write are handled as expected. It says nothing about the scenarios no one thought to write, the states only an adversary or a random search would reach, or behavior under real hardware, real concurrency, and real failure. Aletheia grants delegated authority to an untrusted, probabilistic component and then executes what that component proposes — the gap between "known scenarios work" and "the system is correct" is exactly the gap an attacker, or a misbehaving agent, will find. Closing that gap requires layering disciplines that each attack a different part of the unknown; it is not solved by writing more unit tests.

## The Validation Pyramid

Eleven layers, in order. Each layer proves something the layer below it cannot, and each has an explicit limit — naming the limit is part of the discipline, not an admission of weakness:

1. **Unit** — proves a component behaves correctly for the inputs its author considered. Does not prove correctness for inputs the author did not consider, or in combination with other components.
2. **Integration** — proves components correctly compose across their real boundaries (not mocks). Does not prove the composition holds under concurrency, scale, or adversarial input.
3. **Property-Based** — proves an invariant holds across a randomized input space far larger than any example-based suite. Does not prove the invariant outside the generators' distribution, or under real concurrency/timing.
4. **Fuzzing** — proves a parsing/decoding/evaluation surface survives adversarial and malformed input the property generators didn't think of. Does not prove correctness beyond crash/hang/undefined-behavior — a fuzzer that finds nothing has not proven the surface secure.
5. **Stress & Load** — proves behavior holds at scale and under resource pressure. Does not prove correctness of individual operations; a system can be fast and wrong.
6. **Chaos & Fault-Injection** — proves the system recovers from failures actually injected. Does not prove recovery from every possible failure combination, only the ones exercised.
7. **Formal Verification** (critical components only) — proves a specification holds for all inputs within the model, mathematically. Does not prove the specification matches reality, or that the modeled component is correctly connected to the unmodeled rest of the system.
8. **VM / Emulator** — proves the actual built artifact boots and behaves correctly under an emulated CPU and privilege model. Does not prove timing, driver, or hardware-specific behavior; emulated timing is not hardware timing.
9. **Real Hardware** — proves the artifact behaves correctly on the CPU/accelerators/devices it will actually ship on. Does not, by itself, prove security or performance under adversarial conditions.
10. **Security Audit** — proves, to the confidence of the auditors performing it, that the system resists the specific attack techniques attempted. Does not prove the absence of unknown attack classes.
11. **Performance Validation** — proves the system meets stated latency/throughput/resource targets under the measured conditions. Does not prove those targets hold under conditions not measured (different hardware, different load shape).

No single layer, and no sum of layers, is a correctness proof. The pyramid is a discipline for closing the evidence gap as far as the state of the art allows, and for being explicit about how far that still falls short of "correct."

The detailed subsections below (§38.1-§38.13) implement this pyramid for Aletheia specifically. Two of them — Runtime Assertions & Invariants (§38.8) and Differential Testing (§38.9) — are cross-cutting supplements that operate within and across the eleven layers above, not additional rungs; the pyramid's own layers 10 and 11 (Security Audit, Performance Validation) are detailed in §38.12 and §38.13, after VM/Emulator (§38.10) and Real Hardware (§38.11) testing — which is where Aletheia's delivered evidence currently ends (§38.10).

## 38.1 Unit Testing

Every kernel, runtime, storage, security, IPC, and scheduler component MUST have unit tests covering its logic in isolation, mocking only the boundaries it does not own: capability-table matching and constraint evaluation, entity/version encoding, context ranking, action-validation stages, task-lifecycle transitions, memory classification, IPC message framing. Unit tests run on every commit (§40.1); they are necessary and, per the core principle above, insufficient on their own — they exercise only the scenarios their authors anticipated.

## 38.2 Integration Testing

Verifies real, non-mocked interaction across the boundaries the architecture actually has: microkernel↔system-core (capability/IPC contracts), system-core↔capability-system (authorization on every action), capability-system↔actor-runtime (capability-scoped execution), actor-runtime↔storage (durable task state), storage↔world-model (relationship persistence and traversal), world-model↔AI-runtime (context assembly feeding interpretation), and AI-runtime↔experience (trace rendering of intelligence-originated actions). Each boundary above MUST have at least one integration test exercising it against a real implementation, not a stub, before it counts as part of the architecture rather than a plan for one.

## 38.3 Property-Based Testing

Randomized-scenario invariants that MUST always hold, each stated so a single counterexample falsifies it:

- An actor never accesses a resource without holding a capability naming that access.
- Revocation of a capability prevents all future access derived from it, immediately, for every descendant delegation.
- An isolated actor (agent, application, task) cannot corrupt another actor's memory or state under any randomized interleaving of their operations.
- IPC preserves message ordering and sender authentication across every randomized send/receive interleaving.
- Storage recovery preserves consistency — no partial entity, no orphaned version, no dangling relationship — after a randomized crash point.

These are the properties, not the goals: a property-based suite that cannot be falsified by a generated counterexample is not encoding the property, it is restating it.

## 38.4 Fuzzing

Continuous fuzzing targets: kernel IPC message framing, syscall argument encoding, capability validation/evaluation inputs, entity/relationship serialization and deserialization, storage and semantic-object on-disk formats, network protocol parsers, compatibility-layer syscall/ABI translation, and the AI/model-runtime interface — specifically raw model output before it reaches the action pipeline. Every fuzz target needs an explicit oracle beyond "does it crash": the specific invariant it must never violate, named per subsystem in that subsystem's `V&V` block. Fuzzing MUST run continuously in CI (§40.1), not as a one-time pre-release pass.

## 38.5 Stress & Load

Exercises conditions unit and integration tests don't reach: many concurrent actors, IPC traffic at saturation, memory pressure to exhaustion, CPU saturation, storage exhaustion, semantic graphs at large scale (deep and high-fan-out traversal), concurrent AI workloads, and GPU/NPU saturation. The pass criterion is graceful degradation per the documented pressure-response policy (SCH-003) — shedding optional work, denying new admission, or slowing — never silent corruption, unbounded memory growth, or a crash that takes down unrelated actors.

## 38.6 Chaos & Fault-Injection

Deliberately induced failures: crash system-core services, kill the AI runtime mid-generation, disconnect storage, corrupt IPC messages in flight, drop network connectivity, exhaust memory, simulate device failure or disconnection, restart services mid-operation. The requirement: the OS remains resilient and recovers wherever its design promises recovery (REL-004/005). Stated once here because it is load-bearing across the whole document: **the AI subsystem MUST NEVER be a single point of failure for the OS.** Every chaos scenario that kills or degrades the model/agent runtime MUST leave the deterministic system core, storage, and capability enforcement fully operational (see also §21's `V&V` block).

## 38.7 Formal Verification

Aletheia does not attempt to formally prove the whole OS — that is not tractable, and claiming otherwise would itself violate the evidence-not-proof principle. Formal treatment is prioritized in this order: (1) capability enforcement — unforgeability, attenuation, fail-closed evaluation, because it is the single invariant the rest of the security model rests on; (2) memory isolation between address spaces/actors; (3) IPC security — authentication, ordering, absence of ambient send rights; (4) critical scheduler invariants — priority-inversion and deadline bounds; (5) cryptographic primitives and the security boundaries around them.

Candidate techniques, named without overclaiming: model checking of the capability-evaluation state machine; refinement proofs relating a kernel-space capability implementation to its System Core specification; a seL4-style full-functional-correctness proof of the capability-enforcing kernel core is named as a long-term aspiration, not a committed deliverable — it is among the most expensive verification efforts ever completed for an OS kernel, and is scoped honestly here as a P4/P5-horizon ambition, not an M1-P3 requirement.

## 38.8 Runtime Assertions & Invariants

Development and diagnostic builds MUST actively validate critical assumptions at runtime — capability-evaluation preconditions, entity/version invariants, task-lifecycle transition legality — as live assertions, not only as test-time checks. A violated assertion in a diagnostic build MUST produce rich diagnostic state (the failing invariant, the inputs, the call path) and halt or panic; it MUST NOT continue in an unknown state. Silent corruption is explicitly worse than a loud crash. Release builds MAY compile out the most expensive assertions but MUST retain the security-critical ones — capability checks are never debug-only.

## 38.9 Differential Testing

Compares the real implementation against a reference model wherever "obviously correct by inspection" is not achievable: storage encoding/decoding (round-trip against a reference codec), serialization formats, cryptographic primitives (against a vetted reference implementation of the same primitive), scheduling decisions (against a reference scheduler simulator for the same workload), and protocol framing (against a reference decoder). A divergence between the real implementation and the reference model is a defect regardless of which one is "wrong" — the divergence itself is the signal.

## 38.10 VM / Emulator Testing

The OS is built into a bootable image and launched in a VM/emulator during CI; automated tests verify boot success, kernel initialization, memory management, scheduling, IPC, capability enforcement, service startup, and crash recovery.

**This layer is no longer aspirational.** A bootable `no_std` aarch64 microkernel reference now exists at `kernel/` and is the first executed instance of it:

- Boots on QEMU `virt` at EL1, with a PL011 UART and installed exception vectors.
- Re-proves the M1 System-Core invariants **in kernel space** through 11 in-kernel selftests (`kernel/src/selftest.rs`): fail-closed default-deny with no capability present; the full validate→authorize→execute→verify→record-event pipeline completing on an authorized action; an unforgeable-by-construction capability token (its identity field is module-private) rejecting a fabricated token; delegation attenuation (equal-or-narrower scope permitted); delegation amplification denied; cascading revocation reaching delegated descendants immediately; a malformed/untrusted plan rejected before execution, with zero events recorded; an expired capability denied; scope confinement (a capability minted for one entity does not authorize another); capability-gated secure IPC (an unauthorized send is dropped, an authorized send is delivered); a destructive action forced to `REQUIRE_APPROVAL` rather than executing. All 11 currently pass; the VM's semihosting exit code is the machine-checkable verdict (`0` = all invariants held, `10+i` = invariant `i` failed, `101` = kernel panic, `102` = unexpected CPU exception).
- `scripts/vm-e2e.sh` is the CI gate for this layer: build the kernel, boot it under a watchdog, assert both the invariants marker and the exit-0 marker, fail loudly on any deviation.
- This is a **bridge artifact toward P4, not the finished P4 microkernel**: it is single-address-space today. There is no virtual memory, no per-actor address space, no preemptive scheduling, and no secure boot. Those gaps are named explicitly, not glossed over (`kernel/README.md`, "Not yet here"), and are tracked for P4/P5 (§41).

## 38.11 Real Hardware Testing

VMs prove boot and logical correctness; they do not prove timing, driver behavior, thermal behavior, or the failure modes real silicon exhibits. From P4/P5 onward, Aletheia requires progressive testing on real architectures and accelerators through an automated hardware lab that can: flash a target device with a candidate image; power-cycle and boot it; capture serial console and diagnostic output; run the qualification suite against the running device; detect a hang and reset/recover the device without a human present; and report pass/fail per device class into the CI gate structure of §40. Real-hardware testing is currently N/A for every subsystem except as a named target in that subsystem's `V&V` block's "Hardware validation requirements" line; §41 defines the staged path there.

## 38.12 Security Testing & Independent Auditing

The security threat model (§42) is the scenario catalogue; this subsection is the discipline that goes beyond it: structured threat modeling per subsystem (feeding each `V&V` block's security-invariants line), adversarial and penetration testing targeted specifically at the capability system and the untrusted-content boundary, sustained fuzzing campaigns (§38.4) treated as a security control rather than a QA nicety, and independent audits — by a reviewer who did not implement the component under audit — of the critical security components named in §38.7's priority list, before those components are trusted at a milestone boundary.

## 38.13 Performance & Benchmarking

Measured, regression-gated benchmarks for: boot time (VM today, hardware from P5); IPC latency; scheduling latency and context-switch cost (once real scheduling exists); storage throughput; memory overhead per entity/capability/task; agent startup latency; context/retrieval latency; model-inference scheduling latency; CPU/GPU/NPU utilization under load; and energy efficiency (hardware-bound, P5).

The one number this document can currently cite with evidence, quoted with its measurement caveats intact: under QEMU TCG emulation, on the *same* emulated CPU in the *same* run, a capability-checked Aletheia IPC request/response round-trip measures at approximately **0.79x the cost of one bare `svc` (syscall) privilege-boundary crossing** (`kernel/src/bench.rs`). The defensible claim is **narrow**: the capability authorization check Aletheia *adds* costs less than one bare `svc` trap — it is cheap. This does **not** show that Aletheia IPC is faster than a Linux pipe: the measured loop runs entirely in EL1 and crosses no privilege or address-space boundary, whereas a *real* microkernel IPC round-trip **and** a Linux pipe both pay at least two boundary crossings plus context/address-space switches, which the loop skips. It is therefore **not** a whole-OS "faster than Linux" wall-clock claim, and **not** a bare-metal number (QEMU TCG timing is not hardware timing). The measurement is also single-address-space today; cross-address-space IPC cost and a same-emulator Linux-guest comparison are named, explicitly, as the next performance-validation milestones — tracked against this subsection, not asserted here.

---

# 39. Observability Architecture

At every layer, Aletheia MUST be able to explain, at diagnostic level: what is running, why, what capabilities it holds, what resources it accessed, what IPC occurred, what state changed, what failed, and why recovery did or did not succeed. This is not a debugging convenience. For an AI-native OS whose agents hold delegated capabilities, mutate a semantic world model, and act through a probabilistic component, an operator's ability to reconstruct *why the system did what it did* is a security property, not a nicety — it is the mechanism by which SEC-008 (auditability) and EXP-005 (explainability) are actually fulfilled rather than merely declared.

## 39.1 The Diagnostic Event/Trace Model

Observability is built from the two primitives this document already defines — it does not invent a third:

- **Event** (Section 6.5, IA-006, SEC-008) — the immutable record of what actually happened: intent, context provenance, proposed plan, validation outcome, capability decision, approval, execution, verification. Every action produces exactly one.
- **Trace** (IA-006, EXP-005) — the full causal chain rendered end to end: the event plus everything that fed it (which context items, which capability, which agent, which model call, if any).

Observability's job is to make these two primitives queryable and renderable at diagnostic granularity, not to add a parallel logging system beside them. Concretely: every subsystem's `V&V` block's "Observability requirements" line commits that subsystem to emitting into this same event/trace model. A subsystem that logs to its own private, untraceable log is not compliant.

## 39.2 What Must Be Explainable

For any point in time, or any completed action, the system MUST be able to answer:

- **What is running** — which actors (tasks, agents, applications) are alive, in what lifecycle state.
- **Why** — which intent, goal, or schedule triggered it.
- **What capabilities it holds** — the actor's current capability set and its delegation lineage.
- **What resources it accessed** — entities read/written, devices touched, IPC endpoints used.
- **What IPC occurred** — capability-gated sends/receives, with sender/recipient identity.
- **What state changed** — the entity/version/relationship diff, if any.
- **What failed** — the exact validation/authorization/execution/verification stage, if an action did not succeed.
- **Why recovery did or did not succeed** — for any crash/restart, which durable state was recovered, replayed, or lost.

## 39.3 Observability Requirements

- **OBS-001 — Native, not bolted on.** Diagnostic events are emitted by the subsystems that produce them (capability engine, action pipeline, storage, agent runtime), never reconstructed after the fact from ad hoc logs.
- **OBS-002 — Provenance-complete.** Every diagnostic event carries enough provenance to answer §39.2 in full for the action it describes, without cross-referencing an out-of-band log.
- **OBS-003 — Queryable.** The event/trace store MUST support query by actor, capability, entity, time range, and outcome (success/failure/denied) — the same world-model traversal discipline (Section 19) applied to the system's own operational history.
- **OBS-004 — Bounded cost.** Diagnostic instrumentation MUST NOT itself become the performance bottleneck it exists to diagnose; emission is asynchronous or batched off the action pipeline's critical path wherever verification does not require synchronous recording.
- **OBS-005 — Survives what it diagnoses.** Observability data for a crash MUST be durable *before* the crash it will be used to explain — an in-memory-only trace buffer that dies with the process it was instrumenting is not observability.
- **OBS-006 — Rendered, not just recorded.** The Experience Layer (EXP-005) MUST be able to render any trace end to end for a human; recording without a rendering path satisfies auditability but not explainability.

## 39.4 Special Obligations for the AI-Native Surface

Because intelligence enters through agents, and agents act through delegated, revocable capabilities, observability MUST make agent behavior legible in a way legacy process monitoring does not attempt:

- Every model invocation (which model, which prompt/context, which output) is traceable to the action(s) it fed, with the untrusted-output boundary (SEC-003) visible in the trace — a human MUST be able to see exactly where model output stopped being trusted and started being validated.
- Every capability grant, delegation, and revocation involving an agent is individually traceable, never summarized away into an aggregate "agent activity" count.
- World-model and memory mutations attributable to agent-derived inference MUST be labeled as derived (WM-003), and that label MUST be visible wherever the mutation is rendered, not only in the underlying storage schema.

---

# 40. CI/CD & Release Engineering Architecture

Every subsystem's `V&V` block ends in a **Release gates** line. This section is the contract that makes those gates real: how the pyramid (§38) runs continuously, how individual gate results aggregate into a milestone decision, and what MUST be green before Aletheia calls anything "done."

## 40.1 Pipeline Stages Mapped to the Pyramid

Each commit MUST pass, in order, the stages the changed subsystem's layers require:

1. **Build** — the workspace, and once `kernel/` is the target, the bootable image, MUST build clean.
2. **Unit + Integration** (§38.1-38.2) — run on every commit; blocking.
3. **Property-Based** (§38.3) — run on every commit for the subsystems whose properties are named in §38.3; blocking.
4. **Fuzzing** (§38.4) — runs continuously as a background CI job, not gated per-commit on a full campaign; any newly found crash or invariant violation blocks merge until triaged, and the corpus persists and grows across runs.
5. **Bootable-image build + VM boot test** (§38.10) — `scripts/vm-e2e.sh` (or its successor) runs on every commit touching `kernel/` or anything it depends on; blocking. This is the CI gate that made the VM-testing layer real rather than aspirational.
6. **Stress/Load & Chaos** (§38.5-38.6) — run on a schedule, not every commit, given cost; blocking for release candidates.
7. **Performance regression** (§38.13) — every release candidate is benchmarked against the last accepted baseline; a regression beyond a documented threshold blocks release.
8. **Security** (§38.12) — fuzzing-as-a-security-control results and any outstanding independent-audit findings are checked before a release candidate is promoted.
9. **Real-hardware qualification** (§38.11, §41) — gates hardware-bound milestones (P4/P5) specifically, once the automated hardware lab exists; N/A for hosted-only milestones.

## 40.2 Formal-Verification Obligations Tracked, Not Blocking-by-Default

Formal-verification targets (§38.7) are tracked as open obligations against the priority list — capability enforcement, memory isolation, IPC security, scheduler invariants, crypto boundaries — with an explicit owner and status per obligation. They do not block every commit; they are expensive and slow by nature. But an obligation MUST be resolved, or explicitly re-scoped with the reasoning recorded, before the milestone that depends on the property it covers ships.

## 40.3 Milestone Gates

A milestone (P0-P6, §45) is accepted only when every subsystem whose `V&V` block names that milestone in its **Release gates** line has satisfied that gate, and the milestone's own acceptance criteria (§46 for M1) are green. A milestone gate is the aggregate of its subsystems' gates — there is no separate, softer milestone-level bar that lets an individual subsystem's gate slide.

## 40.4 Release Gate Contract

A release candidate MUST NOT be promoted unless:

- Every blocking pipeline stage (§40.1, items 1-3, 5) is green for the current commit.
- No open, untriaged fuzzing-discovered crash or invariant violation exists (§38.4).
- No performance regression beyond the documented threshold exists versus the last accepted baseline (§38.13).
- Every formal-verification obligation the milestone depends on (§40.2) is resolved.
- For hardware-bound milestones, the relevant hardware-lab qualification (§41) has passed for the target device classes.

Any exception MUST be an explicit, recorded, reasoned deviation — never a silent skip.

---

# 41. Hardware Qualification Strategy

M1-P3 are hosted; nothing in them requires real hardware, and their contracts are written specifically so they do not assume anything a real microkernel or scheduler cannot later provide (ADR-010, MK-007, SCH-004). Hardware qualification is the staged path from that hosted-and-VM-tested state to real silicon.

## 41.1 Staged Path

- **Stage 0 (delivered) — VM/Emulator.** The `kernel/` reference boots on QEMU `virt` at EL1 and re-proves the M1 invariants in kernel space (§38.10). This is not P4 itself; it is the bridge artifact that de-risks P4 by proving the capability-enforcement spine survives the move from hosted userspace to a real privilege level, before virtual memory, preemption, or secure boot are added.
- **Stage 1 (P4) — Real microkernel on metal, VM-tested first.** Capability enforcement, secure IPC, task isolation, memory/address spaces, and interrupts move onto real hardware, but the acceptance bar for P4 itself remains VM-tested (§45) before any hardware-lab claim is made. Virtual memory and per-actor address spaces, preemptive scheduling, real cross-address-space IPC with page-granted transfer, and secure boot are the concrete gaps between today's Stage-0 artifact and a P4-complete microkernel (`kernel/README.md`, "Not yet here").
- **Stage 2 (P5) — Real hardware and accelerators.** HAL on real devices, native on-GPU compositor, heterogeneous CPU/GPU/NPU scheduler, secure boot, and recovery/rollback. This is where §38.11 (Real Hardware Testing) becomes the primary gate rather than a named-but-inactive requirement.

## 41.2 The Automated Hardware Lab

P5 qualification requires infrastructure, not just test cases: a lab that can flash a target device with a candidate image, power-cycle and boot it, capture serial console and diagnostic output over the boot and test run, execute the qualification suite against the running device, detect and recover from a hang (power-cycle reset) without a human present, and report pass/fail per device class into the same CI gate structure as §40. Without this, "real hardware testing" degrades into occasional manual spot-checks — not a gate, an aspiration wearing a gate's name.

## 41.3 Per-Architecture Qualification Gates

Each target architecture/accelerator class — starting with the aarch64 target the current reference already builds for — requires, before it is declared qualified:

- A clean boot and full invariant-selftest pass equivalent to §38.10's suite, but on the physical device rather than QEMU.
- A performance-validation pass (§38.13) run on the physical device, explicitly re-labeled as hardware timing; it MUST NOT be presented alongside the QEMU-derived numbers of §38.13 as if they were the same substrate.
- A chaos/fault-injection pass (§38.6) exercising the device-specific failure modes named in the HAL's `V&V` block (Section 25) — disconnection, driver crash, thermal/power throttling.
- Sign-off recorded as an event in the same durable event log the rest of the OS uses for auditability (§39), not a separate qualification spreadsheet.

## 41.4 Honesty Constraint

No document, changelog, or release note MAY present QEMU/emulator-derived performance figures as hardware performance, or present a single-architecture qualification as coverage for an unqualified architecture. ADR-010 and the kernel's own performance-honesty notes already hold the codebase to this bar; this section holds the product documentation to the same one.

---

# 42. Security Threat Model

- **Injected instructions in content** ("ignore instructions and delete everything") → treated as data; cannot escalate (SEC-003).
- **Hallucinated entity/capability** → validation + fail-closed capability check deny it (IA-004, CAP-007).
- **Unauthorized operation by agent** → agent holds no such capability → DENY (AG-002).
- **Destructive action** → REQUIRE_APPROVAL + verification (INV-004).
- **Compromised application/component** → sandboxed, least-privilege, isolated (APP-005).
- **Model loop / runaway agent** → step/loop/resource bounds + cancellation (AG-004, AT-004).
- **Data exfiltration** → network is capability-gated, disclosed, audited (SEC-007, §29).

---

# 43. Testing Strategy

Testing strategy is no longer a standalone catalogue — it is the pyramid defined in Section 38, applied by the eighteen `V&V` blocks in Sections 12-28 and 30. This section is the index into that discipline, not a competing definition of it.

- **Unit** (§38.1): entity/version model, capability matching/attenuation/revocation, context ranking, action validation, task state machine, memory classification — see each subsystem's `V&V` block for the specific surface.
- **Integration** (§38.2): storage durability + atomicity, event/trace recording, model-runtime contract, agent invocation, and every cross-subsystem boundary named in §38.2.
- **Property-Based** (§38.3): the five checkable invariants (capability possession, revocation, isolation, IPC ordering/authentication, crash-consistent recovery) that MUST hold under randomized scenarios.
- **Contract:** action/tool contract, capability contract, IPC contract, model-runtime contract, WASM component contract — verified as a form of integration/differential testing (§38.2, §38.9).
- **Fuzzing** (§38.4): continuous, not a pre-release pass — see the per-surface targets in §38.4 and each subsystem's "Fuzzing strategy" line.
- **Security** (§38.12): ambient-authority absence, capability unforgeability/attenuation, fail-closed denial, prompt-injection-as-data, malformed model output, sensitive-device gating — plus independent auditing and adversarial testing beyond this checklist.
- **Failure / Chaos** (§38.6): interpretation failure mid-flight, agent crash isolation, storage-unavailable, cancellation interrupting a running task, and the AI-runtime-is-never-a-single-point-of-failure scenario.
- **VM / Emulator** (§38.10): the delivered `kernel/` evidence — 11 in-kernel selftests, `scripts/vm-e2e.sh` as the CI gate.
- **End-to-end:** the full M1 pipeline (Section 36) as an automated test, and the 20 acceptance criteria of Section 46.

For what each layer proves and does not prove, and for the formal-verification, real-hardware, and performance-benchmarking layers this list only summarizes, see Section 38 in full.

---

# 44. Product Metrics

Semantic: time-to-find via world model; provenance-query success rate. Intelligence: valid-structured-output rate, hallucinated-operation rate, correction rate. Security: unauthorized-action prevention (must be 100%), capability-bypass rate (must be 0), approval compliance, audit completeness. System: startup, inference latency, action latency, verification latency, resource pressure response.

These system metrics are captured as diagnostic events under the observability model of Section 39, and the performance figures among them follow the benchmarking discipline of Section 38.13 — including its honesty constraint that emulator-derived numbers are never presented as hardware numbers.

---

# 45. Development Phases

Every phase below carries explicit V&V and release-gate obligations, not just a feature checklist. A phase is not "done" when its features are coded; it is done when its pyramid layers (Section 38) are green and its release gate (Section 40) has been satisfied.

## P0 — Design Foundation (this correction)
Rewrite PRD (this document), Software Architecture as OS architecture, Storage/Security/SDK design, ADRs for first-principles choices. **No implementation before P0 design docs agree.**
**V&V obligation:** none testable yet (no code); the obligation is documentary — this PRD, the SAD, and the ADRs must agree before P1 starts, and Section 38's pyramid must be defined (it now is) before any later phase can be graded against it.

## P1 — Hosted System-Core Reference (Milestone M1)
Implement, in a memory-safe language, the M1 vertical slice (Section 35) in userspace with the full test suite. This proves the premise without hardware.
**V&V obligation:** Unit (§38.1), Integration (§38.2), and Property-Based (§38.3) testing for the invariants named in each subsystem's `V&V` block; Runtime Assertions (§38.8) active in diagnostic builds. Delivered: `cargo test` = 18 passed against the 20 acceptance criteria of Section 46. Fuzzing (§38.4), Stress/Load (§38.5), and Chaos (§38.6) are encouraged but not gating for M1 itself — they gate P2 onward as the surfaces they target (WASM components, multi-agent composition, real concurrency) come online.

## P2 — Component & Application Model
WASM/WASI capability-secure component runtime; SDK; application-as-capability model; multi-agent composition.
**V&V obligation:** Fuzzing (§38.4) of the WASM/WASI import surface and the tool-call schema becomes gating here (see §22's `V&V` block); Stress/Load (§38.5) for many concurrent agents/components; Chaos (§38.6) for component crash isolation (APP-005).
**Partially delivered (vertical slice, ADR-014):** a `wasmi`-based capability-secure component runtime exists at `src/component.rs` and is wired into the System Core (`install_component` / `run_component` / `run_installed`). Untrusted WASM reaches the OS only through an explicit host ABI (`read`/`write`/`emit`); there is **no WASI** and therefore no ambient authority. Every host call authorizes through the *same* `CapEngine` as the deterministic pipeline, against the exact capabilities the component was granted (nothing inherited from the launcher — application-as-capability, with `component.run` gating launch itself), and every allowed effect lands in the *same* immutable event log. Execution is fuel-bounded so a runaway component cannot hang the OS, and `read` copies authorized content into a guest-supplied return buffer so a component can consume and compute over the data it may read. Twelve P2 acceptance tests (`tests/component.rs`, two of them property/fuzz) prove the core invariant: **no capability → the component can do nothing; an attenuated grant → exactly that and no more; every effect is traced; a runaway is bounded; launching is gated; a component reads→transforms→writes real data end to end; and a committed effect survives a later fuel-kill (a trap cannot corrupt state).** The Fuzzing layer (§38.4) is **started** against the untrusted host-ABI boundary: the capability fail-closed default and host robustness are asserted over randomized memory arguments. Still open before P2 is complete: the component SDK, multi-agent composition, and the gating Stress/Chaos campaigns (the fuel-bound test pre-stages the isolation gate but does not discharge it).

## P3 — Experience Layer
Native-architecture experience surface: workspaces, dynamic interfaces, semantic navigation/search, control surfaces (still hosted rendering).
**V&V obligation:** the explainability acceptance bar (EXP-005, §46 criterion 20) extends from the hosted M1 surface to the native-architecture surface without regression; visual-regression and accessibility testing (per this org's standing web/frontend testing rules) apply wherever the surface is rendered through a web-hosted shell.

## P4 — Microkernel
Real microkernel (Rust) providing capability enforcement, secure IPC, task isolation, memory/address spaces, interrupts; System Core rehosted on it. Hardware-bound; VM-tested.
**V&V obligation, partially delivered ahead of schedule:** a bootable `no_std` aarch64 microkernel reference now exists at `kernel/` and has already executed the VM-testing layer (§38.10) that this phase requires as its acceptance bar — 11 in-kernel selftests re-proving the M1 invariants in kernel space, all passing, gated by `scripts/vm-e2e.sh`, plus an initial performance-validation pass (§38.13, the substrate-fair IPC/`svc` ratio). This is a bridge artifact: virtual memory and per-actor address spaces, preemptive scheduling, real cross-address-space IPC, and secure boot remain open before P4 is complete (§41.1). Formal Verification (§38.7) of capability enforcement and IPC security is targeted for this phase's horizon. P4 is not accepted until Real Hardware Testing (§38.11) begins against the automated hardware lab (§41.2) for the target architecture.

## P5 — Hardware, Graphics, Scheduler
HAL on real devices; native on-GPU compositor; heterogeneous CPU/GPU/NPU scheduler; secure boot; recovery/rollback.
**V&V obligation:** Real Hardware Testing (§38.11) becomes the primary gate; per-architecture qualification (§41.3) is required before any device class ships; Stress/Load under real GPU/NPU saturation and Chaos with real device disconnection (§38.5-38.6) replace their modeled/hosted equivalents from earlier phases.

## P6 — Compatibility Environment
Optional sandboxed Linux/POSIX personality; filesystem-projection over the semantic store.
**V&V obligation:** confinement testing — a compromised or malicious guest MUST be provably unable to reach the semantic store or devices except through the capability-gated projection (COMPAT-002, COMPAT-003); this is chaos/fault-injection (§38.6) applied specifically to the compatibility boundary, plus a dedicated security audit (§38.12) of that boundary before P6 ships.

Each phase has its own acceptance criteria; later phases MUST preserve all invariants of Section 34, and MUST satisfy the milestone gate contract of Section 40.3-40.4 before promotion.

---

# 46. MVP Acceptance Criteria (M1)

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

These 20 criteria are the M1-scoped acceptance bar; they are deliberately confined to the pyramid layers M1 can actually exercise in a hosted userspace reference — Unit, Integration, and Property-Based testing (§38.1-38.3), with Runtime Assertions (§38.8) active throughout. Fuzzing, Stress/Load, Chaos, Formal Verification, VM/Emulator, Real Hardware, Security Audit, and full Performance Validation are not part of the M1 bar; they are the explicit obligations of P2 through P5 (Section 45). Where evidence for a later-phase layer already exists ahead of schedule — the VM-testing layer, delivered in `kernel/` per Section 38.10 — it is cited there and in Section 45's P4 entry, not folded into this M1 list, so that M1's scope stays honest about what it does and does not certify.

---

# 47. Future Direction

Bare-metal microkernel boot; native on-GPU compositor; NPU-aware heterogeneous scheduling; secure-boot + rollback; distributed Aletheia across trusted devices; richer multi-agent societies; semantic-native application ecosystem. All future work MUST preserve capability security, untrusted intelligence, deterministic control, local-first privacy, and human control — and MUST satisfy the pyramid layers and release gates of Sections 38 and 40 appropriate to whatever phase carries it.

---

# 48. Source-of-Truth Priority

1. This Product Requirements Document (ALETHEIA-PRD-003)
2. Software Architecture Document (OS architecture, to be rewritten to agree)
3. System / Storage / Security / SDK Design
4. API & Component Contracts
5. Implementation Plan
6. Source Code

Any lower-level conflict MUST be surfaced and resolved explicitly. Engineering MUST NOT silently redefine the product or reintroduce legacy primitives. Verification and validation obligations (Section 38) apply at every level of this hierarchy — a passing test suite at the Source Code level does not override a violated invariant defined higher in this list.

---

# 49. North Star

> **Aletheia is not an AI assistant running on an operating system. Aletheia is an operating system whose primitives are entities, capabilities, context, intent, action, memory, and relationships — where intelligence is a native but untrusted collaborator, authority is always explicit, and the human remains in control.**
