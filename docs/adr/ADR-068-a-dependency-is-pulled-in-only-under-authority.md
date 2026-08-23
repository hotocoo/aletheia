# ADR-068: A dependency is pulled in only under authority

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P1-024 · **Builds on:** ADR-014 (composition with attenuated delegation), ADR-048 (the authority lattice), ADR-066/067 (versioned ABI, live supply chain)

## Context

Composition was the one capability-shaped action that required no capability. `SPAWN_ACTION`
("component.spawn") existed as a constant, was recorded in every audit row — and was never
EVALUATED. `host_spawn` queued whatever spawn request a guest made: naming an installed
application authorized pulling it into execution, and the only thing standing between a component
and running ANY application in the store was... nothing. The attenuated delegation on the child
was real, but the EDGE itself was free.

That made dependency resolution not a security boundary but a naming convention.

## Decision

Resolving a dependency is itself an authorized operation, checked at TWO layers:

* **At the component boundary** (`host_spawn`): the parent's exact authority is evaluated for
  `component.spawn` over THIS child (target = the application entity). Allow queues the edge;
  Deny returns the refused code with the attempt audited DENY; RequireApproval returns the
  approval sentinel — the human gate survives at the component boundary like everywhere else.
* **At fulfilment** (`prepare_spawn`): the System Core RE-EVALUATES authority against the current
  registry before loading any code, auditing a refusal as `ComponentSpawnDenied`. The queue is a
  request, never a verdict — this layer exists for every caller of the resolution path, present
  and future, and is why a guest-level answer can never be the whole story.

The grant is SCOPABLE in the existing lattice: `component.spawn` over `Scope::Entities([app])`
pins exactly which applications a component may ever pull in — the operator names the dependency
set, the engine enforces it, no new mechanism required. Revocation is live (a revoked grant
resolves nothing at the next evaluation), and the pre-existing bounds still hold underneath:
children are attenuated from the parent, spawn depth is bounded, cycles terminate.

## Consequences

Four proofs in `aletheia/tests/component_dependencies.rs`: spawning without authority is refused
BY NAME with nothing queued and no child run; a scoped grant resolves exactly its named dependency
and refuses another; an approval-constrained spawn grant does not compose inline; and a revoked
grant resolves nothing against the live registry. Existing composition tests were updated to hold
spawn authority explicitly — the tightening is the point.

Named non-claims: dependencies are resolved by REQUEST at run time; there is no separate declared
dependency manifest at installation (the capability scope IS the declaration of what may be pulled
in); deeper policy (quota per parent, dependency version pins beyond the ABI) remains future work.
