# Aletheia
## Software Architecture Document (SAD)

**Document ID:** ALETHEIA-SAD-001  
**Version:** 1.0.0  
**Status:** Architecture Definition / Engineering Source of Truth  
**Product:** Aletheia  
**Related PRD:** ALETHEIA-PRD-001  
**Initial AI Model:** MiniCPM5 1B Q8_0  
**External Development Agent:** Fable — external to Aletheia  
**Initial Deployment Model:** Local-first user-space application on a host operating system  

---

# Document Control

## 1. Purpose

This Software Architecture Document defines the production architecture of Aletheia.

The document translates the Product Requirements Document into:

- system boundaries
- architectural principles
- logical components
- runtime processes
- data flows
- persistence boundaries
- security boundaries
- AI execution boundaries
- tool execution boundaries
- event architecture
- integration architecture
- failure handling
- observability
- deployment
- testing
- scalability
- evolution strategy

This document is the technical source of truth for implementation.

Implementation must follow this document unless an architectural decision is explicitly revised through an Architecture Decision Record (ADR).

---

# 2. Architectural Scope

Aletheia is a local-first AI-native computing environment.

The initial architecture is designed to run on top of an existing host operating system.

```text
┌────────────────────────────────────────────────────────────┐
│                    HOST OPERATING SYSTEM                   │
│                                                            │
│  Filesystem │ Processes │ Windowing │ Audio │ Input │ IPC  │
│                                                            │
│  ┌──────────────────────────────────────────────────────┐  │
│  │                      ALETHEIA                         │  │
│  │                                                      │  │
│  │  User Interface                                      │  │
│  │       │                                              │  │
│  │  Application / API Gateway                          │  │
│  │       │                                              │  │
│  │  Orchestration Layer                                │  │
│  │       │                                              │  │
│  │  ┌────┼───────────────┬─────────────────────┐       │  │
│  │  │    │               │                     │       │  │
│  │  AI   │   Project     │     Event           │       │  │
│  │ Runtime│  Intelligence │     System          │       │  │
│  │  │    │               │                     │       │  │
│  │  └────┼───────────────┴─────────────────────┘       │  │
│  │       │                                              │  │
│  │  Tool / Capability / Task / Workflow Runtime         │  │
│  │       │                                              │  │
│  │  Integration Providers + OS Adapters                 │  │
│  │       │                                              │  │
│  │  Persistence / Indexing / Observability              │  │
│  │                                                      │  │
│  │  MiniCPM5 1B Q8_0                                   │  │
│  └──────────────────────────────────────────────────────┘  │
└────────────────────────────────────────────────────────────┘
```

---

# 3. Critical Terminology

## 3.1 Aletheia

Aletheia is the product.

It is the AI-native computing environment.

---

## 3.2 Fable

Fable is an external AI coding model used by the developer to build Aletheia.

Fable is not:

- a runtime component
- an Aletheia service
- an AI model shipped with Aletheia
- a resident model
- an internal agent

The architecture must never include Fable as a production dependency.

```text
Developer
   │
   ▼
 Fable
   │
   ▼
Builds Aletheia
   │
   ▼
Aletheia Runtime
   ├── MiniCPM5 1B Q8_0
   ├── AI Runtime
   ├── Orchestrator
   ├── Project Intelligence
   └── Tool Execution
```

---

## 3.3 MiniCPM5 1B Q8_0

MiniCPM5 1B Q8_0 is the initial resident model.

It is an untrusted probabilistic inference component.

It must not be treated as:

- a system authority
- a permission authority
- a source of truth
- an execution engine
- a filesystem authority

The model produces interpretations.

Aletheia validates and executes them.

---

# 4. Architectural Vision

Aletheia must provide a higher-level semantic layer above the traditional application-and-file operating model.

Traditional model:

```text
Application
    ↓
Files
    ↓
Folders
```

Aletheia model:

```text
User
    ↓
Project
    ↓
Context
    ↓
Sessions
    ↓
Applications
    ↓
Assets
    ↓
Events
    ↓
Tasks
    ↓
Workflows
    ↓
Outputs
```

The architecture must preserve the existing operating system rather than attempt to replace it in the initial product.

Aletheia becomes an intelligence and coordination layer.

---

# 5. Architectural Principles

## AP-001 — System of Record Is Deterministic

The AI must never be the system of record.

System truth must come from:

- database state
- filesystem state
- operating system state
- provider state
- verified tool results

---

## AP-002 — AI Is Untrusted

All model output must be treated as untrusted input.

```text
Model Output
    ↓
Parse
    ↓
Validate
    ↓
Authorize
    ↓
Execute
    ↓
Verify
```

Never:

```text
Model Output
    ↓
Direct System Execution
```

---

## AP-003 — Explicit Boundaries

Every major subsystem must have a clear boundary.

The architecture must avoid:

```text
UI → Model → Shell
```

The architecture should use:

```text
UI
 ↓
Application API
 ↓
Orchestrator
 ↓
Intent / Action Validation
 ↓
Capability Evaluation
 ↓
Tool Registry
 ↓
Deterministic Executor
 ↓
Verification
 ↓
Event Store
```

