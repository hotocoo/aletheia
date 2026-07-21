# Aletheia — Implementation Status

**As of:** 2026-07-20
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable aarch64 microkernel, VM-tested
**Sources of truth:** `docs/Aletheia_Product_Requirements_Document.md` (PRD-003),
`docs/Aletheia_Software_Architecture_Document.md` (SAD-002), `docs/adr/ADR-001..013`.

## What Aletheia is

A from-scratch **AI-native operating system** (not a Linux app). Organized around seven primitives —
**Entity → Capability → Context → Intent → Action → Memory → Relationship** — where intelligence is a
native but **untrusted** collaborator, authority is always an explicit **capability** (no ambient
authority), and a deterministic pipeline executes and verifies everything. See PRD-002 / SAD-002.

The v1 premise (Linux-hosted AI app) was rejected by the product owner; the original docs are retained
as `*_v1_superseded.md` for an auditable before/after.

## Delivered (M1)

A Rust hosted reference implementation of the System Core (`aletheia/`), enforcing the same invariants
a microkernel will later enforce (ADR-010, contract-honest). Runs in userspace; no hardware needed.

- Semantic store: content-addressed, versioned, **encrypted at rest** (ChaCha20-Poly1305), durable.
- Capability engine: possession-based **unforgeable** tokens, attenuated delegation, cascading
  revocation, **fail-closed** ALLOW / DENY / REQUIRE_APPROVAL.
- Intent→Action pipeline: interpret (only probabilistic stage) → parse → validate → authorize →
  approve → execute → **verify** → record immutable event + full trace.
- Intelligence runtime: `ModelRuntime` port + deterministic interpreter fallback + local-model adapter.
  OS is fully functional with no resident model (INT-004).
- Agents: first-class, capability-bounded, revocable actors.
- World model, thin context/memory, tools registry, hosted experience surface (`aletheiad` renders
  explainable traces + world model + audit log).

### 20 M1 acceptance criteria → tests (all green: `cargo test` = 18 passed)

| # | Criterion | Test |
|---|-----------|------|
| 1 | Entity created, content-addressed, retrievable | `spine::spine_end_to_end` |
| 2 | Versioning; prior recoverable | `acceptance::c2_versioning_and_recovery` |
| 3 | Encrypted + survives restart (plaintext absent from disk) | `spine` (raw-bytes + restart) |
| 4 | Relationships + world-model traversal | `acceptance::c4_relationships_world_model` |
| 5 | Every action needs a capability (fail closed) | `spine` + `acceptance` |
| 6 | Capabilities unforgeable (forgery denied) | `acceptance::c6_capabilities_unforgeable` |
| 7 | Delegation attenuation (no amplification) | `acceptance::c7_delegation_attenuation` |
| 8 | Revocation propagates | `acceptance::c8_revocation_propagates` |
| 9 | Destructive requires approval | `acceptance::c9_destructive_requires_approval` |
| 10 | Intent interpreted then validated before execution | `spine` |
| 11 | Malformed model output cannot execute | `acceptance::c11_malformed_output_cannot_execute` |
| 12 | Mid-flight interpretation failure is safe | `acceptance::c12_midflight_interpretation_failure_is_safe` |
| 13 | Verified against real store before success | `spine` |
| 14 | Immutable event with full trace | `spine` |
| 15 | Agent bounded by its capabilities; revocable | `acceptance::c15_agent_bounded_by_capabilities` |
| 16 | Cancellation stops without side effects | `acceptance::c16_cancellation_stops_without_side_effects` |
| 17 | Operates with no resident model | `acceptance::c17_operates_without_model` |
| 18 | No ambient authority | `acceptance::c18_no_ambient_authority` |
| 19 | Untrusted content is data, not instruction | `acceptance::c19_untrusted_content_is_data_not_instruction` |
| 20 | Experience surface renders full trace | `acceptance::c20_experience_surface_renders_trace` |

Plus `security.rs`: expired-capability denial, scope confinement, agent-cannot-self-escalate.

## Deferred (documented, not coded — by design; see PRD §41 / SAD §22)

