# ADR-061: The gate counts itself

**Status:** Accepted · **Date:** 2026-08-22 · **Closes:** ALET-P2-007 (REQ-QUAL-005) · **Builds on:** ADR-012, ADR-013

## Context

Every kernel suite ends its section with a human sentence — `[net] ALL 9 NETWORK INVARIANTS HOLD` —
and every VM gate greps its sentences one by one. That is precise about what it checks and blind
about everything it does not: nothing stopped a family from VANISHING from a boot entirely (the
gate never knew the family existed), or a count from drifting without the gate's grep being edited.
The register row asked for structured, machine-readable markers; the substance is not the format,
it is the two questions the greps could not ask.

## Decision

`scripts/lib-markers.sh` (sourced by all three gates) turns a boot log into a tag=count MAP by
parsing the existing sentences — the kernel's output format is UNCHANGED, because the sentences are
already the wire format and the structure belongs where gating lives. Each gate declares an
expected family/count table and `markers_assert` holds the boot to EXACTLY that map:

* **Missing family ⇒ fail.** A suite that stops running can no longer hide behind its siblings.
* **Unexpected family ⇒ fail.** A new suite joins the expected map deliberately; silently appearing
  invariants are how gates rot. This direction is the one teams forget.
* **Changed count ⇒ fail, named.** `diff` of the two sorted maps names every difference in place.
* **Success ⇒ one machine line.** `GATE-MARKERS-V1: cap=14 conring=9 ...` — CI collects the whole
  family/count surface from a line, never from prose.

The expected maps are MEASURED per target, and that measurement is itself the assertion's second
job: the aarch64 and RISC-V maps came out identical (19 families), which is now an INVARIANT of the
platform — the same arch-independent suites must prove the same counts over either bus. The x86-64
map differs where that machine differs (mm=22, vm=72, usermode=39, plus its own ps2=5), each
difference pinned to measurement at the gate.

The assertion was attacked before shipping: wrong count, missing family, and unexpected family each
verified to fail with a named diff. Portable bash throughout — no associative arrays (macOS still
ships bash 3.2), no jq; a sorted-line diff does the work.

## Consequences

* The per-family prose greps stay — they name the failure for the human. The map adds the coverage
  and drift checks they structurally cannot provide.
* A gate's expected map is now part of the contract: changing a suite's invariant count REQUIRES
  touching the gate, which is exactly where that decision belongs.
* CI (check-ci-parity) already runs every gate script; the new library is exercised by all three.
