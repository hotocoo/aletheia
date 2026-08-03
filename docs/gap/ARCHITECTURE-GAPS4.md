# Aletheia — Comprehensive Repository Audit Findings

**Repository:** `hotocoo/aletheia`
**Project:** Aletheia — From-Scratch AI-Native Operating System
**Audit scope:** Hosted System Core, capability/security model, WASM component runtime, SDK, kernel-core, aarch64 kernel, x86-64 kernel, RISC-V kernel, memory management, virtual memory, user mode, scheduling, IPC, storage, drivers, VM testing, CI/CD, reproducibility, documentation, architecture, and production-readiness.

**Audit objective:** Identify all meaningful correctness, security, reliability, architectural, testing, qualification, CI, reproducibility, and production-readiness gaps found during repository inspection.

---

# 1. Executive Summary

Aletheia is already substantially more serious than a typical hobby operating-system project.

The repository contains:

* A from-scratch Rust System Core.
* Explicit capability-based authority.
* No ambient authority as a core design principle.
* Content-addressed and encrypted storage.
* Deterministic intent-to-action execution.
* Verification after execution.
* Immutable event/audit traces.
* A WASM component runtime.
* Capability-gated host calls.
* Component fuel bounding.
* Component delegation and attenuation.
* A Rust component SDK.
* Property-based and chaos testing.
* A no_std microkernel direction.
* aarch64, x86-64, and RISC-V kernel implementations.
* Physical page-frame allocation.
* Virtual memory management.
* User-mode isolation.
* Per-process address spaces.
* Preemptive scheduling.
* Virtio-blk testing.
* SMP testing.
* VM-backed validation.

This is a strong foundation.

However, the repository currently has a significant gap between:

> **“The feature exists and has been tested in a controlled scenario”**

and:

> **“The feature is fully qualified as a production-grade operating-system subsystem.”**

The largest current risk is not that Aletheia lacks technical ambition.

The largest risk is:

> **The project's qualification, integration, adversarial testing, reproducibility, and production-hardening systems are behind the number of architectural claims being made.**

The most urgent issues are:

1. x86-64 is described as first-class but is not equivalently qualified by CI.
2. There is no single top-level integration workspace or equivalent complete build matrix.
3. Kernel memory-management APIs need stronger validation and lifecycle guarantees.
4. Page-table reclamation and address-space teardown need to be fully defined.
5. SMP TLB shootdown and page-table synchronization need a formal implementation contract.
6. Hand-written x86-64 trap/context-switch assembly requires substantially more adversarial qualification.
7. DMA/device isolation needs an explicit security model.
8. The scheduler and priority-inheritance IPC require more real kernel-level stress validation.
9. WASM sandbox resource limits extend beyond fuel.
10. Component ABI and signing/provenance need platform-grade versioning and lifecycle management.
11. Toolchains and build inputs are not fully pinned.
12. CI does not appear to enforce the complete quality/security gate set expected for a security-critical OS.
13. Status claims and acceptance metrics can drift from the actual implementation.
14. The current kernel milestone should not be interpreted as a production-ready complete operating system.

---

# 2. Severity Classification

| Severity | Meaning                                                            |
| -------- | ------------------------------------------------------------------ |
| **P0**   | Release-blocking or fundamental qualification failure              |
| **P1**   | Serious security, correctness, isolation, or reliability issue     |
| **P2**   | Important engineering or production-readiness gap                  |
| **P3**   | Quality, maintainability, documentation, or future-hardening issue |

---

# 3. P0 — Release-Blocking Findings

---

## ALET-P0-001 — x86-64 is First-Class in Architecture but Not Equivalently Validated in CI

### Severity

**P0**

### Area

CI / Architecture Qualification / x86-64 Kernel

### Finding

The repository describes AMD64/x86-64 as a first-class architecture.

However, the automated VM qualification path does not provide equivalent x86-64 boot validation to the aarch64 and RISC-V targets.

The existing VM gate validates the aarch64 target through QEMU and the repository also contains RISC-V validation paths. The x86-64 implementation contains substantial low-level functionality, including:

* ring-3 user mode,
* syscall entry,
* timer interrupts,
* context switching,
* address-space switching,
* page faults,
* preemptive scheduling.

But without an equivalent automated x86-64 build-and-boot gate, these features can regress while the overall repository remains green.

### Why This Matters

x86-64 is one of the most important target architectures for an operating system.

The x86 path is also one of the most complex paths in the repository because it includes:

* handwritten assembly,
* IDT entry behavior,
* `iretq`,
* ring transitions,
* trap-frame layout,
* CR3 switching,
* timer interrupts,
* page-fault handling,
* scheduler context switching.

A failure in any of these can be catastrophic.

### Required Fix

Add a dedicated x86-64 CI qualification path that:

1. Builds the kernel.
2. Boots it under QEMU.
3. Uses a deterministic firmware path where applicable.
4. Tests user-mode entry.
5. Tests syscall entry.
6. Tests page faults.
7. Tests context switching.
8. Tests timer preemption.
9. Tests address-space isolation.
10. Tests SMP where supported.
11. Validates explicit invariant markers.
12. Requires a deterministic VM exit code.

### Acceptance Criteria

The repository must have an automated x86-64 gate equivalent in strength to the other first-class architectures.

A green CI result must mean:

```text
x86_64:
  build: PASS
  boot: PASS
  user mode: PASS
  syscall boundary: PASS
  page isolation: PASS
  scheduling: PASS
  preemption: PASS
  address-space switching: PASS
  SMP: PASS
  deterministic VM exit: PASS
```