---

## AP-004 — Local First

Core functionality must operate locally.

The architecture must not require cloud AI.

---

## AP-005 — Least Privilege

Every capability must be:

- explicit
- scoped
- inspectable
- revocable

---

## AP-006 — Fail Closed

Uncertainty must not automatically produce broader access.

---

## AP-007 — Events Are First-Class

Important system transitions must produce events.

---

## AP-008 — Durable State

Important state must survive:

- process restart
- model restart
- integration failure
- temporary hardware failure

---

## AP-009 — Idempotent Operations

Operations that may be retried should be designed to avoid duplicate effects.

---

## AP-010 — Provider Isolation

A broken application integration must not crash the core runtime.

---

## AP-011 — Context Is Deliberately Assembled

The entire database must never be blindly dumped into the model.

---

## AP-012 — Explainable Actions

Aletheia must be able to answer:

> Why did this action happen?

---

# 6. High-Level Architecture

```text
┌───────────────────────────────────────────────────────────────┐
│                         USER INTERFACE                        │
│                                                               │
│  Dashboard │ AI Chat │ Projects │ Activity │ Tasks │ Settings │
└───────────────────────────────┬───────────────────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────┐
│                    APPLICATION API LAYER                      │
│                                                               │
│  Request Authentication / Session / API Validation            │
│  Command Routing / Query Routing / DTO Mapping                 │
└───────────────────────────────┬───────────────────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────┐
│                       APPLICATION CORE                        │
│                                                               │
│  Project Service │ Context Service │ Task Service              │
│  Workflow Service│ Memory Service │ Search Service              │
│  Session Service │ Activity Service                            │
└─────────────┬─────────────────┬─────────────────┬─────────────┘
              │                 │                 │
              ▼                 ▼                 ▼
┌───────────────────┐ ┌───────────────────┐ ┌───────────────────┐
│ AI ORCHESTRATION  │ │ EVENT SYSTEM      │ │ TOOL SYSTEM       │
│                   │ │                   │ │                   │
│ Context Assembly  │ │ Event Bus         │ │ Registry          │
│ Prompt Builder    │ │ Event Store       │ │ Validation        │
│ Model Runtime     │ │ Projections       │ │ Authorization     │
│ Output Parser     │ │ Replay            │ │ Execution         │
└─────────┬─────────┘ └─────────┬─────────┘ └─────────┬─────────┘
          │                     │                     │
          ▼                     ▼                     ▼
┌───────────────────┐ ┌───────────────────┐ ┌───────────────────┐
│ MINI CPM5 1B      │ │ PERSISTENCE       │ │ CAPABILITY ENGINE │
│                   │ │                   │ │                   │
│ Local Inference  │ │ Relational DB     │ │ Policy Evaluation │
│ Streaming        │ │ Event Store       │ │ Scope Matching    │
│ Cancellation      │ │ Search Index      │ │ Approval Flow     │
└───────────────────┘ └───────────────────┘ └───────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────┐
│                       ADAPTER LAYER                           │
│                                                               │
│ Filesystem │ Process │ Application Providers │ OS Services    │
└───────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────┐
│                     HOST OPERATING SYSTEM                     │
└───────────────────────────────────────────────────────────────┘
```

---

# 7. Architectural Style

Aletheia should use a **modular monolith with explicit internal boundaries** for the initial implementation.

This is preferred over immediately splitting into distributed microservices.

## 7.1 Why Modular Monolith

Aletheia is initially:

- local
- single-user
- hardware-constrained
- latency-sensitive
- tightly integrated with the local OS

A distributed architecture would introduce unnecessary:

- network overhead
- operational complexity
- serialization boundaries
- failure modes
- deployment complexity

---

## 7.2 Internal Module Boundaries

The application must still be modular.

Recommended modules:

```text
core/
├── domain/
├── application/
├── infrastructure/
├── ai/
├── projects/
├── assets/
├── events/
├── sessions/
├── tasks/
├── workflows/
├── memory/
├── search/
├── tools/
├── capabilities/
├── integrations/
├── observability/
└── api/
```

---

## 7.3 Future Extraction

The following components may eventually become separate processes:

- AI Runtime
- Indexing Worker
- Workflow Worker
- Integration Worker
- Search Service

The initial interfaces must make extraction possible without redesigning the entire product.

---

# 8. Process Architecture

Aletheia should be composed of multiple logical processes where process isolation improves reliability.

## 8.1 Recommended Initial Processes

```text
┌──────────────────────────────────────────┐
│ Aletheia UI Process                      │
│                                          │
│ Rendering / Interaction                  │
└───────────────────┬──────────────────────┘
                    │ IPC / Local API
                    ▼
┌──────────────────────────────────────────┐
│ Aletheia Core Process                    │
│                                          │
│ Domain / Orchestration / API             │
└───────────────┬──────────────┬───────────┘
                │              │
                ▼              ▼
┌──────────────────────┐ ┌─────────────────┐
│ AI Runtime Process   │ │ Worker Process  │
│                      │ │                 │
│ MiniCPM5 Inference  │ │ Indexing        │
│ Model Lifecycle     │ │ Watchers        │
│ Cancellation        │ │ Workflows       │
└──────────────────────┘ └─────────────────┘
```

