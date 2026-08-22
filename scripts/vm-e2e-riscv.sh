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
# Honor the per-crate nightly toolchain via the rustup shim. A Homebrew/system `cargo` earlier in
# PATH ignores rust-toolchain.toml and builds for the host triple, failing the cross build with
# E0463 "can't find crate for core" — which surfaces here only as "FAIL: build".
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
KDIR="$ROOT/kernel-riscv64"
TARGET="riscv64gc-unknown-none-elf"
ELF="$KDIR/target/$TARGET/debug/aletheia-kernel-riscv64"

cd "$KDIR" || { echo "FAIL: no kernel-riscv64 dir"; exit 3; }

# Preflight. Without this the gate dies inside the perl watchdog with "Died at -e line 1." and 21
# marker-missing lines, which reads as a broken kernel rather than as a missing package. A tool the
# gate cannot run is reported by NAME, and the gate still FAILS (never a silent pass) — the point is
# a legible diagnosis, not an exemption.
command -v qemu-system-riscv64 >/dev/null 2>&1 || {
  echo "FAIL: qemu-system-riscv64 not found on PATH"
  echo "  install it: apt-get install -y qemu-system-riscv (on current Ubuntu the riscv64 emulator is no
#   longer part of qemu-system-misc)"
  echo "VM-E2E (riscv64): FAIL"
  exit 1
}

echo "==> building riscv64 kernel"
cargo build || { echo "FAIL: build"; exit 3; }

# Attach a real virtio-blk device (REQ-DRV-004, ADR-036): a fresh 1 MiB raw backing image
# (2048 sectors = 256 4 KiB blocks) so this FIRST-CLASS target drives a real transport, reads
# capacity, and runs the journal + the filesystem namespace over emulated storage. Bare `cargo run`
# omits this and skips the driver green.
IMG="$KDIR/target/virtio-blk-test.img"
dd if=/dev/zero of="$IMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create disk image"; exit 3; }

# A SECOND, PERSISTENT disk (REQ-STOR-003, ADR-038): created ONCE and kept, because the kernel is booted
# TWICE below. Boot 1 creates the durable store; boot 2 must FIND and verify what boot 1 wrote.
PIMG="$KDIR/target/virtio-blk-persistent.img"
rm -f "$PIMG"
dd if=/dev/zero of="$PIMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create persistent image"; exit 3; }

echo "==> booting in QEMU riscv64 'virt' + OpenSBI (120s watchdog, virtio-blk attached, -smp 4 for the SMP suite)"
OUT="$(perl -e 'alarm 120; exec @ARGV or die' \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic \
  -bios default -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -device virtio-gpu-device)"
CODE=$?

echo "----------------------------------------"
echo "$OUT"
echo "----------------------------------------"
echo "vm exit code: $CODE"

