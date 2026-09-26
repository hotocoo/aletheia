#!/usr/bin/env bash
# The programs a new namespace is seeded with are built from Rust source in userland/ and checked
# in under userland/bin/ (ADR-205), the way the model blobs are. This gate rebuilds them with the
# pinned toolchain and requires the result to be byte-identical to what the kernel embeds: a
# checked-in binary that no longer comes from its source is refused, never shipped.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
cd "$ROOT/userland" || exit 2
fail=0
for pair in "aarch64-unknown-none-softfloat aarch64" "riscv64gc-unknown-none-elf riscv64" "x86_64-unknown-none x86_64"; do
  set -- $pair
  if ! cargo build --release --target "$1" >/dev/null 2>&1; then
    echo "  FAIL: userland does not build for $1"; fail=1; continue
  fi
  if ! cargo clippy --release --target "$1" -- -D warnings >/dev/null 2>&1; then
    echo "  FAIL: userland has clippy warnings on $1 (cargo clippy --release --target $1)"; fail=1
  fi
  for prog in hello; do
    built="target/$1/release/$prog"
    shipped="bin/$2/$prog.elf"
    if cmp -s "$built" "$shipped"; then
      echo "  PASS: $shipped is exactly what $1 builds from source"
    else
      echo "  FAIL: $shipped differs from a fresh $1 build (rebuild: cp $built $shipped)"; fail=1
    fi
  done
done
[ "$fail" -eq 0 ] && echo "USERLAND: PASS" || echo "USERLAND: FAIL"
exit "$fail"