---

## 8.2 UI Process

Responsibilities:

- rendering
- user input
- presentation
- local UI state

The UI must not:

- directly access the database
- directly invoke the model
- directly execute filesystem operations

---

## 8.3 Core Process

Responsibilities:

- domain logic
- application orchestration
- API
- permission decisions
- tool lifecycle
- event coordination

This is the primary authority.

---

## 8.4 AI Runtime Process

Responsibilities:

- model loading
- inference
- streaming
- cancellation
- model health
- model lifecycle

The AI runtime must not directly perform arbitrary system operations.

---

## 8.5 Worker Process

Responsibilities:

- filesystem indexing
- background scanning
- event ingestion
- long-running workflows
- expensive processing

---

# 9. Internal Layering

Every module should follow a layered structure where practical.

```text
┌─────────────────────────────┐
│ Interface / API             │
├─────────────────────────────┤
│ Application Services        │
├─────────────────────────────┤
│ Domain                      │
├─────────────────────────────┤
│ Ports / Interfaces          │
├─────────────────────────────┤
│ Infrastructure Adapters     │
└─────────────────────────────┘
```

## 9.1 Interface Layer

Handles:

- HTTP
- IPC
- UI commands
- DTOs
- validation

---

## 9.2 Application Layer

Handles:

- use cases
- orchestration
- transaction boundaries

---

## 9.3 Domain Layer

Contains:

- entities
- value objects
- domain rules
- state transitions

The domain must not depend on:

- UI frameworks
- model SDKs
- filesystem libraries
- database drivers

---

## 9.4 Port Layer

Defines interfaces such as:

```text
ModelProvider
EventPublisher
AssetRepository
ProjectRepository
ToolExecutor
CapabilityEvaluator
FilesystemAdapter
ProcessAdapter
```

---

## 9.5 Infrastructure Layer

Implements:

- database access
- OS APIs
- model runtime
- filesystem watchers
- application integrations

---

# 10. Core Domain Architecture

## 10.1 Domain Relationship

```text
User
 │
 ├── owns
 ▼
Project
 │
 ├── has ───────────► ProjectState
 │
 ├── contains ──────► Asset
 │
 ├── contains ──────► Session
 │
 ├── generates ─────► Event
 │
 ├── contains ──────► Task
 │
 ├── contains ──────► Workflow
 │
 └── produces ───────► Output
```

---

## 10.2 Project Aggregate

The Project is the primary aggregate root.

The Project aggregate owns:

- project identity
- lifecycle
- project configuration
- project associations

It should not necessarily directly own every Asset entity in memory.

Large collections must be queried separately.

---

## 10.3 Entity Identity

All persistent entities require stable IDs.

IDs must not depend solely on:

- filename
- filesystem path
- display name

Recommended:

```text
UUID / ULID
```

---

# 11. Project Architecture

## 11.1 Project Service

Responsibilities:

- create project
- discover project
- open project
- close project
- rename project
- archive project
- retrieve project state

---

## 11.2 Project State

Project state is derived from:

```text
Persisted Project Data
        +
Events
        +
Observed System State
        +
Provider State
```

The system must distinguish:

```text
Persisted Fact
Observed Fact
Derived State
AI Inference
```

---

## 11.3 Project Lifecycle

```text
DISCOVERED
    ↓
REGISTERED
    ↓
ACTIVE
    ↓
INACTIVE
    ↓
ARCHIVED
```

---

# 12. Asset Architecture

## 12.1 Asset Identity

A file path is not sufficient identity.

Aletheia should use:

```text
Asset ID
    +
Current Location
    +
Content Fingerprint
    +
Metadata
```

---

## 12.2 Asset Lifecycle

```text
DISCOVERED
    ↓
REGISTERED
    ↓
MODIFIED
    ↓
MOVED / RENAMED
    ↓
ARCHIVED
    ↓
DELETED / MISSING
```

---

## 12.3 Asset Detection

The asset pipeline:

```text
OS Event
    ↓
Event Normalizer
    ↓
Path Resolution
    ↓
Project Association
    ↓
Asset Identity Resolution
    ↓
Metadata Extraction
    ↓
Persistence
    ↓
Domain Event
```

---

# 13. Event Architecture

Aletheia uses a hybrid event architecture.

## 13.1 Event Types

### Domain Events

Represent meaningful business transitions.

```text
ProjectCreated
AssetRegistered
TaskCompleted
WorkflowFailed
```

### Integration Events

Represent external observations.

```text
FileSystemFileChanged
ApplicationStarted
ProcessExited
```

### Audit Events

Represent security-sensitive operations.

```text
CapabilityGranted
CapabilityDenied
FileDeleted
ToolExecuted
```

---

## 13.2 Event Flow

```text
External Source
      ↓
Adapter
      ↓
Normalizer
      ↓
Event Validation
      ↓
Event Store
      ↓
Projection
      ↓
Application State
```

---

## 13.3 Event Store

The event store should preserve:

```text
event_id
event_type
aggregate_type
aggregate_id
occurred_at
recorded_at
source
actor
payload
schema_version
correlation_id
causation_id
```

