# Aletheia — Implementation Status

**As of:** 2026-07-20
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P4 (start)** — bootable aarch64 microkernel, VM-tested
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

- **P2** WASM/WASI capability-secure component runtime + SDK + multi-agent composition.
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
  a capability-checked Aletheia IPC round-trip ≈ **0.79× one bare `svc` crossing**; a Linux
  pipe round-trip pays ≥2 crossings + a context switch. The microkernel IPC fast-path has the
  lower floor. This is the fast-path ratio, **not** a whole-OS "faster than Linux" claim and
  **not** bare-metal numbers; cross-AS IPC + a same-emulator Linux-guest comparison are next.

## Run it

```bash
cd aletheia
cargo test        # 18 passed — the M1 acceptance bar
cargo run         # aletheiad: boots the hosted System Core + runs the UC-001..004 demo with traces

./scripts/vm-e2e.sh          # build + boot the microkernel in QEMU + assert 11/11 invariants + exit 0
./scripts/linux_pipe_bench.sh # real-Linux IPC baseline for the perf discussion (needs Docker)
```