---

## ALET-P0-002 — There Is No Single Complete Repository-Wide Integration Build

### Severity

**P0**

### Area

Build System / Cargo / Integration

### Finding

The repository consists of multiple Cargo packages and architectural components, but there is no single canonical top-level integration build that proves the entire repository is internally coherent.

This creates a risk where:

* one package builds,
* another package is stale,
* a shared interface changes,
* a target-specific crate is not compiled,
* a component SDK changes,
* a kernel-core API changes,
* but the complete repository remains apparently healthy.

### Why This Matters

An operating system is not merely a collection of independently compiling crates.

The important integration boundaries include:

```text
kernel-core
    ↓
architecture-specific kernel
    ↓
memory management
    ↓
scheduler
    ↓
IPC
    ↓
device layer
    ↓
System Core
    ↓
component runtime
    ↓
SDK
    ↓
experience layer
```

These boundaries need an explicit build and test contract.

### Required Fix

Either:

1. Create a top-level Cargo workspace with carefully designed target-specific members and exclusions,

or:

2. Create a canonical repository-wide build orchestrator that explicitly builds and tests every package and target.

The second option is acceptable if a workspace would create unsuitable dependency or target coupling.

### Acceptance Criteria

A single command or CI pipeline must validate:

* hosted System Core,
* component runtime,
* component SDK,
* kernel-core,
* aarch64 kernel,
* x86-64 kernel,
* RISC-V kernel,
* examples,
* tests,
* target-specific artifacts.

---

## ALET-P0-003 — Architecture Qualification Status Is Not Mechanically Enforced

### Severity

**P0**

### Area

Release Engineering / Documentation / CI

### Finding

The repository distinguishes multiple target architectures and milestone stages, but the distinction between:

* implemented,
* build-tested,
* VM-tested,
* integration-tested,
* hardware-tested,
* production-qualified

is not mechanically enforced.

This means documentation can claim that an architecture is “first-class” while CI only proves a subset of the expected functionality.

### Required Fix

Introduce a machine-readable qualification matrix.

Example:

```yaml
architectures:
  aarch64:
    implemented: true
    build_tested: true
    vm_boot_tested: true
    user_mode_tested: true
    smp_tested: true
    hardware_tested: false
    production_qualified: false

  x86_64:
    implemented: true
    build_tested: true
    vm_boot_tested: true
    user_mode_tested: true
    smp_tested: true
    hardware_tested: false
    production_qualified: false
```

CI must verify that the status document does not claim more than the machine-readable evidence supports.

---

# 4. P1 — Kernel Memory and Virtual Memory Findings

---

## ALET-P1-001 — Raw Physical and Virtual Addresses Are Accepted by Mapping APIs Without Sufficient Validation

### Severity

**P1**

### Area

AArch64 Virtual Memory

### Finding

The page-mapping APIs operate on raw integer addresses and descriptor flags.

A production-grade virtual-memory API must explicitly validate:

* page alignment,
* physical address range,
* virtual address range,
* architectural address-width limits,
* canonical-address rules,
* reserved bits,
* legal descriptor combinations,
* memory attributes,
* executable/writeable combinations,
* ownership of the physical frame,
* whether an existing mapping may be replaced.

### Risk

Invalid page-table entries can result in:

* translation faults,
* memory aliasing,
* accidental access to physical memory,
* incorrect cache attributes,
* executable writable memory,
* corruption of unrelated memory,
* security boundary violations.

### Required Fix

Create typed address abstractions.

Example:

```rust
struct PhysFrame(usize);
struct VirtPage(usize);
struct PageFlags(...);
```

Validate all inputs before page-table mutation.

The API should not allow arbitrary raw descriptor bits without a central validation policy.

---

## ALET-P1-002 — Page-Table Reclamation After Unmap Is Incomplete

### Severity

**P1**

### Area

Virtual Memory / Memory Management

### Finding

Unmapping a leaf mapping does not automatically establish that empty intermediate page tables are reclaimed.

A page-table hierarchy can therefore accumulate unused intermediate tables.

### Risk

Repeated operations such as:

```text
map
unmap
map
unmap
...
```

can consume page-table memory over time.

This is particularly problematic for:

* dynamic processes,
* short-lived components,
* memory-intensive workloads,
* process creation/destruction,
* address-space churn.

### Required Fix

When a mapping is removed:

1. Clear the leaf entry.
2. Determine whether the parent table is empty.
3. Reclaim the parent if empty.
4. Continue upward.
5. Return reclaimed page-table frames to the allocator.
6. Maintain correct ownership/reference accounting.

### Acceptance Criteria

A stress test must demonstrate:

```text
N map/unmap cycles
→ bounded page-table memory
→ no leaked intermediate tables
```

---

## ALET-P1-003 — Physical Frame Ownership Is Not Fully Defined

### Severity

**P1**

### Area

Memory Management

### Finding

The memory system needs an explicit ownership model for physical frames.

Important questions include:

* Who owns a mapped physical frame?
* Can two address spaces map the same frame?
* Is shared memory reference-counted?
* When is a frame freed?
* Can a frame be mapped with incompatible permissions?
* What happens when a process exits?
* What happens when a component is destroyed?
* Can the kernel accidentally free a frame still mapped elsewhere?

### Required Fix

Define a frame ownership model covering:

```text
allocator
    ↓
owner
    ↓
mapping references
    ↓
shared mappings
    ↓
unmap
    ↓
process destruction
    ↓
final reclamation
```