---

## 13.4 Correlation

Every AI task should have:

```text
correlation_id
```

This allows tracing:

```text
User Request
    ↓
AI Request
    ↓
Tool Call
    ↓
Permission Decision
    ↓
Execution
    ↓
Verification
```

---

# 14. AI Architecture

## 14.1 AI System Boundary

```text
┌───────────────────────────────────────────┐
│             ALETHEIA CORE                 │
│                                           │
│  Request                                  │
│    ↓                                      │
│  Context Assembly                          │
│    ↓                                      │
│  Prompt Construction                       │
└──────────────────────┬────────────────────┘
                       │
                       ▼
┌───────────────────────────────────────────┐
│             AI RUNTIME PROCESS             │
│                                           │
│  MiniCPM5 1B Q8_0                         │
│                                           │
│  Text In → Tokens → Inference → Text Out  │
└──────────────────────┬────────────────────┘
                       │
                       ▼
┌───────────────────────────────────────────┐
│             ALETHEIA CORE                 │
│                                           │
│  Parse → Validate → Authorize → Execute   │
└───────────────────────────────────────────┘
```

---

## 14.2 AI Runtime Contract

The AI Runtime should expose:

```text
loadModel()
unloadModel()
health()
generate()
stream()
cancel()
```

Conceptual interface:

```typescript
interface ModelRuntime {
  health(): Promise<ModelHealth>;
  generate(request: GenerationRequest): Promise<GenerationResult>;
  stream(request: GenerationRequest): AsyncIterable<GenerationEvent>;
  cancel(requestId: string): Promise<void>;
}
```

---

## 14.3 AI Request

A request should contain:

```text
request_id
conversation_id
task_id
project_id
model_id
messages
context
generation_parameters
timeout
```

---

## 14.4 AI Response

A response should contain:

```text
request_id
model_id
text
structured_output
finish_reason
usage
latency
```

---

## 14.5 Model Failure

Possible failures:

```text
MODEL_NOT_LOADED
MODEL_CRASHED
TIMEOUT
CANCELLED
OUT_OF_MEMORY
INVALID_OUTPUT
RUNTIME_ERROR
```

The core must handle all explicitly.

---

# 15. Context Architecture

Context assembly is a dedicated subsystem.

```text
User Request
      │
      ▼
Intent Classification
      │
      ▼
Context Retrieval
      ├── Current Project
      ├── Active Session
      ├── Recent Events
      ├── Relevant Assets
      ├── Memory
      ├── Available Tools
      └── Capabilities
      │
      ▼
Context Ranking
      │
      ▼
Context Budgeting
      │
      ▼
Prompt Assembly
      │
      ▼
Model
```

---

## 15.1 Context Sources

```text
Current UI Context
Project State
Recent Events
Search Results
Memory
Task State
Workflow State
Application State
Tool Registry
Capability State
```

---

## 15.2 Context Priority

Recommended order:

1. Current user request
2. Current project
3. Current task
4. Directly relevant system facts
5. Relevant recent events
6. Relevant assets
7. Relevant memory
8. General project context

---

## 15.3 Context Provenance

Each context item should include:

```text
source_type
source_id
retrieved_at
relevance
confidence
```

---

# 16. Intent and Action Architecture

The model should not directly choose arbitrary code.

The model produces an intent or action proposal.

```text
Natural Language
      ↓
Model
      ↓
Intent
      ↓
Action Planner
      ↓
Tool Call
```

---

## 16.1 Intent

Example:

```json
{
  "intent": "find_asset",
  "query": "latest exported mix"
}
```

---

## 16.2 Action Plan

```json
{
  "steps": [
    {
      "tool": "asset.search",
      "arguments": {
        "query": "latest exported mix"
      }
    }
  ]
}
```

---

## 16.3 Action Validation

Validation stages:

```text
Syntax
  ↓
Schema
  ↓
Semantic
  ↓
Tool Existence
  ↓
Capability
  ↓
Policy
  ↓
Approval
  ↓
Execution
```

---

# 17. Tool Architecture

## 17.1 Tool Registry

The Tool Registry is the authoritative list of executable capabilities.

```text
Tool
 ├── identity
 ├── schema
 ├── risk
 ├── capabilities
 ├── executor
 └── verifier
```

---

## 17.2 Tool Lifecycle

```text
REQUESTED
    ↓
VALIDATING
    ↓
AUTHORIZED
    ↓
WAITING_APPROVAL
    ↓
EXECUTING
    ↓
VERIFYING
    ↓
COMPLETED
```

Failure branches:

```text
DENIED
FAILED
CANCELLED
```

---

## 17.3 Tool Executor

The executor must:

1. validate arguments
2. resolve resources
3. check capabilities
4. perform operation
5. verify result
6. emit event
7. return structured result

---

# 18. Capability Architecture

## 18.1 Capability Model

A capability is:

```text
Subject
    +
Action
    +
Resource Scope
    +
Constraints
```

Example:

```text
Subject: Aletheia AI
Action: filesystem.read
Scope: /Projects/MyGame/**
Constraints: local-only
```