- **P2** (partially delivered — see "Delivered (P2 start)" below) WASM/WASI capability-secure
  component runtime + SDK + multi-agent composition. The runtime + app-as-capability model + fuel
  bounding + a content return-buffer (read→transform→write) are delivered and tested; SDK,
  multi-agent composition, and the gating fuzz/stress/chaos campaigns remain.
- **P3** Native-architecture experience layer (workspaces, dynamic interfaces, semantic search).
- **P4** Real microkernel (Rust) on metal: capability enforcement, secure IPC, memory/address spaces,
  interrupts; System Core rehosted on it. VM-tested.
- **P5** HAL on real devices, native on-GPU compositor, heterogeneous CPU/GPU/NPU scheduler, secure
  boot, rollback/recovery.
- **P6** Optional sandboxed Linux/POSIX compatibility environment (see Compatibility Appendix).

These require hardware/GPU/kernel and are not testable in a hosted dev environment; they get
architecture text and phased plans, not blind code (ADR-010).

## Engineering notes

- **Rust-first** (ADR-004): 100% safe Rust; no C toolchain in M1 (`sha2`/`chacha20poly1305` are
  pure-Rust). C/C++/asm only behind audited FFI in later hardware phases.
- **Single crate, module boundaries** mirror the SAD's crate list; splitting into a cargo workspace is
  a mechanical later step (dependency direction already points inward toward `domain`).

## Delivered (P2 start — WASM capability-secure components)

A `wasmi`-based component runtime (`aletheia/src/component.rs`, ADR-014) that runs **untrusted**
WASM as first-class applications while preserving every M1 invariant. This is the layer that lets
Aletheia actually run programs — and it does so with **no ambient authority**.

- **No WASI.** A component reaches the OS only through an explicit host ABI (`read` / `write` /
  `emit` / `spawn`). There is deliberately no ambient filesystem/clock/rand/env. `read` copies
  authorized content into a guest-supplied return buffer, so a component can consume and compute over
  the data it is allowed to read (proven by an end-to-end read→transform→write program).
- **Multi-agent composition.** A component can `spawn` an installed child component; the System Core
  runs the child with a capability **attenuated** (delegated) from the parent — so a child can never
  exceed its parent's authority (a read-only parent cannot hand a child write). Spawn depth is bounded.
- **Same authority mechanism.** Every host call authorizes through the *same* `CapEngine::evaluate`
  the deterministic pipeline uses, against the exact capabilities the component was granted —
  nothing inherited from the launcher.
- **Application-as-capability.** Launching a component at all requires a `component.run` capability;
  the component then executes with *exactly* its `grant_caps`. `install_component` registers WASM as
  an encrypted, content-addressed `Application` entity; `run_installed` launches it from the store.
- **Same audit log.** Allowed effects (entity writes, event emits) land in the one immutable event
  log with the component as actor; every host-call attempt (allowed or denied) is in an explainable
  per-run audit.
- **Fuel-bounded.** A runaway component is trapped out-of-fuel and leaves no effects — it cannot
  hang the OS (pre-stages the P2 stress/chaos gate).

**14 P2 acceptance tests** (`tests/component.rs`, all green; 2 are property/fuzz) prove the core
invariant: a component with no capability can do nothing; with an attenuated grant it can do exactly
that and no more; every effect is traced; reads and writes are capability-gated; a runaway is bounded;
launching is gated; an installed component runs from the store; an approval-required capability is
refused at the component boundary (criterion 9 preserved); a component reads→transforms→writes real
data end to end; a committed effect survives a later fuel-kill (a trap cannot corrupt state); a
component spawns a child that runs under a delegated capability; and a spawned child cannot exceed its
parent's authority. The untrusted host-ABI boundary is **fuzzed** (PRD §38.4): the fail-closed default
and host robustness hold for randomized memory arguments no one enumerated. Deferred (follow-on P2
iterations): the component SDK, richer parent→child data-flow wiring, and the gating stress/chaos campaigns.

## Delivered (P4 start — VM-tested microkernel)

A `no_std` Rust microkernel (`kernel/`) that **boots on QEMU `virt` at EL1** and re-proves the
M1 invariants **in kernel space** — the first executed instance of the PRD's VM-Testing layer
(ADR-012, ADR-013), done contract-honest (ADR-010: no blind hardware code; this runs).

