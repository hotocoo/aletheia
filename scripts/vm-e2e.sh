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

# Preflight. Without this the gate dies inside the perl watchdog with "Died at -e line 1." and 21
# marker-missing lines, which reads as a broken kernel rather than as a missing package. A tool the
# gate cannot run is reported by NAME, and the gate still FAILS (never a silent pass) — the point is
# a legible diagnosis, not an exemption.
command -v qemu-system-aarch64 >/dev/null 2>&1 || {
  echo "FAIL: qemu-system-aarch64 not found on PATH"
  echo "  install it: apt-get install -y qemu-system-arm"
  echo "VM-E2E: FAIL"
  exit 1
}

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
## The SMMUv3 rung's victim (ADR-074): a virtio-blk-pci function rides BEHIND the unit on
# the PCIe root complex (stream id = RID) - exactly what the live proofs kick under
# enforcement. Fresh every run: this gate reformats it by design.
PCIIMG="$KDIR/target/virtio-blk-pci-test.img"
dd if=/dev/zero of="$PCIIMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create pci disk image"; exit 3; }

PIMG="$KDIR/target/virtio-blk-persistent.img"
rm -f "$PIMG"
dd if=/dev/zero of="$PIMG" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create persistent image"; exit 3; }

# The custody anchor (ALET-P1-034, ADR-072): a DETERMINISTIC 32-byte root delivered OUTSIDE any
# disk the vault protects — over the firmware configuration channel, exactly as a real platform
# would provision it. Fixed bytes keep the gate reproducible; DELIVERY, not this demo anchor's
# secrecy, is what the gate proves.
ROOTBIN="$KDIR/target/capvault-root.bin"
printf 'aletheia-capvault-root-0123456789abcdef' | head -c 32 > "$ROOTBIN"
[ "$(wc -c < "$ROOTBIN")" -eq 32 ] || { echo "FAIL: root materialization"; exit 3; }

# The machine declares itself (ADR-074): dump the tree QEMU generates for THIS configuration,
# trim it to its declared total size, and hand it back over the firmware configuration channel
# - the same declared door the custody anchor uses. Direct -kernel ELF boots get NO register-
# level DTB pointer at all (measured: x0 reads 0 at the entry), so this channel is not a
# convenience - it is the only declaration the guest can discover.
DTBRAW="$KDIR/target/virt-dtb-raw.bin"
DTBT="$KDIR/target/virt-dtb.bin"
# `-global arm-smmuv3.stage=2`: QEMU 8.1..9.x create the virt machine's SMMUv3 as a STAGE-1-ONLY unit
# unless told otherwise (IDR0.S2P clear -> the kernel's probe refuses `Stage2Missing`, ADR-074), which
# is exactly how this gate went red on the ubuntu-24.04 runner (QEMU 8.2). Newer QEMU (10+/11)
# advertises both stages regardless and ignores the knob. The property has existed since 8.1, so this
# is safe on every emulator that has the stage-2 model at all.
qemu-system-aarch64 -machine virt,iommu=smmuv3,highmem-ecam=off,gic-version=2 -global arm-smmuv3.stage=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -drive if=none,format=raw,file="$PCIIMG",id=pciblk0 -device virtio-blk-pci,disable-legacy=on,drive=pciblk0 \
  -device virtio-gpu-device \
  -device virtio-keyboard-device -device virtio-tablet-device \
  -machine dumpdtb="$DTBRAW" >/dev/null 2>&1
[ -s "$DTBRAW" ] || { echo "FAIL: device-tree dump"; exit 3; }
set -- $(od -An -tu1 -j4 -N4 "$DTBRAW")
TSZ=$(( $1 << 24 | $2 << 16 | $3 << 8 | $4 ))
head -c "$TSZ" "$DTBRAW" > "$DTBT"
[ "$(wc -c < "$DTBT")" -eq "$TSZ" ] || { echo "FAIL: device-tree trim"; exit 3; }