---

## 18.2 Capability Decision

```text
Request
   ↓
Normalize
   ↓
Match Subject
   ↓
Match Action
   ↓
Match Resource
   ↓
Evaluate Constraints
   ↓
ALLOW / DENY / REQUIRE_APPROVAL
```

---

## 18.3 Decision Outcomes

```text
ALLOW
DENY
REQUIRE_APPROVAL
```

---

## 18.4 Capability Storage

Capabilities must be persistent.

Every decision should be auditable.

---

# 19. Approval Architecture

Sensitive actions should use a two-phase model.

```text
Propose
   ↓
Approval Required
   ↓
User Decision
   ├── Approve
   ├── Deny
   └── Timeout
   ↓
Execute
```

Approval must be bound to:

- exact action
- exact resource
- exact scope
- expiration
- task ID

Approval must not automatically authorize unrelated future operations.

---

# 20. Task Architecture

The Task System manages execution.

## 20.1 Task State Machine

```text
CREATED
   ↓
QUEUED
   ↓
RUNNING
   ├──────────────► WAITING_FOR_APPROVAL
   │                       │
   │                       ▼
   │                    APPROVED
   │                       │
   ▼                       ▼
WAITING ◄────────────── RUNNING
   │
   ▼
COMPLETED
```

Failure states:

```text
FAILED
CANCELLED
```

---

## 20.2 Task Ownership

Each task belongs to:

- user
- project
- session
- correlation context

---

## 20.3 Task Persistence

Task state must be persisted.

A process restart must not silently erase task state.

---

# 21. Workflow Architecture

A Workflow is a durable multi-step execution graph.

```text
Workflow
   ↓
Step 1
   ↓
Step 2
   ↓
Approval
   ↓
Step 3
   ↓
Verification
   ↓
Complete
```

---

## 21.1 Workflow Types

### Deterministic

Known steps.

### AI-Assisted

AI chooses among approved tools.

### Hybrid

Deterministic steps with AI decision points.

---

## 21.2 Workflow Safety

The workflow engine must:

- enforce capabilities
- enforce step limits
- detect loops
- support cancellation
- persist state
- support recovery

---

# 22. Memory Architecture

Memory is not the same as raw event history.

```text
Raw Events
    ↓
Extraction
    ↓
Candidate Memory
    ↓
Validation / Classification
    ↓
Persistent Memory
```

Memory types:

```text
Observed Fact
User Preference
Project Decision
System-Derived Relationship
AI Summary
```

Every memory item should have provenance.

---

# 23. Search Architecture

Aletheia should use layered search.

```text
User Query
    ↓
Query Normalization
    ↓
Structured Search
    ├── Metadata
    ├── Time
    ├── Project
    ├── Application
    └── Asset Type
    ↓
Optional Semantic Search
    ↓
Ranking
    ↓
Results
```

The initial architecture must not require expensive semantic indexing for every operation.

Structured search should remain the baseline.

---

# 24. Filesystem Architecture

## 24.1 Watcher

The watcher observes configured locations.

It must:

- normalize platform-specific events
- debounce duplicates
- detect overflow
- recover from watcher failure

---

## 24.2 Scanner

The scanner performs:

- initial indexing
- reconciliation
- recovery after missed events

Architecture:

```text
Watcher
   │
   ├── Normal Events
   │
   ▼
Event Processor

Scanner
   │
   ├── Startup
   ├── Recovery
   └── Reconciliation
```

---

## 24.3 Eventual Consistency

Filesystem observation is inherently asynchronous.

The system must expose states such as:

```text
OBSERVED
PROCESSING
INDEXED
STALE
MISSING
```

---

# 25. Application Integration Architecture

## 25.1 Provider Interface

Providers should implement:

```text
detect()
health()
getApplicationInfo()
getCurrentContext()
getEntities()
execute()
subscribe()
```

---

## 25.2 Provider Isolation

Each provider must be isolated.

A provider failure:

```text
Provider Failure
    ↓
Provider Marked Unhealthy
    ↓
Core Continues
    ↓
User Notified
```

---

## 25.3 Generic Fallback

When no provider exists:

```text
Process Detection
    +
Filesystem Observation
    +
User Configuration
```

can still provide useful context.

---

# 26. Session Architecture

A Session represents continuous activity.

```text
Session Started
      ↓
Application Activity
      ↓
File Activity
      ↓
User Activity
      ↓
Session Ended
```

Sessions should be created by:

- explicit user action
- application detection
- workflow execution
- configured automation

---

# 27. Persistence Architecture

Aletheia should use multiple persistence categories.

## 27.1 Relational State

Stores:

- projects
- assets
- sessions
- tasks
- workflows
- capabilities
- memories

---

## 27.2 Event Store

Stores immutable events.

---

## 27.3 Search Index

Stores optimized search representations.

---

## 27.4 Model Storage

Stores model files separately from application state.

---

## 27.5 Cache

Caches:

- recent context
- search results
- provider state
- model metadata

Cache data must be disposable.

---

# 28. Transaction Boundaries

Transactions should protect:

```text
Domain State
    +
Event Recording
```

Where possible:

```text
State Change
    +
Event
```

