# ADR-012 — Verification & Validation is first-class architecture (the validation pyramid)

**Status:** Accepted
**Context:** Aletheia is a from-scratch operating system, not an application. Unit and
integration tests alone cannot establish that a security-critical, AI-native OS is correct —
they only show that the scenarios someone thought to write pass. Testing, verification,
validation, observability, fuzzing, and hardware qualification cannot be a final QA phase
bolted on at the end; by then the architecture that makes them possible (observable events,
deterministic verification, capability confinement, fault isolation) is already frozen.
**Decision:** V&V is designed into every subsystem as a first-class concern. Every major
subsystem MUST define: functional requirements, security invariants, safety invariants,
performance targets, failure/recovery behavior, test strategy, fuzzing strategy,
observability requirements, hardware-validation requirements, and release gates. The full
validation pyramid is normative and ordered: Unit → Integration → Property-Based → Fuzzing →
Stress/Load → Chaos/Fault-Injection → Formal Verification (critical components only) →
VM/Emulator → Real Hardware → Security Audit → Performance Validation. Guiding principle:
**tests are evidence that known scenarios work; they are not proof of correctness** — hence
property-based testing, fuzzing, formal methods on the critical core, and runtime invariant
assertions that fail loud with rich diagnostics rather than corrupting silently. The
AI subsystem is explicitly required never to be a single point of failure for the OS.
**Consequences:** Higher up-front design cost per subsystem and a heavier CI pipeline, in
exchange for defensible correctness claims, honest coverage of unknowns, and an OS that can
explain itself at the diagnostic level. Formal verification is scoped to capability
enforcement, memory isolation, IPC security, critical scheduler invariants, and cryptographic
boundaries — not the whole OS — to keep the proof burden tractable. See PRD-003 §V&V.