echo "==> booting in QEMU (120s watchdog, virtio-blk attached, -smp 4 for the SMP suite)"
OUT="$(perl -e 'alarm 300; exec @ARGV or die' \
  qemu-system-aarch64 -machine virt,iommu=smmuv3,highmem-ecam=off,gic-version=2 -global arm-smmuv3.stage=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -drive if=none,format=raw,file="$PCIIMG",id=pciblk0 -device virtio-blk-pci,disable-legacy=on,drive=pciblk0 \
  -fw_cfg name=opt/org.aletheia/capvault-root,file="$ROOTBIN" \
  -fw_cfg name=opt/org.aletheia/dtb,file="$DTBT" \
  -device virtio-gpu-device \
  -device virtio-keyboard-device -device virtio-tablet-device)"
CODE=$?

echo "----------------------------------------"
echo "$OUT"
echo "----------------------------------------"
echo "vm exit code: $CODE"

fail=0
[ "$CODE" -eq 0 ] || { echo "FAIL: expected exit 0, got $CODE"; fail=1; }
echo "$OUT" | grep "ALL 13 INVARIANTS HOLD" >/dev/null        || { echo "FAIL: spine invariants marker missing"; fail=1; }
echo "$OUT" | grep "ALL 14 CAPABILITY-LIFETIME INVARIANTS HOLD" >/dev/null || { echo "FAIL: capability-lifetime invariants marker missing (REQ-CAP-008)"; fail=1; }
echo "$OUT" | grep "ALL 9 IOMMU-CONTRACT INVARIANTS HOLD" >/dev/null || { echo "FAIL: iommu-contract invariants marker missing (ALET-P1-018, ADR-071)"; fail=1; }
# Frequency is authority, heat is a hard ceiling (ALET-P2-022, ADR-076): the OC band needs a
# live grant, the envelope is absolute, trips clamp and cool, the governor never overclocks.
echo "$OUT" | grep "ALL 14 POWER-PERFORMANCE INVARIANTS HOLD" >/dev/null || { echo "FAIL: power-performance invariants marker missing (ALET-P2-022, ADR-076)"; fail=1; }
# Pixels are authority, the scanout is a hard bound (ALET-P2-021, ADR-077): owner tokens,
# exact clipping, painter's z-order, size-honest buffers, and a zero-write quiet frame.
echo "$OUT" | grep "ALL 14 COMPOSITION-CONTRACT INVARIANTS HOLD" >/dev/null || { echo "FAIL: composition-contract invariants marker missing (ALET-P2-021, ADR-077)"; fail=1; }
# The composed frame reaches the display device: real backing pages, one flush per changed
# frame, NOTHING on a quiet one (ALET-P2-021, ADR-078).
echo "$OUT" | grep "ALL 8 REAL-PIXEL COMPOSITION INVARIANTS HOLD" >/dev/null || { echo "FAIL: real-pixel composition invariants marker missing (ALET-P2-021, ADR-078)"; fail=1; }
# Focus is authority, the cursor is the compositor's own (ALET-P2-021, ADR-079).
echo "$OUT" | grep "ALL 13 INPUT-ROUTING INVARIANTS HOLD" >/dev/null || { echo "FAIL: input-routing invariants marker missing (ALET-P2-021, ADR-079)"; fail=1; }
# The input HARDWARE rung (ALET-P2-021, ADR-080): real virtio-input devices through the
# session - identity read back and pinned, DMA-gated queues, armed silence measured, and the
# decode->route path the live desktop pumps driven end to end.
echo "$OUT" | grep "ALL 6 TEXT-GRID INVARIANTS HOLD" >/dev/null || { echo "FAIL: text-grid invariants marker missing (ALET-P2-021, ADR-083)"; fail=1; }
echo "$OUT" | grep "ALL 10 INPUT-HARDWARE INVARIANTS HOLD" >/dev/null || { echo "FAIL: input-hardware invariants marker missing (ALET-P2-021, ADR-080)"; fail=1; }
echo "$OUT" | grep "ALL 10 SMMUV3 INVARIANTS HOLD" >/dev/null || { echo "FAIL: smmuv3 invariants marker missing (ALET-P1-018, ADR-074)"; fail=1; }
echo "$OUT" | grep "enforcement LIVE" >/dev/null || { echo "FAIL: smmuv3 enforcement never turned ON"; fail=1; }
# Custody crosses the platform boundary (ALET-P1-034, ADR-072): the firmware-delivered root must
# open, seal, reopen, rotate, rekey, and refuse every named impostor — on THIS machine.
echo "$OUT" | grep "ALL 14 CUSTODY-DELIVERY INVARIANTS HOLD" >/dev/null || { echo "FAIL: custody-delivery invariants marker missing (ALET-P1-034, ADR-072)"; fail=1; }
echo "$OUT" | grep "platform custody: root DELIVERED over firmware configuration" >/dev/null || { echo "FAIL: the platform did not deliver the custody anchor"; fail=1; }
echo "$OUT" | grep "ALL 22 RISK-ADVISOR INVARIANTS HOLD" >/dev/null || { echo "FAIL: risk-advisor invariants marker missing (REQ-ML-001, ADR-056)"; fail=1; }
# The forest under load: cost measured on this machine, and the properties that must hold at
# any scale. A model that is only verified on 256 fixture rows is verified at a scale no scheduler
# ever runs at (REQ-ML-002, ADR-056).
echo "$OUT" | grep "ALL 8 STRESS INVARIANTS HOLD" >/dev/null || { echo "FAIL: risk-advisor stress markers missing (REQ-ML-002, ADR-056)"; fail=1; }
echo "$OUT" | grep -E "abstaining workload: [0-9]+ tasks, 0 positions move" >/dev/null || { echo "FAIL: an abstaining model moved a scheduling position (ADR-056 fallback broken)"; fail=1; }
echo "$OUT" | grep "ALL 21 MEMORY INVARIANTS HOLD" >/dev/null        || { echo "FAIL: memory invariants marker missing"; fail=1; }
echo "$OUT" | grep "ALL 66 VIRTUAL-MEMORY INVARIANTS HOLD" >/dev/null || { echo "FAIL: virtual-memory invariants marker missing"; fail=1; }
echo "$OUT" | grep "ALL 32 EL0-BOUNDARY INVARIANTS HOLD" >/dev/null  || { echo "FAIL: EL0 user-mode invariants marker missing"; fail=1; }
echo "$OUT" | grep "VIRTIO-BLK INVARIANTS HOLD" >/dev/null    || { echo "FAIL: virtio-blk invariants marker missing (disk attached, driver must run)"; fail=1; }
echo "$OUT" | grep "SMP INVARIANTS HOLD" >/dev/null           || { echo "FAIL: SMP invariants marker missing (-smp 4 boot, suite must run)"; fail=1; }
echo "$OUT" | grep "ALL 15 FILESYSTEM INVARIANTS HOLD" >/dev/null || { echo "FAIL: filesystem invariants marker missing (REQ-FS-001)"; fail=1; }
# The virtio leg proves the namespace over the REAL device too: 5 driver invariants + the 12 fs ones.
echo "$OUT" | grep "ALL 21 VIRTIO-BLK INVARIANTS HOLD" >/dev/null || { echo "FAIL: virtio-blk count wrong (driver + filesystem over the real device)"; fail=1; }
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
echo "$OUT" | grep "ALL 9 DMA-BOUNDARY INVARIANTS HOLD" >/dev/null || { echo "FAIL: DMA-boundary invariants marker missing (REQ-DRV-006)"; fail=1; }
echo "$OUT" | grep "ALL 9 INPUT-RING INVARIANTS HOLD" >/dev/null || { echo "FAIL: input-ring invariants marker missing (REQ-CON-002)"; fail=1; }
echo "$OUT" | grep "ALL 42 CONSOLE INVARIANTS HOLD" >/dev/null || { echo "FAIL: console invariants marker missing (REQ-CON-001)"; fail=1; }
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
# against an expected map declared HERE. The per-family greps above stay as named diagnoses; this
# closes the two holes they cannot see: a family vanishing from the boot entirely, and a count
# changing without the gate being told. Extra families fail too — new suites join this map
# deliberately. Measured on this target (ADR-061); identical to the RISC-V gate's map by design.
source "$ROOT/scripts/lib-markers.sh"
AARCH64_EXPECTED="bench=12 cap=14 compose=8 compositor=14 conring=9 console=42 dma=9 fbcon=6 fs=15 gpu=13 input=13 iommu=9 keys=12 mlrisk-stress=8 mlrisk=22 mlsched=17 mm=21 net=9 persist=10 pm=14 reclaim=9 selftest=13 smp=22 soak=12 textgrid=6 usermode=32 smmu=10 vault=14 vinput=10 virtio=21 vm=66"
if ! printf '%s\n' "$OUT" | markers_assert "$AARCH64_EXPECTED"; then fail=1; fi

