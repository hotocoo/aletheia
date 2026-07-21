#!/usr/bin/env bash
# One-command Oracle VirtualBox target for the Aletheia x86-64 image (macOS host).
#
#   build/aletheia-x86_64.img  ->  VDI  ->  a fully-scripted VBox VM with:
#     * EFI firmware (NOT legacy BIOS)  <- Aletheia boots \EFI\BOOT\BOOTX64.EFI
#     * SATA/AHCI boot disk
#     * serial port -> build/aletheia-serial.log (machine-checkable log)
#
# VirtualBox defaults to BIOS and cannot open the VMware .vmx, so this script does
# what the .vmx does for VMware: it provisions an equivalent VBox VM from scratch.
# Idempotent: re-running unregisters and rebuilds the VM and disk cleanly.
#
# Prereqs: run scripts/build-image.sh first (produces build/aletheia-x86_64.img),
# and have VBoxManage on PATH (VirtualBox installed).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" # kernel-x86_64/
BUILD="$HERE/build"
IMG="$BUILD/aletheia-x86_64.img"
VDI="$BUILD/aletheia-x86_64.vdi"
SERIAL_LOG="$BUILD/aletheia-serial.log"
VM_NAME="${VM_NAME:-Aletheia-x86_64}"
MEM_MB="${MEM_MB:-256}"
CPUS="${CPUS:-1}"

command -v VBoxManage >/dev/null 2>&1 || { echo "error: VBoxManage not on PATH (install VirtualBox)"; exit 1; }
[ -f "$IMG" ] || { echo "error: $IMG missing — run scripts/build-image.sh first"; exit 1; }

echo "==> [1/5] tear down any existing '$VM_NAME' VM + disk (idempotent)"
VBoxManage controlvm "$VM_NAME" poweroff >/dev/null 2>&1 || true
VBoxManage unregistervm "$VM_NAME" --delete >/dev/null 2>&1 || true
VBoxManage closemedium disk "$VDI" --delete >/dev/null 2>&1 || true
rm -f "$VDI"

echo "==> [2/5] convert raw .img -> VBox-native VDI"
VBoxManage convertfromraw "$IMG" "$VDI" --format VDI

echo "==> [3/5] create + register VM (Other 64-bit, EFI firmware)"
VBoxManage createvm --name "$VM_NAME" --ostype "Other_64" --register >/dev/null
VBoxManage modifyvm "$VM_NAME" \
  --firmware efi \
  --memory "$MEM_MB" \
  --cpus "$CPUS" \
  --graphicscontroller vmsvga \
  --nic1 none \
  --audio-enabled off

echo "==> [4/5] attach VDI on a SATA/AHCI controller"
VBoxManage storagectl "$VM_NAME" --name "SATA" --add sata --controller IntelAhci --portcount 1 --bootable on
VBoxManage storageattach "$VM_NAME" --storagectl "SATA" --port 0 --device 0 --type hdd --medium "$VDI"

echo "==> [5/5] wire serial port 1 -> $SERIAL_LOG"
rm -f "$SERIAL_LOG"
VBoxManage modifyvm "$VM_NAME" --uart1 0x3F8 4 --uartmode1 file "$SERIAL_LOG"

echo
echo "VM ready: $VM_NAME"
echo "  disk:   $VDI"
echo "  serial: $SERIAL_LOG"
echo
echo "launch (GUI):       VBoxManage startvm \"$VM_NAME\""
echo "launch (headless):  VBoxManage startvm \"$VM_NAME\" --type headless   # read serial log for output"
