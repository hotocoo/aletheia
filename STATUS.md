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
  `emit`). There is deliberately no ambient filesystem/clock/rand/env. `read` copies authorized
  content into a guest-supplied return buffer, so a component can consume and compute over the data
  it is allowed to read (proven by an end-to-end read→transform→write program).
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

**12 P2 acceptance tests** (`tests/component.rs`, all green; 2 are property/fuzz) prove the core
invariant: a component with no capability can do nothing; with an attenuated grant it can do exactly
that and no more; every effect is traced; reads and writes are capability-gated; a runaway is bounded;
launching is gated; an installed component runs from the store; an approval-required capability is
refused at the component boundary (criterion 9 preserved); a component reads→transforms→writes real
data end to end; and a committed effect survives a later fuel-kill (a trap cannot corrupt state). The
untrusted host-ABI boundary is **fuzzed** (PRD §38.4): the fail-closed default and host robustness hold
for randomized memory arguments no one enumerated. Deferred (follow-on P2 iterations): component SDK,
multi-agent composition, and the gating stress/chaos campaigns.

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

## Run it

```bash
cd aletheia
cargo test        # 36 passed — M1 acceptance + property + security + P2 component invariants
cargo test --test component   # the 12 P2 WASM-component acceptance + fuzz tests
cargo run         # aletheiad: boots the hosted System Core + runs the UC-001..004 demo with traces

./scripts/vm-e2e.sh          # build + boot the microkernel in QEMU + assert 11/11 invariants + exit 0
./scripts/linux_pipe_bench.sh # real-Linux IPC baseline for the perf discussion (needs Docker)
```