Add invariant tests.

---

## ALET-P1-004 — Address-Space Destruction Is Not Fully Qualified

### Severity

**P1**

### Area

Virtual Memory / Process Lifecycle

### Finding

Creating and using address spaces is only part of the problem.

Production correctness also requires safe destruction.

The teardown path must handle:

* user mappings,
* page-table frames,
* kernel mappings,
* shared memory,
* pending tasks,
* active CPU execution,
* TLB state,
* device mappings,
* outstanding IPC,
* faulted processes.

### Risk

Incorrect teardown can cause:

* use-after-free,
* stale TLB translations,
* leaked frames,
* cross-process memory exposure,
* corruption of another address space.

### Required Fix

Add a complete address-space destruction protocol.

---

## ALET-P1-005 — SMP TLB Shootdown Semantics Need a Formal Implementation Contract

### Severity

**P1**

### Area

SMP / MMU / Virtual Memory

### Finding

Local TLB invalidation is insufficient when multiple CPUs can execute the same address space.

If CPU A modifies a page table while CPU B has a stale translation cached, CPU B can continue using an invalid mapping.

### Required Fix

Define:

```text
page-table mutation
    ↓
identify CPUs using address space
    ↓
send shootdown request
    ↓
remote CPU invalidates TLB
    ↓
acknowledgement
    ↓
mutation becomes globally effective
```

The implementation must define behavior for:

* CPU offline state,
* CPU failure,
* concurrent unmap,
* address-space destruction,
* context switches during shootdown,
* interrupt-disabled sections.

---

## ALET-P1-006 — Kernel/User Virtual Address Layout Is Not Yet Production-Hardened

### Severity

**P1**

### Area

Address-Space Security

### Finding

The current identity-map-oriented implementation is useful for early kernel development but is not an adequate final process memory layout.

A production system needs a deliberate layout for:

```text
kernel text
kernel read-only data
kernel writable data
kernel heap
kernel stacks
per-CPU data
MMIO
user code
user read-only data
user heap
user stack
guard pages
shared memory
reserved regions
```

### Required Fix

Define and enforce a formal address-space layout.

The design should include:

* non-executable data,
* non-writable code,
* guard pages,
* stack boundaries,
* kernel/user separation,
* canonical-address validation,
* reserved-hole protection.

---

## ALET-P1-007 — W^X Policy Is Not Yet a Complete Global Invariant

> Progress note (2026-08-03): **resolved on both QEMU targets.** Validation at every dynamic mapping
> API landed 2026-08-02 (ADR-034, REQ-MM-006); the bootstrap identity map is now split at 4 KiB
> granularity from the linker's section symbols, so aarch64 and RISC-V require ZERO W^X descriptors of
> either class (dynamic pages AND bootstrap blocks) and prove text is read-only + executable,
> `.rodata` read-only + non-executable, data/stack writable + non-executable — plus that the mapping
> API refuses to remap or unmap the image span. Virtual-memory invariants 49 → 55 per QEMU target.
> Component memory (WASM) is bounded by the runtime, not by page tables; JIT/dynamically generated
> code does not exist yet and would need its own W^X-with-flip policy. What remains is x86-64's
> inherited OVMF tree (~524 795 W^X leaves), split out as **ALET-P1-031**.

### Severity

**P1**

### Area

Memory Security

### Finding

A production OS should have an explicit global policy preventing writable and executable memory from existing simultaneously unless explicitly justified.

This must cover:

* user mappings,
* kernel mappings,
* component memory,
* JIT memory if ever supported,
* dynamically generated code,
* boot mappings.

### Required Fix

Introduce an explicit W^X policy and tests.

---

## ALET-P1-008 — Memory Attribute Validation Needs Stronger Architecture-Specific Enforcement

### Severity

**P1**

### Area

MMU / Cache / Device Memory

### Finding

Normal RAM, device memory, executable memory, cacheable memory, and strongly ordered regions cannot be treated identically.

The memory manager needs a clear policy for:

* normal cacheable memory,
* device memory,
* MMIO,
* DMA buffers,
* executable code,
* read-only data.

### Required Fix

Centralize memory-attribute validation.

---

# 5. P1 — x86-64 Trap and Context-Switch Findings

---

## ALET-P1-009 — x86-64 Trap Frame Layout Is a High-Risk Manual ABI

### Severity

**P1**

### Area

x86-64 Assembly

### Finding

The x86-64 implementation relies on manually maintained offsets between assembly and Rust structures.

For example:

```text
TrapFrame offset 0
TrapFrame offset 8
TrapFrame offset 16
...
```

If the Rust structure changes without updating assembly offsets, the kernel can silently corrupt registers or control state.

### Required Fix

Add compile-time offset assertions.

Possible approaches:

* `offset_of!` assertions,
* generated assembly constants,
* build-time code generation,
* static layout tests.

---

## ALET-P1-010 — Shared Mutable Trap State Requires Stronger Reentrancy Guarantees

### Severity

**P1**

### Area

Interrupt Handling

### Finding

The trap path uses shared state such as:

```text
CURRENT_FRAME
KERNEL_CTX
```

This is highly sensitive to:

* nested interrupts,
* faults during kernel execution,
* unexpected reentrancy,
* SMP execution,
* context-switch timing.

### Required Fix

Prove or enforce:

```text
one CPU
    → one active trap context
    → one scheduler context
```

Per-CPU storage should be preferred over globally shared mutable state.

