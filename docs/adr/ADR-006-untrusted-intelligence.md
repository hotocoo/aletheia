# ADR-006 — Intelligence is native but untrusted; deterministic pipeline holds authority

**Status:** Accepted
**Context:** Native intelligence must be deeply integrated yet must never hold authority or be trusted as truth.
**Decision:** Model/agent output is untrusted input to a deterministic pipeline: interpret (probabilistic, only stage) → parse → validate → authorize (capability) → approve → execute → verify → record. A deterministic interpreter fallback uses the identical downstream stages (never a bypass). System facts never derive solely from model output.
**Consequences:** Malformed/failed/injected output cannot execute or corrupt state (INV-001/005/006, IA-007). OS fully functional with the model absent (INT-004).
