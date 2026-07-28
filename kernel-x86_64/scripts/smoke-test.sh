#!/usr/bin/env bash
# Automated boot smoke test: boot the Aletheia x86-64 disk image under QEMU + OVMF (UEFI) at
# -smp 4 (the SMP suite must run, not skip) and assert the kernel reached its end-to-end PASS.
# PASS criteria:
#   - QEMU process exit code 33  (kernel isa-debug-exit encodes success 0 as value 0x10)
#   - serial log contains "[e2e] PASS" + all four invariant-family markers (memory, vm, SMP, ring-3)
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

qemu-system-x86_64 -machine q35 -m 256 -smp 4 \
  -drive if=pflash,format=raw,unit=0,file="$CODE",readonly=on \
  -drive if=pflash,format=raw,unit=1,file="$VARS" \
  -drive format=raw,file="$IMG" \
  -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
  -serial file:"$LOG" -display none -no-reboot &
QPID=$!
( sleep 30; kill -9 "$QPID" 2>/dev/null ) &
WPID=$!
wait "$QPID"; RC=$?
kill "$WPID" 2>/dev/null

echo "==== serial log ===="
cat "$LOG"
echo "===================="
echo "QEMU exit code: $RC (expect 33)"

if [ "$RC" -eq 33 ] \
   && grep -q 'MEMORY INVARIANTS HOLD' "$LOG" \
   && grep -q 'ALL 13 VIRTUAL-MEMORY INVARIANTS HOLD' "$LOG" \
   && grep -q 'SMP INVARIANTS HOLD' "$LOG" \
   && grep -q 'RING-3 BOUNDARY INVARIANTS HOLD' "$LOG" \
   && grep -q 'e2e\] PASS' "$LOG"; then
  echo "SMOKE TEST: PASS"
  rm -rf "$WORK"
  exit 0
fi
echo "SMOKE TEST: FAIL"
rm -rf "$WORK"
exit 1
