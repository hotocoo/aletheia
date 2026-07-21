#!/usr/bin/env bash
# End-to-end VM boot test for the Aletheia microkernel (VM-testing release gate, PRD §VV).
#
# Builds the kernel, boots it in QEMU 'virt', and asserts:
#   * the invariant selftests all pass (marker line present),
#   * the e2e PASS marker is emitted,
#   * the VM exits with status 0 (semihosting).
# Any deviation fails the gate with a nonzero status. This is the exact check CI runs.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KDIR="$ROOT/kernel"
TARGET="aarch64-unknown-none-softfloat"
ELF="$KDIR/target/$TARGET/debug/aletheia-kernel"

cd "$KDIR" || { echo "FAIL: no kernel dir"; exit 3; }

echo "==> building kernel"
cargo build || { echo "FAIL: build"; exit 3; }

echo "==> booting in QEMU (60s watchdog)"
OUT="$(perl -e 'alarm 60; exec @ARGV or die' \
  qemu-system-aarch64 -machine virt -cpu cortex-a72 -smp 1 -m 128M -nographic \
  -semihosting-config enable=on,target=native -kernel "$ELF")"
CODE=$?

echo "----------------------------------------"
echo "$OUT"
echo "----------------------------------------"
echo "vm exit code: $CODE"

fail=0
[ "$CODE" -eq 0 ] || { echo "FAIL: expected exit 0, got $CODE"; fail=1; }
echo "$OUT" | grep -q "ALL 11 INVARIANTS HOLD"        || { echo "FAIL: spine invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "MEMORY INVARIANTS HOLD"        || { echo "FAIL: memory invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "VIRTUAL-MEMORY INVARIANTS HOLD" || { echo "FAIL: virtual-memory invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "\[e2e\] PASS"                  || { echo "FAIL: e2e PASS marker missing"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "VM-E2E: PASS"
  exit 0
else
  echo "VM-E2E: FAIL"
  exit 1
fi
