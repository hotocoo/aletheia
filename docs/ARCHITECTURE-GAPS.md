# Aletheia OS — Architecture Gap Register

> **Raised by:** GPT-5.5 (OpenAI)
>
> **Analysis date:** 2026-07-21
>
> **Repository:** `hotocoo/aletheia`
>
> **Purpose:** This document records implementation gaps identified through repository inspection and architecture/status analysis. Each section is written as a proposed GitHub issue and can be split into a standalone issue.

---

## Executive Assessment

Aletheia has already established a strong architectural foundation: a capability-oriented security model, a microkernel direction, target-specific kernel work, physical memory management and virtual-memory work on the strongest architecture path, process isolation foundations, preemptive scheduling work, a capability-aware AI/context layer, WASM component isolation, VM boot gates, and substantial invariant/property testing.

The primary risk is now **architectural imbalance**: the AI/security architecture is ahead of the conventional operating-system substrate required to make Aletheia a complete, independently usable operating system.

The next major phase should prioritize:

```text
kernel-core
    ↓
IPC
    ↓
Cross-architecture process isolation
    ↓
SMP
    ↓
Drivers + devices
    ↓
Persistent storage + recovery
    ↓
Secure boot + trust chain
    ↓
AI execution substrate
    ↓
Native experience layer
The AI-native architecture should remain a first-class design constraint throughout this work, but the underlying OS substrate must be completed before adding more high-level AI features.
Proposed Issue 1 — P6: Complete the Architecture-Independent kernel-core Substrate
Priority: Critical
Category: Kernel / Architecture
Raised by: GPT-5.5 (OpenAI)

> Progress note (2026-07-21): first slice delivered. The `kernel-core/` crate now holds the shared
> `Hal` trait AND the entire capability-secure **spine** (`spine.rs`) + the **invariant selftest
> suite** (`selftest.rs`), which were previously `#[path]`-copied into each target crate. All three
> targets (`kernel/`, `kernel-x86_64/`, `kernel-riscv64/`) depend on the single source and provide
> only their own `hal.rs` backend + console — closing acceptance criterion #1 (core abstractions not
> duplicated across arch crates) for the spine + invariant suite. Criterion #5 ("arch-independent
> invariants run in hosted tests") is met by `kernel-core/tests/invariants.rs` (13 host tests, no
> QEMU). Criterion #4 held: all three VM gates stay green. STILL OPEN: extracting the task / process
> / address-space / scheduler / IPC-endpoint / memory / interrupt abstractions so the per-target
> `usermode.rs`/`vm.rs`/`frames.rs` implement shared `kernel-core` interfaces rather than parallel
> bespoke code, and the fuller cargo-workspace split.
>
> Progress note #2 (2026-07-22): the **scheduler + task abstraction** is now extracted into
> `kernel-core/src/sched.rs` — `TaskId`, a `TaskState` lifecycle, a `TaskContext` backend seam
> (save/restore stays arch-specific), and a `RoundRobin` scheduler (FIFO fairness + block/unblock/
> finish). This lifts the scheduling POLICY the three targets' `usermode.rs` each hand-roll into one
> arch-independent place, proved on the host (`kernel-core/tests/sched.rs`, 6 tests). STILL OPEN:
> wiring each target's asm context switch to drive this scheduler (the asm is unchanged and still
> VM-gated), extracting the address-space / memory / interrupt abstractions, and the cargo-workspace
> split.
>
> Progress note #3 (2026-07-22): the **aarch64 dev backend now DRIVES** `kernel_core::sched::RoundRobin`
> in `kernel/src/usermode.rs::run_scheduler` instead of its hand-rolled `(cur+k)%NTASK` rotation —
> `schedule_next`/`finish` decide the order, the target performs only the context-switch mechanism
> (`resume_frame` + TTBR0 switch) behind the `TaskContext` seam. VM-gated: `scripts/vm-e2e.sh` re-passes
> EL0 invariants 6–9 (round-robin to completion, per-slice register-magic, distinct spaces, timer
> preemption) with the shared scheduler in the loop (exit 0). This converts REQ-KERN-005
> `partial → delivered`. STILL OPEN: wiring x86-64 and RISC-V `usermode.rs` to drive the same
> scheduler, driving the `PriorityScheduler`/`GrantTable` from a target (the REQ-IPC-008/009 path), the
> remaining address-space / memory / interrupt abstractions, and the cargo-workspace split.

