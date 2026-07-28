#!/usr/bin/env bash
# Portable (Linux/CI) Aletheia x86-64 bootable disk image — mtools only, no host root, no loop
# devices, no macOS hdiutil/diskutil.
#
#   cargo build (.efi)  ->  raw FAT32 volume  ->  install \EFI\BOOT\BOOTX64.EFI
#
# The image is a single FAT filesystem written directly onto the whole raw disk (no GPT). UEFI
# firmware (OVMF in QEMU) treats a partition-less FAT disk as a removable-media ESP and boots the
# fallback path \EFI\BOOT\BOOTX64.EFI — so a GPT/partition table is not required to boot. This is
# the same artifact the macOS `build-image.sh` produces functionally, built with tooling that
# exists unprivileged on ubuntu-latest (`apt-get install -y mtools`), so the x86-64 boot gate can
# run in CI at parity with the aarch64/RISC-V legs (ALET-P0-001).
#
# Dependencies: the Rust nightly toolchain (via the rustup shim — see PATH note below) and mtools
# (mformat/mmd/mcopy). Produces build/aletheia-x86_64.img.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" # kernel-x86_64/
BUILD="$HERE/build"
IMG="$BUILD/aletheia-x86_64.img"
SIZE_MB="${SIZE_MB:-64}"
mkdir -p "$BUILD"

# Use the rustup shim, not a Homebrew/system `cargo`, so rust-toolchain.toml (nightly +
# x86_64-unknown-uefi) is honored. A Homebrew `cargo` earlier in PATH silently builds for the host
# triple and fails cross-compilation with E0463 "can't find crate for core".
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

for tool in mformat mmd mcopy; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing $tool (install mtools: apt-get install -y mtools)"; exit 1; }
done

echo "==> [1/3] build kernel .efi (x86_64-unknown-uefi)"
PROFILE="${PROFILE:-release}"
EFI="${EFI:-$HERE/target/x86_64-unknown-uefi/$PROFILE/aletheia-kernel-x86_64.efi}"
if [ ! -f "$EFI" ]; then
  BUILD_FLAGS=""; [ "$PROFILE" = "release" ] && BUILD_FLAGS="--release"
  ( cd "$HERE" && cargo build $BUILD_FLAGS )
fi
[ -f "$EFI" ] || { echo "missing $EFI"; exit 1; }
echo "    using .efi: $EFI"

echo "==> [2/3] create ${SIZE_MB}MiB raw FAT32 volume"
rm -f "$IMG"
dd if=/dev/zero of="$IMG" bs=1048576 count="$SIZE_MB" status=none 2>/dev/null \
  || dd if=/dev/zero of="$IMG" bs=1m count="$SIZE_MB" 2>/dev/null
# -F: force FAT32. -v ALETHEIA: volume label. `::` addresses the whole image as drive letter.
mformat -i "$IMG" -F -v ALETHEIA ::

echo "==> [3/3] install BOOTX64.EFI onto the ESP"
mmd -i "$IMG" ::/EFI
mmd -i "$IMG" ::/EFI/BOOT
mcopy -i "$IMG" "$EFI" ::/EFI/BOOT/BOOTX64.EFI

echo
echo "built: $IMG"
echo "verify: kernel-x86_64/scripts/smoke-test.sh $IMG"
