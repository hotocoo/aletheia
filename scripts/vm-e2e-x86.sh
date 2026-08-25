#!/usr/bin/env bash
# End-to-end VM boot gate for the Aletheia x86-64 (AMD64) microkernel — the third first-class target
# at CI parity with scripts/vm-e2e.sh (aarch64) and scripts/vm-e2e-riscv.sh (RISC-V). Closes
# ALET-P0-001: x86-64 was described as first-class but had no equivalent automated boot gate, so its
# ring-3/syscall/timer/context-switch/paging code could regress while CI stayed green.
#
# Unlike the macOS-only kernel-x86_64/scripts/build-image.sh (hdiutil/diskutil), this drives the
# portable mtools image builder (kernel-x86_64/scripts/build-image-linux.sh) so the gate runs
# unprivileged on ubuntu-latest. It builds the .efi from HEAD (dropping any stale artifact — a
# stale .efi silently boots old code), assembles a FAT ESP image, boots it under QEMU+OVMF at
# -smp 4, and asserts exit 33 + every invariant-family marker. Exit 0 = PASS.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
X86="$ROOT/kernel-x86_64"

# Honor the per-crate nightly toolchain via the rustup shim (a Homebrew/system cargo earlier in
# PATH ignores rust-toolchain.toml and fails cross-compilation with E0463).
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi

echo "==> building x86-64 .efi from HEAD (dropping stale artifact)"
rm -f "$X86/target/x86_64-unknown-uefi/release/aletheia-kernel-x86_64.efi"

echo "==> assembling portable FAT ESP disk image (mtools)"
bash "$X86/scripts/build-image-linux.sh" || { echo "FAIL: image build"; echo "VM-E2E-X86: FAIL"; exit 1; }

echo "==> booting image in QEMU+OVMF (-smp 4, 90s watchdog)"
if bash "$X86/scripts/smoke-test.sh" "$X86/build/aletheia-x86_64.img"; then
  echo "VM-E2E-X86: PASS"
  exit 0
else
  echo "VM-E2E-X86: FAIL"
  exit 1
fi
