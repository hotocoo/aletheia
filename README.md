# Aletheia

**A from-scratch, AI-native operating system.** Not Linux, not a Linux distribution, not a Linux
derivative — its own kernel, process/memory model, IPC, capability architecture, runtime, storage,
and service model, Rust-first. The current macOS-hosted Rust implementation is a **temporary
development environment**; every interface is an Aletheia-owned abstraction designed to be
re-implemented natively later without rewriting the semantic or security architecture.

## The idea

Intelligence is a **native but untrusted** collaborator. A model may interpret language, reason over
supplied context, and *propose* structured plans — but it never holds authority and never executes
anything. Aletheia independently validates, authorizes, approves, executes, and verifies every
operation. Authority is always an explicit, scoped, revocable **capability** — there is no ambient
authority anywhere in the system.

The whole OS turns on one deterministic pipeline:

```text
Intent → build context (World Model + capabilities + memory)
       → ModelProvider proposes a structured Plan
       → schema + semantic validation
       → capability evaluation (authority)
       → policy / human approval (governance)   ← independent of authority
       → execution
       → independent verification
       → immutable provenance event
```

The AI is the only probabilistic stage. Everything after it is deterministic and testable.

## Layered architecture

```text
EXPERIENCE / APPLICATIONS        explainable traces, world view, audit — CLIENTS of the boundary
        │  Service API / IPC boundary (service.rs): Request/Response over in-proc + Unix socket
SYSTEM CORE (aletheia/src)
  ├── domain            seven primitives: Entity·Capability·Context·Intent·Action·Memory·Relationship
  ├── storage           content-addressed, versioned, encrypted-at-rest, durable semantic store
  ├── capabilities      the sole AUTHORITY engine: mint · attenuated delegation · revocation · evaluate
  ├── policy            the GOVERNANCE engine: human approval, separate from capability authority
  ├── worldmodel        typed, provenance-bearing relationship graph + traversal
  ├── intent_action     the deterministic pipeline (parse · validate · authorize · execute · verify · record)
  ├── ai/               the AI subsystem (below) — model-agnostic
  ├── component         capability-secure WASM component runtime (no ambient authority)
  ├── agents            first-class, capability-bounded, revocable actors
  ├── syscore           composition root wiring the pipeline + task lifecycle + approvals
  └── service           capability-gated Service API + IPC (in-process + Unix socket) — the app boundary
MICROKERNEL (kernel/)            no_std Rust microkernel, boots on QEMU, re-proves invariants (P4 start)
        │  Aletheia HAL (kernel/src/hal.rs) — arch-independent contract; no Linux/macOS/POSIX imports
HARDWARE                         AMD64/x86-64 · RISC-V (first-class targets) — aarch64 (bootstrap/dev)
```

Dependency direction points inward toward `domain`; nothing reaches around the capability engine.
Aletheia is its own OS: AMD64 and RISC-V are hardware targets, Rust is the implementation language,
and every OS abstraction belongs to Aletheia (ADR-019).

### Authority vs. governance (two independent axes)

- **Capabilities** answer *authority*: is this subject permitted to do this at all? → `Allow / Deny /
  RequireApproval`. Unforgeable, possession-based, attenuated on delegation, cascading revocation,
  fail-closed.
- **Policy** answers *governance*: even when authorized, must a human approve this? A destructive op
  with full authority still needs approval; an approval-constrained capability needs approval even
  for a safe read. Pending approvals are durable (replayed from the event log) and bound to the exact
  intent — approval confers no authority.

## The AI subsystem (`aletheia/src/ai/`)

AI is a **first-class, Aletheia-owned subsystem** behind a model-agnostic `ModelProvider`. The model
runtime is an implementation detail: the Core never depends on llama.cpp APIs or a hardcoded model.

