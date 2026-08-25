#!/usr/bin/env bash
# Automated boot smoke test: boot the Aletheia x86-64 disk image under QEMU + OVMF (UEFI) at
# -smp 4 (the SMP suite must run, not skip) and assert the kernel reached its end-to-end PASS.
# PASS criteria:
#   - QEMU process exit code 33  (kernel isa-debug-exit encodes success 0 as value 0x10)
#   - serial log contains "[e2e] PASS" + all four invariant-family markers (memory, vm, SMP, ring-3)
#   - the kernel built its OWN address map, MADE IT LIVE (CR3), and the live tree has ZERO W^X
#     violations (ALET-P1-031) — the whole suite after that point runs on the kernel's own tree
# A 30s watchdog guards against a hang (triple fault / no exit). Exit 0 = PASS, 1 = FAIL.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMG="${1:-$HERE/build/aletheia-x86_64.img}"
[ -f "$IMG" ] || { echo "missing image: $IMG (run scripts/build-image.sh first)"; exit 1; }

# Locate OVMF/edk2 UEFI firmware across hosts: macOS (Homebrew qemu) and Linux/CI (Debian/Ubuntu
# `ovmf` package). CODE is the read-only firmware; VARSSRC is a writable NVRAM template we copy.
# Override with OVMF_CODE / OVMF_VARS to point at a custom build.
QSHARE="$(brew --prefix qemu 2>/dev/null)/share/qemu"
CODE=""
for c in "${OVMF_CODE:-}" \
         "$QSHARE/edk2-x86_64-code.fd" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
         /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
         /usr/share/OVMF/OVMF_CODE.secboot.fd /usr/share/edk2/x64/OVMF_CODE.4m.fd \
         /usr/share/qemu/edk2-x86_64-code.fd; do
  [ -n "$c" ] && [ -f "$c" ] && { CODE="$c"; break; }
done
VARSSRC=""
for v in "${OVMF_VARS:-}" \
         "$QSHARE/edk2-i386-vars.fd" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
         /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd \
         /usr/share/edk2/x64/OVMF_VARS.4m.fd /usr/share/qemu/edk2-i386-vars.fd; do
  [ -n "$v" ] && [ -f "$v" ] && { VARSSRC="$v"; break; }
done
[ -n "$CODE" ]    || { echo "OVMF firmware (CODE) not found — install ovmf (apt-get install -y ovmf) or set OVMF_CODE"; exit 1; }
[ -n "$VARSSRC" ] || { echo "OVMF NVRAM (VARS) template not found — install ovmf or set OVMF_VARS"; exit 1; }

WORK="$(mktemp -d)"
VARS="$WORK/vars.fd"
LOG="$WORK/serial.log"
cp "$VARSSRC" "$VARS"
: > "$LOG"

# A SECOND, scratch disk on the virtio-pci bus (REQ-DRV-005, ADR-037): 1 MiB = 2048 sectors = 256
# 4 KiB blocks, so the shared driver suite runs the journal + the whole filesystem namespace over a
# real device on this target too. The boot disk above stays untouched — the kernel never writes to the
# medium it booted from.
SCRATCH="$WORK/virtio-blk-test.img"
dd if=/dev/zero of="$SCRATCH" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create scratch disk"; exit 1; }

# A THIRD disk, PERSISTENT (REQ-STOR-003, ADR-038): created once and kept across the TWO boots below.
# Boot 1 creates the durable store on it; boot 2 must FIND and verify what boot 1 wrote — the difference
# between "the OS can write" and "the OS remembers". The scratch disk is reformatted by the suites; the
# boot medium is never written at all.
PERSIST="$WORK/virtio-blk-persistent.img"
dd if=/dev/zero of="$PERSIST" bs=1048576 count=1 2>/dev/null || { echo "FAIL: create persistent disk"; exit 1; }

# The custody anchor (ALET-P1-034, ADR-072): a DETERMINISTIC 32-byte root delivered over the
# firmware configuration ioports, outside every disk the vault protects. Fixed bytes keep the
# gate reproducible; DELIVERY is what is proved here, not this demo anchor's secrecy.
ROOTBIN="$WORK/capvault-root.bin"
printf 'aletheia-capvault-root-0123456789abcdef' | head -c 32 > "$ROOTBIN"
[ "$(wc -c < "$ROOTBIN")" -eq 32 ] || { echo "FAIL: create custody anchor"; exit 1; }

