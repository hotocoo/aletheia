# Aletheia — Toolchain Policy

**As of:** 2026-08-03. Enforced by `scripts/quality-gate.sh` (CI job `quality`, both pipelines).

## What is pinned, and why

| Crate | Channel | Components | Why |
|-------|---------|-----------|-----|
| `kernel/` (aarch64) | `nightly` (floating) | `rust-src`, `llvm-tools-preview`, `clippy`, `rustfmt` | `build-std` needs `rust-src` |
| `kernel-riscv64/` | `nightly` (floating) | same | same |
| `kernel-x86_64/` | `nightly` (floating) | `clippy`, `rustfmt` | `#![feature(abi_x86_interrupt)]` for the IDT handlers |
| `kernel-core/`, `aletheia/`, `component-sdk/` | `stable` | `clippy`, `rustfmt` | no unstable features; named for shim selection (below) |

### The dated pin is NOT in place — stated, not implied

A dated pin (`channel = "nightly-<date>"`) is the right answer, and this wave did not land it. It was
written, and then reverted: installing the dated toolchain failed on the workstation this was developed on
(`rustup` rolled the install back with a download/rename error), so every gate would have been running
against a *different* compiler than the one the files named. Landing it would have meant claiming a pin
whose gates had never been run against it — the exact kind of unverified claim this repo's register exists
to prevent. GAPS4 **ALET-P2-001 therefore stays open**, with this as the reason.

What DID land, and is verified: every toolchain file now requests `clippy` and `rustfmt` explicitly, so
the quality gate cannot be skipped for lack of a component; and the host crates name a channel (below).

To finish the row: install a dated nightly successfully, put that date in the three bare-metal files and
in the `quality` job of **both** pipelines (`check-ci-parity.sh` requires them to agree), then re-run
`quality-gate.sh`, `build-all.sh`, `e2e-all.sh` and `conformance.sh` before committing.

## Why the host crates name a channel at all

Not for reproducibility — for **shim selection**. Without a `rust-toolchain.toml`, `cargo` resolves to
whatever is first on `PATH`, and on a macOS workstation that is often Homebrew's cargo, which ignores
`rust-toolchain.toml` entirely and builds for the host triple. That has produced real failures in this
repo (a cross build failing with `E0463: can't find crate for core`, surfacing only as "FAIL: build" in a
VM gate). Naming the channel makes the rustup shim take over.

`scripts/vm-e2e*.sh` additionally prepend `$HOME/.cargo/bin` to `PATH` for the same reason. Both
mechanisms are kept: the scripts fix the invocation, the toml fixes the resolution.

## Why the exact stable version is NOT pinned

A specific stable release would force every contributor and CI runner to download that exact version
before anything builds, for crates that use no unstable features and are already covered by
`clippy -D warnings` and `--locked`. The bare-metal crates — where the compiler genuinely affects the
artifact — carry the dated pin. If a stable-release regression ever bites, this is the file to change.

## Components

Every pin requests `clippy` and `rustfmt` explicitly, so the quality gate cannot be skipped for lack of a
component (a missing component would otherwise look like a passing run on a host that has neither).

## Bumping the pin

1. `rustup toolchain install nightly-<date> --profile minimal --component rust-src,clippy,rustfmt`
2. Update the three bare-metal `rust-toolchain.toml` files, the `quality` job in **both** pipelines, and
   the table above (the same date in all six places — `check-ci-parity.sh` requires the pipelines to agree).
3. Run `scripts/quality-gate.sh`, `scripts/build-all.sh`, `scripts/e2e-all.sh`, `scripts/conformance.sh`.
4. Commit as one change, with the gate results in the message.
