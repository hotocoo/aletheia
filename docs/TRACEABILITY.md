# Aletheia — Requirement Traceability Matrix

**Machine-checkable** by `scripts/check-traceability.sh` (run in CI). This closes gap-register
**Issue 12** ("Establish Machine-Checkable Architecture Traceability"): every architectural
requirement has a stable identifier and maps to its ADR, implementation module, test, and VM gate,
so a claim of "delivered" is auditable against real evidence and **deferred work is explicitly
distinguished from implemented work**.

## How it is checked

The table below is the single source of truth. `scripts/check-traceability.sh` parses every row
whose Req ID begins with `REQ-` and enforces:

- **`delivered` / `partial`** — the Implementation and Test columns must name real files that exist
  in the tree (each `;`-separated path is checked), and any named VM Gate script must exist. A row
  marked delivered with a missing or `-` evidence path **fails the build** — the check that "CI can
  detect requirements marked delivered without evidence."
- **`deferred`** — evidence columns are `-` (nothing is built yet); the row documents the ADR /
  phased plan only. A deferred row is never counted as delivered.
- Every row's Status must be one of `delivered` / `partial` / `deferred`.
- **Target-specific rows** (GAPS2 Issue #1). Requirements whose implementation is per-CPU — kernel
  boot (`REQ-KERN-001/002/003`), memory (`REQ-MEM-{AARCH64,X86,RISCV}-001`), and user-mode
  (`REQ-USER-{AARCH64,X86,RISCV}-001`) — are split into one row **per target**, each naming that
  target's own implementation and its own VM gate (`scripts/vm-e2e.sh` for aarch64,
  `kernel-x86_64/scripts/smoke-test.sh` for x86-64, `scripts/vm-e2e-riscv.sh` for RISC-V). This closes
  the hole where a generic row listed several impls but omitted a target's gate, so a target-specific
  regression could compile and escape the requirement gate. A regression in one target's user-mode now
  fails that target's named gate, not a sibling's.

Running a VM gate itself (booting QEMU) remains the existing `vm-e2e*` / `smoke-test` CI jobs; this
check verifies the **mapping** is real and that no delivered claim is evidence-free.

## Matrix