---

## ALET-P1-011 — Interrupt Entry and Fault Entry Need More Adversarial Testing

### Severity

**P1**

### Area

x86-64 Reliability

### Required Tests

* timer interrupt during syscall,
* page fault during user execution,
* invalid user stack,
* invalid instruction pointer,
* nested interrupt,
* unexpected kernel-originated interrupt,
* malformed user register state,
* task exit during preemption,
* interrupt while switching CR3,
* context switch during a pending timer event.

---

## ALET-P1-012 — x86-64 Kernel Stack Safety Needs Explicit Guarding

### Severity

**P1**

### Area

Kernel Security

### Finding

Kernel stacks are security-critical.

The implementation needs explicit protection against:

* stack overflow,
* recursive faults,
* interrupt nesting,
* scheduler stack exhaustion.

### Required Fix

Add:

* guard pages,
* stack bounds,
* overflow detection,
* per-CPU stack policy,
* double-fault strategy.

---

## ALET-P1-013 — Page-Fault Handling Needs a Formal Fault Classification Model

### Severity

**P1**

### Area

Memory Faults

### Finding

A page fault may be caused by:

* legitimate demand paging,
* permission violation,
* non-present page,
* malformed address,
* stack growth,
* use-after-free,
* kernel bug,
* user attack.

These must not all be handled identically.

### Required Fix

Define a formal fault classification:

```text
fault
 ├── recoverable user fault
 ├── invalid user access
 ├── lazy allocation
 ├── stack growth
 ├── device/MMIO fault
 ├── kernel bug
 └── fatal corruption
```

---

# 6. P1 — Scheduler and Concurrency Findings

---

## ALET-P1-014 — Scheduler Testing Does Not Fully Represent Real Multicore Contention

### Severity

**P1**

### Area

Scheduler

### Finding

Hosted policy tests are useful but cannot fully prove the behavior of a real interrupt-driven multicore scheduler.

The scheduler needs validation for:

* concurrent enqueue,
* concurrent dequeue,
* task migration,
* task exit while queued,
* duplicate queue entries,
* starvation,
* fairness,
* priority changes,
* CPU hotplug behavior.

### Required Fix

Add deterministic stress tests with explicit invariants.

---

## ALET-P1-015 — Task Lifecycle State Transitions Need Formal Invariants

### Severity

**P1**

### Required State Model

```text
Created
  ↓
Runnable
  ↓
Running
  ├── Runnable
  ├── Blocked
  ├── Exited
  └── Faulted
```

Illegal transitions must be rejected.

Examples:

```text
Exited → Running
Faulted → Runnable
Destroyed → Blocked
```

must be impossible.

---

## ALET-P1-016 — Priority Inheritance Needs End-to-End IPC Validation

### Severity

**P1**

### Finding

Priority inheritance cannot be proven only through an isolated policy abstraction.

The real kernel must test:

```text
high-priority task
    ↓ waits on
low-priority task
    ↓ owns resource
priority inheritance
    ↓
low-priority task executes
    ↓
resource released
    ↓
priority restored
```

Also test:

* nested inheritance,
* multiple waiters,
* cancellation,
* timeout,
* task death,
* dependency cycles.

---

## ALET-P1-017 — Blocking IPC Cancellation Semantics Need Explicit Proof

### Severity

**P1**

### Finding

Every blocking operation must define what happens when:

* the caller is cancelled,
* the endpoint is destroyed,
* the peer dies,
* a timeout occurs,
* the process exits.

### Required Fix

Add a cancellation protocol and tests proving:

```text
cancelled operation
→ no stale waiter
→ no leaked grant
→ no corrupted endpoint
→ no unexpected wakeup
```

---

# 7. P1 — Device and DMA Security Findings

---

## ALET-P1-018 — DMA Isolation Model Is Not Fully Defined

### Severity

**P1**

### Finding

Device DMA can bypass normal CPU page-table isolation.

A production OS must define:

* which physical memory a device may access,
* who owns DMA buffers,
* when ownership transfers,
* whether an IOMMU is required,
* how devices are isolated from each other,
* how device reset affects outstanding DMA.

### Required Fix

Define one of:

```text
IOMMU-enforced DMA isolation
```

or:

```text
explicit trusted-device boundary
```

The choice must be documented and tested.

---

## ALET-P1-019 — Virtio-Blk Testing Is Insufficient for Production Storage Qualification

### Severity

**P1**

### Finding

The VM test uses a tiny ephemeral disk.

This proves that a basic driver path can run.

It does not prove:

* persistence across reboot,
* journal replay,
* crash recovery,
* multi-block operations,
* large I/O,
* queue exhaustion,
* malformed device behavior,
* reset recovery,
* partial writes,
* corruption handling.

### Required Fix

Add:

* persistent image tests,
* reboot tests,
* crash injection,
* reset tests,
* queue stress,
* large-volume tests,
* malformed descriptor tests.

---

## ALET-P1-020 — Storage Error Semantics Need a Formal Contract

### Severity

**P1**

### Finding

Every storage layer needs explicit semantics for:

```text
success
partial success
temporary failure
permanent failure
device removal
timeout
reset
corruption
```

The system must not accidentally treat a partial or failed write as successful.

---

# 8. P1 — WASM Component Runtime Findings

---

## ALET-P1-021 — Fuel Is Not a Complete Sandbox Resource Model

### Severity

**P1**

### Finding

Fuel bounds instruction execution.

It does not automatically bound:

* memory growth,
* output size,
* number of host calls,
* call depth,
* component spawning,
* retained state,
* number of concurrent components.

### Required Limits

Each component should have explicit budgets for:

```text
CPU
memory
host calls
output bytes
spawn count
spawn depth
execution duration
persistent state
```

---

## ALET-P1-022 — Component ABI Needs Explicit Versioning

### Severity

**P1**

### Finding

The component runtime and SDK are tightly coupled to the host ABI.

A real operating system needs components to survive independent version evolution.

### Required Fix

Define:

```text
ABI version
ABI feature set
minimum supported version
compatibility rules
rejection behavior
```

A component compiled against an incompatible ABI must fail deterministically.

---

## ALET-P1-023 — Component Installation Needs Complete Supply-Chain Verification

### Severity

**P1**

### Finding

Signing/provenance is not equivalent to a complete supply-chain security model.

A production installation system must address:

* root trust,
* key rotation,
* revocation,
* rollback protection,
* replay protection,
* version constraints,
* dependency provenance,
* developer mode.

### Required Fix

Define the complete lifecycle:

```text
developer key
    ↓
publisher identity
    ↓
package signature
    ↓
dependency verification
    ↓
installation
    ↓
update
    ↓
revocation
    ↓
rollback protection
```

---

## ALET-P1-024 — Component Dependency Resolution Is Not Yet a Complete Security Boundary

### Severity

**P1**

### Finding

If components can spawn or depend on other components, dependency resolution itself becomes security-sensitive.

The system must prevent:

* dependency confusion,
* malicious replacement,
* unauthorized capability inheritance,
* dependency cycles,
* unbounded dependency graphs.

---

# 9. P1 — Capability Security Findings

---

## ALET-P1-025 — Capability Revocation Must Be Validated Under Concurrency

### Severity

**P1**

### Finding

Revocation is tested functionally, but production correctness requires testing concurrent behavior.

Example:

```text
Task A
  holds capability

Task B
  revokes capability

Task A
  concurrently attempts action
```

The system must define the linearization point.

### Required Fix

Test:

* revoke vs authorize race,
* revoke vs delegation race,
* revoke vs execution race,
* revoke vs process destruction.

---

## ALET-P1-026 — Capability Lifetime and Persistence Need a Complete Model

### Severity

**P1**

### Questions That Must Be Answered

* Are capabilities persistent across reboot?
* Are they bound to a process instance?
* Are they bound to an identity?
* What happens after restore?
* Can a snapshot restore an already-revoked capability?
* Can capabilities be replayed?
* Can capability tokens be copied between machines?

---

## ALET-P1-027 — Capability Scope Must Be Formally Composable

### Severity

**P1**

### Finding

Capabilities can be attenuated.

The system must formally define how multiple constraints combine.

For example:

```text
parent scope:
  entity.read
  target = A|B|C

child scope:
  entity.read
  target = B|C

effective scope:
  entity.read
  target = B|C
```

The rule must always be intersection or another formally proven non-amplifying operation.

---

# 10. P1 — Cryptography and Secure Storage Findings

---

## ALET-P1-028 — Encryption at Rest Does Not Automatically Solve Key Management

### Severity

**P1**

### Finding

Encrypted storage requires a complete key-management design.

The system must define:

* key generation,
* root key storage,
* key derivation,
* rotation,
* recovery,
* backup,
* destruction,
* device migration,
* compromise response.

### Important

Encryption with a strong cipher is not enough if the key lifecycle is weak.

---

## ALET-P1-029 — Nonce/IV Lifecycle Must Be Proven for Every Encrypted Object

### Severity

**P1**

### Finding

ChaCha20-Poly1305 requires nonce uniqueness under a key.

The system must guarantee:

```text
same key
+
same nonce
+
different plaintext
=
never allowed
```

### Required Fix

Add explicit nonce uniqueness tests and durable crash-safe nonce allocation semantics.

---

## ALET-P1-030 — Encrypted Content-Addressing Requires Careful Identity Semantics

### Severity

**P1**

### Finding

The system uses content-addressed entities while also encrypting data.

The architecture must explicitly define whether the content address is derived from:

```text
plaintext
```

or:

```text
ciphertext
```

or:

```text
canonical encrypted object
```

Each has different privacy and deduplication implications.

---

# 11. P2 — CI and Reproducibility Findings

---

## ALET-P2-001 — Rust Toolchains Are Not Fully Pinned

### Severity

**P2**

### Finding

CI uses floating toolchain channels such as:

```text
stable
nightly
```

This means future compiler updates can change:

* compilation behavior,
* generated code,
* warnings,
* optimizer behavior,
* build-std behavior,
* kernel behavior.

### Required Fix

Pin exact versions.

Example:

```text
stable-1.XX.Y
nightly-YYYY-MM-DD
```

Upgrade intentionally.

---

## ALET-P2-002 — Kernel Builds Do Not Uniformly Enforce Locked Dependency Resolution

### Severity

**P2**

### Finding

The hosted path uses locked dependency behavior, while kernel build scripts invoke ordinary Cargo builds.

### Required Fix

Use one reproducibility policy.

Every release artifact should be built from:

```text
exact source revision
+
exact toolchain
+
exact dependency lock
+
exact build flags
```

---

## ALET-P2-003 — CI Quality Gates Are Incomplete for a Security-Critical OS

### Severity

**P2**

### Required Gates

At minimum:

```text
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo test --all-targets
dependency advisory audit
license audit
SBOM generation
unsafe-code audit
assembly audit
VM boot tests
artifact reproducibility
```

