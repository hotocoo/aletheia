# ADR-022: AI execution substrate and heterogeneous compute scheduler

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

Aletheia is AI-native at the semantic/capability layer (ADR-002, ADR-017, ADR-018), but the hardware
execution substrate for AI workloads is not yet a first-class OS concern (gap-register Issue 9). AI is
an **untrusted** collaborator (ADR-006): its access to accelerators and memory must be
capability-authorized like everything else, not ambient.

## Decision

Model AI execution as capability-authorized OS resources, layered over the kernel substrate:

```text
AI Runtime → Model Manager → CPU/GPU/NPU Scheduler → Capability IPC (ADR-020) → Kernel
```

**Phase 1 — model lifecycle (hosted-first).** A `ModelManager` owning load/unload, residency,
versioning, and **memory accounting** as a resource quota. Model provenance + integrity verified
before execution (ties to secure boot, ADR-025: a model is a signed, measured artifact). Inference is
**cancellable and preemptible** by policy — reuse `kernel-core::ipc` cancellation + `sched`
preemption. This phase is testable on the hosted System Core (the `ai/` subsystem already exists) with
no accelerator.

**Phase 2 — accelerator abstraction.** A capability-gated device class (per ADR-023's driver model):
GPU/NPU discovery, command queues, DMA + IOMMU-enforced isolation, shared buffers via ADR-020's
grant-table. A model reaches an accelerator ONLY through an `accel.submit` capability; no ambient
device access. Requires real hardware — deferred, VM/where-possible-gated.

**Phase 3 — heterogeneous scheduler.** Represent CPU/GPU/NPU execution in one scheduler view with
resource quotas, inference priority, multi-model routing, and speculative-execution management.
Depends on SMP (ADR-021) and the accelerator abstraction.

## Consequences

- AI workloads gain explicit resource accounting; a model cannot touch unauthorized accelerator
  resources (capability + IOMMU).
- Phase 1 is largely hosted-testable now; Phases 2–3 are hardware-bound and stay `deferred`
  (`REQ-AI-003`) until brought up.
- Preserves the "untrusted intelligence" invariant end-to-end: the AI substrate is scheduled and
  sandboxed by the OS, never trusted to self-authorize.
