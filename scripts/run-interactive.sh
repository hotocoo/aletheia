#!/usr/bin/env bash
# Boot Aletheia and USE it (REQ-CON-001, ADR-044).
#
#   ./scripts/run-interactive.sh [aarch64|riscv64|x86_64]
#
# Builds the chosen target with `--features interactive` and boots it under QEMU with a persistent
# disk, so the machine comes up, runs its invariant suites, and then hands you a prompt:
#
#   aletheia> help
#   aletheia> write notes hello
#   aletheia> ls
#   aletheia> halt
#
# The disk is kept between runs (`kernel*/target/interactive-persistent.img`), so what you write is
# still there next time — delete that file to start from an empty namespace. Ctrl-A X quits QEMU.
#
# This is NOT a gate: an interactive session has no verdict. scripts/console-e2e.sh is the gate that
# drives the same build with a scripted operator and asserts the transcript.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$HOME/.cargo/bin/cargo" ]; then export PATH="$HOME/.cargo/bin:$PATH"; fi
TARGET="${1:-aarch64}"

make_disks() {
  # $1 = directory to keep them in. The scratch disk is recreated every run (the boot suites
  # reformat it); the persistent one is created once and then left alone.
  mkdir -p "$1"
  dd if=/dev/zero of="$1/interactive-scratch.img" bs=1048576 count=1 2>/dev/null
  [ -f "$1/interactive-persistent.img" ] \
    || dd if=/dev/zero of="$1/interactive-persistent.img" bs=1048576 count=1 2>/dev/null
}

case "$TARGET" in
  aarch64)
    DIR="$ROOT/kernel"; TRIPLE="aarch64-unknown-none-softfloat"; BIN="aletheia-kernel"
    ( cd "$DIR" && cargo build --features interactive ) || exit 3
    make_disks "$DIR/target"
    echo "==> booting aarch64 — type \`help\` at the prompt, \`halt\` to stop, Ctrl-A X to quit QEMU"
    exec qemu-system-aarch64 -machine virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 128M -nographic \
      -semihosting-config enable=on,target=native \
      -kernel "$DIR/target/$TRIPLE/debug/$BIN" \
      -global virtio-mmio.force-legacy=false \
      -drive "if=none,format=raw,file=$DIR/target/interactive-scratch.img,id=blk0" \
      -device virtio-blk-device,drive=blk0 \
      -drive "if=none,format=raw,file=$DIR/target/interactive-persistent.img,id=blk1" \
      -device virtio-blk-device,drive=blk1 \
      -netdev user,id=n0 -device virtio-net-device,netdev=n0
    ;;
  riscv64)
    DIR="$ROOT/kernel-riscv64"; TRIPLE="riscv64gc-unknown-none-elf"; BIN="aletheia-kernel-riscv64"
    ( cd "$DIR" && cargo build --features interactive ) || exit 3
    make_disks "$DIR/target"
    echo "==> booting riscv64 — type \`help\` at the prompt, \`halt\` to stop, Ctrl-A X to quit QEMU"
    exec qemu-system-riscv64 -machine virt -cpu rv64 -smp 4 -m 128M -nographic -bios default \
      -kernel "$DIR/target/$TRIPLE/debug/$BIN" \
      -global virtio-mmio.force-legacy=false \
      -drive "if=none,format=raw,file=$DIR/target/interactive-scratch.img,id=blk0" \
      -device virtio-blk-device,drive=blk0 \
      -drive "if=none,format=raw,file=$DIR/target/interactive-persistent.img,id=blk1" \
      -device virtio-blk-device,drive=blk1 \
      -netdev user,id=n0 -device virtio-net-device,netdev=n0
    ;;
  x86_64|x86-64|amd64)
    DIR="$ROOT/kernel-x86_64"
    IMG="$DIR/build/aletheia-x86_64-interactive.img"
    CARGO_FEATURES=interactive IMG="$IMG" bash "$DIR/scripts/build-image-linux.sh" || exit 3
    make_disks "$DIR/build"
    CODE=""
    for c in "${OVMF_CODE:-}" /opt/homebrew/share/qemu/edk2-x86_64-code.fd \
             /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd \
             /usr/share/edk2/x64/OVMF_CODE.4m.fd; do
      [ -n "$c" ] && [ -f "$c" ] && { CODE="$c"; break; }
    done
    VARSSRC=""
    for v in "${OVMF_VARS:-}" /opt/homebrew/share/qemu/edk2-i386-vars.fd \
             /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd \
             /usr/share/edk2/x64/OVMF_VARS.4m.fd; do
      [ -n "$v" ] && [ -f "$v" ] && { VARSSRC="$v"; break; }
    done
    [ -n "$CODE" ] && [ -n "$VARSSRC" ] || { echo "OVMF firmware not found — install ovmf or set OVMF_CODE/OVMF_VARS"; exit 1; }
    # OVMF rewrites its NVRAM, so each run gets its own copy of the template.
    cp "$VARSSRC" "$DIR/build/interactive-vars.fd"
    echo "==> booting x86-64 — type \`help\` at the prompt, \`halt\` to stop, Ctrl-A X to quit QEMU"
    exec qemu-system-x86_64 -machine q35 -m 256 -smp 4 -cpu qemu64,+smep -nographic \
      -drive "if=pflash,format=raw,unit=0,file=$CODE,readonly=on" \
      -drive "if=pflash,format=raw,unit=1,file=$DIR/build/interactive-vars.fd" \
      -drive "format=raw,file=$IMG" \
      -drive "if=none,format=raw,file=$DIR/build/interactive-scratch.img,id=blk0" \
      -device virtio-blk-pci,drive=blk0 \
      -drive "if=none,format=raw,file=$DIR/build/interactive-persistent.img,id=blk1" \
      -device virtio-blk-pci,drive=blk1 \
      -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
      -device isa-debug-exit,iobase=0xf4,iosize=0x04 -no-reboot
    ;;
  *)
    echo "usage: $0 [aarch64|riscv64|x86_64]"
    exit 2
    ;;
esac
