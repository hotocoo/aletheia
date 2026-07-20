# ADR-010 — Hosted System-Core reference before microkernel-on-metal (contract-honest)

**Status:** Accepted
**Context:** Bare-metal boot, on-GPU compositing, and NPU scheduling are untestable without hardware; but the premise-defining behaviors (capabilities, semantic store, action pipeline, agents) are testable in userspace.
**Decision:** M1 is a hosted userspace reference implementation of the System Core that enforces the SAME invariants the microkernel will (kernel-contracts crate). Contracts must not assume anything a real microkernel cannot provide. Microkernel-on-metal is P4 and swaps the hosted realization without changing layers above.
**Consequences:** Proves the premise now; defers hardware honestly; avoids a pile of non-booting no_std crates. "As much as feasible" is replaced by "what runs and proves a premise-defining behavior in userspace."