Problem
Core kernel abstractions risk becoming distributed across architecture-specific implementations. This creates the possibility that AArch64, x86-64, and RISC-V evolve into partially independent kernels rather than hardware backends implementing one coherent Aletheia kernel model.
Goal
Extract architecture-independent kernel primitives into a real shared kernel-core substrate.
Scope
Task abstraction
Process abstraction
Address-space abstraction
Capability primitives
IPC endpoint abstraction
Scheduler interfaces
Memory-management interfaces
Interrupt abstraction
Architecture-independent invariant suite
Explicit architecture backend boundaries
Target Architecture
kernel-core
├── task
├── process
├── address_space
├── capability
├── ipc
├── scheduler
├── memory
├── interrupt
└── invariants

kernel-aarch64
kernel-x86_64
kernel-riscv64
Acceptance Criteria
Core kernel abstractions are not duplicated across architecture crates.
All supported targets compile against the same core interfaces where applicable.
Hardware-specific implementations are isolated behind explicit backend interfaces.
Existing VM gates remain green.
Architecture-independent invariants run in hosted tests.
Dependencies
This should precede significant expansion of target-specific kernel features.
Proposed Issue 2 — Implement Real Capability-Secure Kernel IPC
Priority: Critical
Category: Kernel / Security
Raised by: GPT-5.5 (OpenAI)

> Progress note (2026-07-21): a first kernel-mediated IPC endpoint is delivered on BOTH user-mode
> targets — aarch64 (`kernel/src/usermode.rs`, EL0 invariants 11-13) and x86-64
> (`kernel-x86_64/src/usermode.rs`, ring-3 invariants 11-13) and RISC-V
> (`kernel-riscv64/src/usermode.rs`, U-mode invariants 11-13), VM-gated. Two processes in separate
> address spaces exchange a message body through a kernel endpoint, authorized by the same
> `CapEngine` (`ipc.send`/`ipc.recv`); unauthorized send/recv is rejected fail-closed.
>
> Progress note #2 (2026-07-21): **capability transfer through IPC + attenuation** and **bounded
> message queues** are now delivered in the shared `kernel-core` spine (`Channel::send_transfer`,
> `Channel::bounded`, `Message.cap`, `CapGrant`). A capability-transferring send authorizes the send
> AND delegates a capability from the sender to the recipient, attenuated by the SAME rules as
> `CapEngine::delegate` — so a transfer can never amplify beyond what the sender holds; the recipient
> receives a real, auditable, revocable registry token in `msg.cap`. All-or-nothing + fail-closed: an
> unauthorized send, an amplifying grant, or a full bounded queue enqueues nothing and mints no token.
> Proved in hosted arch-independent tests (`kernel-core/tests/invariants.rs`). Because it lives in the
> shared spine, all three targets gain it at once. STILL OPEN: asynchronous notifications,
> timeout/cancellation, zero-copy shared-memory channels, priority inheritance, IPC trace/replay, and
> wiring `send_transfer` into the per-target cross-address-space usermode IPC path.
>
> Progress note #3 (2026-07-22): **asynchronous notifications, deadline/timeout-aware receive,
> cancellation, and IPC tracing + deterministic replay** are now delivered in the shared
> `kernel-core::ipc` module (consolidated from `spine`; re-exported so all three targets + the
> selftest suite are unaffected), each authorized by the same `CapEngine` and proved fail-closed
> (`kernel-core/tests/ipc.rs`, 9 tests). Design + phasing recorded in ADR-020. STILL OPEN:
> zero-copy shared-memory channels (needs a frame grant-table), priority inheritance (needs the
> extracted scheduler wired into the IPC blocking path), and wiring `send_transfer` into each
> target's cross-AS usermode fast-path.
Problem
Aletheia's microkernel architecture requires IPC to be a first-class kernel primitive. Higher-level services, components, applications, AI agents, and device services must communicate through explicit capability-authorized boundaries rather than ad hoc hosted APIs or Unix-socket-style abstractions.
Goal
Implement a real kernel IPC substrate suitable for building the entire OS above it.
Scope
Kernel-mediated endpoint objects
Synchronous IPC
Asynchronous notifications
Capability transfer through IPC
Capability attenuation during transfer
Shared-memory channels with explicit authorization
Bounded message sizes
Timeout semantics
Cancellation
Zero-copy paths where appropriate
Deadlock avoidance
Priority inheritance/donation design
IPC tracing and replay support
Target Model
Process A
    │
    │ capability-authorized message
    ▼