echo "$OUT" | grep "\[e2e\] PASS" >/dev/null                  || { echo "FAIL: e2e PASS marker missing"; fail=1; }
echo "$OUT" | grep "PERSISTENT MEDIUM: boot #1, 0 entities verified" >/dev/null || { echo "FAIL: first boot did not create the durable store on the persistent medium"; fail=1; }

# ---- SECOND BOOT on the SAME persistent medium: the OS must REMEMBER (REQ-STOR-003) ----
echo "==> rebooting the same image against the SAME persistent disk (cross-reboot proof)"
OUT2="$(perl -e 'alarm 300; exec @ARGV or die' \
  qemu-system-aarch64 -machine virt,iommu=smmuv3,highmem-ecam=off,gic-version=2 -global arm-smmuv3.stage=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -drive if=none,format=raw,file="$PCIIMG",id=pciblk0 -device virtio-blk-pci,disable-legacy=on,drive=pciblk0 \
  -fw_cfg name=opt/org.aletheia/capvault-root,file="$ROOTBIN" \
  -fw_cfg name=opt/org.aletheia/dtb,file="$DTBT" \
  -device virtio-gpu-device \
  -device virtio-keyboard-device -device virtio-tablet-device)"
CODE2=$?
echo "$OUT2" | grep -E "PERSISTENT MEDIUM" || true
echo "second boot exit code: $CODE2"
[ "$CODE2" -eq 0 ] || { echo "FAIL: second boot expected exit 0, got $CODE2"; fail=1; }
echo "$OUT2" | grep "PERSISTENT MEDIUM: boot #2, 1 entities verified" >/dev/null || { echo "FAIL: the OS did not remember across the reboot (boot #2 must verify boot #1's entity)"; fail=1; }

