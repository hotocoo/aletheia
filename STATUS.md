# Aletheia — Implementation Status

**As of:** 2026-07-21
**Milestone delivered:** M1 — Hosted System-Core Reference (Rust); **P2 (start)** — WASM capability-secure component runtime; **P4 (start)** — bootable microkernel on THREE CPU targets, VM-tested: aarch64 (bootstrap) + AMD64/x86-64 (first-class) + **RISC-V/RV64GC (first-class)**; **P5 (start)** — real memory management: physical page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) + **EL0 user-mode with a capability-gated syscall boundary, hardware address-space isolation, per-process address spaces (separate TTBR0), and preemptive multitasking (full trap-frame context switch + round-robin scheduler + GICv2/generic-timer IRQ preemption)**, VM-tested on the aarch64 dev backend
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
  bounding + a content return-buffer (read→transform→write) + multi-agent composition (spawn) + the
  **Rust component SDK** + the **property/chaos gate** are delivered and tested; only the longer-running
  soak/adversarial stress campaigns remain.
- **P3** Native-architecture experience layer (workspaces, dynamic interfaces, semantic search).
  *(Started: capability-gated keyword search over the World Model is delivered — see the P3 section.)*
- **P4** Real microkernel (Rust) on metal: capability enforcement, secure IPC, memory/address spaces,
  interrupts; System Core rehosted on it. VM-tested.
- **P5** (partially delivered — see "Delivered (P5 start)" below) real memory management: a physical
  page-frame allocator + MMU virtual memory (identity map + dynamic map/unmap) are delivered and
  VM-tested on the aarch64 dev backend. Still deferred: higher-half (TTBR1) split, timer-driven
  preemption (GIC), HAL on real devices, native on-GPU compositor, heterogeneous CPU/GPU/NPU scheduler,
  secure boot, rollback/recovery.
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
and host robustness hold for randomized memory arguments no one enumerated. The **component SDK** is now
delivered (Rust authoring layer over the host ABI — see its section below), and the gating
property/chaos campaign is now green (`tests/component_chaos.rs`, below). Deferred (follow-on P2
iterations): richer parent→child data-flow wiring, and longer-running soak/adversarial stress.

## Delivered (2026-07-21 — P2 component SDK: author components in Rust)

The layer that lets a developer **write an Aletheia component in Rust** instead of hand-assembling
WASM/WAT against the raw host ABI. A small `no_std` crate (`component-sdk/`, `aletheia-component-sdk`)
wraps the four capability-gated host calls behind safe, typed functions — and nothing else, because
there is nothing else to reach (no WASI, no ambient authority; ADR-014).

- **API**: `write_output(&[u8])`, `emit_event(&str)`, `read_entity(&str, &mut [u8]) -> len`,
  `spawn_child(app, action)` — each returns `Result<_, HostError>` where `HostError` maps the host's
  ABI sentinels (`-1` Denied / `-2` NeedsApproval / `-3` Bad). A `component_main!` macro exports the
  guest's `run() -> i32` and provides the `no_std` `#[panic_handler]` (a panicking component traps and
  leaves no effects — the host's per-call all-or-nothing + fuel boundary already guarantees this).
- **Example**: `examples/hello-component/` — a real guest authored with the SDK (writes an Output
  entity, emits an event), compiled to `wasm32-unknown-unknown`.
- **Verified by the SAME bar as the runtime**: `aletheia/tests/sdk_component.rs` (4 tests, green) runs
  the SDK-authored guest through the *unchanged* `SysCore` and asserts it is exactly capability-bounded
  — no capability ⇒ it changes nothing (write denied, exits 1, zero effects); granted `entity.write` +
  `event.emit` ⇒ it does exactly those, exits 0, and the stored entity holds precisely the bytes the
  SDK wrote; granted only `entity.write` ⇒ it writes but its emit is denied (attenuation, exits 2).
- **No new CI toolchain dependency**: the example is prebuilt to a committed fixture
  (`aletheia/tests/fixtures/hello_component.wasm`, 306 B) by `scripts/build-example-component.sh`, so
  the hosted `cargo test` gate stays green with no wasm target required; regenerate the fixture with
  that script (needs `rustup target add wasm32-unknown-unknown`) whenever the SDK or example changes.
  `clippy -D warnings` clean on host + wasm32.
- **Deferred (follow-on)**: an `alloc`-backed convenience layer (owned buffers for `read_entity`),
  richer parent→child data-flow, and typed entity/metadata helpers.

## Delivered (2026-07-21 — P3 start: capability-gated World-Model search)

The first slice of the P3 experience layer: **search over the World Model**, subject to the same
capability discipline as everything else. `ContextEngine::search_world` (ADR-018's search seam, in
its always-available NO-embedding form — the `SemanticRetriever` embedding path stays an optional
extension) scores entities by keyword match across type/metadata/UTF-8 content and returns the top
hits most-relevant-first (deterministic; ties broken by id). It is **authorization-before-inclusion**:
only entities the caller may `entity.read` are ever considered, so an unauthorized entity never
appears even when it matches, and a caller with no read authority gets nothing (fail closed). Exposed
as `SysCore::search(offered, query, limit)` (read-only). Verified by `aletheia/tests/search.rs`:
capability-gated (a reader scoped to one entity never sees another that matches), ranked (a two-term
match outranks a one-term match), and fail-closed. Deferred (P3): embedding-backed semantic search,
workspaces, and dynamic interfaces.

## Delivered (2026-07-21 — P2 property/chaos gate)

The runtime's two load-bearing invariants, proved over the RANDOMIZED space the fixed tests don't
enumerate — `aletheia/tests/component_chaos.rs` (3 tests, green; 2 are `proptest` campaigns of 64
cases each): for any random (capability-set × host-call-sequence × fuel), (1) **no effect without a
capability** and (2) **effects ⊆ grant** — and fuel exhaustion can only *reduce* effects, never
manufacture an unauthorized one, and never hangs the OS (every run returns a verdict). Plus a
cross-run isolation test: authority never leaks from a privileged component run to a later
unprivileged one. Hosted suite now **66 passed**. Deferred: longer-running soak / adversarial stress.

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

Deferred (next): both first-class HAL backends are now VM-tested and executed (see the x86-64 and
RISC-V delivered sections below). The shared **`kernel-core`** crate now holds not just the `Hal`
trait but the entire capability-secure **spine** and the **invariant selftest suite** as well — see
the kernel-core substrate section below (gap-register Issue 1). The remaining mechanical item is the
fuller cargo-**workspace crate split** of the hosted crate (SAD §4 — module boundaries + dependency
direction already match the crate list).

## Delivered (2026-07-21 — x86-64 bootable development image)

The first **bootable Aletheia disk image**: Aletheia boots as its own OS on **AMD64/x86-64** under
UEFI firmware, calls `ExitBootServices` to take the machine, brings up its own GDT/IDT + 8259 PIC +
8254 PIT, proves a timer IRQ actually fires, and re-proves the 11 capability-secure spine invariants
in x86-64 kernel space. Code in `kernel-x86_64/` (ADR-019 first-class AMD64 target, now executed —
contract-honest: written outside-in and boot-verified, not blind hardware code).

- **Boot model**: `x86_64-unknown-uefi` PE at `\EFI\BOOT\BOOTX64.EFI` on a GPT **EFI System
  Partition**; own COM1 serial + GOP framebuffer console; own `#[global_allocator]` (8 MiB static
  bump heap) + `#[panic_handler]` that stay valid after ExitBootServices. Post-exit the kernel
  **owns** the firmware page-table hierarchy: a physical frame allocator seeded from the UEFI
  memory map (`frames.rs`) + map/unmap over the live PML4 (`vm.rs`) — see the P5 note below. UEFI
  is the hardware/platform integration layer (ADR-019); the OS above it is entirely Aletheia-owned
  — no Linux/macOS/POSIX, no third-party OS framework.
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

## Delivered (2026-07-21 — x86-64 ring-3 user-mode + PIT-driven preemption)