Kernel Endpoint
    │
    ▼
Process B
Acceptance Criteria
Two isolated processes can communicate through kernel-mediated capabilities.
Unauthorized endpoint access is rejected.
Capability transfer is explicit and auditable.
IPC timeout and cancellation semantics are tested.
IPC behavior is covered by property tests and VM gates.
Dependencies
Depends on the core process and capability abstractions from Issue 1.
Proposed Issue 3 — Complete Cross-Architecture User Mode and Process Isolation
Priority: Critical
Category: Kernel / Memory / Security
Raised by: GPT-5.5 (OpenAI)
Problem
The strongest isolation and user-mode implementation is currently architecture-dependent. x86-64 and RISC-V require equivalent user-mode, address-space, MMU, and preemption foundations before Aletheia can claim consistent process isolation across supported targets.
Required Capability Matrix (updated 2026-07-21 — x86-64 user-mode delivered)
Capability	AArch64	x86-64	RISC-V
Physical allocator	Done/advanced	Done	Gap
MMU	Done/advanced	Done	Gap
User mode	Done/advanced	Done	Gap
Per-process address space	Done/advanced	Done	Gap
Preemption	Done/advanced	Done	Gap
SMP	Gap	Gap	Gap

> Progress note (2026-07-21): x86-64 now proves the same 10 ring-3 user-mode invariants as the
> aarch64 EL0 suite — cap-gated `int 0x80` syscall boundary, supervisor-page isolation, per-process
> PML4 address spaces, and cooperative + PIT-preemptive multitasking (`kernel-x86_64/src/usermode.rs`,
> VM-gated via `scripts/smoke-test.sh`). The remaining Issue-3 gap is the RISC-V U-mode column.
Scope
x86-64 ring-3 execution
x86-64 per-process address spaces
x86-64 preemptive scheduling
RISC-V U-mode
RISC-V Sv39/Sv48 virtual memory
RISC-V page allocation
RISC-V timer interrupts
RISC-V external interrupt handling
Cross-target process lifecycle semantics
Acceptance Criteria
An untrusted user process can execute on each supported architecture.
User memory cannot access kernel memory.
Processes have isolated address spaces.
Faults are contained to the appropriate process boundary.
Equivalent isolation invariants pass on all supported targets.
Proposed Issue 4 — Implement SMP and Multicore Scheduling
Priority: Critical
Category: Kernel / Scheduling
Raised by: GPT-5.5 (OpenAI)
Problem
The current kernel foundation must evolve from single-core assumptions toward real multicore execution. This is essential for modern hardware and for future CPU/GPU/NPU scheduling.
Scope
Secondary CPU/hart bring-up
Per-CPU data
Per-CPU scheduler state
Inter-processor interrupts
TLB shootdown
Lock hierarchy and concurrency audit
CPU affinity
Load balancing
Work stealing where appropriate
Cross-core wakeups
Atomic ordering audit
NUMA abstraction planning
Acceptance Criteria
Multiple cores/harts boot and execute kernel work.
Tasks can be scheduled across CPUs safely.
Cross-core synchronization is tested under stress.
TLB invalidation is correct across CPUs.
No single-core assumptions remain in shared scheduler/memory paths.
Proposed Issue 5 — Build the Aletheia Device and Driver Architecture
Priority: Critical
Category: Kernel / Hardware
Raised by: GPT-5.5 (OpenAI)
Problem
A complete operating system requires a real device model and driver architecture. Basic platform primitives are insufficient for persistent storage, networking, graphics, input, and accelerator hardware.
Scope
Device discovery
ACPI and/or device-tree abstraction
PCIe
USB
NVMe
Block-device interface
Network-device abstraction
GPU device abstraction
Input devices
Audio devices
Power management
Hotplug
Driver isolation
Driver restart/recovery
Device capabilities
DMA and IOMMU integration
Target Architecture
Application
    ↓
