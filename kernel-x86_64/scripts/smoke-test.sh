#!/usr/bin/env bash
# Automated boot smoke test: boot the Aletheia x86-64 disk image under QEMU + OVMF (UEFI) and
# assert the kernel reached its end-to-end PASS. PASS criteria:
#   - QEMU process exit code 33  (kernel isa-debug-exit encodes success 0 as value 0x10)
#   - serial log contains "[e2e] PASS"
# A 30s watchdog guards against a hang (triple fault / no exit). Exit 0 = PASS, 1 = FAIL.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMG="${1:-$HERE/build/aletheia-x86_64.img}"
[ -f "$IMG" ] || { echo "missing image: $IMG (run scripts/build-image.sh first)"; exit 1; }

QSHARE="$(brew --prefix qemu 2>/dev/null)/share/qemu"
[ -d "$QSHARE" ] || QSHARE=/opt/homebrew/share/qemu
CODE="$QSHARE/edk2-x86_64-code.fd"
VARSSRC="$QSHARE/edk2-i386-vars.fd"
[ -f "$CODE" ] || { echo "OVMF firmware not found: $CODE"; exit 1; }

WORK="$(mktemp -d)"
VARS="$WORK/vars.fd"
LOG="$WORK/serial.log"
cp "$VARSSRC" "$VARS"
: > "$LOG"

qemu-system-x86_64 -machine q35 -m 256 \
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

if [ "$RC" -eq 33 ] && grep -q 'e2e\] PASS' "$LOG"; then
  echo "SMOKE TEST: PASS"
  rm -rf "$WORK"
  exit 0
fi
echo "SMOKE TEST: FAIL"
rm -rf "$WORK"
exit 1