The x86-64 backend now proves the **same 13 user-mode invariants** the aarch64 EL0 suite does
(10 base ring-3 invariants + capability-secure IPC 11-13) — the first executed ring-3 privilege
boundary on the first-class AMD64 target. Code in
`kernel-x86_64/src/usermode.rs` (+ ring-3 GDT segments & TSS in `gdt.rs`, per-process address spaces
in `vm.rs`, DPL=3 syscall/#PF/timer vectors in `idt.rs`). `scripts/smoke-test.sh` now also gates on
`RING-3 BOUNDARY INVARIANTS HOLD`; QEMU exit 33 + `[e2e] PASS` still hold (13/13 green).

- **Real ring 3 (CPL 3)**: `iretq` drops to unprivileged code in USER-only pages; the one door back
  is an `int 0x80` DPL=3 gate authorized by the SAME `CapEngine` the deterministic pipeline uses. A
  save-first trap path stores the full register file into the running task's `TrapFrame`; a single
  `resume_frame`/`resume_return` primitive both starts a fresh task and resumes a preempted one.
- **Hardware isolation** (a ring-3 read of a supervisor-only page faults and is contained) and
  **per-process PML4 address spaces** (a page private to process A is unreachable from B at the same
  VA) — each process gets a private copy of the low PDPT with its 1 GiB user slot cleared, while the
  kernel/RAM/framebuffer identity mappings stay shared.
- **Preemptive multitasking**: the free-running 8254 **PIT IRQ0**, taken in ring 3, preempts two
  non-yielding tasks; the round-robin scheduler switches them and each resumes with its register
  state (progress counter) intact. Cooperative (`SYS_YIELD`) scheduling is proven too.
- **Two hard-won gotchas (documented in-code)**: (1) `x86_64-unknown-uefi` makes `extern "C"` the
  **Microsoft x64 ABI** — the trap assembly and its boundary fns are `extern "sysv64"` so the frame
  pointer arrives in RDI, not RCX. (2) QEMU/OVMF enforce the ring-3 code segment's **4 GiB limit** on
  the `iret` target, so the user region lives in the **1..2 GiB** range (below 4 GiB), not a high slot.
- **Deferred (P5)**: higher-half kernel, TSS+IST double-fault stack, APIC/HPET + calibrated TSC, SMP,
  and the RISC-V ring-3 backend.

## Delivered (2026-07-21 — RISC-V first-class backend, VM-tested)

The **second first-class target executed** (ADR-019): the Aletheia microkernel now boots on
**RISC-V / RV64GC** under QEMU `virt` and re-proves the 11 capability-secure spine invariants in
RISC-V kernel space — contract-honest (ADR-010: written outside-in and boot-verified, not blind
hardware code). Code in `kernel-riscv64/`.

- **Boot model**: QEMU loads **OpenSBI** (`-bios default`, M-mode) which hands off to our `-kernel`
  ELF entry (`_start`) in **S-mode** with `a0`=hartid, `a1`=DTB; the kernel parks secondary harts,
  sets its stack, clears BSS, installs an `stvec` trap vector, and runs. It drives the QEMU `virt`
  **NS16550A UART** directly for a robust console, and genuinely exercises the **S→M SBI boundary**
  (the RISC-V privilege-crossing interface) by calling the SBI Base extension — live boot shows
  `spec v3.0, impl=OpenSBI`. Timer is the S-mode `rdtime` (`time` CSR, 10 MHz on `virt`), shown
  advancing at boot.
- **Machine exit** is the **SiFive-test device** (MMIO `0x0010_0000`), NOT SBI SRST: SRST can only
  request a clean shutdown (exit 0) and so cannot signal a *failing* invariant, whereas the
  SiFive-test finisher encodes a code — `FINISHER_PASS` ⇒ QEMU exit 0 (e2e PASS), `(code<<16) |
  FINISHER_FAIL` ⇒ QEMU exit `code` (per-invariant failure / panic 101 / trap 102).
- **Reuses** the SAME `spine.rs` / `selftest.rs` the aarch64 and x86-64 kernels compile (shared via
  `#[path]`, no fork/copy); the aarch64 and x86-64 targets are untouched and still green. The one
  duplicated `Hal` trait across the three kernel crates is unified by the mechanical workspace/
  `kernel-core` split (the documented follow-up).
- **Verified**: `scripts/vm-e2e-riscv.sh` builds the kernel, boots it in QEMU riscv64 `virt`+OpenSBI,
  and asserts the SBI-boundary marker + `ALL 11 INVARIANTS HOLD` + the memory / virtual-memory /
  user-mode markers (below) + `[e2e] PASS` + **exit 0** (60s watchdog). Wired into CI as the
  `vm-e2e-riscv` job (GitHub + GitLab), alongside the aarch64 gate. `clippy -D warnings` clean.
- **Deferred (P5 follow-on)**: Sv48 + higher-half, PLIC/external interrupts, a frame-backed kernel
  heap, and SMP (secondary-hart bring-up). (Sv39 MMU + frame allocator + U-mode + per-process
  address spaces + timer preemption are now DELIVERED — see the RISC-V P5 parity section below.)

## Delivered (2026-07-21 — RISC-V P5 parity: Sv39 MMU, U-mode, per-process spaces, preemption, IPC)

The RISC-V backend now proves the **same memory-management + user-mode invariant suite** the aarch64
dev backend and the x86-64 image do — closing the cross-architecture process-isolation gap
(ARCHITECTURE-GAPS Issue 3): the RISC-V column of the capability matrix (physical allocator, MMU,
user mode, per-process address space, preemption) is no longer a gap. Contract-honest (ADR-010):
every module was written outside-in and boot-verified in QEMU; a wrong page table / bad trap faults
to `exit 102`, never a silent hang. All three new modules are riscv64-crate-only; the `#[path]`-shared
`spine.rs`/`selftest.rs` are untouched and the aarch64 + x86-64 targets stay green.

- **Physical page-frame allocator** (`frames.rs`) — intrusive LIFO free-list over RAM above the
  kernel image (QEMU `virt` DRAM base 0x8000_0000; `-m 128M`), identical in shape to the aarch64
  allocator. **7 memory invariants** proved live (distinct/aligned/in-range alloc, real R/W frame,
  misaligned-free rejected, exhaustion fail-closed, free revives allocation).
- **Sv39 virtual memory** (`vm.rs`) — 3-level Sv39 page tables built from frames: peripheral GiB as a
  Device gigapage leaf, RAM as 2 MiB megapage leaves; A/D set on every leaf (the RISC-V analogue of
  the aarch64 Access-Flag anti-hang move). The identity map is asserted by a **software page-table
  walk BEFORE `satp` is written**; then dynamic map/unmap is proved by writing through a fresh VA and
  observing the bytes in the mapped physical frame. **13 virtual-memory invariants** live.
- **U-mode + preemption + IPC** (`usermode.rs`) — drops the CPU to **U-mode** and reaches the OS
  through exactly one door: an `ecall` authorized by the **same `CapEngine`** the deterministic
  pipeline uses (`sscratch` holds the current task's save-first trap frame; `sepc`/`sstatus` carry
  the resume PC/privilege; `fence.i` makes freshly written user code fetchable). Proves **13 U-mode
  boundary invariants**: cap-gated syscall (deny w/o cap, allow ⇒ one event); hardware isolation (a
  U-mode load of a supervisor-only page faults, contained); **per-process `satp` address spaces** (A
  reaches its own page, B cannot reach A's VA); a **cooperative round-robin scheduler** (two tasks in
  distinct spaces run A,B,A,B… to exit, each echoing its own register magic through the shared code
  VA); **timer preemption** (the S-mode timer IRQ — armed via the **SBI TIME extension** + `sie.STIE`,
  cleared purely by re-arming, no interrupt-controller dance — preempts two non-yielding tasks and
  each resumes with its progress counter advanced); and **capability-secure kernel-mediated IPC**
  across distinct address spaces (send/recv fail-closed without the `ipc.send`/`ipc.recv` capability).
  A dead-timer escape (bounded spin countdown → self-exit) keeps the preemption test from ever
  hanging. `cargo run` now re-proves **11 spine + 7 memory + 13 virtual-memory + 13 user-mode**
  invariants + exit 0; `clippy -D warnings` clean.

## Delivered (2026-07-21 — P5 start: physical frame allocator + MMU virtual memory)

The first **real memory management** in kernel space, on the aarch64 dev backend — the layer that
turns "a capability-secure spine that boots" into an OS that can own physical memory and translate
addresses. Two bricks, each landed green and VM-asserted (ADR-010: written outside-in, boot-verified,
never blind hardware code; a wrong page table faults to `exit 102`, never a silent hang). Both new
modules (`kernel/src/frames.rs`, `kernel/src/vm.rs`) are aarch64-crate-only; the `#[path]`-shared
`spine.rs`/`selftest.rs` are untouched, and the x86-64 + RISC-V targets are re-verified green.

- **Physical page-frame allocator** (`frames.rs`) — an intrusive LIFO free-list over the RAM *above*
  the kernel image/stack/bump-heap (each free frame stores the next-free link in its own first 8
  bytes, so there is no side table). 4 KiB frames; `alloc` / `alloc_zeroed` (page-table shape) /
  `free`; fail-closed on exhaustion; rejects misaligned/out-of-range frees. **7 memory invariants**
  proved live in QEMU: real read/write frame, distinct/aligned/in-range allocation, misaligned-free
  rejected, exhaustion denies (fail-closed), freeing revives allocation.
- **MMU virtual memory** (`vm.rs`) — the first live address-translation regime. Builds an identity map
  from frame-allocator frames (peripheral GiB = Device, RAM = Normal; **Access Flag set in every
  descriptor**; 4 KiB granule, 39-bit VA, TTBR0). It **asserts the map with a software page-table walk
  BEFORE flipping `SCTLR.M`** (the single highest-leverage anti-hang move), enables translation with
  the MAIR/TCR/TTBR0 + invalidate/barrier dance, then proves **dynamic** virtual memory: map a fresh
  frame at a brand-new VA, write through the VA, observe the bytes land in the *different* physical
  frame the VA points at, unmap, confirm it no longer resolves. **13 virtual-memory invariants** green
  live in QEMU. `scripts/vm-e2e.sh` now asserts the memory + virtual-memory markers alongside the 11
  spine invariants.
- **Deferred (P5 follow-on)**: higher-half (TTBR1) kernel/user split, timer-driven (involuntary)
  preemption (GIC + generic-timer IRQ), a frame-backed kernel heap (the static bump heap stays
  load-bearing for now), and the x86-64/RISC-V MMU backends. (EL0 user-mode + the cap-gated syscall
  boundary + per-process address spaces + cooperative multitasking are now delivered — see the EL0
  section.)

## Delivered (2026-07-21 — P5: EL0 user-mode, per-process address spaces, preemptive multitasking)

The brick that makes the privilege boundary **real**. Until now every invariant was re-proved
*in kernel space* (EL1) — the benchmark's own honesty note says the measured loop "crosses no
privilege/address-space boundary," so isolation was logical, not hardware-enforced. This wave
drops the CPU to **EL0** (unprivileged), runs a genuinely less-privileged instruction stream in
its own EL0-only pages, and lets it reach the OS through *exactly one door*: an `svc` trap that
lands in the EL1 vector and is authorized by the **same `CapEngine`** the deterministic pipeline
uses. Code in `kernel/src/usermode.rs` (aarch64 dev backend; `#[path]`-shared `spine.rs` untouched,
x86-64 + RISC-V re-verified green). Contract-honest (ADR-010): written outside-in, boot-verified;
an *unexpected* fault stays fatal (`exit 102`) so a real bug can never masquerade as a pass.

- **Real EL0 excursions, one-shot** (not a scheduler — that is the follow-on multitasking brick).
  `enter_user` saves the kernel's callee-saved context and `eret`s to EL0 with a tiny
  position-independent stub in a fresh EL0-executable page (new `vm::USER_CODE`/`USER_DATA` AP
  bits = EL0 R/W, PXN); the stub issues one `svc` (or faults); the **0x400 vector** (`Lower EL,
  AArch64, Synchronous`, previously fatal) decodes `ESR_EL1.EC` — SVC `0x15` vs Data-Abort-lower-EL
  `0x24` — dispatches, then resumes the kernel via `enter_user_return`. The 0x200 EL1 `svc`
  bench fast-path is untouched. User pages are mapped into the **live** TTBR0 (`vm::active_root`).
- **Same authority mechanism at the boundary.** The syscall handler authorizes through
  `CapEngine::evaluate` against the process's granted capabilities — nothing ambient. Allow ⇒ the
  effect happens (an event recorded in the Store, actor = the EL0 process); Deny ⇒ −1, zero effect.
- **Per-process address spaces** (`vm::switch_address_space`, `vm::build_identity` per process):
  each EL0 process runs under its **own TTBR0 root**. A page private to process A, mapped at a
  virtual address, is **unreachable from process B at that same VA** — B takes a contained
  translation fault. Same VA, present in A's space and absent from B's ⇒ the spaces are genuinely
  separate, not one flat memory. The TTBR0 switch flushes the TLB (`tlbi vmalle1`); every process
  root replicates the kernel identity map, which is what makes switching TTBR0 mid-execution safe.
- **Cooperative multitasking — the first executed Aletheia context switch.** The whole trap path
  is unified on a **save-first** entry: `0x400` saves the full register file (x0–x30 + ELR + SPSR
  + SP_EL0) into the running task's `TrapFrame` *before* any clobber (`TPIDR_EL1` = current frame,
  `TPIDR_EL0` = save-time scratch), then decodes/dispatches; `resume_frame` restores a whole frame
  and `eret`s (the same primitive starts a fresh task and resumes a preempted one). Two EL0 tasks
  `yield` (`SYS_YIELD`) under a **round-robin scheduler**, running to completion in a deterministic
  `A,B,A,B,A,B,A,B`. Each task runs in **its own TTBR0 address space** (the scheduler switches
  address spaces per slice), and both tasks share ONE code VA — so a task carries a **register-magic
  in a callee-saved reg** it replays as the syscall arg every slice, and the kernel asserting each
  slice reports *its* task's magic proves BOTH that the entire register file (not just the PC) rode
  through each context switch AND that the per-slice address-space switch routed the shared VA to
  the right task's code.