---

## ALET-P2-004 — Dependency Security Scanning Is Required

### Severity

**P2**

### Finding

A security-critical OS should continuously scan:

* direct dependencies,
* transitive dependencies,
* build dependencies,
* procedural macros,
* WASM runtime dependencies.

### Required Fix

Add automated dependency vulnerability auditing.

---

## ALET-P2-005 — License and SBOM Generation Are Missing Production Controls

### Severity

**P2**

### Finding

Distribution of an operating system requires knowing exactly what is included.

Generate:

```text
SPDX SBOM
CycloneDX SBOM
license report
dependency provenance
```

---

## ALET-P2-006 — Reproducible Builds Are Not Yet a Release Property

### Severity

**P2**

### Required Goal

Given:

```text
same source
same toolchain
same dependencies
same build configuration
```

the produced artifact should be byte-identical or have a documented reproducibility exception.

---

# 12. P2 — Testing Findings

---

## ALET-P2-007 — Marker-Based VM Testing Can Be Strengthened

### Severity

**P2**

### Finding

The VM script validates output markers such as:

```text
ALL ... INVARIANTS HOLD
MEMORY INVARIANTS HOLD
VIRTUAL-MEMORY INVARIANTS HOLD
[e2e] PASS
```

This is useful but can become fragile if the test output is manually changed.

### Required Fix

Add structured machine-readable output.

Example:

```json
{
  "memory": "pass",
  "vm": "pass",
  "usermode": "pass",
  "smp": "pass",
  "virtio_blk": "pass"
}
```

---

## ALET-P2-008 — Fault Injection Coverage Is Insufficient

### Required Faults

* allocation failure,
* page-table exhaustion,
* malformed syscall,
* invalid capability,
* revoked capability,
* device timeout,
* device reset,
* corrupted storage,
* invalid WASM,
* fuel exhaustion,
* memory exhaustion,
* task cancellation,
* CPU failure.

Every failure should prove:

```text
no unauthorized side effect
no memory corruption
no leaked capability
no leaked resource
no deadlock
```

---

## ALET-P2-009 — Long-Running Soak Testing Is Needed

### Severity

**P2**

### Finding

Short deterministic tests cannot detect:

* memory leaks,
* scheduler drift,
* stale state,
* capability-table growth,
* page-table leaks,
* long-term fragmentation,
* rare race conditions.

### Required Fix

Add:

```text
hours-long VM soak
multi-process stress
component churn
map/unmap churn
IPC churn
scheduler churn
storage churn
```

---

## ALET-P2-010 — Property Tests Need Larger and More Diverse Campaigns

### Severity

**P2**

### Finding

Property tests are valuable, but a limited number of randomized cases is not equivalent to exhaustive confidence.

### Required Fix

Run:

* larger CI campaigns,
* nightly campaigns,
* deterministic seed capture,
* failure artifact retention,
* corpus minimization.

---

# 13. P2 — Documentation and Traceability Findings

---

## ALET-P2-011 — Manual Status Metrics Can Drift

### Severity

**P2**

### Finding

Status documents contain:

* milestone claims,
* test counts,
* acceptance criteria,
* “all green” statements.

These can become stale.

### Required Fix

Generate metrics from CI.

For example:

```text
tests passed
architectures booted
VM gates passed
property cases executed
```

should be generated automatically.

---

## ALET-P2-012 — Requirement Traceability Should Be Machine-Checked

### Severity

**P2**

### Finding

The project uses requirement identifiers and ADRs.

This is good.

However, the mapping:

```text
requirement
    ↓
implementation
    ↓
test
    ↓
CI gate
```

should be mechanically validated.

### Required Matrix

```text
REQ-ID
  ↓
source file
  ↓
test
  ↓
CI job
```

Any requirement without evidence should fail the release qualification report.

---

## ALET-P2-013 — “Implemented” and “Production-Ready” Need Stronger Separation

### Severity

**P2**

### Finding

A feature can be implemented and tested while still not being production-ready.

The status system should explicitly separate:

```text
Designed
Implemented
Unit-tested
Property-tested
VM-tested
Stress-tested
Hardware-tested
Security-reviewed
Production-qualified
```

---

# 14. P2 — OS Architecture Gaps

These are not necessarily bugs in the current implementation. They are major systems that must eventually be designed and implemented before Aletheia can be considered a complete production OS.

---

## ALET-P2-014 — Boot Chain Is Not Yet a Complete Production System

Required components include:

```text
firmware interaction
bootloader
kernel loading
memory map acquisition
hardware discovery
secure boot
verified boot
rollback protection
recovery boot
failure recovery
```

---

## ALET-P2-015 — Secure Boot Is Not Yet Delivered

The OS needs:

```text
root of trust
signed bootloader
signed kernel
signed system components
key rotation
revocation
rollback prevention
recovery key
```

---

## ALET-P2-016 — Update and Rollback System Is Not Yet Delivered

A production OS requires:

```text
atomic update
A/B slots or equivalent
verified update
power-loss safety
rollback
version anti-rollback
recovery
```

---

## ALET-P2-017 — Recovery Architecture Is Not Yet Delivered

The system must survive:

* failed update,
* corrupted filesystem,
* broken component,
* kernel panic,
* invalid configuration,
* storage failure.

---

## ALET-P2-018 — Filesystem and Persistent Storage Architecture Is Incomplete

The OS needs a complete storage model covering:

```text
filesystem
journaling
crash consistency
metadata integrity
encryption
snapshots
quotas
garbage collection
recovery
```

---

## ALET-P2-019 — Driver Model Is Not Yet Complete

A production driver framework needs:

```text
driver discovery
driver lifecycle
driver isolation
driver permissions
device ownership
hotplug
reset
failure recovery
versioning
```

---

## ALET-P2-020 — Networking Stack Is Not Yet Delivered

A full OS requires:

```text
network device drivers
Ethernet
IPv4
IPv6
TCP
UDP
DNS
TLS integration
firewall
capability-gated network access
```

---

## ALET-P2-021 — Graphics and Compositor Architecture Is Not Yet Delivered

The native experience layer requires:

```text
display discovery
GPU abstraction
compositor
window/surface model
input
GPU isolation
buffer ownership
display synchronization
```

---

## ALET-P2-022 — Power Management Is Not Yet Delivered

Required:

```text
sleep
wake
CPU frequency
idle states
device power
battery
thermal management
```

---

## ALET-P2-023 — Hardware Discovery Is Not Yet Complete

A production OS needs:

```text
ACPI or equivalent
PCI/PCIe
USB
I2C
SPI
interrupt controllers
timers
storage discovery
network discovery
GPU discovery
```

---

## ALET-P2-024 — Native AI Runtime Is Not Yet a Complete OS Subsystem

The intelligence layer needs explicit architecture for:

```text
model lifecycle
model loading
model permissions
memory budget
CPU/GPU/NPU scheduling
context management
inference cancellation
model provenance
model updates
model isolation
```

The model must remain untrusted.

The OS must never allow:

```text
model output
    ↓
direct authority
```

The correct model remains:

```text
model output
    ↓
interpretation
    ↓
validation
    ↓
capability authorization
    ↓
approval if required
    ↓
execution
    ↓
verification
```

---

# 15. P2 — AI-Native Architecture Findings

---

## ALET-P2-025 — Context Lifecycle Needs Formal Resource Boundaries

Context can grow without bound.

The OS needs policies for:

```text
context size
retention
compression
summarization
expiration
privacy
ownership
access control
```

---

## ALET-P2-026 — Memory Must Be Treated as a Security Boundary

AI memory can contain:

* secrets,
* personal data,
* credentials,
* private documents,
* action history.

The system needs:

```text
memory capabilities
memory provenance
memory deletion
memory expiration
memory audit
memory isolation
```

---

## ALET-P2-027 — Relationship Graph Access Needs Capability Enforcement

The relationship model must prevent unauthorized inference.

For example:

```text
Entity A
    related_to
Entity B
```

does not imply every actor may discover the relationship.

Graph traversal must remain capability-gated.

---

## ALET-P2-028 — Intent Confusion Attacks Need Dedicated Testing

Untrusted content may contain instructions.

The system must test:

```text
document:
  "delete all files"

model:
  interprets content

authorization:
  must not treat document text as authority
```

The system should distinguish:

```text
data
instructions
intent
authority
```

---

# 16. P2 — Security Model Findings

---

## ALET-P2-029 — Threat Model Needs Continuous Maintenance

The threat model should cover:

```text
malicious user
malicious component
malicious model
malicious document
malicious network peer
compromised driver
compromised dependency
physical attacker
malicious administrator
```

---

## ALET-P2-030 — Security Boundaries Need Explicit Enumeration

The architecture should explicitly list every boundary:

```text
user
    ↓
component
    ↓
System Core
    ↓
kernel
    ↓
driver
    ↓
device
```

Every crossing must define:

```text
input validation
authority
memory isolation
failure behavior
audit behavior
```

---

## ALET-P2-031 — Denial-of-Service Is Not the Same as Unauthorized Access

A component may be unable to read unauthorized data but still consume:

* CPU,
* memory,
* storage,
* IPC capacity,
* scheduler capacity.

The security model must include availability.

---

# 17. P3 — Maintainability Findings

---

## ALET-P3-001 — Assembly and Rust Boundaries Need Centralized Documentation

Every assembly boundary should document:

```text
calling convention
register preservation
stack layout
interrupt state
frame layout
clobbers
CPU assumptions
```

---

## ALET-P3-002 — Unsafe Code and Assembly Should Have Audit Ownership

Every unsafe/assembly block should have:

```text
invariant
reason unsafe is required
safety preconditions
caller obligations
test coverage
review owner
```

---

## ALET-P3-003 — Architectural Invariants Should Be Centralized

Important invariants are currently distributed across:

* code,
* tests,
* status documents,
* ADRs,
* scripts.

A central invariant registry would improve maintainability.

Example:

```text
INV-MEM-001
INV-CAP-001
INV-IPC-001
INV-SCHED-001
INV-VM-001
```

---

# 18. Recommended Priority Order

## Phase 1 — Immediate

1. Add x86-64 CI boot qualification.
2. Create complete repository-wide integration validation.
3. Pin Rust toolchains.
4. Enforce locked reproducible builds.
5. Add fmt/clippy/security/dependency gates.
6. Add exact trap-frame layout assertions.
7. Review all page-table APIs.
8. Define frame ownership.
9. Define address-space destruction.
10. Define SMP TLB shootdown.

---

## Phase 2 — Kernel Hardening

1. Page-table reclamation.
2. W^X enforcement.
3. Kernel/user memory layout.
4. Guard pages.
5. Per-CPU trap state.
6. Fault classification.
7. Scheduler stress.
8. IPC cancellation.
9. Priority inheritance end-to-end tests.
10. DMA isolation model.

