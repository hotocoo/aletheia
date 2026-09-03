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

# The custody anchor (ALET-P1-034, ADR-072): a DETERMINISTIC 32-byte root delivered over the
# firmware configuration channel, outside every disk the vault protects. Fixed bytes keep the
# gate reproducible; DELIVERY is what is proved here, not this demo anchor's secrecy.
ROOTBIN="$KDIR/target/capvault-root.bin"
printf 'aletheia-capvault-root-0123456789abcdef' | head -c 32 > "$ROOTBIN"
[ "$(wc -c < "$ROOTBIN")" -eq 32 ] || { echo "FAIL: root materialization"; exit 3; }

echo "==> booting in QEMU riscv64 'virt' + OpenSBI (120s watchdog, virtio-blk attached, -smp 4 for the SMP suite)"
OUT="$(perl -e 'alarm 300; exec @ARGV or die' \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic \
  -bios default -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -fw_cfg name=opt/org.aletheia/capvault-root,file="$ROOTBIN" \
  -device virtio-gpu-device \
  -device virtio-keyboard-device -device virtio-tablet-device)"
CODE=$?

echo "----------------------------------------"
echo "$OUT"
echo "----------------------------------------"
echo "vm exit code: $CODE"

fail=0
[ "$CODE" -eq 0 ] || { echo "FAIL: expected exit 0, got $CODE"; fail=1; }
echo "$OUT" | grep "S->M boundary OK" >/dev/null              || { echo "FAIL: SBI boundary marker missing"; fail=1; }
echo "$OUT" | grep "ALL 13 INVARIANTS HOLD" >/dev/null        || { echo "FAIL: invariants marker missing"; fail=1; }
echo "$OUT" | grep "ALL 14 CAPABILITY-LIFETIME INVARIANTS HOLD" >/dev/null || { echo "FAIL: capability-lifetime invariants marker missing (REQ-CAP-008)"; fail=1; }
echo "$OUT" | grep "ALL 9 IOMMU-CONTRACT INVARIANTS HOLD" >/dev/null || { echo "FAIL: iommu-contract invariants marker missing (ALET-P1-018, ADR-071)"; fail=1; }
# Pixels are authority, the scanout is a hard bound (ALET-P2-021, ADR-077).
echo "$OUT" | grep "ALL 14 COMPOSITION-CONTRACT INVARIANTS HOLD" >/dev/null || { echo "FAIL: composition-contract invariants marker missing (ALET-P2-021, ADR-077)"; fail=1; }
# The composed frame reaches the display device: real backing pages, one flush per changed
# frame, NOTHING on a quiet one (ALET-P2-021, ADR-078).
echo "$OUT" | grep "ALL 8 REAL-PIXEL COMPOSITION INVARIANTS HOLD" >/dev/null || { echo "FAIL: real-pixel composition invariants marker missing (ALET-P2-021, ADR-078)"; fail=1; }
# Focus is authority, the cursor is the compositor's own (ALET-P2-021, ADR-079).
echo "$OUT" | grep "ALL 13 INPUT-ROUTING INVARIANTS HOLD" >/dev/null || { echo "FAIL: input-routing invariants marker missing (ALET-P2-021, ADR-079)"; fail=1; }
# The input HARDWARE rung (ALET-P2-021, ADR-080): real virtio-input devices through the session.
echo "$OUT" | grep "ALL 7 TEXT-GRID INVARIANTS HOLD" >/dev/null || { echo "FAIL: text-grid invariants marker missing (ALET-P2-021, ADR-083)"; fail=1; }
echo "$OUT" | grep "ALL 12 WINDOW-MANAGER INVARIANTS HOLD" >/dev/null || { echo "FAIL: window-manager invariants marker missing (ALET-P2-021, ADR-084)"; fail=1; }
echo "$OUT" | grep "ALL 6 WINDOW-STORM INVARIANTS HOLD" >/dev/null || { echo "FAIL: window-storm invariants marker missing (REQ-QUAL-007, ADR-086)"; fail=1; }
echo "$OUT" | grep "ALL 5 SCHEDULER-STORM INVARIANTS HOLD" >/dev/null || { echo "FAIL: scheduler-storm invariants marker missing (REQ-QUAL-007, ADR-087)"; fail=1; }
echo "$OUT" | grep "ALL 5 FILESYSTEM-STORM INVARIANTS HOLD" >/dev/null || { echo "FAIL: filesystem-storm invariants marker missing (REQ-QUAL-007, ADR-088)"; fail=1; }
# The desktop this CPU actually RUNS (ADR-085): the shared desktop, on this target's own devices,
# with both managed windows up. A machine that only PROVES the contracts is not a machine that
# shows them, so the gate holds the live line, not just the suites.
echo "$OUT" | grep "\[desktop\] LIVE: .* 2 managed windows" >/dev/null || { echo "FAIL: the live desktop did not come up with its two windows (ALET-P2-021, ADR-085)"; fail=1; }
echo "$OUT" | grep "ALL 10 INPUT-HARDWARE INVARIANTS HOLD" >/dev/null || { echo "FAIL: input-hardware invariants marker missing (ALET-P2-021, ADR-080)"; fail=1; }
# Custody crosses the platform boundary (ALET-P1-034, ADR-072), proved over the SECOND bus.
echo "$OUT" | grep "ALL 14 CUSTODY-DELIVERY INVARIANTS HOLD" >/dev/null || { echo "FAIL: custody-delivery invariants marker missing (ALET-P1-034, ADR-072)"; fail=1; }
echo "$OUT" | grep "platform custody: root DELIVERED over firmware configuration" >/dev/null || { echo "FAIL: the platform did not deliver the custody anchor"; fail=1; }
echo "$OUT" | grep "ALL 22 RISK-ADVISOR INVARIANTS HOLD" >/dev/null || { echo "FAIL: risk-advisor invariants marker missing (REQ-ML-001, ADR-056)"; fail=1; }
# The forest under load: cost measured on this machine, and the properties that must hold at
# any scale. A model that is only verified on 256 fixture rows is verified at a scale no scheduler
# ever runs at (REQ-ML-002, ADR-056).
echo "$OUT" | grep "ALL 8 STRESS INVARIANTS HOLD" >/dev/null || { echo "FAIL: risk-advisor stress markers missing (REQ-ML-002, ADR-056)"; fail=1; }
echo "$OUT" | grep -E "abstaining workload: [0-9]+ tasks, 0 positions move" >/dev/null || { echo "FAIL: an abstaining model moved a scheduling position (ADR-056 fallback broken)"; fail=1; }
echo "$OUT" | grep "ALL 21 MEMORY INVARIANTS HOLD" >/dev/null  || { echo "FAIL: memory-management marker missing"; fail=1; }
echo "$OUT" | grep "ALL 66 VIRTUAL-MEMORY INVARIANTS HOLD" >/dev/null || { echo "FAIL: virtual-memory marker missing"; fail=1; }
echo "$OUT" | grep "ALL 32 USER-MODE BOUNDARY INVARIANTS HOLD" >/dev/null || { echo "FAIL: user-mode marker missing"; fail=1; }
echo "$OUT" | grep "SMP INVARIANTS HOLD" >/dev/null           || { echo "FAIL: SMP invariants marker missing (-smp 4 boot, suite must run)"; fail=1; }
echo "$OUT" | grep "ALL 15 FILESYSTEM INVARIANTS HOLD" >/dev/null || { echo "FAIL: filesystem invariants marker missing (REQ-FS-001)"; fail=1; }
# 5 driver invariants + the 12 filesystem behaviors, all over the REAL device (REQ-DRV-004).
echo "$OUT" | grep "ALL 21 VIRTIO-BLK INVARIANTS HOLD" >/dev/null || { echo "FAIL: virtio-blk invariants marker missing (disk attached, driver must run)"; fail=1; }
echo "$OUT" | grep "ALL 9 NETWORK INVARIANTS HOLD" >/dev/null || { echo "FAIL: network invariants marker missing (REQ-NET-001/003; NIC attached, suite must run)"; fail=1; }
# Graphics over the REAL device (REQ-GFX-001): display info + the whole 2D resource lifecycle.
echo "$OUT" | grep "ALL 13 VIRTIO-GPU INVARIANTS HOLD" >/dev/null || { echo "FAIL: virtio-gpu invariants marker missing (REQ-GFX-001; GPU attached, suite must run)"; fail=1; }
# The framebuffer console renders text into REAL backing pages and hands the frame over; detach revokes (REQ-GFX-002).
echo "$OUT" | grep "ALL 6 FRAMEBUFFER-CONSOLE INVARIANTS HOLD" >/dev/null || { echo "FAIL: framebuffer-console invariants marker missing (REQ-GFX-002)"; fail=1; }
echo "$OUT" | grep "ALL 10 DURABLE-STORE INVARIANTS HOLD" >/dev/null || { echo "FAIL: durable-store invariants marker missing (REQ-STOR-003)"; fail=1; }
# Long-running soak (ALET-P2-009, ADR-063): lifecycles under repetition — journal transactions,
# namespace mutations, capability grants, task generations — proved on this machine, with its own
# heap meter holding the allocation-free claim exactly and its own clock reporting throughput.
echo "$OUT" | grep "ALL 12 SOAK INVARIANTS HOLD" >/dev/null || { echo "FAIL: soak invariants marker missing (ALET-P2-009, ADR-063)"; fail=1; }
echo "$OUT" | grep -E "\[soak\] journal: [0-9]+ txs .* => [0-9]+ tx/s" >/dev/null || { echo "FAIL: the soak never measured journal throughput on this machine"; fail=1; }
# The machine measures itself (ALET-P2-010, ADR-064, REQ-PERF-002): structural benchmark gates on
# THIS machine, with its measured costs REPORTED beside them.
echo "$OUT" | grep "ALL 12 BENCHMARK INVARIANTS HOLD" >/dev/null || { echo "FAIL: benchmark invariants marker missing (ALET-P2-010, ADR-064, REQ-PERF-002)"; fail=1; }
echo "$OUT" | grep -E "\[bench\] authority: [0-9]+ checks \\| " >/dev/null || { echo "FAIL: the machine never measured its own authority-check cost (REQ-PERF-002)"; fail=1; }
echo "$OUT" | grep "ALL 9 INPUT-RING INVARIANTS HOLD" >/dev/null || { echo "FAIL: input-ring invariants marker missing (REQ-CON-002)"; fail=1; }
echo "$OUT" | grep "ALL 42 CONSOLE INVARIANTS HOLD" >/dev/null || { echo "FAIL: console invariants marker missing (REQ-CON-001)"; fail=1; }
echo "$OUT" | grep "ALL 9 DMA-BOUNDARY INVARIANTS HOLD" >/dev/null || { echo "FAIL: DMA-boundary invariants marker missing (REQ-DRV-006)"; fail=1; }
# The advisor must be RESIDENT and CONSULTED on this booted machine, not merely verified
# (REQ-ML-003, ADR-056): the live-path invariants hold, a model is actually resident, the
# commissioning workload really went through it, and the advised drain was a permutation of the
# model-free one -- advice reordered equals and did nothing else.
# The memory boundary (ADR-081): the allocator's own reading reached the resident service before
# anything was admitted through it, and commissioning was refused nothing.
echo "$OUT" | grep "\[mlsched\] memory: .* frames free - bounded admission ON" >/dev/null || { echo "FAIL: the allocator's reading never reached the resident advisor (ADR-081)"; fail=1; }
echo "$OUT" | grep "commissioning: .*, 0 refused at the memory boundary" >/dev/null || { echo "FAIL: commissioning was refused at the memory boundary (ADR-081)"; fail=1; }
echo "$OUT" | grep "ALL 17 LIVE-ADVISORY INVARIANTS HOLD" >/dev/null || { echo "FAIL: live-advisory invariants marker missing (REQ-ML-003, ADR-056)"; fail=1; }
# Reclaim under pressure (REQ-ML-005 wired, ADR-082): the policy suite, and the REAL storm on this
# machine's allocator that entered pressure and came back with every frame.
echo "$OUT" | grep "ALL 9 RECLAIM INVARIANTS HOLD" >/dev/null || { echo "FAIL: reclaim invariants marker missing (REQ-ML-005, ADR-082)"; fail=1; }
echo "$OUT" | grep "storm: pressure entered and cleared, every frame back EXACTLY" >/dev/null || { echo "FAIL: the reclaim storm did not come back (ADR-082)"; fail=1; }
true || { echo "FAIL: live-advisory invariants marker missing (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep "\[mlsched\] RESIDENT:" >/dev/null            || { echo "FAIL: no model took up residence (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep -E "\[mlsched\] commissioning: [0-9]+ tasks admitted over [0-9]+ s" >/dev/null || { echo "FAIL: the resident advisor was never consulted by a workload (REQ-ML-003)"; fail=1; }
echo "$OUT" | grep "advised drain is a permutation of the model-free one" >/dev/null || { echo "FAIL: the advised drain was not proved a permutation of the model-free one (INV-014)"; fail=1; }
echo "$OUT" | grep "risk advisor: RESIDENT" >/dev/null             || { echo "FAIL: mlstat did not report a resident advisor (REQ-ML-003)"; fail=1; }

# ---- STRUCTURED MARKER MAP (ALET-P2-007, REQ-QUAL-004): the whole family/count surface, held ----
# against an expected map declared HERE. Measured on this target; IDENTICAL to the aarch64 gate's
# map by design — the same arch-independent suites must prove the same counts over either bus, and
# a divergence here is exactly what this assertion exists to catch. See scripts/lib-markers.sh.
source "$ROOT/scripts/lib-markers.sh"
RISCV_EXPECTED="bench=12 cap=14 compose=8 compositor=14 conring=9 console=42 dma=9 fbcon=6 fs=15 fsstorm=5 gpu=13 input=13 iommu=9 keys=12 mlrisk-stress=8 mlrisk=22 mlsched=17 mm=21 net=9 persist=10 pm=14 reclaim=9 selftest=13 smp=22 soak=12 textgrid=7 schedstorm=5 wm=12 wmstorm=6 usermode=32 vault=14 vinput=10 virtio=21 vm=66"
if ! printf '%s\n' "$OUT" | markers_assert "$RISCV_EXPECTED"; then fail=1; fi

echo "$OUT" | grep "\[e2e\] PASS" >/dev/null                  || { echo "FAIL: e2e PASS marker missing"; fail=1; }
echo "$OUT" | grep "PERSISTENT MEDIUM: boot #1, 0 entities verified" >/dev/null || { echo "FAIL: first boot did not create the durable store on the persistent medium"; fail=1; }

# ---- SECOND BOOT on the SAME persistent medium: the OS must REMEMBER (REQ-STOR-003) ----
echo "==> rebooting the same kernel against the SAME persistent disk (cross-reboot proof)"
OUT2="$(perl -e 'alarm 300; exec @ARGV or die' \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic \
  -bios default -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -fw_cfg name=opt/org.aletheia/capvault-root,file="$ROOTBIN" \
  -device virtio-gpu-device \
  -device virtio-keyboard-device -device virtio-tablet-device)"
CODE2=$?
echo "$OUT2" | grep -E "PERSISTENT MEDIUM" || true
echo "second boot exit code: $CODE2"
[ "$CODE2" -eq 0 ] || { echo "FAIL: second boot expected exit 0, got $CODE2"; fail=1; }
echo "$OUT2" | grep "PERSISTENT MEDIUM: boot #2, 1 entities verified" >/dev/null || { echo "FAIL: the OS did not remember across the reboot (boot #2 must verify boot #1's entity)"; fail=1; }

# ---- THIRD BOOT, platform silent: custody refuses FAIL-CLOSED, machine continues ----
# Without the firmware item there is NO root anywhere — not on disk, not compiled in. The vault
# must stay sealed BY NAME while every other subsystem keeps working (ADR-072).
echo "==> third boot WITHOUT the firmware root (absence must be a named refusal, not a crash)"
OUT3="$(perl -e 'alarm 300; exec @ARGV or die' \
  qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic \
  -bios default -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -device virtio-gpu-device \
  -device virtio-keyboard-device -device virtio-tablet-device)"
CODE3=$?
echo "$OUT3" | grep -E "\[vault\]" || true
echo "third boot exit code: $CODE3"
[ "$CODE3" -eq 0 ] || { echo "FAIL: third boot expected exit 0, got $CODE3"; fail=1; }
echo "$OUT3" | grep "PLATFORM ROOT ABSENT (RootNotProvided)" >/dev/null || { echo "FAIL: absent platform root was not refused by name"; fail=1; }
echo "$OUT3" | grep "PERSISTENT MEDIUM: boot #3," >/dev/null || { echo "FAIL: third boot did not witness the durable store"; fail=1; }
echo "$OUT3" | grep "\[e2e\] PASS" >/dev/null || { echo "FAIL: third boot did not reach e2e PASS (one sealed vault must not kill the machine)"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "VM-E2E (riscv64): PASS"
  exit 0
else
  echo "VM-E2E (riscv64): FAIL"
  exit 1
fi
