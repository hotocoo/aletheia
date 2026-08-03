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
# Honor the per-crate nightly toolchain via the rustup shim. A Homebrew/system `cargo` earlier in
# PATH ignores rust-toolchain.toml and builds for the host triple, failing the cross build with
# E0463 "can't find crate for core" — which surfaces here only as "FAIL: build".
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
KDIR="$ROOT/kernel"
TARGET="aarch64-unknown-none-softfloat"
ELF="$KDIR/target/$TARGET/debug/aletheia-kernel"

cd "$KDIR" || { echo "FAIL: no kernel dir"; exit 3; }

echo "==> building kernel"
cargo build || { echo "FAIL: build"; exit 3; }

# Attach a real virtio-blk device (REQ-DRV-001, ADR-023): a fresh 1 MiB raw backing image
# (2048 sectors = 256 4 KiB blocks) so the driver probes a real transport, reads capacity, and runs
# the journal over emulated storage. Bare `cargo run` omits this and skips the driver green.
IMG="$KDIR/target/virtio-blk-test.img"
dd if=/dev/zero of="$IMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create disk image"; exit 3; }

# A SECOND, PERSISTENT disk (REQ-STOR-003, ADR-038). The scratch disk above is reformatted by the
# destructive suites; this one is created ONCE and then kept, because the kernel is booted TWICE below.
# Boot 1 must create the store; boot 2 must FIND and verify what boot 1 wrote — the difference between
# "the OS can write" and "the OS remembers".
PIMG="$KDIR/target/virtio-blk-persistent.img"
rm -f "$PIMG"
dd if=/dev/zero of="$PIMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create persistent image"; exit 3; }

echo "==> booting in QEMU (120s watchdog, virtio-blk attached, -smp 4 for the SMP suite)"
OUT="$(perl -e 'alarm 120; exec @ARGV or die' \
  qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0)"
CODE=$?

echo "----------------------------------------"
echo "$OUT"
echo "----------------------------------------"
echo "vm exit code: $CODE"

fail=0
[ "$CODE" -eq 0 ] || { echo "FAIL: expected exit 0, got $CODE"; fail=1; }
echo "$OUT" | grep -q "ALL 11 INVARIANTS HOLD"        || { echo "FAIL: spine invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 21 MEMORY INVARIANTS HOLD"        || { echo "FAIL: memory invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 62 VIRTUAL-MEMORY INVARIANTS HOLD" || { echo "FAIL: virtual-memory invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 24 EL0-BOUNDARY INVARIANTS HOLD"  || { echo "FAIL: EL0 user-mode invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "VIRTIO-BLK INVARIANTS HOLD"    || { echo "FAIL: virtio-blk invariants marker missing (disk attached, driver must run)"; fail=1; }
echo "$OUT" | grep -q "SMP INVARIANTS HOLD"           || { echo "FAIL: SMP invariants marker missing (-smp 4 boot, suite must run)"; fail=1; }
echo "$OUT" | grep -q "ALL 15 FILESYSTEM INVARIANTS HOLD" || { echo "FAIL: filesystem invariants marker missing (REQ-FS-001)"; fail=1; }
# The virtio leg proves the namespace over the REAL device too: 5 driver invariants + the 12 fs ones.
echo "$OUT" | grep -q "ALL 20 VIRTIO-BLK INVARIANTS HOLD" || { echo "FAIL: virtio-blk count wrong (driver + filesystem over the real device)"; fail=1; }
echo "$OUT" | grep -q "ALL 4 NETWORK INVARIANTS HOLD" || { echo "FAIL: network invariants marker missing (REQ-NET-001; NIC attached, suite must run)"; fail=1; }
echo "$OUT" | grep -q "ALL 9 DURABLE-STORE INVARIANTS HOLD" || { echo "FAIL: durable-store invariants marker missing (REQ-STOR-003)"; fail=1; }
echo "$OUT" | grep -q "ALL 9 DMA-BOUNDARY INVARIANTS HOLD" || { echo "FAIL: DMA-boundary invariants marker missing (REQ-DRV-006)"; fail=1; }
echo "$OUT" | grep -q "\[e2e\] PASS"                  || { echo "FAIL: e2e PASS marker missing"; fail=1; }
echo "$OUT" | grep -q "PERSISTENT MEDIUM: boot #1, 0 entities verified" || { echo "FAIL: first boot did not create the durable store on the persistent medium"; fail=1; }

# ---- SECOND BOOT on the SAME persistent medium: the OS must REMEMBER (REQ-STOR-003) ----
echo "==> rebooting the same image against the SAME persistent disk (cross-reboot proof)"
OUT2="$(perl -e 'alarm 120; exec @ARGV or die' \
  qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0)"
CODE2=$?
echo "$OUT2" | grep -E "PERSISTENT MEDIUM" || true
echo "second boot exit code: $CODE2"
[ "$CODE2" -eq 0 ] || { echo "FAIL: second boot expected exit 0, got $CODE2"; fail=1; }
echo "$OUT2" | grep -q "PERSISTENT MEDIUM: boot #2, 1 entities verified" || { echo "FAIL: the OS did not remember across the reboot (boot #2 must verify boot #1's entity)"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "VM-E2E: PASS"
  exit 0
else
  echo "VM-E2E: FAIL"
  exit 1
fi