must be committed consistently.

---

# 29. Concurrency Architecture

Aletheia will have concurrent operations:

- UI requests
- filesystem events
- AI inference
- indexing
- workflows
- application events

The architecture must prevent:

- duplicate task execution
- stale state overwrites
- event races
- concurrent destructive actions

---

## 29.1 Locking

Use locks only where required.

Prefer:

- optimistic concurrency
- idempotency
- version checks

---

# 30. Caching

Cache layers may include:

```text
UI Cache
Context Cache
Search Cache
Provider State Cache
Model Metadata Cache
```

Caches must have:

- TTL
- invalidation strategy
- size limits

No cache should be treated as authoritative state.

---

# 31. Observability Architecture

## 31.1 Logs

Structured logs must include:

```text
timestamp
level
service
module
message
correlation_id
task_id
project_id
error_code
```

---

## 31.2 Traces

A single user request should produce:

```text
Trace
 ├── API Request
 ├── Context Assembly
 ├── AI Request
 ├── Output Parsing
 ├── Tool Validation
 ├── Capability Evaluation
 ├── Approval
 ├── Execution
 └── Verification
```

---

## 31.3 Metrics

Required metrics:

```text
ai_request_latency
ai_failure_count
tool_execution_count
tool_failure_count
capability_denial_count
approval_wait_time
event_processing_latency
indexing_latency
task_completion_rate
```

---

# 32. Error Architecture

Errors must be structured.

```text
{
  code,
  message,
  category,
  retryable,
  user_action,
  correlation_id
}
```

Categories:

```text
VALIDATION
AUTHORIZATION
NOT_FOUND
CONFLICT
TIMEOUT
RESOURCE
INTEGRATION
MODEL
PERSISTENCE
INTERNAL
```

---

# 33. Recovery Architecture

## 33.1 AI Recovery

```text
Timeout
   ↓
Cancel
   ↓
Mark Failed
   ↓
Preserve Task
   ↓
User Can Retry
```

---

## 33.2 Worker Recovery

Workers should persist checkpoints where practical.

---

## 33.3 Event Recovery

If an event processor fails:

```text
Event
   ↓
Processing Failure
   ↓
Retry / Dead Letter
```

---

## 33.4 Dead Letter Queue

Events that repeatedly fail should be isolated rather than blocking all processing.

---

# 34. Security Architecture

Security boundaries:

```text
┌──────────────────────────────┐
│ Untrusted External Content   │
│ Files / App Data / Model     │
└───────────────┬──────────────┘
                │
                ▼
┌──────────────────────────────┐
│ Validation Boundary          │
└───────────────┬──────────────┘
                │
                ▼
┌──────────────────────────────┐
│ Capability Boundary          │
└───────────────┬──────────────┘
                │
                ▼
┌──────────────────────────────┐
│ Deterministic Executor       │
└──────────────────────────────┘
```

---

# 35. Threat Model

Threats include:

- prompt injection
- malicious project files
- path traversal
- unauthorized tool calls
- model hallucinations
- malicious integrations
- compromised plugins
- corrupted state
- accidental destructive actions

---

# 36. Prompt Injection Defense

The architecture must separate:

```text
System Instructions
    >
Developer Rules
    >
Application Policy
    >
User Request
    >
Retrieved Data
    >
Untrusted Content
```

Retrieved content must never automatically become instruction authority.

---

# 37. Privacy Architecture

The default data flow:

```text
User Data
   ↓
Local Aletheia
   ↓
Local Database
   ↓
Local Model
```

No external transmission should occur without explicit configuration.

---

# 38. Network Architecture

The core system should not require external network access.

Optional network operations must be:

- explicit
- capability-controlled
- auditable

---

# 39. Deployment Architecture

## 39.1 Local Installation

Recommended components:

```text
Aletheia Launcher
    ↓
Core Process
    ├── UI
    ├── AI Runtime
    └── Workers
```

---

## 39.2 Data Directory

The data directory should contain:

```text
aletheia/
├── database/
├── events/
├── indexes/
├── models/
├── logs/
├── cache/
├── workflows/
└── configuration/
```

---

## 39.3 Configuration

Configuration must be:

- versioned
- validated
- migratable

---

# 40. Model Resource Management

MiniCPM5 1B is the only initial resident AI model.

The architecture must treat model resources as constrained.

The AI runtime should support:

- lazy loading
- model health checks
- cancellation
- unloading
- memory reporting
- inference limits

The model must not unnecessarily remain active when configured otherwise.

---

# 41. Performance Architecture

Performance priorities:

1. UI responsiveness
2. system stability
3. user workflow latency
4. AI inference latency
5. background indexing throughput

Background work must not monopolize resources.

---

# 42. Gaming Performance

When gaming:

- indexing should be throttled
- background AI should be controlled
- CPU usage should be bounded
- disk activity should be minimized
- optional GPU usage should be configurable

Aletheia must prioritize the active application when configured for gaming mode.

---

# 43. Resource Scheduler

The scheduler may classify workloads:

```text
INTERACTIVE
AI_INFERENCE
BACKGROUND_INDEXING
WORKFLOW
MAINTENANCE
```