- Boot: stack/BSS/heap (bump alloc), PL011 UART, EL1 exception vectors, ARM semihosting exit.
- In-kernel capability-secure spine: content-addressed store, capability engine with an
  **unforgeable-by-construction** `CapToken` (private id field — stronger than the hosted
  string token), and the validate→authorize→execute→verify→record pipeline + secure IPC.
- **11 in-kernel invariant selftests** (M1 acceptance, re-proved live) drive the VM exit code;
  all green. `scripts/vm-e2e.sh` is the CI VM gate (build→boot→assert→exit 0).
- **Performance validation** (QEMU TCG; same emulated CPU, same run — substrate-fair ratio):
  the capability authorization check Aletheia *adds* ≈ **0.79× one bare `svc` trap** (two
  checks) — cheap. This does **NOT** show Aletheia IPC < Linux IPC: the measured loop runs in
  EL1 and crosses no privilege/address-space boundary, while a real microkernel IPC AND a Linux
  pipe both pay ≥2 crossings + context/AS switches. Whole-OS "faster than Linux" stays a
  benchmark program (cross-AS IPC vs a same-emulator Linux guest = next milestone), never a
  claim; and **not** bare-metal numbers.

## Delivered (Alpha wave — 2026-07-21): policy engine, AI subsystem, Context Engine

Elevating the M1 reference from a scripted demo toward the real layered architecture the SAD (§3/§4,
§10) already commits to — the code now catches up to the docs.

- **Policy & approval engine** (`policy.rs`, ADR-015) — a governance axis SEPARATE from capability
  authority. Capabilities decide *authorized?*; policy decides *needs human approval?*. Both approval
  triggers (destructive risk + approval-constrained capability) unified in one place. Durable
  pending-approval lifecycle (request → list → grant/deny → execute) persisted via the immutable
  event log and replayed on open; **approval confers no authority** (caps re-evaluated on execution).
- **AI subsystem** (`ai/`, ADR-017) — AI as a first-class Aletheia-owned subsystem behind a
  model-agnostic `ModelProvider` (`config`/`provider`/`context`/`prompt`/`runtime`/`llama`). Primary
  hosted model = `GnLOLot/MiniCPM5-1B-Claude-Opus-Fable5-V2-Thinking-GGUF` (Q8_0) resolved from the
  HF cache by configurable reference (weights never in git); deterministic interpreter is fallback +
  test oracle. **Live-validated** against the running model: no-think mode + GBNF grammar yields
  correct plan JSON; a strict grammar alone collides with the model's `<think>` phase.
- **Context Engine** (`ai/context.rs`, ADR-018) — native capability-aware **Context Fabric (NOT RAG)**.
  Structured-first layered retrieval around the World Model (direct → structured → relationships →
  memory); authorization enforced BEFORE any entity enters context; budgeted for the small model;
  semantic/vector + document knowledge are OPTIONAL seams (no embedding server / vector DB required).

- **Service API / IPC boundary + Core Alpha daemon** (`service.rs`, ADR-016) — `Request`/`Response`
  across all six surfaces (world/capabilities/policy/audit/components/intents); in-process + Unix-
  socket transports (length-prefixed JSON, std-only, no async/deps). Authorization stays inside the
  Core (fail-closed); the boundary only marshals. `aletheiad serve` = the long-running Core;
  `aletheiad demo` = a client over the boundary. The M1 scenario is reproduced as conformance tests
  THAT TRANSIT THE API (+ a socket round-trip) — apps/tests no longer call Core internals.

- **Aletheia HAL + first-class target matrix** (`kernel/src/hal.rs`, ADR-019) — the kernel is now
  written against an Aletheia-owned `Hal` trait (timer/privilege/exit), not a specific CPU. **AMD64/
  x86-64 and RISC-V are declared first-class targets**; aarch64 is the bootstrap/dev backend (VM-
  tested, still 11/11 green through the HAL). The x86-64/RISC-V backends are `cfg`-gated contracts —
  no untested bring-up code ships (ADR-010). The HAL imports no Linux/macOS/Darwin/POSIX.