# One boot, into the log path given. The NVRAM copy is per-boot (OVMF rewrites it), the disks are not:
# that is what makes the second boot a real reboot of the same machine rather than a fresh one.
boot_once() {
  local log="$1" vars="$2" root="${3:-}"
  local fwargs=""
  if [ -n "$root" ]; then
    fwargs="-fw_cfg name=opt/org.aletheia/capvault-root,file=$root"
  fi
  : > "$log"
  cp "$VARSSRC" "$vars"
  # iommu_platform=on + disable-legacy=on on every virtio device: the device then REQUIRES
  # VIRTIO_F_IOMMU_PLATFORM, which both proves the driver negotiates it and routes the device's
  # DMA through the VT-d unit the [dmar] suite programs (ADR-073). Modern-only is required for
  # the flag, and intel-iommu must precede the devices it serves.
  qemu-system-x86_64 -machine q35 -m 256 -smp 4 \
    -cpu qemu64,+smep \
    -device intel-iommu \
    -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$vars" \
    -drive format=raw,file="$IMG" \
    -drive if=none,format=raw,file="$SCRATCH",id=blk0 \
    -device virtio-blk-pci,drive=blk0,disable-legacy=on,iommu_platform=on \
    -drive if=none,format=raw,file="$PERSIST",id=blk1 \
    -device virtio-blk-pci,drive=blk1,disable-legacy=on,iommu_platform=on \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0,disable-legacy=on,iommu_platform=on \
    -device virtio-gpu-pci,disable-legacy=on,iommu_platform=on \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    $fwargs \
    -serial file:"$log" -display none -no-reboot &
  local qpid=$!
  # 90s: the boot runs every suite PLUS the live VT-d probes, whose bounded device-kick timeouts
  # and MMIO fault-status polling each cost real seconds under TCG. The guard exists to catch
  # HANGS (triple fault / silent wedge), not to bound slow-but-progressing boots.
  ( sleep 90; kill -9 "$qpid" 2>/dev/null ) &
  local wpid=$!
  wait "$qpid"; local rc=$?
  kill "$wpid" 2>/dev/null
  return $rc
}

boot_once "$LOG" "$VARS" "$ROOTBIN"; RC=$?

echo "==== serial log ===="
cat "$LOG"
echo "===================="
echo "QEMU exit code: $RC (expect 33)"

if [ "$RC" -eq 33 ] \
   && grep -q 'ALL 22 MEMORY INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 14 CAPABILITY-LIFETIME INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 14 CUSTODY-DELIVERY INVARIANTS HOLD' "$LOG" \
   && grep -q 'platform custody: root DELIVERED over firmware configuration' "$LOG" \
   && grep -q 'ALL 9 IOMMU-CONTRACT INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 12 VT-D INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 22 RISK-ADVISOR INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 8 STRESS INVARIANTS HOLD' "$LOG" \
   && grep -qE 'abstaining workload: [0-9]+ tasks, 0 positions move' "$LOG" \
   && grep -q 'ALL 72 VIRTUAL-MEMORY INVARIANTS HOLD' "$LOG" \
   && grep -q 'kernel map built @' "$LOG" \
   && grep -q 'kernel map ACTIVE' "$LOG" \
   && grep -q 'live W\^X audit: .* 0 violations' "$LOG" \
   && grep -q 'SMP INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 39 RING-3 BOUNDARY INVARIANTS HOLD' "$LOG" \
   && grep -q 'TERMINATED (Fault(UserNotMapped)); system continues' "$LOG" \
   && grep -q 'ALL 15 FILESYSTEM INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 21 VIRTIO-BLK INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 10 DURABLE-STORE INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 12 SOAK INVARIANTS HOLD' "$LOG" \
   && grep -qE '\[soak\] journal: [0-9]+ txs .* => [0-9]+ tx/s' "$LOG" \
   && grep -q 'ALL 12 BENCHMARK INVARIANTS HOLD' "$LOG" \
   && grep -qE '\[bench\] authority: [0-9]+ checks \| ' "$LOG" \
   && grep -q 'ALL 9 NETWORK INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 13 VIRTIO-GPU INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 6 FRAMEBUFFER-CONSOLE INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 9 DMA-BOUNDARY INVARIANTS HOLD' "$LOG" \