# ---- THIRD BOOT, platform silent: custody refuses FAIL-CLOSED, machine continues ----
# Without the firmware item there is NO root anywhere — not on disk, not compiled in. The vault
# stays sealed BY NAME while every other subsystem keeps working (ADR-072).
echo "==> third boot WITHOUT the firmware root (absence must be a named refusal, not a crash)"
OUT3="$(perl -e 'alarm 300; exec @ARGV or die' \
  qemu-system-aarch64 -machine virt,iommu=smmuv3,highmem-ecam=off,gic-version=2 -global arm-smmuv3.stage=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
  -semihosting-config enable=on,target=native -kernel "$ELF" \
  -global virtio-mmio.force-legacy=false \
  -drive if=none,format=raw,file="$IMG",id=blk0 -device virtio-blk-device,drive=blk0 \
  -drive if=none,format=raw,file="$PIMG",id=blk1 -device virtio-blk-device,drive=blk1 \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  -drive if=none,format=raw,file="$PCIIMG",id=pciblk0 -device virtio-blk-pci,disable-legacy=on,drive=pciblk0 \
  -fw_cfg name=opt/org.aletheia/dtb,file="$DTBT" \
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
  echo "VM-E2E: PASS"
  exit 0
else
  echo "VM-E2E: FAIL"
  exit 1
fi