- **Security hardening (22-agent adversarial-review pass)** — the wave was adversarially reviewed and
  the confirmed findings fixed: (CRITICAL) unauthenticated audit read + capability-token/decrypted-
  content leakage via the event log → tokens/content are no longer logged and `QueryAudit`/
  `ListApprovals` are `audit.read`-gated; (CRITICAL) ungated `Revoke` → now requires `capability.grant`
  authority (no owner-lockout); (HIGH) ungated `ResolveApproval` → only a principal who could perform
  the bound action may grant/deny; (HIGH) one-time owner bootstrap guard; (HIGH) socket frame cap
  (8 MiB) + per-connection read timeout (slow-loris/OOM); (HIGH) Context-Engine now enforces
  capability-before-inclusion for relationship EDGES, not just entities. Clippy `-D warnings` clean.

Deferred (next): x86-64 + RISC-V HAL backends (VM-tested bring-up, P4/P5); the cargo-**workspace crate
split** (SAD §4 — mechanical; module boundaries + dependency direction already match the crate list).

## Delivered (2026-07-21 — x86-64 bootable development image)

The first **bootable Aletheia disk image**: Aletheia boots as its own OS on **AMD64/x86-64** under
UEFI firmware, calls `ExitBootServices` to take the machine, brings up its own GDT/IDT + 8259 PIC +
8254 PIT, proves a timer IRQ actually fires, and re-proves the 11 capability-secure spine invariants
in x86-64 kernel space. Code in `kernel-x86_64/` (ADR-019 first-class AMD64 target, now executed —
contract-honest: written outside-in and boot-verified, not blind hardware code).

- **Boot model**: `x86_64-unknown-uefi` PE at `\EFI\BOOT\BOOTX64.EFI` on a GPT **EFI System
  Partition**; own COM1 serial + GOP framebuffer console; own `#[global_allocator]` (8 MiB static
  bump heap) + `#[panic_handler]` that stay valid after ExitBootServices; firmware identity paging
  kept (no page tables yet). UEFI is the hardware/platform integration layer (ADR-019); the OS above
  it is entirely Aletheia-owned — no Linux/macOS/POSIX, no third-party OS framework.
- **Artifacts** (from `kernel-x86_64/scripts/build-image.sh`, macOS host, no mtools/xorriso/grub —
  only rust + hdiutil/diskutil + python3 + qemu-img): `build/aletheia-x86_64.img` (raw GPT/ESP) +
  `build/aletheia-x86_64.vmdk` (VMware) + `aletheia-x86_64.vmx`.
- **Verified**: boots in **QEMU 11 under OVMF/UEFI** (`edk2-x86_64-code.fd`) → full serial boot log
  + QEMU exit 33 = `[e2e] PASS`; `scripts/smoke-test.sh` is the automated boot gate (exit 33 +
  "[e2e] PASS"). QEMU-under-OVMF is the same UEFI firmware family VMware uses; VMware itself was not
  driven from the build host — attach the `.vmdk`/`.vmx` (UEFI firmware) to run it there.
- **Reuses** the SAME `spine.rs` / `selftest.rs` the aarch64 kernel compiles (shared via `#[path]`,
  no fork/copy); the aarch64 `-kernel` target is untouched and still green. The workspace/`kernel-core`
  crate split that unifies the one duplicated `Hal` trait is the mechanical follow-up.
- **Deferred (P5)**: own page tables/higher-half, TSS+IST double-fault stack, APIC/HPET + calibrated
  TSC, SMP, a real page-frame allocator, and the RISC-V first-class backend.

## Run it

```bash
cd aletheia
cargo test        # 59 passed — M1 acceptance + conformance + property + security + P2 component + policy + AI
cargo run -- serve  # long-running Core Alpha behind the Unix-socket IPC boundary (clients issue Requests)
cargo test --test component   # the 14 P2 WASM-component acceptance + fuzz tests
cargo run         # aletheiad: boots the hosted System Core + runs the UC-001..004 demo with traces

./scripts/vm-e2e.sh          # build + boot the microkernel in QEMU + assert 11/11 invariants + exit 0
./scripts/linux_pipe_bench.sh # real-Linux IPC baseline for the perf discussion (needs Docker)
```