fail=0
[ "$CODE" -eq 0 ] || { echo "FAIL: expected exit 0, got $CODE"; fail=1; }
echo "$OUT" | grep -q "S->M boundary OK"              || { echo "FAIL: SBI boundary marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 13 INVARIANTS HOLD"        || { echo "FAIL: invariants marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 14 CAPABILITY-LIFETIME INVARIANTS HOLD" || { echo "FAIL: capability-lifetime invariants marker missing (REQ-CAP-008)"; fail=1; }
echo "$OUT" | grep -q "ALL 22 RISK-ADVISOR INVARIANTS HOLD" || { echo "FAIL: risk-advisor invariants marker missing (REQ-ML-001, ADR-056)"; fail=1; }
# The forest under load: cost measured on this machine, and the properties that must hold at
# any scale. A model that is only verified on 256 fixture rows is verified at a scale no scheduler
# ever runs at (REQ-ML-002, ADR-056).
echo "$OUT" | grep -q "ALL 8 STRESS INVARIANTS HOLD" || { echo "FAIL: risk-advisor stress markers missing (REQ-ML-002, ADR-056)"; fail=1; }
echo "$OUT" | grep -qE "abstaining workload: [0-9]+ tasks, 0 positions move" || { echo "FAIL: an abstaining model moved a scheduling position (ADR-056 fallback broken)"; fail=1; }
echo "$OUT" | grep -q "ALL 21 MEMORY INVARIANTS HOLD"  || { echo "FAIL: memory-management marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 66 VIRTUAL-MEMORY INVARIANTS HOLD" || { echo "FAIL: virtual-memory marker missing"; fail=1; }
echo "$OUT" | grep -q "ALL 32 USER-MODE BOUNDARY INVARIANTS HOLD" || { echo "FAIL: user-mode marker missing"; fail=1; }
echo "$OUT" | grep -q "SMP INVARIANTS HOLD"           || { echo "FAIL: SMP invariants marker missing (-smp 4 boot, suite must run)"; fail=1; }
echo "$OUT" | grep -q "ALL 15 FILESYSTEM INVARIANTS HOLD" || { echo "FAIL: filesystem invariants marker missing (REQ-FS-001)"; fail=1; }
# 5 driver invariants + the 12 filesystem behaviors, all over the REAL device (REQ-DRV-004).
echo "$OUT" | grep -q "ALL 21 VIRTIO-BLK INVARIANTS HOLD" || { echo "FAIL: virtio-blk invariants marker missing (disk attached, driver must run)"; fail=1; }
echo "$OUT" | grep -q "ALL 9 NETWORK INVARIANTS HOLD" || { echo "FAIL: network invariants marker missing (REQ-NET-001/003; NIC attached, suite must run)"; fail=1; }
# Graphics over the REAL device (REQ-GFX-001): display info + the whole 2D resource lifecycle.
echo "$OUT" | grep -q "ALL 13 VIRTIO-GPU INVARIANTS HOLD" || { echo "FAIL: virtio-gpu invariants marker missing (REQ-GFX-001; GPU attached, suite must run)"; fail=1; }
# The framebuffer console renders text into REAL backing pages and hands the frame over; detach revokes (REQ-GFX-002).
echo "$OUT" | grep -q "ALL 6 FRAMEBUFFER-CONSOLE INVARIANTS HOLD" || { echo "FAIL: framebuffer-console invariants marker missing (REQ-GFX-002)"; fail=1; }
echo "$OUT" | grep -q "ALL 10 DURABLE-STORE INVARIANTS HOLD" || { echo "FAIL: durable-store invariants marker missing (REQ-STOR-003)"; fail=1; }
echo "$OUT" | grep -q "ALL 9 INPUT-RING INVARIANTS HOLD" || { echo "FAIL: input-ring invariants marker missing (REQ-CON-002)"; fail=1; }
echo "$OUT" | grep -q "ALL 42 CONSOLE INVARIANTS HOLD" || { echo "FAIL: console invariants marker missing (REQ-CON-001)"; fail=1; }
echo "$OUT" | grep -q "ALL 9 DMA-BOUNDARY INVARIANTS HOLD" || { echo "FAIL: DMA-boundary invariants marker missing (REQ-DRV-006)"; fail=1; }
# The advisor must be RESIDENT and CONSULTED on this booted machine, not merely verified
# (REQ-ML-003, ADR-056): the live-path invariants hold, a model is actually resident, the
# commissioning workload really went through it, and the advised drain was a permutation of the
# model-free one -- advice reordered equals and did nothing else.
echo "$OUT" | grep -q "ALL 12 LIVE-ADVISORY INVARIANTS HOLD" || { echo "FAIL: live-advisory invariants marker missing (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep -q "\[mlsched\] RESIDENT:"            || { echo "FAIL: no model took up residence (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep -qE "\[mlsched\] commissioning: [0-9]+ tasks admitted over [0-9]+ s" || { echo "FAIL: the resident advisor was never consulted by a workload (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep -q "advised drain is a permutation of the model-free one" || { echo "FAIL: the advised drain was not proved a permutation of the model-free one (INV-014)"; fail=1; }
echo "$OUT" | grep -q "risk advisor: RESIDENT"             || { echo "FAIL: mlstat did not report a resident advisor (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep -q "\[e2e\] PASS"                  || { echo "FAIL: e2e PASS marker missing"; fail=1; }
echo "$OUT" | grep -q "PERSISTENT MEDIUM: boot #1, 0 entities verified" || { echo "FAIL: first boot did not create the durable store on the persistent medium"; fail=1; }

# ---- SECOND BOOT on the SAME persistent medium: the OS must REMEMBER (REQ-STOR-003) ----
echo "==> rebooting the same kernel against the SAME persistent disk (cross-reboot proof)"
OUT2="$(perl -e 'alarm 120; exec @ARGV or die' \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic \
  -bios default -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -device virtio-gpu-device)"
CODE2=$?
echo "$OUT2" | grep -E "PERSISTENT MEDIUM" || true
echo "second boot exit code: $CODE2"
[ "$CODE2" -eq 0 ] || { echo "FAIL: second boot expected exit 0, got $CODE2"; fail=1; }
echo "$OUT2" | grep -q "PERSISTENT MEDIUM: boot #2, 1 entities verified" || { echo "FAIL: the OS did not remember across the reboot (boot #2 must verify boot #1's entity)"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "VM-E2E (riscv64): PASS"
  exit 0
else
  echo "VM-E2E (riscv64): FAIL"
  exit 1
fi
