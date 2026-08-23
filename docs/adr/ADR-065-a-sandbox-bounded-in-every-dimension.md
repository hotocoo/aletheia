# ADR-065: The sandbox is bounded in every dimension

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P1-021 · **Builds on:** ADR-014 (the component runtime and its fuel bound)

## Context

The WASM component sandbox (ADR-014) bounded exactly one resource: fuel. Fuel measures COMPUTE —
instructions bought by a run. It says nothing about how much MEMORY a guest may hold, how large its
TABLES may grow, how DEEP its call stack may wind, or how much WALL-CLOCK time it may consume.
Those are four separate ways to hurt the machine hosting you:

* a guest that grows linear memory without limit exhausts host RAM — the classic
  allocation-bomb denial of service;
* unbounded table growth is the same attack through the second growable resource;
* an infinitely recursing guest dies at whatever stack bound the ENGINE shipped with, as an
  anonymous trap nobody configured and nobody can name in an audit log;
* a guest whose work is I/O-shaped (many host calls) may spend arbitrarily much real time while
  burning almost no fuel, because host-call bodies are not fuel-metered.

A register row that says "resource model beyond fuel" stays open until each of these has a number,
a refusal, and a name.

## Decision

`SandboxLimits` (`aletheia/src/component.rs`) carries five explicit bounds. There is no
constructor that yields "no limit": `defaults()` bounds every dimension (4 MiB of memory,
1024 table elements, 16 Ki stack slots, 256 frames of recursion, a 30 s clock), and opting out of
any one of them must be WRITTEN (`deadline_ms: 0`), so a forgotten limit fails closed rather
than running wide.

* **Memory and tables** ride wasmi's store limiter, scoped to THIS store, which lives and dies with
  this one run. Growth past a cap FAILS the way the spec allows — `memory.grow`/`table.grow`
  answer -1 — so a well-behaved guest keeps running inside its cap; a deaf guest that ignores the
  -1 never gets another byte and simply burns its fuel asking. Instance/table/memory COUNTS are
  pinned to 1: a component gets one of each.
* **Stack** is two engine compilation limits — operand-stack height and recursion depth — set per
  run because the engine itself is created per run. Exceeding either traps
  `StackOverflow`, reported as `KillReason::Stack`.
* **Wall clock** is enforced where it can honestly be: at every HOST-CALL CROSSING. A crossing
  that arrives after the deadline audits a `DEADLINE` row and refuses by trap; nothing further
  can be authorized on a clock the run has already outrun. Between crossings, fuel bounds the
  work — that division of labor is stated, not hidden.

Every outcome now names WHICH bound held (`killed_by`: fuel / deadline / stack), because an
audit log that only says "an error" makes the three indistinguishable and the operator guesses. An
overrun the guest managed to FINISH inside (a pure-compute guest that never crosses the boundary)
is still reported, in `deadline_exceeded`, because silence would read as compliance.

Composition follows capability: the whole spawn tree runs inside the ROOT's envelope
(`SysCore::compose_run` passes one `SandboxLimits` down), so budget narrows down the tree
exactly as attenuated authority does — a child can never out-resource its parent.

## Consequences

Host-proved in `aletheia/tests/component_resources.rs` (10 tests): caps hold EXACTLY (a guest
that counts its wins reports precisely the configured page/element budget); a deaf hog dies of
fuel, never memory; a recursion bomb is killed BY NAME and depth is enforced exactly as configured
in both directions; a clock-eater is stopped at a crossing whose DEADLINE audit row is the LAST
thing recorded; defaults bound every dimension; the explicit opt-out exists only when written; and
a spawned child inherits the root's envelope.

Named non-claims: the deadline cannot interrupt a single host-call BODY (bounded by the store's
operation granularity, not the clock); between crossings fuel-to-time ratio varies with the
workload, so wall-clock for pure-compute guests is bounded via fuel alone; limits are per-RUN
today — binding per-component limits into installation records is future work, stated in the code
where `run_installed` deliberately uses the defaults rather than pretending otherwise.
