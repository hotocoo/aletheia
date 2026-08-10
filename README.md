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
MICROKERNEL                      no_std Rust microkernel, boots on QEMU, re-proves the invariants (P4)
   kernel/ · kernel-x86_64/ ·        aarch64 (-kernel) · AMD64/x86-64 (UEFI/OVMF) · RISC-V (SBI, S-mode)
   kernel-riscv64/                   — same shared spine.rs/selftest.rs, three CPU targets, all VM-tested
        │  Aletheia HAL — arch-independent contract per crate; no Linux/macOS/POSIX imports
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
├── config     AiConfig: registry default → persisted selection → env (MODEL_REF / MODEL_PATH / …)
├── registry   the pinned model set + the operator's selection (ADR-052)
├── context    native Context Engine (Context Fabric) — capability-aware, structured-first, budgeted
├── prompt      intent/planner protocol + plan grammar / JSON schema + <think>-stripping extraction
├── runtime    HF-cache GGUF discovery + llama-server lifecycle
├── bench      the operation-surface benchmark + the served-model identity check (ADR-052)
└── llama       LlamaCppProvider — dependency-free localhost HTTP to llama-server; deterministic fallback
```

### Which model, and how you change it

The model is a property of the **system**, not a constant in the source. Aletheia ships a registry of
pinned manifests — repo, exact file, quant, **measured** sha256, context, sampling, and the
structured-output strategy — embedded in the binary so it cannot disagree with what it was built
from. The **weights are never committed**; `aletheiad model pull` fetches them into the local Hugging
Face cache on demand.

```bash
aletheiad model list          # every model this OS knows about; * marks the running selection
aletheiad model use lfm2.5    # switch, persisted under $HOME/.aletheia — survives a reboot
aletheiad model status        # what is selected, whether its weights are here, what is being served
aletheiad model pull          # provision the selected model's weights
aletheiad model bench         # run the whole operation surface through it (below)
```

| id | model | notes |
|----|-------|-------|
| `lfm2.5` | **LFM2.5-2.6B (Q4_K_M)** | the default resident model |
| `minicpm` | MiniCPM5-1B-Thinking (Q8_0) | the previous default, kept so earlier measurements keep their baseline |
| `aletheia-lm` | **Aletheia's own model** | registered *before* its weights exist — see below |

`aletheia-lm` is deliberately selectable while it is still being pretrained. Selecting it says
`NOT YET TRAINED` and names the environment variable that will point at the finished weights; it does
**not** quietly fall back to another model. When no model is available at all, the **deterministic
interpreter** takes over — the OS is fully functional with no resident model, and that interpreter is
also the test oracle.

Model quirks live in the manifest, not in the provider: MiniCPM is a *thinking* model whose forced
`<think>` phase collides with a strict grammar (so it runs in no-think mode), while LFM2.5 returns
**empty output** under that same grammar and is constrained by JSON schema instead. One model's
workaround is not every model's.

### Does the model actually drive the OS?

`aletheiad model bench` asks that question rather than assuming it. One intent per registered
operation goes through the **same** provider and the **same** validation the pipeline uses; the
deterministic interpreter runs the identical set as a control arm. Before any number is recorded, the
backend is asked what it is serving and the answer must match the selected manifest — the endpoint is
a *port*, and a benchmark that measures whatever holds it would publish another model's latency under
this model's name.

Measured on one workstation (LFM2.5-2.6B-Q4_K_M, llama.cpp, `-c 8192`): **6/6 operations planned
correctly on two consecutive runs**, median ~3.5 s, control arm 6/6 at 0 ms. Getting there closed
four real defects — including a health probe that had tried only the first resolved address since
ADR-017, which made a *running* model indistinguishable from no model at all. See
[ADR-052](docs/adr/ADR-052-the-model-is-a-system-property.md).

This benchmark covers the hosted Core's **six operations**. It does **not** drive the kernel console
(`kernel-core/src/shell.rs`), which runs in kernel space with no inference engine underneath it and
has its own gate, `scripts/console-e2e.sh`.

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
cargo run -- model list          # what is registered, and which one is selected
cargo run -- model pull          # provision the SELECTED model's weights into the HF cache (once)
llama-server -m "$(python3 -c 'import glob,os;print(glob.glob(os.path.expanduser("~/.cache/huggingface/hub/models--LiquidAI--LFM2.5-2.6B-GGUF/snapshots/*/*.gguf"))[0])')" -c 8192 --port 8080
cargo run -- model status        # confirms the backend is serving the model you selected
cargo run -- model bench         # every registered operation through the real model; exits non-zero on any failure
MODEL_ENDPOINT=http://localhost:8080 cargo run   # provider becomes healthy → model interprets intents

./scripts/vm-e2e.sh              # build + boot the aarch64 microkernel in QEMU + assert invariants (P4)
./scripts/vm-e2e-riscv.sh        # ...the RISC-V target        ./scripts/vm-e2e-x86.sh   # ...the x86-64 target
./scripts/vm-e2e-vbox.sh         # the SAME x86-64 image on a SECOND hypervisor: Oracle VirtualBox (ADR-046)
./scripts/e2e-all.sh             # all three CPU targets + the VirtualBox rung, one aggregate pass/fail
./scripts/conformance.sh         # every target proves the SAME core semantic contract
./scripts/check-traceability.sh  # every "delivered" requirement maps to evidence that EXISTS
./scripts/check-ci-parity.sh     # ...and that CI actually RUNS it (every arch boot-gated, both pipelines agree)
```