Priority:

```text
INTERACTIVE
    >
AI_INFERENCE
    >
WORKFLOW
    >
INDEXING
    >
MAINTENANCE
```

The exact policy should be configurable.

---

# 44. API Architecture

The API should separate:

```text
Commands
Queries
Events
Streams
```

## Commands

Change state.

```text
CreateProject
StartTask
ApproveAction
```

## Queries

Read state.

```text
GetProject
SearchAssets
GetTask
```

## Events

Represent facts.

```text
ProjectCreated
TaskCompleted
```

## Streams

Represent ongoing data.

```text
AI Token Stream
Task Progress Stream
```

---

# 45. IPC Architecture

Local processes should communicate through authenticated local IPC.

Requirements:

- process identity
- request validation
- request IDs
- timeouts
- cancellation
- structured errors

The UI must not be able to bypass core authorization.

---

# 46. Versioning

The following require versions:

- database schema
- event schema
- tool schema
- workflow schema
- configuration schema
- API contracts

---

# 47. Database Migration

Migrations must be:

- ordered
- transactional where possible
- versioned
- testable
- reversible where practical

---

# 48. Event Schema Evolution

Events must include:

```text
schema_version
```

Old events must remain readable.

Migration strategies:

- upcasting
- compatibility readers
- explicit migration

---

# 49. Testing Architecture

## 49.1 Unit Tests

Test:

- domain rules
- state machines
- validators
- capability matching
- context ranking

---

## 49.2 Integration Tests

Test:

- database
- model runtime
- event system
- filesystem adapter
- tool executor

---

## 49.3 Contract Tests

Test:

- AI runtime contract
- provider contract
- tool contract
- IPC contract

---

## 49.4 Security Tests

Test:

- path traversal
- capability bypass
- prompt injection
- malformed output
- unauthorized operations

---

## 49.5 Failure Tests

Test:

- model crash
- worker crash
- database unavailable
- filesystem event overflow
- provider crash
- partial tool failure

---

## 49.6 End-to-End Test

Required flow:

```text
Create Project
    ↓
Observe File
    ↓
Persist Asset
    ↓
Create Event
    ↓
User Requests Search
    ↓
Assemble Context
    ↓
Invoke Model
    ↓
Parse Action
    ↓
Validate
    ↓
Authorize
    ↓
Execute
    ↓
Verify
    ↓
Persist Result
```

---

# 50. Architecture Decision Records

The project must maintain ADRs for major decisions.

Required initial ADRs:

- ADR-001 Modular Monolith
- ADR-002 Local-First AI
- ADR-003 MiniCPM5 1B Initial Model
- ADR-004 AI Cannot Directly Execute System Operations
- ADR-005 Capability-Based Authorization
- ADR-006 Event-Based Activity Model
- ADR-007 Project as Primary Aggregate
- ADR-008 Provider-Based Integrations
- ADR-009 Local IPC
- ADR-010 Structured Tool Execution

---

# 51. Recommended Repository Architecture

```text
aletheia/
├── apps/
│   ├── desktop/
│   ├── core/
│   ├── ai-runtime/
│   └── worker/
│
├── packages/
│   ├── domain/
│   ├── contracts/
│   ├── api/
│   ├── events/
│   ├── tools/
│   ├── capabilities/
│   ├── context/
│   ├── tasks/
│   ├── workflows/
│   ├── memory/
│   ├── search/
│   ├── integrations/
│   └── observability/
│
├── database/
│   ├── migrations/
│   └── seeds/
│
├── tests/
│   ├── unit/
│   ├── integration/
│   ├── contract/
│   ├── security/
│   └── e2e/
│
├── docs/
│   ├── adr/
│   ├── architecture/
│   └── api/
│
└── scripts/
```

---

# 52. Example Request Lifecycle

User:

> Find the latest export of this project.

```text
1. UI receives request
        ↓
2. Core creates request context
        ↓
3. Current Project is identified
        ↓
4. Context Service retrieves relevant state
        ↓
5. Tool Registry exposes asset.search
        ↓
6. Model receives bounded context
        ↓
7. Model produces structured intent
        ↓
8. Parser validates schema
        ↓
9. Planner creates action
        ↓
10. Capability Engine evaluates
        ↓
11. Tool Executor runs search
        ↓
12. Result is verified
        ↓
13. Event is recorded
        ↓
14. Result is returned to user
```

---

# 53. Example Destructive Request Lifecycle

User:

> Delete all duplicate exports.

```text
User Request
    ↓
AI Interpretation
    ↓
Candidate Discovery
    ↓
Duplicate Analysis
    ↓
Plan Generated
    ↓
User Review
    ↓
Explicit Approval
    ↓
Capability Evaluation
    ↓
Deletion Execution
    ↓
Verification
    ↓
Audit Event
```

The AI must not directly delete files merely because the user used natural language.

The system must convert the request into a controlled plan.

---

# 54. Architecture Invariants

The following invariants must always hold:

## INV-001

The AI cannot directly execute arbitrary operating-system commands.

## INV-002

All executable AI actions pass through the Tool Registry.

## INV-003

All tool actions pass through capability evaluation.