&& grep -q 'ALL 9 INPUT-RING INVARIANTS HOLD' "$LOG" \
&& grep -q 'ALL 42 CONSOLE INVARIANTS HOLD' "$LOG" \
&& grep -q 'ALL 12 LIVE-ADVISORY INVARIANTS HOLD' "$LOG" \
&& grep -q '\[mlsched\] RESIDENT:' "$LOG" \
&& grep -q 'advised drain is a permutation of the model-free one' "$LOG" \
   && grep -q 'PERSISTENT MEDIUM: boot #1, 0 entities verified' "$LOG" \
   && grep -q 'e2e\] PASS' "$LOG"; then
  # ---- STRUCTURED MARKER MAP (ALET-P2-007, REQ-QUAL-004, ADR-061): the whole family/count ----
  # surface held against an expected map declared HERE — measured on this target. The per-family
  # greps above stay as named diagnoses; this closes what they cannot see: a family vanishing from
  # the boot, or a count changing without the gate being told. Extra families fail too.
  # shellcheck disable=SC1091
  source "$HERE/../scripts/lib-markers.sh"
  X86_EXPECTED="bench=12 cap=14 conring=9 console=42 dma=9 dmar=12 fbcon=6 fs=15 gpu=13 iommu=9 keys=12 mlrisk-stress=8 mlrisk=22 mlsched=12 mm=22 net=9 persist=10 ps2=5 selftest=13 smp=22 soak=12 usermode=39 vault=14 virtio=21 vm=72"
  if ! markers_assert "$X86_EXPECTED" < "$LOG"; then
    echo "SMOKE TEST: FAIL (structured marker map)"
    exit 1
  fi
  # ---- SECOND BOOT against the SAME persistent disk: the OS must REMEMBER (REQ-STOR-003) ----
  echo "==> rebooting the same image against the SAME persistent disk (cross-reboot proof)"
  LOG2="$WORK/serial2.log"
  boot_once "$LOG2" "$WORK/vars2.fd" "$ROOTBIN"; RC2=$?
  grep -E 'PERSISTENT MEDIUM' "$LOG2" || true
  echo "second boot exit code: $RC2 (expect 33)"
  if [ "$RC2" -eq 33 ] \
     && grep -q 'PERSISTENT MEDIUM: boot #2, 1 entities verified' "$LOG2" \
     && grep -q 'e2e\] PASS' "$LOG2"; then
    # ---- THIRD BOOT, platform silent: custody refuses FAIL-CLOSED, machine continues ----
    echo "==> third boot WITHOUT the firmware root (absence must be a named refusal, not a crash)"
    LOG3="$WORK/serial3.log"
    boot_once "$LOG3" "$WORK/vars3.fd"; RC3=$?
    grep -E '\[vault\]' "$LOG3" || true
    echo "third boot exit code: $RC3 (expect 33)"
    if [ "$RC3" -eq 33 ] \
       && grep -q 'PLATFORM ROOT ABSENT (RootNotProvided)' "$LOG3" \
       && grep -q 'PERSISTENT MEDIUM: boot #3,' "$LOG3" \
       && grep -q 'e2e\] PASS' "$LOG3"; then
      echo "SMOKE TEST: PASS"
      rm -rf "$WORK"
      exit 0
    fi
    echo "SMOKE TEST: FAIL (absent platform root was not refused by name, or the machine did not continue)"
    rm -rf "$WORK"
    exit 1
  fi
  echo "SMOKE TEST: FAIL (the OS did not remember across the reboot)"
  rm -rf "$WORK"
  exit 1
fi
echo "SMOKE TEST: FAIL"
rm -rf "$WORK"
exit 1