Capability IPC
    ↓
Device Service
    ↓
Driver
    ↓
Hardware
Acceptance Criteria
Devices are discovered through a unified model.
Drivers do not require ambient authority beyond their assigned capabilities.
At least one persistent storage path is available through the new architecture.
Driver failure can be contained and diagnosed.
Proposed Issue 6 — Build Persistent Storage, Filesystem/Object Storage, and Recovery
Priority: Critical
Category: Storage / Reliability
Raised by: GPT-5.5 (OpenAI)
Problem
The semantic/content-addressed system layer requires a real OS storage substrate. A bootable image and hosted durable store are not sufficient to provide general-purpose persistent operating-system storage.
Target Stack
Physical Disk
    ↓
Block Device
    ↓
Storage Driver
    ↓
Partitioning
    ↓
Filesystem / Object Store
    ↓
Encrypted Storage Layer
    ↓
Semantic Store
    ↓
World Model
Scope
Persistent filesystem or object store
Crash consistency
Journaling or copy-on-write semantics
Integrity checking
Encryption key lifecycle
Secure key storage integration
Snapshots
Rollback
Corruption detection
Atomic transactions
Boot-state persistence
Recovery tooling
Acceptance Criteria
The OS can persist state across reboots.
Interrupted writes do not corrupt committed state.
Storage corruption is detectable.
Recovery can restore a known-good state.
Semantic system state can be built on the persistent storage layer.
Proposed Issue 7 — Build Secure Boot and the Complete Chain of Trust
Priority: Critical
Category: Security / Boot
Raised by: GPT-5.5 (OpenAI)