```text
ai/
├── provider   ModelProvider trait (one seam — a native Aletheia model service implements it later)
├── config     AiConfig from env: AI_PROVIDER / MODEL_BACKEND / MODEL_ENDPOINT / MODEL_REF (+ MODEL_PATH)
├── context    native Context Engine (Context Fabric) — capability-aware, structured-first, budgeted
├── prompt      intent/planner protocol + GBNF plan grammar + <think>-stripping JSON extraction
├── runtime    HF-cache GGUF discovery + llama-server lifecycle
└── llama       LlamaCppProvider — dependency-free localhost HTTP to llama-server; deterministic fallback
```

**Hosted-dev model:** `GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF` (Q8_0). It is a
**declared first-party model** (pinned in [models/minicpm.toml](models/minicpm.toml)) that ships with
Aletheia and is **provisioned on demand** — `aletheiad model pull` fetches it into the local Hugging
Face cache. The **weights are never committed to the repo**; the repo carries the integration + the
pinned declaration, so a fresh clone provisions the exact model on first use.
Validated live: MiniCPM is a *thinking* model, so a strict JSON grammar collides with its `<think>`
phase; Aletheia runs it in **no-think mode + GBNF grammar** (`enable_thinking=false`, temp 0.7),
which yields clean, correct plan JSON. When no model is available, the **deterministic interpreter**
takes over (the OS is fully functional with no resident model) and serves as the test oracle.

### Context Engine — Context Fabric, not RAG

Aletheia understands its own world and provides the **smallest useful, authorized** context per task
rather than dumping data into the model. Layered retrieval, structured-first:

```text
intent → capability-aware retrieval
  ├── direct         subject · focus entity · held authority          [always]
  ├── structured     entity queries (type, properties, ownership)     [always]
  ├── relationships  world-model traversal from the focus             [always]
  ├── memory         relevant past actions                            [when relevant]
  ├── semantic       embeddings for ambiguous NL search               [OPTIONAL seam]
  └── knowledge      documents / transcripts / images                 [OPTIONAL seam]
→ rank / dedup / compress / budget → compact typed AiContext → model
```

Every entity is authorized (`entity.read`) **before** it enters context; a subject with no capability
gets no world context. Semantic/vector and document knowledge are optional interfaces — **no
embedding server or vector database is required** for normal OS operation.

## Run it

```bash
cd aletheia
cargo test                       # full conformance + unit suite (deterministic; no model needed)
cargo run                        # aletheiad demo: runs UC-001..004 as a CLIENT over the service boundary
cargo run -- serve               # long-running Core Alpha behind the Unix-socket IPC boundary

# Optional: use the real local model as the primary AI provider (hosted dev)
cargo run -- model pull          # provision the first-party MiniCPM model into the HF cache (once)
cargo run -- serve --ensure-model &   # (or start llama-server yourself:)
llama-server -m "$(python3 -c 'import glob,os;print(glob.glob(os.path.expanduser("~/.cache/huggingface/hub/models--GnLOLot--MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF/snapshots/*/*.gguf"))[0])')" -c 8192 --port 8080
MODEL_ENDPOINT=http://localhost:8080 cargo run   # provider becomes healthy → model interprets intents

./scripts/vm-e2e.sh              # build + boot the microkernel in QEMU + assert invariants (P4)
```

Configuration (all optional; defaults shown):

```text
AI_PROVIDER=local            # or "deterministic" to force the fallback interpreter
MODEL_BACKEND=llama_cpp
MODEL_ENDPOINT=http://localhost:8080
MODEL_REF=GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF
# MODEL_PATH=/abs/path/to/model.gguf   # explicit override; otherwise resolved from the HF cache
```

## Status

See [STATUS.md](STATUS.md) for the delivered milestones and test counts, and
[docs/](docs/) for the PRD, SAD, and ADRs (ADR-015 policy/approval separation, ADR-016 Service
API/IPC boundary, ADR-017 AI subsystem, ADR-018 Context Engine).

## Strategic path

```text
macOS-hosted Rust prototype
  → platform-independent Aletheia architecture
  → native Rust-first Aletheia kernel and system runtime
  → completely standalone Aletheia operating system
```

The hosted implementation preserves the security and semantic concepts as the actual foundation of a
new OS — not a demo, and not built on Linux.