- **Timer-driven (involuntary) preemption** — a real preemptive scheduler. A **GICv2** (distributor
  `0x0800_0000`, CPU interface `0x0801_0000`) + the **EL1 generic timer** (PPI INTID 30) deliver a
  periodic IRQ to vector `0x480`; the handler saves the preempted task's frame (the same save-first
  prologue as `svc`, shared via an asm macro), re-arms `CNTP_TVAL` **before** EOI (the timer
  condition is level-triggered — EOI-without-rearm would storm), and the scheduler round-robins. The
  two tasks run with IRQs unmasked (SPSR `0x340`) and **never yield** — a tight `add x19; subs x20;
  b.ne` loop — yet the timer preempts them and each resumes with its counter (`x19`) advanced,
  proving state survives an *involuntary* switch. Contract-honest anti-hang: the loop is **bounded**
  (`x20` countdown) so a timer that never fires makes the task self-exit → a clean failure, never a
  spin; and `-machine virt,gic-version=2` is pinned so a GICv3 can't silently swallow the MMIO CPU
  interface. The GIC/timer are torn down after the test so the benchmark is unperturbed.
- **13 EL0-boundary invariants proved live in QEMU** (exit `80+i` on failure): (1) an EL0 process
  with **no capability** is denied at the boundary and leaves zero effect; (2) a **capability-granted**
  EL0 process is authorized via the same `CapEngine` and records exactly one event; (3) **hardware
  address-space isolation** — an EL0 read of kernel memory takes a permission Data Abort that is
  contained (proving EL0 truly cannot touch EL1 memory, not just "shouldn't"); (4) process A reaches
  a page in **its own** address space; (5) process B **cannot** reach A's page at the same VA
  (per-process isolation); (6) the **round-robin scheduler** runs two tasks (each in its own space)
  A,B,A,B,… to completion; (7) each task **resumes with its own register magic** at the shared VA
  (full context + the per-slice address-space switch); (8) the two scheduled tasks occupy **distinct
  TTBR0 address spaces**; (9) the **generic-timer IRQ preempts** two non-yielding tasks and the
  scheduler round-robins both; (10) each task's **register counter advances across preemptions**
  (state preserved under an involuntary switch); (11) **capability-secure IPC** — a message is
  delivered kernel-mediated across distinct address spaces; (12) an IPC send **without** the
  `ipc.send` capability is denied, endpoint untouched (fail-closed); (13) an IPC recv **without** the
  `ipc.recv` capability is denied, the queued message intact (fail-closed). `cargo run` now boots and
  re-proves **11 spine + 7 memory + 13 virtual-memory + 13 user-mode** invariants + exit 0.
- **Deferred (P5 follow-on)**: higher-half (TTBR1) kernel/user split, a frame-backed kernel heap
  (the static bump heap stays load-bearing for now), SMP (secondary-hart bring-up), and the
  x86-64/RISC-V EL0/preemption backends.

## Delivered (2026-07-21 — kernel-core substrate: shared spine + hosted arch-independent invariants)

The first real slice of gap-register **Issue 1** (architecture-independent `kernel-core`): the
capability-secure **spine** (`spine.rs` — content-addressed store, unforgeable capability engine,
intent→action pipeline, secure IPC) and the **invariant selftest suite** (`selftest.rs` — the 11 M1
acceptance criteria) are no longer `#[path]`-copied into each target crate. They now live **once** in
`kernel-core/` as real library modules that all three targets (`kernel/` aarch64, `kernel-x86_64/`,
`kernel-riscv64/`) depend on. This directly satisfies Issue 1 criterion #1 ("core kernel abstractions
are not duplicated across architecture crates") — previously only the `Hal` trait was shared; the
spine itself was textually included.

- **One source of truth, three backends.** Each target keeps only what is genuinely
  architecture-specific — its `hal.rs` backend `impl Hal` and its own console (`kprintln!`). The
  spine has zero architecture dependency (pure `no_std` + `alloc`), so it compiles identically for
  all three CPUs from the same file.
