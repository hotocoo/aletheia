# ADR-163 — What an interactive boot pays for: every contract, none of the storms

- **Status:** accepted
- **Date:** 2026-09-23
- **Requirement:** REQ-PERF-004
- **Supersedes:** nothing. Acts on ADR-162's evidence.

## Context

ADR-162 measured where boot time goes. On every CPU the heavy laps are the load tests - the
storms (`fsstorm`, `schedstorm`, `shellstorm`, `wmstorm`), the soak, the ML-risk stress and the
bench - and, on aarch64, the performance-validation pass run after the suites. Together they are
roughly two thirds of aarch64's boot and half of riscv64's. They prove that bounds hold under
volume, on this machine's own heap: real proofs, and the reason the desktop does not leak per
event. They are also the same proof every time, and a person waiting for a prompt pays for it
on every boot while the gate image proves the identical code on every push.

The contract suites - capabilities, memory, TLS, HTTP, the renderer, the policy, the IOMMU -
cost tens of milliseconds each and say what THIS boot's machine does. They stay.

## Decision

One constant in each kernel's `kmain`:

```rust
const STORMS_AT_BOOT: bool = !cfg!(feature = "interactive");
```

The seven load blocks (`bench`, `soak`, `mlrisk-stress`, `wmstorm`, `schedstorm`, `fsstorm`,
`shellstorm`) and aarch64's `bench::run()` pass run when it is true. The gate image - built
without the feature by `scripts/vm-e2e*.sh`, `scripts/conformance.sh`, the x86-64 smoke test -
keeps every one of them and every expected-map count; the conformance contract is unchanged at
383. The interactive image prints one line where the summary prints:

```
[boot] deferred in this interactive image (ADR-163): bench, soak, mlrisk-stress, wmstorm, schedstorm, fsstorm, shellstorm - the gate image proves them on every push
```

**What stays before the prompt, deliberately:** the VT-d suite on x86-64, 3.5 s under TCG. It
programs a real remapping unit with per-device windows and PROVES enforcement - a revoked page
denied with a fault naming its source - and a machine that offers a console before its IOMMU is
proved to enforce would be offering the wrong thing first. Its cost is the emulator's (page
walks under TCG); hardware is expected to be far cheaper and has not been measured.

## Proof

Gate images: the three boot gates and conformance (383) unchanged and green. Interactive images:
every interactive gate (console, desktop, keyboard, virtio-input, HTTPS, browser window) green,
each boot log carrying the `deferred` line and a `[boot] suites:` summary without the deferred
families. The interactive boot's suite time is in `docs/BOOT-COST.md` next to the gate image's.

## Alternatives considered

**A `selftest storms` console verb** to run the deferred proofs on demand. Not this wave: the
storms need what the console does not own (the ML model, the desktop's heap hook, a spare block
device), and a verb that ran a third of them would misname what it proves. The gate image is the
on-demand proof: run it.

**Deferring the VT-d suite too.** Rejected above.

**Making the storms smaller.** A different decision - about what the proof needs - and one the
storm ADRs (086, 087) made deliberately. Not reopened here.

## Consequences

An interactive boot pays for its contracts and not for its load tests; the gate image pays for
both on every push. `docs/BOOT-COST.md` records both images' cost. Anyone reading an interactive
boot log sees exactly which proofs it did not run and where to find them.
