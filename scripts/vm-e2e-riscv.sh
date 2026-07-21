#!/usr/bin/env bash
# End-to-end VM boot test for the Aletheia RISC-V microkernel (VM-testing release gate, PRD §VV).
#
# Builds the riscv64 kernel and boots it in QEMU 'virt' under OpenSBI (`-bios default`), then asserts:
#   * the S->M SBI boundary answered (marker line present),
#   * the invariant selftests all pass (marker line present),
#   * the e2e PASS marker is emitted,
#   * the VM exits with status 0 (SiFive-test FINISHER_PASS).
# Any deviation fails the gate with a nonzero status. This is the RISC-V twin of scripts/vm-e2e.sh
# and the exact check CI runs for the second first-class target (ADR-019).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KDIR="$ROOT/kernel-riscv64"
TARGET="riscv64gc-unknown-none-elf"
ELF="$KDIR/target/$TARGET/debug/aletheia-kernel-riscv64"

cd "$KDIR" || { echo "FAIL: no kernel-riscv64 dir"; exit 3; }

echo "==> building riscv64 kernel"
cargo build || { echo "FAIL: build"; exit 3; }

echo "==> booting in QEMU riscv64 'virt' + OpenSBI (60s watchdog)"
OUT="$(perl -e 'alarm 60; exec @ARGV or die' \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 1 -m 128M -nographic \
  -bios default -kernel "$ELF")"
CODE=$?

echo "----------------------------------------"
echo "$OUT"
echo "----------------------------------------"
echo "vm exit code: $CODE"

fail=0
[ "$CODE" -eq 0 ] || { echo "FAIL: expected exit 0, got $CODE"; fail=1; }
echo "$OUT" | grep -q "S->M boundary OK"              || { echo "FAIL: SBI boundary marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 11 INVARIANTS HOLD"        || { echo "FAIL: invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 7 MEMORY INVARIANTS HOLD"  || { echo "FAIL: memory-management marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 13 VIRTUAL-MEMORY INVARIANTS HOLD" || { echo "FAIL: virtual-memory marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 13 USER-MODE BOUNDARY INVARIANTS HOLD" || { echo "FAIL: user-mode marker missing"; fail=1; }
echo "$OUT" | grep -q "\[e2e\] PASS"                  || { echo "FAIL: e2e PASS marker missing"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "VM-E2E (riscv64): PASS"
  exit 0
else
  echo "VM-E2E (riscv64): FAIL"
  exit 1
fi