### Run it as an OS, in Oracle VirtualBox

No QEMU, no OVMF, no `mtools`, no WSL — VirtualBox, Rust and Python 3 are enough, on **Windows,
macOS or Linux**:

```bash
./scripts/vbox-install.sh                 # build the image + install a persistent VM named "Aletheia"
./scripts/vbox-install.sh --interactive   # ...the build that hands the machine to a console you can type at
VBoxManage startvm Aletheia               # watch it boot; the serial log is the machine-checkable verdict
```

The VM it provisions has **2 vCPUs and 512 MiB**, which is what this OS needs rather than what was
convenient to type: the VirtualBox gate boots the same image at that size on every run (`MEM_MB` and
`CPUS` override it). The interactive build gives you a console with a real line editor — arrows,
`Home`/`End`, `Delete`, a history walked with the up arrow, `Tab` completion over the names that
exist — and 27 commands over the namespace (`ls`, `cat`, `write`, `append`, `cp`, `mv`, `grep`,
`hexdump`, `find`, `wc`, `df`, `mem`, `reboot`, `halt`, …; type `help`). It works from the VM
window's own keyboard as well as over a host serial pipe.

`kernel-x86_64/scripts/mkesp.py` writes the bootable GPT/ESP image with nothing but the Python
standard library, so the same artifact comes out byte-identical on all three hosts.
[docs/VIRTUALBOX.md](docs/VIRTUALBOX.md) is the full walkthrough — reading the boot log, the
interactive shell over a host serial pipe, and what VirtualBox *cannot* cover.

**Why two hypervisors.** A kernel that boots only on QEMU has proved "correct against QEMU": the
emulator and the kernel can be wrong together, and more QEMU testing cannot find it. VirtualBox
brings its own EFI, its own ACPI tables, SATA/AHCI (**no virtio-blk**) and no `isa-debug-exit`, so
the verdict there is the serial log with marker parity against the QEMU gate, and the four device
families it cannot emulate are printed as SKIP rather than quietly dropped. Adding the rung caught
two real defects on its first runs — an invariant that had encoded OVMF's memory map, and a
mis-declared kernel-image extent that only overlapped the user region at a larger guest RAM size
(the gate now boots at two memory sizes for exactly that reason). See
[ADR-046](docs/adr/ADR-046-second-hypervisor-qualification-virtualbox.md).

Configuration (all optional; defaults shown):

```text
AI_PROVIDER=local            # or "deterministic" to force the fallback interpreter
MODEL_BACKEND=llama_cpp
MODEL_ENDPOINT=http://localhost:8080
MODEL_REF=LiquidAI/LFM2.5-2.6B-GGUF    # from the selected manifest; setting it leaves the registry
# MODEL_PATH=/abs/path/to/model.gguf   # explicit override; otherwise resolved from the HF cache
# ALETHEIA_MODEL=minicpm               # use a REGISTERED model for one run without persisting a switch
# ALETHEIA_LM_MODEL=/path/to/aletheia-lm.gguf   # where `model use aletheia-lm` will look, once trained
```

Resolution is an order, not a merge: **environment** beats the **persisted selection**, which beats
the manifest marked `default`. Setting `MODEL_REF` detaches the configuration from the registry — the
model is then reported as `(unregistered)` and `model bench` refuses to run, because a model whose
identity Aletheia cannot verify must not be measured under a registry name.

## Research

[docs/research/RUST-OS-DEEP-RESEARCH.md](docs/research/RUST-OS-DEEP-RESEARCH.md) is the survey this
architecture is argued against rather than asserted over: the 2026 Rust-OS field (Redox, Asterinas's
framekernel, Theseus, seL4's verification economics, Rust-for-Linux), precisely what Rust buys a
kernel and the four places the guarantee leaks, the Kani/Verus/Miri verification landscape, and the
2026 agent-OS literature — which converged independently on the two decisions this repository made
first: **no ambient authority**, and **authority separate from governance**. It closes with five
vindicated decisions and six exposures, each mapped to an open row in the gap register.

## Status

See [STATUS.md](STATUS.md) for the delivered milestones and test counts, and
[docs/](docs/) for the PRD, SAD, and ADRs (ADR-015 policy/approval separation, ADR-016 Service
API/IPC boundary, ADR-017 AI subsystem, ADR-018 Context Engine, ADR-021 SMP, ADR-027 capability
concurrency, ADR-029 mapping-API admission check, ADR-030 frame ownership, ADR-031 page-table reclamation, ADR-032 address-space destruction, ADR-033 erase on free, ADR-034 W^X). Open findings — what is
NOT claimed — live in [docs/gap/ARCHITECTURE-GAPS4-REGISTER.md](docs/gap/ARCHITECTURE-GAPS4-REGISTER.md).

## Strategic path

```text
macOS-hosted Rust prototype
  → platform-independent Aletheia architecture
  → native Rust-first Aletheia kernel and system runtime
  → completely standalone Aletheia operating system
```

The hosted implementation preserves the security and semantic concepts as the actual foundation of a
new OS — not a demo, and not built on Linux.