- **Console decoupling.** `selftest::run` no longer hard-codes a console macro; it reports each check
  through a caller-supplied `report(index, passed, name)` logger. Each kernel passes a `kprintln!`
  closure that prints the familiar `  [pass NN] name` lines; the invariant logic and its naming are
  defined exactly once in `kernel-core`.
- **Arch-independent invariants now run in HOSTED tests** (Issue 1 acceptance criterion #5): because
  the spine is arch-independent, `kernel-core/tests/invariants.rs` proves the whole suite on the host
  in a fast `cargo test` (13 tests, no QEMU) — running the SAME `selftest::run()` the three kernels
  boot, plus granular named per-invariant tests. This complements (does not replace) the per-target
  QEMU VM gates.
- **Capability transfer through IPC + bounded queues** (gap-register Issue 2) now live in the shared
  spine, so all three targets gain them at once: `Channel::send_transfer` authorizes a send AND
  delegates a capability from sender to recipient, **attenuated** by the same rules as
  `CapEngine::delegate` (a transfer can never amplify); the recipient receives a real, auditable,
  revocable registry token in `Message.cap`. `Channel::bounded` refuses a send to a full inbox
  fail-closed. All-or-nothing: an unauthorized send, an amplifying grant, or a full queue enqueues
  nothing and mints no token. Proved in `kernel-core/tests/invariants.rs`.
- **Verified green on every gate.** After the extraction + IPC extension: hosted `kernel-core` 17/17;
  **all three VM gates still pass** — aarch64 (`vm-e2e.sh`, exit 0), RISC-V (`vm-e2e-riscv.sh`, exit
  0), x86-64 (`smoke-test.sh`, exit 33) — re-proving 11 spine + memory + virtual-memory + user-mode
  invariants from the shared source. `clippy -D warnings` clean.
- **Deferred (Issue 1 follow-on):** extracting the remaining arch-independent primitives the register
  lists (task / process / address-space / scheduler / interrupt abstractions) into `kernel-core` so
  the per-target `usermode.rs`/`vm.rs`/`frames.rs` implement shared interfaces rather than parallel
  bespoke code; and the fuller cargo-workspace split.

## Delivered (2026-07-22 — P6 substrate: IPC tail, scheduler abstraction, security suite, traceability gate)

Four contract-honest bricks advancing the gap register's top-priority P6 items, plus the phased
architecture text for the hardware-bound issues. Every code brick is TDD'd, hosted-proved, and
clippy-clean; the aarch64 VM gate stays green (exit 0, all invariant markers). `kernel-core` hosted
suite grows **17 → 41** (6 suites).

- **IPC substrate tail** (gap Issue 2, ADR-020) — the IPC layer is consolidated into one
  arch-independent `kernel-core::ipc` module (re-exported from `spine` so all three targets and the
  selftest suite are unaffected) and extended with the primitives a real microkernel IPC needs beyond
  synchronous send + bounded queues + attenuated capability transfer: **asynchronous notifications**
  (coalescing seL4-style badge, capability-gated signal), **deadline/timeout-aware receive** (a
  message past its deadline is dropped, never delivered late — fail-closed), **cancellation** of an
  undelivered message by id, and **tracing + deterministic replay** (`replay()` reconstructs the exact
  delivered sequence from the trace alone). 9 new hosted tests.
- **Adversarial security-behaviour suite** (gap Issue 11) — permanent regressions that attack the
  capability engine as an adversary would: **confused deputy** (no ambient authority — `evaluate`
  consults only offered tokens), **capability laundering** (cannot mint fresh authority from a
  revoked/expired parent, nor launder a broader scope through a transfer), **TOCTOU / stale
  capability** (revocation is immediate — no cached authorization window), and **cross-principal
  leakage** (scope confinement + action-wildcard does not over-match a neighbouring namespace).
  9 hosted tests; all green (the engine holds).
- **Machine-checkable traceability gate** (gap Issue 12) — `docs/TRACEABILITY.md` is a machine-readable
  matrix of **45 requirements** (34 delivered, 2 partial, 9 deferred), each mapping
  ReqID → ADR → implementation → test → VM gate → status. `scripts/check-traceability.sh` (pure bash,
  no new CI dep) FAILS the build if any delivered/partial requirement lacks Implementation+Test
  evidence that exists in the tree, or carries an unknown status; deferred work is explicitly
  distinguished and never counted as delivered. Wired as the `traceability` job in both GitHub Actions
  and GitLab CI; negative-tested against three bad fixtures.
- **Arch-independent scheduler + task abstraction** (gap Issue 1, first extraction beyond the shared
  spine) — `kernel-core::sched`: `TaskId`, a `TaskState` lifecycle, a `TaskContext` backend seam
  (save/restore stays arch-specific), and a `RoundRobin` scheduler (FIFO fairness + block/unblock/
  finish transitions), lifting the scheduling **policy** the three targets' `usermode.rs` each
  hand-roll into one place, proved on the host (6 tests). Wiring each target's asm context switch to
  drive it is the documented follow-on (its asm is unchanged and still VM-gated).
- **Component signature verification** (gap Issue 7 / ADR-025 Phase 1, secure boot's hosted first
  slice) — a component is a content-addressed `Application` entity; its provenance is a detached
  HMAC-SHA256 signature over its content hash under a trusted key (`aletheia/src/provenance.rs`,
  `crypto::hmac_sha256` built on the existing `sha2`, RFC-4231-validated, no new dependency). Under an
  opt-in secure policy (`set_require_signed_components`, default OFF for back-compat), `run_installed`
  refuses a component whose stored signature is missing or does not verify (fail closed), and
  `install_signed_component` refuses an untrusted/tampered signature at install. 5 hosted tests +
  crypto/provenance unit tests; the aletheia hosted suite grows to **77 passed** with zero regressions.
  Asymmetric keys, a key hierarchy, measured boot, and rollback protection remain ADR-025 Phase 2–3.
- **Phased-plan ADRs 020–026** — contract-honest architecture text for the hardware-bound issues
  (SMP, AI execution substrate, device/driver model, persistent storage, secure boot, fault recovery)
  so no deferred requirement implies code that does not exist; each names its hosted-testable first
  slice where one exists.

## Delivered (2026-07-24 — REQ-SMP-002: SMP secondary bring-up + cross-core concurrency substrate, VM-gated at -smp 4)

The SMP cliff (gap #4, ADR-021 Phase 1) is broken: Aletheia now **boots and runs on multiple real
CPUs**. `kernel/src/smp.rs` (+ `boot.s::_secondary_start`) powers on every present secondary via the
PSCI `CPU_ON` firmware call (HVC conduit), gives each a private 16 KiB stack and per-CPU identity
(`TPIDR_EL1`), and enables its MMU over the SAME kernel page tables core 0 built — then proves the
cross-core substrate with **13 VM-gated invariants** (`scripts/vm-e2e.sh` now boots `-smp 4` and
asserts `ALL 13 SMP INVARIANTS HOLD`; with `-smp 1` the suite skips green like virtio-with-no-disk,
and the pinned gate makes a silent skip impossible).

- **Bring-up + identity (inv 1-3):** PSCI accepts CPU_ON for 3 secondaries; all come online with
  translation on; each core's MPIDR affinity + TPIDR_EL1 are distinct (per-CPU data works).
- **Cross-core memory model (inv 4-7):** 4 cores hammer one counter — the total is EXACT (real
  atomicity, no lost increments); a release/acquire mailbox publishes a payload every core observes
  and answers with a per-CPU transform. The kernel bump allocator moved from load-then-store to
  **CAS** (`heap.rs`) — the first removed single-core assumption (two cores could previously be
  handed the same bytes).
- **ADR-027 on REAL cores (inv 8-11):** the `with_authorization` atomic authorize+execute primitive
  — until now proved only under host threads — runs under the kernel's first **`SpinLock`** while 3
  secondaries commit and core 0 revokes: commits flow pre-revoke (progress-gated, never a fixed
  spin), the revoke linearizes inside the lock hold, **ZERO commits land after it**, and all 64
  post-revoke attempts per core fail closed. GAPS2 #9's mechanism is now SMP-proved.
- **IPI (inv 12):** a GICv2 **SGI** from core 0 is delivered to and acknowledged on every
  secondary's banked CPU interface (polled IAR with PSTATE.I masked — the secondaries never touch
  the core-0-owned vector table). Distributor state is restored after.
- **Stability (inv 13):** all secondaries park in WFE; online mask + counters unchanged.
- **Concurrency rules (load-bearing):** secondaries never print (PL011 unserialized); every engine
  access sits under the one SpinLock; liveness waits are progress-gated with CNTPCT deadlines.

Gates: aarch64 vm-e2e PASS (71 invariants incl. SMP 13) · riscv64 + x86-64 e2e PASS ·
conformance 3/3 PASS · kernel-core hosted 72 · clippy/fmt clean. **Honesty:** this is ADR-021
Phase 1 + the concurrency-substrate slice — per-CPU run queues/work stealing (Phase 2), TLB
shootdown, the lock-hierarchy/atomic-ordering audit, and x86-64/RISC-V bring-up parity stay open
under **REQ-SMP-001 (partial)**. Traceability: **54 reqs — 46 delivered / 5 partial / 3 deferred**.

## Delivered (2026-07-22 — REQ-CAP-006: capability concurrency semantics, the SMP prerequisite)

The gate the audit itself put **before** SMP (GAPS2 #9 "the capability model needs a formal
concurrency specification before SMP"; #9 precedes #4). Until now the capability engine's safety
rested on an implicit single-core assumption: Rust's borrow checker serializes a `&self` `evaluate`
against a `&mut self` `revoke`. SMP breaks that — two cores behind one lock can interleave the
pipeline's *authorize* and *execute* steps, so an effect acts on a capability revoked in the gap
(a classic time-of-check/time-of-use bug). ADR-027 specifies the guarantee and `kernel-core` now
implements + proves it, contract-honest.

- **Specified (ADR-027):** authorization and the effect it authorizes commit inside **one critical
  section**; an effect executes only if its capability is live at that point; revocation is immediate
  and permanent (no cached authorization, no authority resurrection). **Option A** (single lock —
  re-check inside the critical section, no epochs) is chosen and built; **Option B** (generation/epoch
  tokens for a future lock-free authorize) is documented but deliberately NOT built (ADR-010 + YAGNI).
- **Implemented (additive):** `CapEngine::with_authorization(action, target, offered, commit)` runs
  the check and, iff `Allow`, the `commit` closure within one `&self` call — because `revoke` needs
  `&mut self`, under the engine lock no revoke can linearize between check and effect, making the
  TOCTOU gap **unrepresentable**. `CapEngine::authorize` is the read-only variant that also names
  *which* token matched (an `Authorization`); `evaluate`/`revoke`/`mint`/`delegate` signatures are
  **untouched** (both authorization paths route through one private `test_token` matcher, so they
  cannot drift). The `now` clock is fixed at construction, so it is not shared-mutable under concurrency.
- **Proved under REAL threads (`kernel-core/tests/cap_concurrency.rs`, 5 tests):** the naive
  `check(); … ; act();` pattern is stale by construction; then an `RwLock`-guarded engine is hammered
  by committer threads vs. a revoker (progress-gated so the Allow path is genuinely exercised
  regardless of thread-wakeup order — a fixed spin races the scheduler) and asserts the disciplined
  primitive **never** commits under a revoked capability and that **revocation is permanent** (a
  committer that observes revoke-completed can never then see `Allow`). `kernel-core` hosted suite
  **72 passed**; aarch64 VM gate still green (`[e2e] PASS`, exit 0); `clippy -D warnings` + `fmt` clean.

**Honesty (advisor):** this proves the **mechanism** under host threads — it does **not** prove an
SMP-safe kernel (none exists). Wiring `with_authorization` into each target's real trap/IPC path, plus
the TLB-shootdown / atomic-ordering audit, is the SMP integration still **deferred** under
**REQ-SMP-001** (gap #4, ADR-021); REQ-CAP-006 is the prerequisite spec that unblocks it. Traceability
green (**53 reqs — 45 delivered / 4 partial / 4 deferred**).

## Delivered (2026-07-22 — REQ-DRV-003: virtio-blk driver — the FIRST real hardware driver, VM-gated)

The named next slice of ADR-023, executed. Until now the `kernel_core::storage::BlockDevice` seam was
only ever backed by an in-memory `MemBlockDevice`; this brick implements it over a **genuine emulated
block device** — a **virtio-blk** driver over **modern (v2) virtio-mmio** on the aarch64 QEMU `virt`
dev backend (`kernel/src/virtio.rs`) — so the write-ahead journal (REQ-STOR-002) now runs over real
emulated storage. This closes gap-register Issue 5's "no concrete driver" hole. Contract-honest
(ADR-010): written outside-in, boot-verified; a wrong ring layout faults/hangs into the 60s VM
watchdog, never a silent pass.

- **Discovery + modern handshake.** Scans the 32 virtio-mmio slots for magic (`0x74726976`) +
  `DeviceID==2`; init = reset → `ACKNOWLEDGE|DRIVER` → feature negotiation (accept **only**
  `VIRTIO_F_VERSION_1` + `VIRTIO_BLK_F_FLUSH` when offered) → `FEATURES_OK` read-back → queue-0 setup
  → `DRIVER_OK`. **Fails closed** on a legacy (v1) transport — no silent wrong-mode driver.
- **Split virtqueue + request protocol.** A 3-descriptor chain (header / data / status) with `dsb`
  barriers around `QueueNotify` and the used-ring poll (the classic virtqueue ordering trap), a
  bounded poll (anti-hang), and descriptors carrying **physical** addresses (VA==PA under the identity
  map). `read_block`/`write_block`/`flush` each issue one request. The 4 KiB `BlockDevice` block maps
  to **8** 512-byte virtio sectors (sector = idx × 8).
- **No ambient authority (REQ-DRV-002 over the real device).** The driver holds only the frames it
  allocated; wrapped in `DeviceGuard`, every block op is authorized by the SAME `CapEngine` — proved
  live: no capability ⇒ no bytes move; a write capability's bytes land and read back.
- **5 VM-gated invariants** (`scripts/vm-e2e.sh`, exit 0, `ALL 5 VIRTIO-BLK INVARIANTS HOLD`):
  (1) device discovered + initialized; (2) capacity read matches the attached 1 MiB image (256 × 4 KiB
  blocks); (3) write→read-back virtqueue round-trip returns the written bytes; (4) `Journal` commit +
  a FRESH `recover` reproduce state from the device bytes alone (crash-consistency over real storage —
  the ADR-023 payoff); (5) capability-gated I/O via `DeviceGuard`. **Graceful skip** under bare
  `cargo run` (`[virtio] no device (skipped)`) so the disk-less runner stays green; the gate attaches
  the disk and forces modern mmio (`-global virtio-mmio.force-legacy=false` — QEMU defaults the mmio
  transport to legacy v1). `clippy` clean.
- **Two gotchas (documented in-code):** (1) QEMU's `virtio-blk-device` on `virt` presents **legacy
  (v1)** virtio-mmio unless `virtio-mmio.force-legacy=false`; the driver's version check caught this
  as a fail-closed init error before any I/O. (2) descriptor addresses are physical DMA targets — the
  identity map makes VA==PA, so frame-allocated ring/buffer addresses are handed to the device raw.
- **Deferred (umbrella REQ-DRV-001):** hotplug, DMA/IOMMU confinement, driver-crash isolation +
  supervisor restart (ADR-023 Phase 3), and the RISC-V/x86-64 virtio backends (the aarch64 driver is
  the reference — the same cross-target spread pattern as blocking-IPC/priority-inheritance).
  Traceability green: **52 requirements — 44 delivered / 4 partial / 4 deferred**.

## Delivered (2026-07-22 — REQ-DRV-002: capability-authorized device access)

Fourth P7 brick — the capability model extended to hardware (no ambient device authority).
`kernel-core/src/device.rs` `DeviceGuard` wraps any `BlockDevice` and gates every read/write/flush on
the SAME `CapEngine`, so device I/O is authorized exactly like an entity write or an IPC send. Proved
over the **real** `MemBlockDevice` — deny/allow decides actual bytes, not a registry boolean:

- **No capability ⇒ no I/O** (`tests/device.rs`): read/write/flush all `Denied`, nothing moves.
- **Read-only capability reads but cannot write** (attenuation): a `dev.blk.read` holder reads the
  real block yet a write is `Denied` and the device is confirmed unchanged.
- **Write capability's bytes actually land** and read back; a `dev.blk.*` wildcard authorizes both.

`kernel-core` **67 passed**; aarch64 VM gate green (compiles no_std); `clippy -D warnings` clean.
**Honesty (advisor):** NEW req **REQ-DRV-002** delivered; the umbrella **REQ-DRV-001** (device
discovery, a real hardware driver, hotplug, DMA/IOMMU, restart) stays `partial` — the concrete
**virtio-blk driver, which will implement this very `BlockDevice` trait**, is the named next slice,
deferred (ADR-023, hardware-bound, ADR-010). A precise **virtio-blk implementation plan** (virtio-mmio
discovery → feature negotiation → split-virtqueue setup → request protocol → `BlockDevice` impl → QEMU
wiring → VM-gated invariants) is now written into ADR-023 so a fresh-context session executes it in one
focused pass. Traceability green (51 reqs — 43 delivered / 4 partial / 4 deferred).

## Delivered (2026-07-22 — REQ-CONF-001: cross-architecture semantic conformance gate, GAPS2 #2)

Third P7 brick — the consolidation the audit called the #1 systemic risk: *silent behavioral
divergence* between the three CPU backends. `scripts/conformance.sh` boots all three targets and
asserts each proves the **same core semantic contract** — 10 arch-neutral named behaviors
(capability-secure cross-AS IPC, the grant-table's cap-gate/zero-copy/revoke, blocking IPC's
block/wake/resume, and priority inheritance's inversion-avoidance/service/receive).

- **Spec'd on named behaviors, not invariant counts** (advisor): the contract substrings deliberately
  omit the privilege term (el0/u-mode/ring3), the address-space term (TTBR0/satp/PML4), and the trap
  term (svc/ecall/int 0x80), so a genuine behavior matches on every arch. Per-arch invariants
  (x86-64's 46 total vs aarch64/RISC-V's 53 — long mode can't do the MMU-off→on flip) are **extensions**
  reported informationally, never failures. x86-64 is SKIPPED (never silently passed) where the host
  lacks the image toolchain.
- **Result:** all three targets PASS all 10 core behaviors — the "one coherent kernel, three backends"
  thesis is now machine-checked, not asserted. Traceability green (50 reqs — 42 delivered / 3 partial
  / 5 deferred). Follow-on: wire as a CI job alongside `e2e-all`; extend the contract as new
  cross-target behaviors land.

## Delivered (2026-07-22 — REQ-STOR-002: crash-consistent journaled block store)

Second P7 brick (persistent storage, ADR-024). A general-purpose OS needs storage that survives power
loss without corruption. `kernel-core/src/storage.rs` delivers the arch-independent middle of that
stack: a **write-ahead journal** over an abstract `BlockDevice` seam (a real virtio-blk driver,
REQ-DRV-001, will implement the same trait). `alloc`-only — the journal core is kernel-portable.

- **Atomic commit protocol:** write the redo data to the journal area → flush → write a **checksummed
  commit record** → flush (*the atomic pivot*) → apply to home blocks → flush.
- **Recovery is binary:** if the commit record's magic is absent or its checksum (over header **and**
  journal payload) fails, the transaction is uncommitted and nothing is applied; otherwise the journal
  is replayed idempotently. So for **every** crash point recovery yields the pre- or the fully-applied
  state — never torn.
- **Proved (`kernel-core/tests/storage.rs`, 5 tests):** the load-bearing **crash-at-every-prefix
  sweep** (capture a 2-block txn's ordered writes; for every prefix K, materialize the device with
  only the first K writes, recover, assert both home blocks are pre OR fully post — never torn); a
  torn commit record → rolled back; a torn journal payload → rolled back (checksum load-bearing,
  corruption surfaced not swallowed); full-commit replay is idempotent; a blank device recovers to
  nothing (fail closed). `kernel-core` hosted suite **63 passed**; aarch64 VM gate still green (the
  new module compiles no_std); `clippy -D warnings` clean.

**Honesty (advisor):** NEW requirement **REQ-STOR-002** delivered. The umbrella **REQ-STOR-001**
(full stack: real driver, filesystem/object store, encryption-at-rest layer, semantic-store-on-
persistent) stays `partial` — the journal is its crash-consistent middle; the ends are follow-ons
(REQ-DRV-001 driver next). Traceability green (49 reqs — 41 delivered / 3 partial / 5 deferred).

## Delivered (2026-07-22 — REQ-BOOT-002: asymmetric component provenance, ed25519 + key hierarchy)

First P7 brick (secure-boot Phase 2, ADR-025). Phase 1's `TrustStore` was **symmetric** HMAC — the
verifier held the secret and could therefore forge. This wave adds **asymmetric** provenance in
`aletheia/src/provenance.rs` (ed25519, pure-Rust dalek; ADR-004-consistent, no C toolchain):

- **`SigningIdentity`** holds the PRIVATE key and signs a component's content hash; **`AsymTrustStore`**
  holds trusted **public keys ONLY** — so possession of the verifier's trust anchor confers **no
  ability to sign**. A compromised verifier still cannot forge (the property Phase 1 lacked).
- **Root→signing-key hierarchy:** a trusted root ENDORSES a component-signing key (`endorse`), which
  signs components; `verify_chain` accepts a component only if a trusted root endorsed its signer AND
  the signer signed the component — signing authority can be delegated/rotated without trusting every
  signer directly.
- **Fail-closed:** empty store verifies nothing; malformed keys/signatures are verification failures
  (never mistaken for valid); tamper (different hash), unendorsed signer, and untrusted-root
  endorsement are all rejected.
- **Proved:** 3 new hosted tests (`provenance.rs`, fixed-seed keypairs for determinism) + the 3
  legacy HMAC tests unchanged; aletheia suite **81 passed**; `clippy -D warnings` clean.

**Honesty (advisor):** this is a NEW requirement **REQ-BOOT-002**, marked delivered. The full
**REQ-BOOT-001 "secure boot + chain of trust"** stays `partial`: a firmware→bootloader→kernel measured
chain with a hardware root of trust (TPM/secure enclave) and anti-rollback (a monotonic counter needs
persistent secure storage) are hardware-bound (ADR-025 Phase 3) — designed, not claimed. Traceability
green (48 reqs — 40 delivered / 2 partial / 6 deferred).

## Delivered (2026-07-22 — blocking IPC + priority inheritance on ALL THREE targets; x86 exit-race fixed)

Closing the divergence the aarch64-only blocking IPC opened (GAPS2 #2, advisor's steer): REQ-IPC-010
blocking IPC and REQ-IPC-009 priority inheritance are now proved through the real user-mode path on
**all three first-class targets** — aarch64 EL0 (TTBR0), RISC-V U-mode (satp), x86-64 ring-3 (PML4) —
each **22 boundary invariants**, VM-gated:

- RISC-V: `run_blocking_ipc` + `run_priority_ipc` in `kernel-riscv64/src/usermode.rs`, new
  `_stub_recv_exit` asm stub, a0(`regs[10]`) delivery; `scripts/vm-e2e-riscv.sh` "ALL 22 … HOLD".
- x86-64: same in `kernel-x86_64/src/usermode.rs`, new `stub_recv_exit` asm stub, rdi(`regs[RDI]`)
  delivery; `kernel-x86_64/scripts/smoke-test.sh` "ALL 22 … HOLD", exit 33.
- Per-target pieces (advisor: highest-asm-content spread): a two-syscall receiver stub in each ISA,
  the `IPC_BLOCK_MODE` recv branch in each trap dispatcher, frame-register delivery per `TrapFrame`
  layout, and the priosched wiring — composed from each target's existing syscall stubs.

**Bug fixed (x86 exit-race):** the extra invariants exposed a latent race — `kmain` re-enabled
interrupts after the ring-3 suite, so a PIT IRQ latched during the suite fired between "[e2e] PASS"
and `exit(0)`; with no live scheduler left, `resume_return` jumped into the last excursion's stale
`KERNEL_CTX` → triple fault → QEMU exit 255 (the "x86 exit-255 flake"). Fixed by keeping IF=0 through
the halt/exit (as aarch64/RISC-V already do). x86 now exits 33 deterministically.

`check-traceability.sh` green (47 reqs — 39 delivered / 2 partial / 6 deferred); REQ-IPC-009/010 rows
name all three gates (GAPS2 #1). The IPC substrate (gap Issue 2) is delivered cross-target.

## Delivered (2026-07-22 — REQ-IPC-009 → delivered: priority inheritance proved end-to-end on aarch64)

The payoff of the blocking-IPC vehicle: REQ-IPC-009 priority inheritance is now proved **end-to-end
through the real blocking-IPC path** (not a hosted re-run of coherent-by-construction policy — the bar
the advisor + GAPS2 #5 set). New EL0 invariants **20-22** in `kernel/src/usermode.rs::run_priority_ipc`,
VM-gated by `scripts/vm-e2e.sh` (now **ALL 22 EL0-BOUNDARY INVARIANTS HOLD**, exit 0):

- **20 — inversion avoided at the real dispatch point:** a HIGH-priority EL0 receiver blocks on the
  endpoint a LOW-priority task services; the blocked HIGH `wait`s on that endpoint (held by LOW) so
  `kernel_core::priosched::PriorityScheduler` boosts LOW's effective priority to HIGH — and
  `schedule_next` dispatches the boosted LOW **ahead of a Ready MEDIUM** task. A priority-blind
  scheduler would have run MEDIUM and starved HIGH indirectly.
- **21 — the boosted LOW services:** the dispatched LOW runs and sends, waking the blocked HIGH.
- **22 — HIGH receives:** HIGH resumes as the highest-priority task and receives the body across the
  two distinct address spaces.

MEDIUM is a scheduler-only Ready competitor (the proof is the dispatch *decision* under real
contention, not MEDIUM's execution). `clippy -D warnings` clean; `check-traceability.sh` green:
**47 requirements — 39 delivered / 2 partial / 6 deferred**. The **entire IPC substrate scope
(gap Issue 2) is now delivered** — synchronous, transfer+attenuation, bounded queues, async
notifications, timeout, cancellation, trace/replay, zero-copy shared memory (all-target real MMU),
blocking IPC, and priority inheritance. Follow-on: spread blocking-IPC + priority inheritance to
x86-64/RISC-V (the aarch64 proof is the reference); GAPS2 #5 closed on the dev backend.

## Delivered (2026-07-22 — REQ-IPC-010: real blocking IPC on aarch64, the vehicle for priority inheritance)

The chosen next feature (advisor: "the cheap-conversion phase is over; pick a feature"). Until now the
kernel IPC endpoint was a non-blocking single-slot mailbox (`recv` on empty returned a fail-value).
This wave adds **real blocking IPC** on the aarch64 dev backend — the substrate REQ-IPC-009 priority
inheritance needs to be VM-proved end-to-end. New EL0 invariants **17-19** in
`kernel/src/usermode.rs::run_blocking_ipc`, proved live by `scripts/vm-e2e.sh` (now **ALL 19
EL0-BOUNDARY INVARIANTS HOLD**, exit 0):

- **17 — recv blocks:** a receiver that `recv`s an EMPTY endpoint is genuinely descheduled — the
  handler signals the block, and the scheduler moves it to `Blocked` (`kernel_core::sched` block()).
- **18 — send wakes + delivers:** the sender's `send` deposits the body; because a receiver is
  blocked-waiting, the kernel wakes it (`unblock` ⇒ `Ready`) and delivers the body across the two
  distinct TTBR0 address spaces (into the receiver's saved `x0`).
- **19 — receiver resumes:** the woken receiver resumes *past its `svc`* with the body in `x0` and
  exits reporting it — proving the block→wake→deliver→resume round trip, not just a mailbox drain.

Guarded so every other test is untouched: blocking is behind an `IPC_BLOCK_MODE` flag (default off),
so `run_ipc`'s non-blocking mailbox semantics are unchanged. `clippy -D warnings` clean;
`check-traceability.sh` green. **This is the vehicle, not the priority proof:** REQ-IPC-009 stays
`partial` until a follow-on drives this blocking endpoint under `PriorityScheduler` with a 3rd
medium-priority task, showing the boosted holder runs ahead of it (priority inversion avoided) — the
genuine end-to-end priority-inheritance VM proof (GAPS2 #5).

## Delivered (2026-07-22 — REQ-IPC-008 → delivered: grant-table through the REAL aarch64 MMU path)

Converting the zero-copy shared-memory grant-table from hosted-only (`partial`) to VM-gated
(`delivered`) by driving it through the real per-target path — the honesty currency this project runs
on (GAPS2 #3: "the real path is what matters"), the same move that made REQ-KERN-005 real. The shared
`GrantTable` is the arch-independent authority/lifecycle layer; the aarch64 `vm.rs` performs the actual
mapping (the documented seam). New EL0 invariants **14-16** in `kernel/src/usermode.rs::run_shared_memory`,
proved live by `scripts/vm-e2e.sh` (now **ALL 16 EL0-BOUNDARY INVARIANTS HOLD**, exit 0):

- **14 — capability-gated (fail-closed):** without a `memory.share` capability the grant is refused and
  nothing is mapped; with it, the share is authorized through the SAME `CapEngine` the pipeline uses.
- **15 — zero-copy across address spaces:** the grant maps ONE physical frame into TWO distinct
  process TTBR0 roots, and both resolve the shared VA to the SAME physical frame — one page present in
  two separate address spaces is exactly zero-copy shared memory (no copy through any queue).
- **16 — revocation unmaps:** revoking the grant tears down the grantee's page mapping while the
  grantor keeps its own access (the per-target "revoke ⇒ unmap" seam).

`check-traceability.sh` green; `clippy -D warnings` clean on the aarch64 kernel.

**All three targets now prove it** (cross-target, GAPS2 #2): the identical `run_shared_memory` invariants
14-16 pass on aarch64 (TTBR0, `scripts/vm-e2e.sh`), RISC-V (Sv39 satp, `scripts/vm-e2e-riscv.sh`, "ALL
16 USER-MODE BOUNDARY INVARIANTS HOLD"), and x86-64 (PML4, `kernel-x86_64/scripts/smoke-test.sh`, "ALL
16 RING-3 BOUNDARY INVARIANTS HOLD", exit 33) — one arch-independent `GrantTable` authority layer, three
real per-target MMU backends. **Follow-on (GAPS2 #3):** wiring EL0/ring-3/U-mode code itself (not just
the kernel-verified mapping) to read/write the shared page across the boundary.

## Delivered (2026-07-22 — GAPS2 Issue #1: target-specific traceability, no gate can be escaped)

A second architecture audit (`docs/ARCHITECTURE-GAPS2.md`) flagged that the traceability matrix proved
*evidence files exist* but not that the *correct target* executes them: the generic `REQ-USER-*` /
`REQ-MEM-*` rows listed several implementations yet omitted `kernel-x86_64/scripts/smoke-test.sh` from
their VM-Gate column, so an x86-64-specific user-mode or memory regression could compile and **escape
the requirement gate**. Fixed by splitting those into **one row per target**, each naming that target's
own implementation and its own VM gate:

- `REQ-USER-AARCH64-001` / `REQ-USER-X86-001` / `REQ-USER-RISCV-001` (EL0 / ring-3 / U-mode, each →
  `vm-e2e.sh` / `smoke-test.sh` / `vm-e2e-riscv.sh`).
- `REQ-MEM-AARCH64-001` / `REQ-MEM-X86-001` / `REQ-MEM-RISCV-001` (frame allocator + MMU per target →
  the same three gates). (Kernel-boot rows `REQ-KERN-001/002/003` were already per-target.)

A regression in one target's user-mode now fails **that target's** named gate, not a sibling's.
`check-traceability.sh` green: **46 requirements — 36 delivered / 4 partial / 6 deferred** (the
authoritative counts; dated sections below are point-in-time snapshots). Remaining GAPS2 items tracked:
cross-target conformance suite (#2), IPC-transfer through the real per-target user-mode path (#3),
SMP + capability concurrency spec (#4/#9), end-to-end VM-tested priority inheritance (#5), real
secure-boot chain (#6), persistent storage (#7), fault supervision (#8).

## Delivered (2026-07-22 — REQ-KERN-005: aarch64 target DRIVES the shared kernel-core scheduler, VM-gated)

The "wire, don't pile" brick: instead of adding a fourth unwired kernel-core policy module, this wave
makes a real target *drive* the shared scheduler, converting REQ-KERN-005 from `partial` (policy +
hosted tests, nothing drove it) to `delivered` (a target uses it, VM-gated) — the honest delivered bar.

- The aarch64 dev backend's cooperative multitasking (`kernel/src/usermode.rs::run_scheduler`) no
  longer hand-rolls its `(cur+k)%NTASK` rotation. It now drives `kernel_core::sched::RoundRobin`:
  `schedule_next` decides which EL0 task runs next, a yielded task is rotated to the tail, an exited
  task is `finish`ed and leaves the rotation. The target performs ONLY the context-switch *mechanism*
  (`resume_frame` + TTBR0 address-space switch) behind the `TaskContext` seam.
- Reproduces the exact `A,B,A,B,A,B,A,B` sequence the bespoke loop did — **VM-proved live**, not just
  argued: `scripts/vm-e2e.sh` re-passes EL0 invariants 6 (round-robin to completion), 7 (each task
  resumes with its own register-magic at the shared VA), 8 (distinct TTBR0 spaces), and 9 (timer
  preemption round-robins) with the shared scheduler in the loop; exit 0, all 11+7+13+13 invariants.
- **ALL THREE first-class targets now drive the one shared scheduler** — RISC-V
  (`kernel-riscv64/src/usermode.rs`, VM-gated by `scripts/vm-e2e-riscv.sh`, U-mode invariants 6/8) and
  x86-64 (`kernel-x86_64/src/usermode.rs`, VM-gated by `kernel-x86_64/scripts/smoke-test.sh`, ring-3
  invariant 6, exit 33) drive the identical `kernel_core::sched::RoundRobin` via the same swap. Each
  target performs ONLY its arch context switch behind the `TaskContext` seam — the "one coherent
  kernel, three backends" thesis (gap Issue 1 / GAPS2 Issue 2) is now real for the scheduler policy.
- **Follow-on (documented):** driving the `PriorityScheduler`/`GrantTable` from a target (the path to
  REQ-IPC-008/009 `delivered`, GAPS2 Issues 3/5), and a formal cross-target conformance suite
  (GAPS2 Issue 2). (Pre-existing, non-CI-gated clippy lints in the x86-64 kernel — `gdt.rs` descriptor
  dead-code, `run_in_space` arg count — are untouched by this brick; a separate cleanup.)

## Delivered — kernel-core policy, PARTIAL (2026-07-22 — REQ-IPC-009: priority inheritance + priority-aware scheduling)

**Status honesty (traceability `partial`):** this is the arch-independent *policy* + hosted proof; it
is the same shape as REQ-KERN-005 — no target drives it and no VM gate exercises it yet, so it is
`partial`, not `delivered`, until wired into a target's `usermode.rs` and VM-gated (the accumulated
kernel-core policy — KERN-005 scheduler, IPC-008 grant-table, IPC-009 priority scheduler — is wired in
one target-integration brick next).

The second remaining IPC scope item, closing the IPC substrate's kernel-core policy work (gap Issue 2
/ ADR-020). The round-robin scheduler is fair but priority-blind, so it is prey to **unbounded
priority inversion**: a high task H blocks on an endpoint a low task L holds while an unrelated medium
task M preempts L forever. `kernel-core/src/priosched.rs` (`PriorityScheduler`) breaks this the way a
real microkernel does, as arch-independent policy inherited by all three targets:

- **Priority donation** — when a task `wait`s on an endpoint held by another, the holder's *effective*
  priority rises to the waiter's, and **transitively** to anything blocked behind it across a chain of
  held endpoints. Effective priority is derived on read (a visited set makes a deadlock cycle
  terminate rather than hang), so `release` withdraws donation automatically.
- **Inversion avoided** — `schedule_next` runs the highest-effective-priority Ready task (FIFO
  tiebreak), so a boosted low holder outranks an unrelated medium task and finishes its critical
  section; the inversion is bounded to that section.
- **Capability-gated** — acquiring or waiting on a kernel endpoint is authorized by the SAME
  `CapEngine` (no ambient endpoint access); every refusal is fail-closed.
- **Hand-off on release** — releasing an endpoint hands it to its highest-priority waiter, which is
  unblocked and becomes the new holder.
- **Per-target seam (ADR-010):** the scheduling *policy* is here; the actual register save/restore +
  address-space switch stays each target's `TaskContext` seam (same split as `kernel-core::sched`).
- **Proved on the host** — `kernel-core/tests/priosched.rs` (9 tests): base-with-no-donors, cap-gated
  fail-closed, acquire/busy/wait semantics, single + transitive inheritance, inversion avoidance,
  release-withdraws-and-hands-off, and non-holder-release fail-closed. `kernel-core` hosted suite now
  **58 passed** (8 suites); `clippy -D warnings` clean; `check-traceability.sh` green (34 delivered / 5
  partial / 6 deferred — IPC-008/009 are `partial`: kernel-core policy proved, per-target wiring +
  VM gate pending). The IPC scope's remaining items are the per-target wiring of this policy and
  wiring `send_transfer` into each target's cross-address-space `usermode.rs` fast-path.

## Delivered — kernel-core policy, PARTIAL (2026-07-22 — REQ-IPC-008: zero-copy shared-memory grant-table)

**Status honesty (traceability `partial`):** arch-independent *policy* + hosted proof, not yet driven
by any target or VM gate — `partial` like REQ-KERN-005 until wired into a target's `vm.rs` mapping and
VM-gated.

The bulk-data companion to the message-copy `Channel`, closing the first of the two remaining IPC
scope items (gap Issue 2 / ADR-020). The synchronous fast-path copies a message body into the
receiver's inbox — correct for control messages, wrong for a page of data. A real microkernel shares
one physical frame region between endpoints under explicit authority; this wave delivers the
**arch-independent authority + lifecycle layer** of that mechanism in `kernel-core/src/grant.rs`
(`GrantTable`), inherited by all three targets from one source.

- **Capability-gated establishment** — a share requires `memory.share` authority checked through the
  SAME `CapEngine` the pipeline uses; no capability ⇒ no grant (fail-closed).
- **Attenuation, never amplification** — a grant can only narrow the grantor's own access; a read-only
  holder can never mint a read-write grant (the memory analogue of `CapEngine::delegate`).
- **Zero-copy backing** — the region's bytes live exactly once (`Rc<RefCell<[u8]>>`); a read-write
  holder's write is observed by every reader with no copy through any queue, made observable by
  `region_refcount` (rises per live grant, falls on revoke).
- **Bounded access** — every read/write is confined to `[0, len)` (the model of the MMU refusing an
  access past the shared frame); an `offset+len` overflow is refused, never wraps.
- **Revocation unmaps** — revoking a grant drops that endpoint's handle fail-closed (later access
  denied) and releases its share of the backing.
- **Per-target seam (ADR-010):** turning a granted region into a real page-table mapping in each
  endpoint's address space is each target's `vm.rs` (map/unmap already delivered) — the same split by
  which `kernel-core::sched` owns the scheduling policy while each target owns the context switch.
- **Proved on the host** — `kernel-core/tests/grant.rs` (8 tests): cap-gated, zero-copy read-sees-write,
  read-only-cannot-write, attenuate-but-never-amplify, no-share-without-access, bounded, and
  revocation-unmaps-and-releases. `kernel-core` hosted suite **49 passed** at this brick (7 suites);
  `clippy -D warnings` clean. Traceability status `partial` (kernel-core policy proved; per-target
  `vm.rs` mapping + VM gate pending).

## Run it

```bash
cd aletheia
cargo test        # 67 passed — M1 acceptance + conformance + property + security + P2 component + policy + AI
cargo run -- serve  # long-running Core Alpha behind the Unix-socket IPC boundary (clients issue Requests)
cargo test --test component   # the 14 P2 WASM-component acceptance + fuzz tests
cargo run         # aletheiad: boots the hosted System Core + runs the UC-001..004 demo with traces

(cd ../kernel-core && cargo test)  # 41 passed — the shared spine invariants + IPC substrate (async/timeout/cancel/trace-replay) + adversarial security-behaviour suite + arch-independent scheduler, proved on the HOST, no QEMU
./scripts/check-traceability.sh    # requirement traceability gate: every delivered/partial requirement maps to existing impl+test evidence (gap Issue 12)

./scripts/e2e-all.sh         # ONE command, all three targets: aarch64 + RISC-V QEMU gates + x86-64 disk-image smoke-test -> single PASS/FAIL
./scripts/vm-e2e.sh          # aarch64 microkernel in QEMU: 11 spine + 7 memory + 13 virtual-memory + 13 EL0 user-mode invariants + exit 0
./scripts/vm-e2e-riscv.sh    # RISC-V/RV64GC first-class target (QEMU virt + OpenSBI, S-mode): 11 spine + 7 memory + 13 Sv39 vm + 13 U-mode invariants + exit 0
./scripts/linux_pipe_bench.sh # real-Linux IPC baseline for the perf discussion (needs Docker)
```

### Boot the OS end-to-end

```bash
# The NEW P5 memory-management work (frame allocator + MMU) runs on the aarch64 dev backend.
# Boot it directly as a -kernel ELF in QEMU (this IS the e2e VM test):
cd kernel && cargo run          # boots Aletheia, proves 11+7+13+10 invariants live (incl. EL0 user-mode + preemptive multitasking), exits 0

# A real bootable DISK IMAGE (Aletheia as its own OS on AMD64/x86-64 under UEFI):
cd kernel-x86_64 && bash scripts/build-image.sh   # -> build/aletheia-x86_64.{img,vmdk}
bash scripts/smoke-test.sh                         # boot the image in QEMU+OVMF, assert exit 33
#   • QEMU:       qemu-system-x86_64 -bios <OVMF_CODE.fd> -drive format=raw,file=build/aletheia-x86_64.img -serial stdio
#   • VMware:     attach build/aletheia-x86_64.vmdk to a UEFI VM
#   • VirtualBox: attach build/aletheia-x86_64.img (see scripts/build-vbox.sh)
# NOTE: the x86-64 image now proves 7 memory (frame allocator from the UEFI map) + 6 virtual-memory
# (map/unmap over the live UEFI PML4 hierarchy) + 11 spine invariants. x86-64 can't do aarch64's
# "MMU off->on" flip (long mode requires paging), so its vm suite proves the honest subset: walk +
# edit the live hierarchy. smoke-test.sh gates all three marker families + exit 33.
```
