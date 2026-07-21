#!/usr/bin/env bash
# Reproducible Aletheia x86-64 bootable disk image (macOS host).
#
#   cargo build (.efi)  ->  GPT disk w/ FAT32 ESP  ->  install \EFI\BOOT\BOOTX64.EFI
#   ->  patch partition type to EFI System Partition  ->  raw .img + VMware .vmdk
#
# No external image tooling (no mtools/xorriso/grub) — only the Rust toolchain, macOS
# hdiutil/diskutil, python3, and qemu-img. Produces build/aletheia-x86_64.{img,vmdk}.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" # kernel-x86_64/
BUILD="$HERE/build"
IMG="$BUILD/aletheia-x86_64.img"
VMDK="$BUILD/aletheia-x86_64.vmdk"
SIZE_MB="${SIZE_MB:-64}"
mkdir -p "$BUILD"

echo "==> [1/5] build kernel .efi (x86_64-unknown-uefi)"
( cd "$HERE" && cargo +nightly build )
EFI="$HERE/target/x86_64-unknown-uefi/debug/aletheia-kernel-x86_64.efi"
[ -f "$EFI" ] || { echo "missing $EFI"; exit 1; }

echo "==> [2/5] create ${SIZE_MB}MiB GPT disk with a FAT32 partition"
rm -f "$IMG"
dd if=/dev/zero of="$IMG" bs=1m count="$SIZE_MB" status=none
DISK="$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$IMG" | head -n1 | awk '{print $1}')"
cleanup() { hdiutil detach "$DISK" >/dev/null 2>&1 || true; }
trap cleanup EXIT
diskutil partitionDisk "$DISK" GPT "MS-DOS FAT32" ALETHEIA 100% >/dev/null

echo "==> [3/5] install BOOTX64.EFI onto the ESP"
VOL="/Volumes/ALETHEIA"
mkdir -p "$VOL/EFI/BOOT"
cp "$EFI" "$VOL/EFI/BOOT/BOOTX64.EFI"
sync
diskutil unmount "$VOL" >/dev/null
cleanup
trap - EXIT

echo "==> [4/5] set partition type -> EFI System Partition"
python3 "$HERE/scripts/set-esp-type.py" "$IMG"

echo "==> [5/5] convert raw image -> VMware VMDK"
rm -f "$VMDK"
qemu-img convert -f raw -O vmdk "$IMG" "$VMDK"

echo
echo "built: $IMG"
echo "built: $VMDK"
echo "verify: kernel-x86_64/scripts/smoke-test.sh"
