#!/usr/bin/env bash
# Repository quality gate (GAPS4 ALET-P2-003/004/005, REQ-QUAL-002).
#
# The boot gates prove the OS behaves. This proves the SOURCE is in the state the project claims:
#   [1] formatting          — `cargo fmt --check`, every crate
#   [2] lints               — `cargo clippy -D warnings`, every crate + every CPU target
#   [3] advisories          — `cargo audit` against the committed lockfiles
#   [4] licenses            — every dependency's license is on the allow-list
#   [5] SBOM                — a deterministic inventory of every dependency, written to build/sbom/
#
# DOCTRINE (the same one the VM gates follow): a check that cannot run must SKIP LOUDLY, never pass
# silently. `cargo audit` and the license/SBOM steps need `cargo-audit` / `cargo metadata`; when a tool is
# missing the gate prints an explicit SKIP line and sets a flag that the summary reports. A green summary
# therefore means "everything that could run, ran" and names anything that could not.
#
# Exit 0 iff every check that ran passed.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 3
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

# Crates, and the target each one lints for (host crates lint for the host).
CRATES=(aletheia kernel-core component-sdk kernel kernel-riscv64 kernel-x86_64)
declare -a TARGETS=("" "" "" "aarch64-unknown-none-softfloat" "riscv64gc-unknown-none-elf" "x86_64-unknown-uefi")

fail=0
skips=()
hr() { printf '========================================================================\n'; }
step() { hr; echo "==> $1"; hr; }

step "[1] formatting — cargo fmt --check"
for c in "${CRATES[@]}"; do
  [ -f "$c/Cargo.toml" ] || continue
  if cargo fmt --manifest-path "$c/Cargo.toml" --all -- --check >/dev/null 2>&1; then
    echo "  PASS fmt: $c"
  else
    echo "  FAIL fmt: $c (run: cargo fmt --manifest-path $c/Cargo.toml --all)"
    fail=1
  fi
done

step "[2] lints — cargo clippy -D warnings"
# Each crate is linted from ITS OWN directory, because a bare-metal crate's `.cargo/config.toml` is what
# supplies its target and `build-std` — passing `--target` from the repo root instead makes rustc look for
# a precompiled `core` that does not exist for `-none` targets (E0463), which reads like a broken lint run
# but is really a broken invocation. `--all-targets` only for host crates: the kernels have no test target.
for i in "${!CRATES[@]}"; do
  c="${CRATES[$i]}"
  t="${TARGETS[$i]}"
  [ -f "$c/Cargo.toml" ] || continue
  if [ -n "$t" ]; then
    out="$(cd "$c" && cargo clippy --locked -- -D warnings 2>&1)"
    rc=$?
  else
    out="$(cd "$c" && cargo clippy --locked --all-targets -- -D warnings 2>&1)"
    rc=$?
  fi
  if [ "$rc" -eq 0 ]; then
    echo "  PASS clippy: $c ${t:+($t)}"
  else
    echo "  FAIL clippy: $c ${t:+($t)}"
    echo "$out" | grep -E '^(error|warning)' | head -20
    fail=1
  fi
done

step "[3] advisories — cargo audit"
if command -v cargo-audit >/dev/null 2>&1 || cargo audit --version >/dev/null 2>&1; then
  for c in aletheia component-sdk; do
    [ -f "$c/Cargo.lock" ] || continue
    if cargo audit --file "$c/Cargo.lock" >/dev/null 2>&1; then
      echo "  PASS audit: $c/Cargo.lock"
    else
      echo "  FAIL audit: $c/Cargo.lock"
      cargo audit --file "$c/Cargo.lock" 2>&1 | tail -25
      fail=1
    fi
  done
else
  echo "  SKIP audit: cargo-audit not installed (install: cargo install cargo-audit --locked)"
  skips+=("advisories")
fi

step "[4] licenses + [5] SBOM — from cargo metadata (no extra toolchain)"
if cargo metadata --version >/dev/null 2>&1 || cargo --version >/dev/null 2>&1; then
  mkdir -p build/sbom
  python3 "$ROOT/scripts/sbom.py" || fail=1
else
  echo "  SKIP sbom: cargo unavailable"
  skips+=("licenses+sbom")
fi

hr
if [ "${#skips[@]}" -gt 0 ]; then
  echo "SKIPPED (tool unavailable on this host, never a silent pass): ${skips[*]}"
fi
if [ "$fail" -eq 0 ]; then
  echo "QUALITY-GATE: PASS"
  exit 0
else
  echo "QUALITY-GATE: FAIL"
  exit 1
fi
