# ADR-015: Policy & approval separated from capability authority

**Status:** Accepted · **Date:** 2026-07-21

## Context

The v1 pipeline conflated two questions in one place: the capability engine returned
`RequireApproval`, and `syscore` additionally hard-coded a destructive-risk check inline. The PRD
requires that "capability authorization remains separate from human approval for destructive or
high-risk actions" (SAD §10 lists `approve()` as its own pipeline stage).

## Decision

Introduce a distinct **policy engine** (`policy.rs`) as a second, independent axis:

- **Capabilities** decide *authority*: `Allow | Deny | RequireApproval`. Unchanged.
- **Policy** decides *governance*: given the capability decision and the operation's risk, must a
  human approve? Both approval triggers — destructive-risk operation, and an approval-constrained
  capability — are unified here.

Approvals have a durable lifecycle: `PendingApproval` is bound to the exact intent, persisted via
the immutable event log (`ApprovalRequested` / `ApprovalResolved`) and replayed into an in-memory
registry on open (survives restart). Granting re-runs the bound intent with approval satisfied;
**approval confers no authority** — capabilities are re-evaluated on execution.

## Consequences

- The AI is doubly separated from execution: it neither authorizes nor approves.
- `capability_decision` (authority) and `approval` (governance) are distinct fields in the trace.
- The storage layer stays ignorant of policy (approvals ride the existing event log) — no new
  storage↔policy coupling; the durable source of truth is the audit log.
- The same separation maps cleanly onto the future native kernel: capabilities are a KC-CAP concern;
  approval is a system-service concern above it.
