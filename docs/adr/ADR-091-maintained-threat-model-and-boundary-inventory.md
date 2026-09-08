# ADR-091: maintained threat model and explicit security-boundary inventory

**Status:** Accepted · **Date:** 2026-09-08 · **Advances:** ALET-P2-029, ALET-P2-030, ALET-P2-031

## Decision

Maintain one security threat-model document, `docs/THREAT-MODEL.md`, as the explicit inventory of
security boundaries. Every boundary names its crossing data, authoritative admission point,
unauthorized-effect failure, and availability/DoS failure class. `scripts/check-threat-model.sh`
runs in CI and checks the inventory's internal consistency and evidence paths.

## Security/DoS separation

Authorization answers **who may cause an effect**. DoS answers **whether a bounded resource remains
available**. They are independent: an authorized request can exhaust a resource, while an available
system can still have an authorization bypass. Security regressions therefore test unauthorized
effects and state preservation separately from exhaustion behavior.

## Consequences

New security-sensitive boundaries must be inventoried with evidence. New resource-exhaustion
mechanisms must state their bounded resource and refusal semantics without treating refusal as an
authorization proof. Residual threats remain open/deferred until enforcement exists.