---

## Phase 3 — Component Runtime Hardening

1. Resource quotas beyond fuel.
2. ABI versioning.
3. Component dependency security.
4. Key rotation.
5. Revocation.
6. Rollback protection.
7. Replay protection.
8. Long-running component soak testing.

---

## Phase 4 — OS Completion

1. Bootloader.
2. Secure boot.
3. Recovery.
4. Update/rollback.
5. Persistent storage.
6. Filesystem.
7. Driver model.
8. Networking.
9. Graphics/compositor.
10. Input.
11. Power management.
12. Hardware discovery.

---

# 19. Final Audit Verdict

Aletheia should not be categorized as a toy operating system.

The project already contains several genuinely strong design decisions:

* capabilities instead of ambient authority,
* explicit untrusted intelligence,
* deterministic execution boundaries,
* verification after effects,
* capability attenuation,
* encrypted storage,
* content addressing,
* WASM isolation,
* property-based testing,
* VM-backed kernel validation,
* multi-architecture development,
* Rust-first implementation.

The project is substantially ahead of typical hobby OS efforts in architectural discipline.

However, the current state should be described as:

> **A serious, actively developing OS foundation with a validated hosted System Core and increasingly capable VM-tested microkernel infrastructure — not yet a production-qualified operating system.**

The most important problem is not the absence of features.

It is the gap between:

```text
implemented
```

and:

```text
fully qualified
```

The project should now focus on building a qualification system where every important claim is backed by:

```text
requirement
    ↓
implementation
    ↓
invariant
    ↓
unit test
    ↓
property test
    ↓
fault injection
    ↓
VM test
    ↓
stress test
    ↓
hardware test
    ↓
CI gate
```

The highest immediate priority is:

> **Bring x86-64 to the same automated qualification level as the other first-class targets, then harden memory management, SMP behavior, device/DMA isolation, scheduler correctness, reproducible builds, and adversarial testing.**

Once those foundations are hardened, Aletheia will have a much stronger basis for moving from:

```text
research-grade OS foundation
```

toward:

```text
production-grade operating system platform
```

---

# 20. Audit Checklist

## Architecture

* [ ] Every first-class architecture has equivalent CI qualification.
* [ ] Build status is mechanically verified.
* [ ] VM boot status is mechanically verified.
* [ ] Hardware qualification is separately tracked.
* [ ] Production qualification is separately tracked.

## Memory

* [ ] Raw addresses are validated.
* [ ] Page flags are validated.
* [ ] Frame ownership is defined.
* [ ] Page tables are reclaimed.
* [ ] Address spaces are safely destroyed.
* [ ] TLB shootdown is implemented.
* [ ] W^X is enforced.
* [ ] Kernel/user layout is hardened.
* [ ] Guard pages exist.
* [ ] Fault classes are defined.

## x86-64

* [ ] Trap-frame offsets are compile-time checked.
* [ ] Trap state is per-CPU where appropriate.
* [ ] Nested interrupts are tested.
* [ ] Page faults are tested.
* [ ] Syscalls are tested.
* [ ] Timer preemption is tested.
* [ ] Context switching is stress-tested.
* [ ] SMP is tested.

## Scheduler

* [ ] State transitions are formally defined.
* [ ] No duplicate task execution.
* [ ] No lost tasks.
* [ ] No starvation.
* [ ] Priority inheritance works end-to-end.
* [ ] Cancellation is leak-free.
* [ ] Task death is safe.

## IPC

* [ ] Endpoint lifecycle is defined.
* [ ] Timeout behavior is defined.
* [ ] Cancellation is defined.
* [ ] Peer death is defined.
* [ ] Grant lifecycle is safe.
* [ ] Priority inheritance is tested.

## Devices

* [ ] DMA model is defined.
* [ ] Device ownership is defined.
* [ ] Device reset is tested.
* [ ] Virtio queues are stress-tested.
* [ ] Storage persistence is tested.
* [ ] Crash recovery is tested.

## Components

* [ ] CPU limits.
* [ ] Memory limits.
* [ ] Host-call limits.
* [ ] Output limits.
* [ ] Spawn limits.
* [ ] ABI versioning.
* [ ] Dependency security.
* [ ] Signature verification.
* [ ] Key rotation.
* [ ] Revocation.
* [ ] Rollback protection.

## CI

* [ ] Exact Rust toolchain versions.
* [ ] Locked dependency builds.
* [ ] Formatting gate.
* [ ] Clippy gate.
* [ ] Dependency audit.
* [ ] License audit.
* [ ] SBOM.
* [ ] Unsafe-code audit.
* [ ] Assembly audit.
* [ ] All architecture build gates.
* [ ] All architecture VM gates.

## OS Completion

* [ ] Bootloader.
* [ ] Secure boot.
* [ ] Recovery.
* [ ] Update system.
* [ ] Rollback.
* [ ] Persistent storage.
* [ ] Filesystem.
* [ ] Driver framework.
* [ ] Networking.
* [ ] Graphics.
* [ ] Input.
* [ ] Power management.
* [ ] Hardware discovery.
* [ ] Native experience layer.
* [ ] AI runtime.
* [ ] AI memory security.
* [ ] AI context lifecycle.

---

**Bottom line: Aletheia's foundation is credible. The next engineering challenge is not proving that the architecture can work; it is proving that every boundary continues to work under concurrency, failure, adversarial input, long-running workloads, multiple CPUs, real devices, upgrades, and reproducible production builds.**
