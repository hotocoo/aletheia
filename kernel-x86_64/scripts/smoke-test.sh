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

# One boot, into the log path given. The NVRAM copy is per-boot (OVMF rewrites it), the disks are not:
# that is what makes the second boot a real reboot of the same machine rather than a fresh one.
boot_once() {
  local log="$1" vars="$2"
  : > "$log"
  cp "$VARSSRC" "$vars"
  qemu-system-x86_64 -machine q35 -m 256 -smp 4 \
    -cpu qemu64,+smep \
    -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$vars" \
    -drive format=raw,file="$IMG" \
    -drive if=none,format=raw,file="$SCRATCH",id=blk0 \
    -device virtio-blk-pci,drive=blk0 \
    -drive if=none,format=raw,file="$PERSIST",id=blk1 \
    -device virtio-blk-pci,drive=blk1 \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -serial file:"$log" -display none -no-reboot &
  local qpid=$!
  ( sleep 30; kill -9 "$qpid" 2>/dev/null ) &
  local wpid=$!
  wait "$qpid"; local rc=$?
  kill "$wpid" 2>/dev/null
  return $rc
}

boot_once "$LOG" "$VARS"; RC=$?

echo "==== serial log ===="
cat "$LOG"
echo "===================="
echo "QEMU exit code: $RC (expect 33)"

if [ "$RC" -eq 33 ] \
   && grep -q 'ALL 22 MEMORY INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 11 CAPABILITY-LIFETIME INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 20 RISK-ADVISOR INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 72 VIRTUAL-MEMORY INVARIANTS HOLD' "$LOG" \
   && grep -q 'kernel map built @' "$LOG" \
   && grep -q 'kernel map ACTIVE' "$LOG" \
   && grep -q 'live W\^X audit: .* 0 violations' "$LOG" \
   && grep -q 'SMP INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 34 RING-3 BOUNDARY INVARIANTS HOLD' "$LOG" \
   && grep -q 'TERMINATED (Fault(UserNotMapped)); system continues' "$LOG" \
   && grep -q 'ALL 15 FILESYSTEM INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 21 VIRTIO-BLK INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 9 DURABLE-STORE INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 5 NETWORK INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 9 DMA-BOUNDARY INVARIANTS HOLD' "$LOG" \
&& grep -q 'ALL 9 INPUT-RING INVARIANTS HOLD' "$LOG" \
&& grep -q 'ALL 40 CONSOLE INVARIANTS HOLD' "$LOG" \
   && grep -q 'PERSISTENT MEDIUM: boot #1, 0 entities verified' "$LOG" \
   && grep -q 'e2e\] PASS' "$LOG"; then
  # ---- SECOND BOOT against the SAME persistent disk: the OS must REMEMBER (REQ-STOR-003) ----
  echo "==> rebooting the same image against the SAME persistent disk (cross-reboot proof)"
  LOG2="$WORK/serial2.log"
  boot_once "$LOG2" "$WORK/vars2.fd"; RC2=$?
  grep -E 'PERSISTENT MEDIUM' "$LOG2" || true
  echo "second boot exit code: $RC2 (expect 33)"
  if [ "$RC2" -eq 33 ] \
     && grep -q 'PERSISTENT MEDIUM: boot #2, 1 entities verified' "$LOG2" \
     && grep -q 'e2e\] PASS' "$LOG2"; then
    echo "SMOKE TEST: PASS"
    rm -rf "$WORK"
    exit 0
  fi
  echo "SMOKE TEST: FAIL (the OS did not remember across the reboot)"
  rm -rf "$WORK"
  exit 1
fi
echo "SMOKE TEST: FAIL"
rm -rf "$WORK"
exit 1