> Progress note (2026-07-22): the **component-signature** slice is delivered on the hosted System Core
> (ADR-025 Phase 1) — `aletheia/src/provenance.rs` (a trust anchor + HMAC-SHA256 sign/verify over a
> component's content hash, built on the existing `sha2`, no new dep) wired into `SysCore`: under an
> opt-in secure policy (default off) `run_installed` refuses an unsigned/invalid component fail-closed
> and `install_signed_component` refuses an untrusted signature at install. Verified by
> `aletheia/tests/component_signing.rs` (5 tests) + crypto/provenance unit tests. STILL OPEN (Phase
> 2–3, hardware-bound): asymmetric keys + a root→stage key hierarchy, UEFI Secure Boot / measured boot
> into a TPM, anti-downgrade/rollback protection, and model-provenance verification.

Problem
Runtime capability security must be complemented by boot-time integrity. The system needs a verifiable chain from firmware through the kernel, system services, applications, and AI models.
Target Chain
Firmware
    ↓
Verified Bootloader
    ↓
Verified Kernel
    ↓
Verified System Services
    ↓
Verified Applications
    ↓
Verified Models
Scope
Signed bootloader
Signed kernel
Measured boot
TPM/Secure Enclave integration where available
Key hierarchy
Rollback protection
Anti-downgrade policy
Signed system components
Component signature verification
Model provenance verification
Acceptance Criteria
Unsigned or tampered boot components cannot execute under secure policy.
Rollback protection is defined and tested.
System component provenance is verifiable.
Model provenance and integrity can be verified before execution.
Proposed Issue 8 — Build Fault Recovery, Supervision, and Production Reliability
Priority: High
Category: Reliability / Runtime
Raised by: GPT-5.5 (OpenAI)
Problem
Fail-closed behavior is excellent for security and development gates, but a production operating system also needs controlled recovery from component, service, driver, and storage failures.
Target Lifecycle
Component Crash
    ↓
Detect
    ↓
Contain
    ↓
Record
    ↓
Restart / Recover
    ↓
Restore State
Scope
Supervisor hierarchy
Service restart
Crash isolation
Watchdogs
Health checks
Journal replay
Transaction recovery
System recovery mode
Safe mode
Last-known-good boot
Rollback
Failure diagnostics
Acceptance Criteria
A failed isolated service cannot corrupt unrelated services.
Supervisors can restart recoverable services.
Crash state is persisted for diagnosis.
The system can enter a recovery path after failed boot or failed service startup.
Proposed Issue 9 — Build the AI Execution Substrate and Heterogeneous Compute Scheduler
Priority: Critical
Category: AI Runtime / Scheduler / Hardware
Raised by: GPT-5.5 (OpenAI)
Problem
Aletheia is AI-native at the semantic and capability architecture level, but the actual hardware execution substrate for AI workloads must become a first-class OS concern.
Target Architecture
AI Runtime
    ↓
Model Manager
    ↓
CPU/GPU/NPU Scheduler
    ↓
Capability IPC
    ↓
Kernel
Scope
Model lifecycle
Model loading/unloading
Model memory accounting
Model residency
Model versioning
Model provenance
Model integrity verification
Model failure recovery
Execution
Inference cancellation
Inference priority
Resource quotas
Multi-model routing
Speculative execution management
Accelerators
GPU/NPU discovery
Accelerator abstraction
Command queues
DMA
IOMMU
Shared buffers
Capability-authorized accelerator access
GPU/NPU scheduling
Memory residency policy
Acceptance Criteria
AI workloads have explicit resource accounting.
Models cannot access unauthorized accelerator resources.
Inference can be cancelled and preempted according to policy.
CPU/GPU/NPU execution is represented in the scheduler architecture.
Model provenance and integrity are auditable.
Dependencies
Requires sufficient process, IPC, memory, device, and SMP foundations.
Proposed Issue 10 — Build the Native Aletheia Experience Layer
Priority: Critical
Category: User Experience / System Architecture
Raised by: GPT-5.5 (OpenAI)
Problem
Aletheia's defining user experience is not yet a complete native operating environment. The AI/context architecture must ultimately become the primary interaction model rather than remaining primarily a hosted system service and trace surface.
Target Experience
User Intent
    ↓
Context
    ↓
AI Interpretation
    ↓
Proposed Action
    ↓
Capability Check
    ↓
Approval if Required
    ↓
Execution
    ↓
Verification
    ↓
Explainable Result
Scope
Desktop/windowing model
Workspace model
Semantic navigation
Universal search
Dynamic UI generation
Application surfaces
Notifications
Clipboard
File browsing
Task switching
Accessibility
Input system
Display compositor
GPU rendering
Explainable action surfaces
Context-aware system navigation
Acceptance Criteria
Users can perform core OS tasks through native Aletheia surfaces.
AI-generated actions remain capability-authorized and auditable.
The experience layer is not dependent on a conventional hosted desktop as its primary architecture.
Applications can expose capabilities and surfaces to the native experience layer.
Proposed Issue 11 — Expand Testing from Invariant Validation to System Behavior
Priority: High
Category: Testing / Security / Reliability
Raised by: GPT-5.5 (OpenAI)

> Progress note (2026-07-22): the **security** category's core threats now have a permanent
> regression suite — `kernel-core/tests/security_behavior.rs` (9 hosted tests): confused-deputy (no
> ambient authority), capability laundering (revoked/expired parent, transfer amplification), TOCTOU /
> stale-capability (immediate revocation, no cached authorization window), and cross-principal leakage
> (scope confinement + action-wildcard non-over-match). STILL OPEN: the concurrency / reliability /
> AI-security categories (interrupt storms, nested faults, power-loss/disk-corruption, prompt
> injection, model-output fuzzing) and failure-injection harnesses for storage/services/drivers.

Problem
Aletheia already has a strong invariant, property, chaos, and VM-gate testing direction. The next stage needs broader system-behavior coverage, especially around concurrency, recovery, storage, device failure, and AI-specific security threats.
Required Test Categories
Kernel correctness
Memory corruption
Concurrency
Deadlocks
Race conditions
Interrupt storms
Nested faults
Stack exhaustion
Security
Confused deputy attacks
Capability laundering
TOCTOU conditions
Stale capability use
Cross-process leakage
DMA attacks
Malicious-driver behavior
Reliability
Power loss
Disk corruption
Interrupted transactions
Crash recovery
Service restart
AI security
Prompt injection
Malicious context
Hallucinated capability requests
Action-plan mutation
Stale context
Authorization/execution races
Model output fuzzing
Acceptance Criteria
Each major subsystem has behavior-level tests in addition to unit/invariant tests.
Failure injection is used for storage, services, drivers, and AI execution paths.
Security regression tests are permanently retained.
Critical tests run in CI and/or VM gates.
Proposed Issue 12 — Establish Machine-Checkable Architecture Traceability
Priority: Medium
Category: Architecture / Documentation / Engineering Process
Raised by: GPT-5.5 (OpenAI)

> Progress note (2026-07-22): **delivered.** `docs/TRACEABILITY.md` is a machine-readable matrix of
> 45 requirements, each mapping stable ReqID → ADR → implementation → test → VM gate → status.
> `scripts/check-traceability.sh` (pure bash, no new CI dep) fails the build if any requirement marked
> delivered/partial lacks Implementation+Test evidence that exists in the tree, or carries an unknown
> status; deferred work is explicitly distinguished and never counted as delivered. Wired as the
> `traceability` CI job on GitHub + GitLab, and negative-tested (delivered-without-evidence,
> missing-file, unknown-status all fail with a precise message). This satisfies the acceptance
> criteria "CI can detect requirements marked delivered without evidence" and "deferred work is
> explicitly distinguished from implemented work." FOLLOW-ON: auto-generate the STATUS summary counts
> from the matrix so the two can never drift.

Problem
The architecture and status documentation is substantially ahead of the implementation. This creates a risk that a requirement is described as delivered while only a partial implementation exists.
Target Traceability
Requirement ID
    ↓
ADR
    ↓
Implementation Module
    ↓
Test
    ↓
VM Gate
    ↓
Status
Scope
Stable requirement identifiers
ADR-to-implementation references
Implementation-to-test references
Test-to-VM-gate references
Status generation or validation
CI check for missing traceability
Acceptance Criteria
Critical architectural requirements have machine-readable identifiers.
Each claimed delivered requirement maps to implementation and test evidence.
CI can detect requirements marked delivered without evidence.
Deferred work is explicitly distinguished from implemented work.
Recommended Execution Order
P6 — Kernel Substrate
Architecture-independent kernel-core
Capability-secure IPC
Cross-architecture user mode and process isolation
SMP and multicore scheduling
P7 — Hardware and Persistence
Device and driver architecture
Persistent storage and recovery
Secure boot and chain of trust
P8 — Operational Reliability
Supervision and fault recovery
Expanded system-behavior testing
Machine-checkable architecture traceability
P9 — AI Execution Substrate
Model lifecycle management
CPU/GPU/NPU scheduling
Accelerator isolation
AI resource accounting and provenance
P10 — Native Aletheia Experience
Native compositor and input
Semantic navigation and workspaces
Dynamic capability-aware interfaces
AI-mediated operating-system interaction
Architectural Principle
Aletheia should not become merely:
Conventional OS
    +
AI Assistant
The target architecture is:
Capability-Secure Microkernel
          +
Context Fabric
          +
AI-Native Execution Substrate
          +
Semantic System Model
          +
Dynamic Capability-Aware Experience
The implementation priority should preserve that distinction while closing the conventional OS substrate gaps required to make the architecture real.
Attribution
This gap register was produced from repository inspection and architecture analysis by GPT-5.5 (OpenAI) on 2026-07-21.
It is a technical proposal for review, not an assertion that every item is entirely absent from the repository. Individual implementation status should be verified against the current code before each item is converted into a narrowly scoped implementation issue.

After uploading it, I recommend you commit it on a branch named:

```text
docs/architecture-gap-register
Then open a PR titled:
docs: add architecture gap register and implementation roadmap
This gives Aletheia a canonical, version-controlled roadmap before we start turning the proposed gaps into implementation work.