# ADR-026: Fault recovery, supervision, and production reliability

**Status:** Proposed (deferred — phased plan, no blind code) · **Date:** 2026-07-22

## Context

Fail-closed behaviour is right for security and dev gates, but a production OS also needs controlled
**recovery** from component, service, driver, and storage failures (gap-register Issue 8). Aletheia's
isolation primitives (capabilities, per-process address spaces, WASM fuel bounds) already contain
faults; what's missing is the supervision layer that detects, records, and restarts.

## Decision

An Erlang/OTP-style supervision hierarchy over the capability-isolated services:

```text
Component Crash → Detect → Contain → Record → Restart/Recover → Restore State
```

**Phase 1 — supervision + restart (hosted-first).** A `Supervisor` owns child services (each a
capability-bounded component, ADR-014); a child crash (WASM trap / fuel-kill / panic) is already
contained + leaves no partial effect (proved by the component chaos gate). The supervisor detects
exit, records crash state to the event log for diagnosis, and restarts per a policy (one-for-one /
one-for-all / rate-limited backoff). **Hosted-testable now** on the component runtime: crash a child,
assert siblings are undisturbed and the child restarts.

**Phase 2 — health + watchdogs.** Liveness/health checks; a watchdog that restarts a wedged service;
crash-loop detection that stops restarting and escalates.

**Phase 3 — system recovery.** Journal replay + transaction recovery over the persistent store
(ADR-024); safe mode / last-known-good boot (ties to ADR-025); failure diagnostics surfaced through
the experience layer's explainable traces.

## Consequences

- A failed isolated service cannot corrupt unrelated services (already true via isolation); the
  supervisor adds automated restart + recorded crash state.
- Phase 1 is hosted-testable on the existing component runtime and is the honest first slice; storage
  recovery (Phase 3) depends on ADR-024. Stays `deferred` (`REQ-REL-001`) until Phase 1 lands.
- Recovery paths are policy-driven and auditable — recovery is itself a capability-authorized action,
  not an ambient override.
