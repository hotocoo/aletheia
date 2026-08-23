# ADR-066: The component declares what it speaks

**Status:** Accepted · **Date:** 2026-08-23 · **Closes:** ALET-P1-022, ALET-P2-049 · **Builds on:** ADR-014 (the component runtime), ADR-065 (the sandbox bounded in every dimension)

## Context

The component runtime had an interface but no NAME for it. Guests imported four functions under
module `"aletheia"`, exported `run() -> i32` and `memory` — and nothing anywhere recorded which of
those facts a given binary was BUILT against. Two ways this hurts, both silent until they are not:

* the host evolves a signature; every previously installed component breaks at run time with a
  link error indistinguishable from a corrupted binary;
* a guest is built against a future or past SDK and fails somewhere far from the cause.

"Explicit versioning" means a component can SAY what it speaks, and the host can refuse — by name,
with both sides of the disagreement reported — anything it cannot vouch for.

## Decision

A component declares its ABI by carrying a custom section named `"aletheia.abi"` whose payload is
its version as FOUR LITTLE-ENDIAN BYTES (`ABI_VERSION = 1`). The declaration travels WITH the code:
re-signing or copying the bytes cannot strip it, and no side metadata has to be trusted.

Two gates, one rule:

* **At install** — `SysCore::install_component` compiles the module (nothing is instantiated, no
  guest code runs) and refuses undeclared, malformed (wrong byte length, duplicated section), or
  foreign-version modules BEFORE their bytes are stored. Unrunnable code never enters the record;
  the refusal itself is audited as `ComponentInstallRefused`; admitted components carry their
  declared version in the entity's metadata as evidence of WHAT was admitted.
* **At run** — every execution path re-checks after compilation and before any guest state exists,
  so ad-hoc runs are held to the identical standard.

The SDK stamps guests automatically: `component_main!` emits the section from the SDK's own
`ABI_VERSION`, so an SDK-built component can never silently outlive the interface it was written
against — verified end-to-end by rebuilding the real example with the real wasm32 toolchain and
running it through the unchanged gates.

The v1 import surface itself is PINNED BY A LIVE PROBE: a suite module importing all four
documented signatures must still link against the host's linker. Changing a signature without
bumping the version now fails the suite loudly; bumping the version without migrating guests
refuses every existing component at the door — both loud, neither quiet.

## Consequences

Eight proofs in `aletheia/tests/component_abi.rs`: current-version guests pass; undeclared guests
are refused at both gates by name (`abi: ... does not declare ...`) with the install refusal
audited; malformed declarations are refused as their own kind; a foreign version names BOTH sides
(`component declares ABI v999, host speaks v1`); metadata carries the declared version; the SDK
fixture declares v1 end-to-end; and the import probe pins the surface.

**Found while building this wave — ALET-P2-049, opened and closed in the same breath:** on a
workstation whose PATH prefers a Homebrew rust over the rustup toolchain, the example-build script
compiled with a rustc whose sysroot silently lacks wasm32 std, failing with E0463 far from the
cause. cargo resolves `rustc` from PATH even when cargo itself comes from rustup, so the script now
pins BOTH binaries via `rustup which`. Same family as the gate-runner defects of ADR-055's era
(P2-037/038): the harness is part of the claim.