| Req ID | Title | ADR | Implementation | Test | VM Gate | Status |
|--------|-------|-----|----------------|------|---------|--------|
| REQ-CAP-001 | Fail-closed authorization (no capability ⇒ deny) | ADR-003 | kernel-core/src/spine.rs | kernel-core/tests/invariants.rs; kernel-core/tests/security_behavior.rs | scripts/vm-e2e.sh | delivered |
| REQ-CAP-002 | Unforgeable capability tokens | ADR-003 | kernel-core/src/spine.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e.sh | delivered |
| REQ-CAP-003 | Delegation attenuation (never amplifies) | ADR-003 | kernel-core/src/spine.rs | kernel-core/tests/invariants.rs; kernel-core/tests/security_behavior.rs | scripts/vm-e2e.sh | delivered |
| REQ-CAP-004 | Cascading revocation | ADR-003 | kernel-core/src/spine.rs | kernel-core/tests/invariants.rs; kernel-core/tests/security_behavior.rs | scripts/vm-e2e.sh | delivered |
| REQ-CAP-005 | Policy/approval separation for destructive actions | ADR-015 | kernel-core/src/spine.rs; aletheia/src/policy.rs | kernel-core/tests/invariants.rs | - | delivered |
| REQ-PIPE-001 | Intent→Action pipeline: validate→authorize→execute→verify→record | ADR-002 | kernel-core/src/spine.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e.sh | delivered |
| REQ-PIPE-002 | Malformed model output cannot execute | ADR-006 | kernel-core/src/spine.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e.sh | delivered |
| REQ-STORE-001 | Content-addressed, versioned semantic store | ADR-005 | kernel-core/src/spine.rs; aletheia/src/storage.rs | kernel-core/tests/invariants.rs | - | delivered |
| REQ-STORE-002 | Encrypted at rest (ChaCha20-Poly1305) | ADR-005 | aletheia/src/storage.rs | aletheia/tests/acceptance.rs | - | delivered |
| REQ-IPC-001 | Capability-gated synchronous IPC | ADR-016 | kernel-core/src/ipc.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e.sh | delivered |
| REQ-IPC-002 | Capability transfer with attenuation | ADR-016 | kernel-core/src/ipc.rs | kernel-core/tests/invariants.rs | - | delivered |
| REQ-IPC-003 | Bounded message queues | ADR-016 | kernel-core/src/ipc.rs | kernel-core/tests/invariants.rs | - | delivered |
| REQ-IPC-004 | Asynchronous notifications (coalescing badge) | ADR-020 | kernel-core/src/ipc.rs | kernel-core/tests/ipc.rs | - | delivered |
| REQ-IPC-005 | Deadline / timeout-aware receive | ADR-020 | kernel-core/src/ipc.rs | kernel-core/tests/ipc.rs | - | delivered |
| REQ-IPC-006 | Message cancellation | ADR-020 | kernel-core/src/ipc.rs | kernel-core/tests/ipc.rs | - | delivered |
| REQ-IPC-007 | IPC tracing + deterministic replay | ADR-020 | kernel-core/src/ipc.rs | kernel-core/tests/ipc.rs | - | delivered |
| REQ-IPC-008 | Zero-copy shared-memory channels | ADR-020 | kernel-core/src/grant.rs | kernel-core/tests/grant.rs | - | partial |
| REQ-IPC-009 | Priority inheritance / donation | ADR-020 | kernel-core/src/priosched.rs | kernel-core/tests/priosched.rs | - | partial |
| REQ-COMP-001 | WASM components, no ambient authority | ADR-014 | aletheia/src/component.rs | aletheia/tests/component.rs | - | delivered |
| REQ-COMP-002 | Fuel-bounded component execution | ADR-014 | aletheia/src/component.rs | aletheia/tests/component.rs | - | delivered |
| REQ-COMP-003 | Multi-agent spawn with attenuated delegation | ADR-014 | aletheia/src/component.rs | aletheia/tests/component.rs | - | delivered |
| REQ-COMP-004 | Rust component SDK | ADR-014 | component-sdk/src/lib.rs | aletheia/tests/sdk_component.rs | - | delivered |
| REQ-COMP-005 | Component property / chaos gate | ADR-014 | aletheia/src/component.rs | aletheia/tests/component_chaos.rs | - | delivered |
| REQ-EXP-001 | Capability-gated World-Model search | ADR-018 | aletheia/src/ai/context.rs | aletheia/tests/search.rs | - | partial |
| REQ-EXP-002 | Native desktop / compositor / dynamic UI | ADR-009 | - | - | - | deferred |
| REQ-SVC-001 | Service API / IPC boundary + daemon | ADR-016 | aletheia/src/service.rs | aletheia/tests/conformance.rs | - | delivered |
| REQ-AI-001 | Model-agnostic, untrusted AI provider | ADR-017 | aletheia/src/ai/mod.rs; aletheia/src/ai/runtime.rs | aletheia/tests/acceptance.rs | - | delivered |
| REQ-AI-002 | Context Fabric (capability-aware, not RAG) | ADR-018 | aletheia/src/ai/context.rs | aletheia/tests/search.rs | - | delivered |
| REQ-AI-003 | AI execution substrate + heterogeneous CPU/GPU/NPU scheduler | ADR-022 | - | - | - | deferred |
| REQ-KERN-001 | aarch64 microkernel boots + spine invariants | ADR-019 | kernel/src/main.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e.sh | delivered |
| REQ-KERN-002 | x86-64 bootable image + spine invariants | ADR-019 | kernel-x86_64/src/main.rs | kernel-core/tests/invariants.rs | kernel-x86_64/scripts/smoke-test.sh | delivered |
| REQ-KERN-003 | RISC-V microkernel + spine invariants | ADR-019 | kernel-riscv64/src/main.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e-riscv.sh | delivered |
| REQ-KERN-004 | Shared kernel-core spine + selftest (Issue 1 slice) | ADR-019 | kernel-core/src/spine.rs; kernel-core/src/selftest.rs | kernel-core/tests/invariants.rs | scripts/vm-e2e.sh | delivered |
| REQ-KERN-005 | Shared kernel-core task/scheduler policy (Issue 1 rest) | ADR-019 | kernel-core/src/sched.rs; kernel/src/usermode.rs | kernel-core/tests/sched.rs | scripts/vm-e2e.sh | delivered |
| REQ-MEM-AARCH64-001 | Physical frame allocator + MMU virtual memory (aarch64) | ADR-019 | kernel/src/frames.rs; kernel/src/vm.rs | kernel/src/vm.rs | scripts/vm-e2e.sh | delivered |
| REQ-MEM-X86-001 | Physical frame allocator (UEFI map) + MMU map/unmap over live PML4 (x86-64) | ADR-019 | kernel-x86_64/src/frames.rs; kernel-x86_64/src/vm.rs | kernel-x86_64/src/vm.rs | kernel-x86_64/scripts/smoke-test.sh | delivered |
| REQ-MEM-RISCV-001 | Physical frame allocator + Sv39 MMU virtual memory (RISC-V) | ADR-019 | kernel-riscv64/src/frames.rs; kernel-riscv64/src/vm.rs | kernel-riscv64/src/vm.rs | scripts/vm-e2e-riscv.sh | delivered |
| REQ-USER-AARCH64-001 | EL0 user-mode: cap-gated syscall + per-process address space + preemptive multitasking (aarch64) | ADR-019 | kernel/src/usermode.rs | kernel/src/usermode.rs | scripts/vm-e2e.sh | delivered |
| REQ-USER-X86-001 | ring-3 user-mode: cap-gated syscall + per-process PML4 + PIT preemption (x86-64) | ADR-019 | kernel-x86_64/src/usermode.rs | kernel-x86_64/src/usermode.rs | kernel-x86_64/scripts/smoke-test.sh | delivered |
| REQ-USER-RISCV-001 | U-mode: cap-gated ecall + per-process satp + S-timer preemption (RISC-V) | ADR-019 | kernel-riscv64/src/usermode.rs | kernel-riscv64/src/usermode.rs | scripts/vm-e2e-riscv.sh | delivered |
| REQ-SMP-001 | SMP / multicore scheduling | ADR-021 | - | - | - | deferred |
| REQ-SEC-001 | Adversarial security-behaviour regressions | ADR-003 | kernel-core/src/spine.rs | kernel-core/tests/security_behavior.rs | - | delivered |
| REQ-DRV-001 | Device / driver architecture | ADR-023 | - | - | - | deferred |
| REQ-STOR-001 | Persistent storage / filesystem / recovery | ADR-024 | - | - | - | deferred |
| REQ-BOOT-001 | Secure boot + chain of trust (component signature verification) | ADR-025 | aletheia/src/provenance.rs; aletheia/src/crypto.rs | aletheia/tests/component_signing.rs | - | partial |
| REQ-REL-001 | Fault recovery / supervision | ADR-026 | - | - | - | deferred |

> Deferred rows carry an ADR (the phased plan) but no code — by design (ADR-010: no blind hardware
> code). They become `partial`/`delivered` only when a real implementation and test land, at which
> point this check begins enforcing their evidence.
