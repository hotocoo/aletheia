#!/usr/bin/env bash
# Single repository-wide integration build across EVERY Aletheia crate and CPU target (ALET-P0-002).
#
# The repo is intentionally NOT one Cargo workspace: the three kernels each pin their own
# rust-toolchain.toml and cross-compile to different `no_std`/UEFI targets (aarch64-unknown-none,
# riscv64gc-unknown-none, x86_64-unknown-uefi), which a single workspace/target cannot express. So
# the "complete build" is a matrix: each crate built with its own pinned toolchain and target, with
# one aggregate pass/fail — no crate can silently rot while others stay green.
#
# Host crates (aletheia, kernel-core) are also TESTED here (they carry the hosted acceptance +
# policy suites). The bare-metal kernels are BUILT here; their boot behavior is gated by the
# per-target VM e2e scripts (scripts/vm-e2e*.sh), not by this build.
#
# Uses the rustup shim (a Homebrew/system cargo earlier in PATH ignores rust-toolchain.toml and
# fails cross builds with E0463). Ensures each cross target is present (idempotent). Exit 0 iff all.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

# Ensure the nightly cross targets exist for the bare-metal kernels (host crates need no target add).
if command -v rustup >/dev/null 2>&1; then
  rustup target add aarch64-unknown-none-softfloat --toolchain nightly >/dev/null 2>&1 || true
  rustup target add riscv64gc-unknown-none-elf     --toolchain nightly >/dev/null 2>&1 || true
  rustup target add x86_64-unknown-uefi            --toolchain nightly >/dev/null 2>&1 || true
fi

declare -a NAMES RESULTS
overall=0

run() { # $1 = label, $2 = crate dir, $3.. = cargo args
  local label="$1" dir="$2"; shift 2
  echo "========================================================================"
  echo "==> $label  (cargo $*)"
  echo "========================================================================"
  if ( cd "$ROOT/$dir" && cargo "$@" ); then
    NAMES+=("$label"); RESULTS+=("PASS")
  else
    NAMES+=("$label"); RESULTS+=("FAIL"); overall=1
  fi
}

# Host crates: build + test (locked to Cargo.lock for reproducibility).
run "aletheia (host, test)"    aletheia       test  --locked
run "kernel-core (host, test)" kernel-core    test  --locked
# Bare-metal kernels: cross build (boot behavior gated separately by scripts/vm-e2e*.sh).
run "kernel aarch64 (build)"   kernel         build --locked
run "kernel riscv64 (build)"   kernel-riscv64 build --locked
run "kernel x86-64 (build)"    kernel-x86_64  build --locked --release

echo "========================================================================"
echo "BUILD-ALL SUMMARY"
echo "========================================================================"
for i in "${!NAMES[@]}"; do printf '  %-28s : %s\n' "${NAMES[$i]}" "${RESULTS[$i]}"; done
echo "========================================================================"
if [ "$overall" -eq 0 ]; then echo "BUILD-ALL: PASS"; else echo "BUILD-ALL: FAIL"; fi
exit "$overall"
