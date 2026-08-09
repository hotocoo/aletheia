# Aletheia — Toolchain Policy

**As of:** 2026-08-09. Enforced by `scripts/quality-gate.sh` (CI job `quality`, both pipelines).

## What is pinned, and why

| Crate | Channel | Components | Why |
|-------|---------|-----------|-----|
| `kernel/` (aarch64) | `nightly-2026-08-09` | `rust-src`, `llvm-tools-preview`, `clippy`, `rustfmt` | `build-std` needs `rust-src` |
| `kernel-riscv64/` | `nightly-2026-08-09` | same | same |
| `kernel-x86_64/` | `nightly-2026-08-09` | `clippy`, `rustfmt` | `#![feature(abi_x86_interrupt)]` for the IDT handlers |
| `kernel-core/`, `aletheia/`, `component-sdk/` | `stable` | `clippy`, `rustfmt` | no unstable features; named for shim selection (below) |

### The dated pin IS in place (2026-08-09) — ALET-P2-001 closed

The three bare-metal crates pin `nightly-2026-08-09` (rustc 1.99.0-nightly, commit `771916f90`,
2026-08-08; LLVM 23.1.0). Both pipelines install that exact toolchain in every job that needs
nightly, so a gate result now names the compiler it was produced by.

**Why this took a second attempt, recorded because the register exists to prevent unverified
claims.** The first attempt (2026-08-03) wrote the pin and then reverted it: installing the dated
toolchain failed on the development workstation — rustup rolled the install back with a
download/rename error — so the files would have named a compiler the gates had never run against.
The same failure reproduced on 2026-08-09 as a *partial* install: `rustup toolchain uninstall`
left files behind (`could not remove 'component' file`, `os error 145: The directory is not
empty`), and the next install then failed with `Missing manifest in toolchain`. Removing the
toolchain directory outright and reinstalling succeeded. The lesson is worth keeping: a rustup
install that reports failure can leave a *named but unusable* toolchain behind, which reads as
"installed" to `rustup toolchain list`.

Verified before landing: the pin installs cleanly with `rust-src`, `llvm-tools-preview`, `clippy`
and `rustfmt` plus all three cross targets, and the gates listed under **Bumping the pin** were
re-run against it.

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
2. Update the three bare-metal `rust-toolchain.toml` files, **every** nightly-installing job in both
   pipelines (not only `quality` — a job that installs a different nightly than the toml names makes
   rustup fetch a second toolchain and hides which one built the artifact), and the table above.
   `check-ci-parity.sh` requires the two pipelines to agree.
3. Run `scripts/quality-gate.sh`, `scripts/build-all.sh`, `scripts/e2e-all.sh`, `scripts/conformance.sh`,
   and `scripts/vm-e2e-vbox.sh` (the second-hypervisor rung, ADR-046) on a host with VirtualBox.
4. Commit as one change, with the gate results in the message.