## INV-004

Destructive operations require appropriate authorization.

## INV-005

System facts are not derived solely from model output.

## INV-006

A failed AI inference cannot corrupt core state.

## INV-007

A failed integration cannot crash the core system.

## INV-008

Important operations are auditable.

## INV-009

User data remains local by default.

## INV-010

The UI cannot bypass core authorization.

---

# 55. MVP Architecture Boundary

The MVP should implement:

```text
Desktop UI
    ↓
Core Runtime
    ├── Project Service
    ├── Asset Service
    ├── Event Service
    ├── Task Service
    ├── Context Service
    ├── AI Orchestrator
    ├── Tool Registry
    ├── Capability Engine
    └── Audit Service
        ↓
    Persistence
        ↓
    OS Adapters
```

The initial system does not need:

- distributed microservices
- cloud orchestration
- multi-user tenancy
- complex cluster scheduling
- frontier model routing
- massive distributed vector infrastructure

The architecture must remain extensible without implementing unnecessary complexity prematurely.

---

# 56. Future Evolution

Aletheia may evolve toward:

```text
Current
   ↓
Modular Monolith
   ↓
Multi-Process Runtime
   ↓
Optional Service Extraction
   ↓
Deeper OS Integration
   ↓
AI-Native Computing Platform
```

Potential future components:

- dedicated semantic index
- multimodal model runtime
- multiple local models
- model routing
- hardware-aware inference scheduler
- distributed local agents
- deeper desktop shell integration
- native OS image
- hardware acceleration services

Future architecture must preserve the original principles.

---

# 57. Architecture Acceptance Criteria

The architecture is acceptable when:

1. The AI is isolated from direct arbitrary system execution.
2. Every executable operation passes through a registered tool.
3. Tools are capability-controlled.
4. Sensitive actions support approval.
5. Projects are first-class domain entities.
6. Events can be persisted and traced.
7. The model can be replaced without rewriting the core.
8. The UI cannot bypass authorization.
9. Provider failures are isolated.
10. Model failures are isolated.
11. Important state survives process restarts.
12. The system supports local-only operation.
13. Background work can be throttled.
14. Task execution is observable.
15. Tool execution is verifiable.
16. AI context is bounded and traceable.
17. Untrusted content cannot override system policy.
18. The architecture can evolve beyond the initial model.
19. The architecture can support gaming and creative workflows.
20. A complete user request can be traced from input to verified result.

---

# 58. Final Architecture

The final conceptual architecture is:

```text
                         USER
                          │
                          ▼
                    ┌───────────┐
                    │    UI     │
                    └─────┬─────┘
                          │
                          ▼
                 ┌─────────────────┐
                 │  APPLICATION API │
                 └────────┬────────┘
                          │
                          ▼
                 ┌─────────────────┐
                 │  CORE RUNTIME   │
                 │                 │
                 │  Projects       │
                 │  Assets         │
                 │  Sessions       │
                 │  Tasks          │
                 │  Workflows      │
                 │  Memory         │
                 └───────┬─────────┘
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
       ┌────────┐  ┌──────────┐  ┌─────────┐
       │ Context│  │ AI       │  │ Events  │
       │ Engine │  │ Runtime  │  │ System  │
       └────┬───┘  └────┬─────┘  └────┬────┘
            │           │             │
            └───────────┼─────────────┘
                        ▼
                ┌───────────────┐
                │ ACTION LAYER  │
                │               │
                │ Parser        │
                │ Validator     │
                │ Planner       │
                └───────┬───────┘
                        │
                        ▼
                ┌───────────────┐
                │ CAPABILITIES  │
                │               │
                │ Policy        │
                │ Approval      │
                └───────┬───────┘
                        │
                        ▼
                ┌───────────────┐
                │ TOOL REGISTRY │
                └───────┬───────┘
                        │
                        ▼
                ┌───────────────┐
                │ DETERMINISTIC │
                │ EXECUTORS     │
                └───────┬───────┘
                        │
                        ▼
          ┌─────────────────────────────┐
          │ HOST OS / APPLICATIONS      │
          │                             │
          │ Filesystem                  │
          │ Processes                   │
          │ Creative Applications       │
          │ Games                       │
          │ Development Tools           │
          └─────────────────────────────┘
```

The central architectural principle is:

> **MiniCPM5 interprets. Aletheia validates. Capabilities authorize. Tools execute. The system verifies. Events record what happened.**

Aletheia must remain an environment where the AI is powerful enough to understand the user's work, but the architecture is strong enough that the AI cannot silently become the authority over the user's computer.

---

# 59. Source-of-Truth Priority

Implementation priority:

1. Product Requirements Document
2. This Software Architecture Document
3. System Design Document
4. Database Design Document
5. Security Design
6. UX/UI Design
7. API Contracts
8. Implementation Plan
9. Source Code

If a lower-level implementation conflicts with this document, the conflict must be surfaced.

No developer, agent, or model may silently redefine the architecture.

---

# 60. Architecture North Star

> **Aletheia is an AI-native computing environment where intelligence interprets the user's work, deterministic infrastructure controls the computer, and the creator remains in control.**
